//! Command-line surface and startup wiring.
//!
//! This is where the pieces meet: parse flags, load config, resolve a session,
//! start MCP servers, register subagents, then hand off to either the TUI or
//! the headless runner. Keeping it in one place means the two front ends are
//! provably built from the same agent.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, subagent};
use crate::config::{self, CliOverrides, PermissionMode};
use crate::mcp;
use crate::session::{Session, SessionStore};
use crate::tools::ToolRegistry;

#[derive(Parser, Debug)]
#[command(
    name = "grok",
    version,
    about = "An agentic coding harness for xAI's Grok models",
    long_about = None
)]
pub struct Cli {
    /// Prompt to run without the interactive UI. Reads stdin when given `-`.
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub print: Option<String>,

    /// Model to use, e.g. grok-4-1-fast-non-reasoning.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Permission mode: default, acceptEdits, plan, bypassPermissions.
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,

    /// Approve every action without asking. Equivalent to
    /// `--mode bypassPermissions`, and only meaningful with `--print`.
    #[arg(long)]
    pub yes: bool,

    /// Directory to work in. Defaults to the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Resume a session by id or unique id prefix.
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,

    /// Resume the most recent session in this directory.
    #[arg(short = 'c', long)]
    pub r#continue: bool,

    /// Override the API base URL. Mostly useful for testing against a mock.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// Cap the tokens in each response.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<u32>,

    /// Write debug logs to this file.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List saved sessions for this directory.
    Sessions,
    /// Print the resolved configuration and exit.
    Config,
    /// List available tools and exit.
    Tools,
    /// Check that the API key and endpoint work.
    Doctor,
}

/// Parse arguments and run.
pub async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load .env before reading any environment-derived config.
    dotenvy::dotenv().ok();
    init_logging(cli.log_file.as_deref());

    let workspace = resolve_workspace(cli.cwd.as_deref())?;

    let mode = match (&cli.mode, cli.yes) {
        (Some(raw), _) => Some(
            PermissionMode::parse(raw)
                .with_context(|| format!("`{raw}` is not a permission mode"))?,
        ),
        (None, true) => Some(PermissionMode::BypassPermissions),
        (None, false) => None,
    };

    let overrides = CliOverrides {
        model: cli.model.clone(),
        base_url: cli.base_url.clone(),
        permission_mode: mode,
        max_tokens: cli.max_tokens,
    };
    let config = config::load(&workspace, &overrides)?;

    match cli.command {
        Some(Command::Sessions) => return print_sessions(&workspace),
        Some(Command::Config) => return print_config(&config),
        Some(Command::Tools) => return print_tools(&config),
        Some(Command::Doctor) => return doctor(&config).await,
        None => {}
    }

    if config.api_key.trim().is_empty() {
        bail!(
            "XAI_API_KEY is not set.\n\n\
             Get a key from https://console.x.ai/ then either export it:\n\
             \x20   export XAI_API_KEY=...\n\
             or put it in a .env file in this directory:\n\
             \x20   XAI_API_KEY=..."
        );
    }

    let store = SessionStore::for_workspace(&workspace);
    let session = resolve_session(&cli, &config, store.as_ref())?;

    // Warn rather than fail: a machine with no writable home can still work.
    if store.is_none() {
        tracing::warn!("no home directory; sessions will not be saved");
    }

    let cancel = CancellationToken::new();
    let mut tools = ToolRegistry::with_builtins();

    // MCP servers are started before the agent so their tools are advertised
    // from the first request.
    let (servers, failures) = mcp::connect_all(&config.mcp_servers).await;
    for server in &servers {
        for tool in server.as_tools() {
            tools.register(tool);
        }
    }

    // The subagent tool needs an event channel; the front end replaces this
    // sender with its own once it starts.
    let (bootstrap_tx, _bootstrap_rx) = mpsc::channel(64);
    let definitions = subagent::discover(&workspace);
    subagent::register_if_available(&mut tools, &definitions, &config, bootstrap_tx)?;

    let mut agent = Agent::new(config, session, tools, cancel)?;

    match cli.print {
        Some(prompt) => {
            let prompt = read_prompt(&prompt)?;
            crate::headless::run(&mut agent, &prompt, &failures).await
        }
        None => {
            // Without a terminal, raw mode fails with a bare "Device not
            // configured", which tells the user nothing. Catch it here, where
            // the actionable alternative is known.
            use std::io::IsTerminal;
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                bail!(
                    "the interactive interface needs a terminal.\n\n\
                     For scripts and pipelines use non-interactive mode instead:\n\
                     \x20   grok -p \"your prompt\"\n\
                     \x20   echo \"your prompt\" | grok -p -"
                );
            }
            for (name, error) in &failures {
                tracing::warn!(server = %name, %error, "MCP server unavailable");
            }
            crate::tui::run(agent, store).await
        }
    }
}

/// `-p -` reads the prompt from stdin, so grok can sit in a pipeline.
fn read_prompt(raw: &str) -> Result<String> {
    if raw != "-" {
        return Ok(raw.to_string());
    }
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer).context("reading the prompt from stdin")?;
    if buffer.trim().is_empty() {
        bail!("no prompt on stdin");
    }
    Ok(buffer)
}

fn resolve_workspace(requested: Option<&std::path::Path>) -> Result<PathBuf> {
    let dir = match requested {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    // Canonicalize once, here, so every later path check compares like with
    // like. A workspace reached through a symlink would otherwise make every
    // sandbox check fail.
    dir.canonicalize().with_context(|| format!("resolving {}", dir.display()))
}

fn resolve_session(cli: &Cli, config: &config::Config, store: Option<&SessionStore>) -> Result<Session> {
    let restore = |summary: crate::session::SessionSummary| -> Result<Session> {
        Session::load(&summary.path)
            .with_context(|| format!("resuming session {}", summary.id))
    };

    if let Some(id) = &cli.resume {
        let Some(store) = store else { bail!("cannot resume: no session directory available") };
        let Some(found) = store.find(id) else {
            bail!("no session matching `{id}`. Run `grok sessions` to list them.")
        };
        return restore(found);
    }

    if cli.r#continue {
        let Some(store) = store else { bail!("cannot continue: no session directory available") };
        let Some(found) = store.most_recent() else {
            bail!("no earlier sessions in this directory")
        };
        return restore(found);
    }

    Ok(match store {
        Some(store) => store.create(&config.workspace, &config.model),
        None => Session::in_memory(&uuid::Uuid::new_v4().to_string(), &config.model),
    })
}

fn init_logging(path: Option<&std::path::Path>) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("GROK_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));

    // Logging must never go to stdout or stderr in TUI mode: it would scribble
    // over the alternate screen. Without a log file, drop the output.
    match path {
        Some(path) => {
            if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_ansi(false)
                    .try_init();
            }
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::sink)
                .try_init();
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn print_sessions(workspace: &std::path::Path) -> Result<()> {
    let Some(store) = SessionStore::for_workspace(workspace) else {
        println!("No session directory available.");
        return Ok(());
    };
    let sessions = store.list();
    if sessions.is_empty() {
        println!("No sessions for {}", workspace.display());
        return Ok(());
    }
    println!("{:<10}  {:<7}  {:<26}  {}", "ID", "MSGS", "MODEL", "TITLE");
    for session in sessions {
        println!(
            "{:<10}  {:<7}  {:<26}  {}",
            &session.id[..8.min(session.id.len())],
            session.message_count,
            session.model,
            session.label()
        );
    }
    Ok(())
}

fn print_config(config: &config::Config) -> Result<()> {
    println!("workspace          {}", config.workspace.display());
    println!("model              {}", config.model);
    println!("base_url           {}", config.base_url);
    println!("permission_mode    {}", config.permission_mode.as_str());
    println!("context_window     {}", config.context_window());
    println!("compact_at         {}", config.compact_at());
    println!("max_tool_iterations {}", config.max_tool_iterations);
    println!("theme              {}", config.theme);
    // Never print the key itself.
    println!("api_key            {}", if config.api_key.is_empty() { "not set" } else { "set" });

    if !config.permissions.allow.is_empty() {
        println!("\nallow");
        for rule in &config.permissions.allow {
            println!("  {rule}");
        }
    }
    if !config.permissions.deny.is_empty() {
        println!("\ndeny");
        for rule in &config.permissions.deny {
            println!("  {rule}");
        }
    }
    if !config.mcp_servers.is_empty() {
        println!("\nmcp servers");
        for (name, server) in &config.mcp_servers {
            println!("  {name:<18} {} {}", server.command, server.args.join(" "));
        }
    }
    Ok(())
}

fn print_tools(config: &config::Config) -> Result<()> {
    let registry = ToolRegistry::with_builtins();
    for tool in registry.iter() {
        println!("{:<18} {:?}", tool.name(), tool.kind());
    }
    let agents = subagent::discover(&config.workspace);
    if !agents.is_empty() {
        println!("\nsubagents");
        for agent in agents {
            println!("  {:<16} {}", agent.name, agent.description);
        }
    }
    Ok(())
}

/// Verify the key and endpoint with one cheap round trip.
async fn doctor(config: &config::Config) -> Result<()> {
    println!("workspace   {}", config.workspace.display());
    println!("model       {}", config.model);
    println!("endpoint    {}", config.base_url);

    if config.api_key.trim().is_empty() {
        println!("\nXAI_API_KEY is not set.");
        println!("Get a key from https://console.x.ai/ and export it, or put it in .env");
        bail!("no API key");
    }
    println!("api key     set ({} chars)", config.api_key.len());

    print!("\nchecking the API… ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let client = crate::api::ApiClient::new(&config.api_key, &config.base_url)?;
    let options = crate::api::RequestOptions {
        model: config.model.clone(),
        max_tokens: Some(16),
        ..Default::default()
    };
    match client
        .complete(
            vec![crate::api::Message::user("Reply with the single word: ok")],
            vec![],
            &options,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(completion) => {
            println!("ok");
            println!(
                "response    {:?}",
                completion.message.content.unwrap_or_default().trim()
            );
            println!("tokens      {} in, {} out", completion.usage.prompt_tokens, completion.usage.completion_tokens);
            Ok(())
        }
        Err(e) => {
            println!("failed");
            println!("\n{e}");
            bail!("the API check failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_argument_parser_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_invocation_starts_an_interactive_session() {
        let cli = Cli::parse_from(["grok"]);
        assert!(cli.print.is_none());
        assert!(!cli.r#continue);
        assert!(cli.command.is_none());
    }

    #[test]
    fn print_mode_and_its_short_form_both_parse() {
        assert_eq!(Cli::parse_from(["grok", "-p", "fix it"]).print.as_deref(), Some("fix it"));
        assert_eq!(
            Cli::parse_from(["grok", "--print", "fix it"]).print.as_deref(),
            Some("fix it")
        );
    }

    #[test]
    fn yes_implies_bypassing_permissions_but_an_explicit_mode_wins() {
        let cli = Cli::parse_from(["grok", "--yes"]);
        assert!(cli.yes);
        assert!(cli.mode.is_none());

        let explicit = Cli::parse_from(["grok", "--yes", "--mode", "plan"]);
        assert_eq!(explicit.mode.as_deref(), Some("plan"));
    }

    #[test]
    fn resume_and_continue_parse() {
        assert_eq!(Cli::parse_from(["grok", "--resume", "abc123"]).resume.as_deref(), Some("abc123"));
        assert!(Cli::parse_from(["grok", "-c"]).r#continue);
    }

    #[test]
    fn subcommands_parse() {
        assert!(matches!(Cli::parse_from(["grok", "sessions"]).command, Some(Command::Sessions)));
        assert!(matches!(Cli::parse_from(["grok", "doctor"]).command, Some(Command::Doctor)));
        assert!(matches!(Cli::parse_from(["grok", "config"]).command, Some(Command::Config)));
        assert!(matches!(Cli::parse_from(["grok", "tools"]).command, Some(Command::Tools)));
    }

    #[test]
    fn an_unknown_flag_is_rejected() {
        assert!(Cli::try_parse_from(["grok", "--nonsense"]).is_err());
    }

    #[test]
    fn a_literal_prompt_passes_through_unchanged() {
        assert_eq!(read_prompt("fix the bug").unwrap(), "fix the bug");
    }

    #[test]
    fn the_workspace_defaults_to_the_current_directory() {
        let resolved = resolve_workspace(None).unwrap();
        assert!(resolved.is_absolute(), "later sandbox checks depend on an absolute root");
    }

    #[test]
    fn a_missing_workspace_directory_is_reported() {
        let err = resolve_workspace(Some(std::path::Path::new("/definitely/not/here"))).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "got: {err}");
    }
}
