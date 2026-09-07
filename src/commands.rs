//! Slash commands.
//!
//! Two kinds:
//!
//! * **Built-ins** change harness state — model, mode, context, session. They
//!   resolve to a [`CommandAction`] the UI performs; this module does not touch
//!   the terminal or the agent.
//! * **Custom commands** are markdown files in `.grok/commands/`. They are
//!   prompt templates: `/review src/auth.rs` expands the file's body with
//!   `$ARGUMENTS` replaced and sends the result as a normal user message.
//!
//! Parsing is deliberately separate from execution so the whole surface is
//! testable without a running agent or a terminal.
//!
//! ```markdown
//! ---
//! description: Review a file for correctness bugs
//! argument-hint: <path>
//! ---
//!
//! Review $ARGUMENTS. Report only defects you can demonstrate.
//! ```

use std::path::{Path, PathBuf};

use crate::config::PermissionMode;

/// What the UI should do in response to a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    /// Show text in the transcript. No model call.
    Show(String),
    /// A report only the front end can produce, because it needs live state
    /// (the running agent, its usage, its connected servers). Carries the
    /// command name; the front end matches on it.
    ///
    /// This is a distinct variant rather than a `Show` placeholder because a
    /// placeholder is indistinguishable from a real answer: an earlier version
    /// returned `Show("/agents is unavailable right now")`, the UI never
    /// noticed, and five commands silently printed that instead of working.
    Report(String),
    /// Send this text to the model as a user message.
    Prompt(String),
    /// Switch model.
    SetModel(String),
    /// Switch permission mode.
    SetMode(PermissionMode),
    /// Open the model picker.
    PickModel,
    /// Open the mode picker.
    PickMode,
    /// Open the session picker.
    PickSession,
    /// Drop the conversation, keeping the system prompt.
    Clear,
    /// Summarize and drop old history now.
    Compact,
    /// Write a GROK.md for this project.
    InitProject,
    /// Leave.
    Quit,
    /// Nothing matched.
    Unknown(String),
}

/// A command the user can invoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// Hint shown after the name in the palette, e.g. `<path>`.
    pub argument_hint: Option<String>,
    /// `Some` for custom commands: the template body.
    pub template: Option<String>,
}

impl SlashCommand {
    /// How the palette renders this entry.
    pub fn display(&self) -> String {
        match &self.argument_hint {
            Some(hint) => format!("/{} {hint}", self.name),
            None => format!("/{}", self.name),
        }
    }
}

/// Built-in commands, in the order the palette shows them.
pub fn builtins() -> Vec<SlashCommand> {
    let entry = |name: &str, description: &str, hint: Option<&str>| SlashCommand {
        name: name.into(),
        description: description.into(),
        argument_hint: hint.map(str::to_string),
        template: None,
    };
    vec![
        entry("help", "Show available commands and keybindings", None),
        entry("model", "Show or switch the model", Some("[name]")),
        entry("mode", "Show or switch the permission mode", Some("[name]")),
        entry("clear", "Start a fresh conversation", None),
        entry("compact", "Summarize and shorten the conversation now", None),
        entry("context", "Show what is currently filling the context window", None),
        entry("cost", "Show token usage for this session", None),
        entry("tools", "List available tools", None),
        entry("mcp", "Show connected MCP servers", None),
        entry("agents", "List available subagents", None),
        entry("resume", "Switch to an earlier session", None),
        entry("init", "Write a GROK.md describing this project", None),
        entry("quit", "Exit grok-cli", None),
    ]
}

/// Built-ins plus any custom commands found on disk.
pub fn all(workspace: &Path) -> Vec<SlashCommand> {
    let mut commands = builtins();
    for custom in discover_custom(workspace) {
        // A custom command may deliberately shadow a built-in.
        commands.retain(|c| c.name != custom.name);
        commands.push(custom);
    }
    commands
}

/// Read `.grok/commands/*.md`, user-wide first so project files win.
pub fn discover_custom(workspace: &Path) -> Vec<SlashCommand> {
    let mut dirs = Vec::new();
    if let Some(user) = crate::config::user_config_dir() {
        dirs.push(user.join("commands"));
    }
    dirs.push(crate::config::project_config_dir(workspace).join("commands"));

    let mut found: Vec<SlashCommand> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut batch: Vec<SlashCommand> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    return None;
                }
                let text = std::fs::read_to_string(&path).ok()?;
                parse_custom(&text, &path)
            })
            .collect();
        batch.sort_by(|a, b| a.name.cmp(&b.name));
        for command in batch {
            found.retain(|c| c.name != command.name);
            found.push(command);
        }
    }
    found
}

/// Parse one custom command file.
fn parse_custom(text: &str, path: &Path) -> Option<SlashCommand> {
    let name = path.file_stem()?.to_string_lossy().into_owned();
    let (frontmatter, body) = split_frontmatter(text);

    let mut description = String::new();
    let mut argument_hint = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "description" => description = value.to_string(),
            "argument-hint" | "argument_hint" => argument_hint = Some(value.to_string()),
            _ => {}
        }
    }

    // A command with no body has nothing to send.
    if body.trim().is_empty() {
        return None;
    }
    if description.is_empty() {
        description = format!("custom command ({name})");
    }

    Some(SlashCommand {
        name,
        description,
        argument_hint,
        template: Some(body.trim().to_string()),
    })
}

fn split_frontmatter(text: &str) -> (String, String) {
    let trimmed = text.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else {
        return (String::new(), text.to_string());
    };
    match after.find("\n---") {
        Some(end) => (after[..end].to_string(), after[end + 4..].to_string()),
        // An unterminated fence is a typo. Treating the whole file as a prompt
        // is more useful than silently dropping the command.
        None => (String::new(), text.to_string()),
    }
}

/// Interpret a line of input beginning with `/`.
pub fn dispatch(input: &str, workspace: &Path) -> CommandAction {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return CommandAction::Prompt(trimmed.to_string());
    };

    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };

    // Custom commands are checked first so they can shadow built-ins.
    if let Some(custom) = discover_custom(workspace).into_iter().find(|c| c.name == name)
        && let Some(template) = custom.template
    {
        return CommandAction::Prompt(expand(&template, args));
    }

    match name {
        "help" | "?" => CommandAction::Show(help_text(workspace)),
        "quit" | "exit" | "q" => CommandAction::Quit,
        "clear" | "new" => CommandAction::Clear,
        "compact" => CommandAction::Compact,
        "init" => CommandAction::InitProject,
        "resume" => CommandAction::PickSession,
        "model" => {
            if args.is_empty() {
                CommandAction::PickModel
            } else {
                CommandAction::SetModel(args.to_string())
            }
        }
        "mode" => {
            if args.is_empty() {
                return CommandAction::PickMode;
            }
            match PermissionMode::parse(args) {
                Some(mode) => CommandAction::SetMode(mode),
                None => CommandAction::Show(format!(
                    "Unknown mode `{args}`. Valid modes: {}",
                    PermissionMode::all()
                        .iter()
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        // These need live state only the front end has.
        "context" | "cost" | "tools" | "mcp" | "agents" => {
            CommandAction::Report(name.to_string())
        }
        other => CommandAction::Unknown(other.to_string()),
    }
}

/// Substitute arguments into a custom command's template.
///
/// `$ARGUMENTS` is replaced wherever it appears; positional `$1`, `$2` … map to
/// whitespace-separated words. A template with no placeholder gets the
/// arguments appended, so `/review src/a.rs` does something sensible even when
/// the author forgot to include one.
pub fn expand(template: &str, args: &str) -> String {
    let mut out = template.to_string();
    let has_placeholder = out.contains("$ARGUMENTS") || out.contains("$1");

    for (i, word) in args.split_whitespace().enumerate().take(9) {
        out = out.replace(&format!("${}", i + 1), word);
    }
    out = out.replace("$ARGUMENTS", args);

    if !has_placeholder && !args.is_empty() {
        out.push_str("\n\n");
        out.push_str(args);
    }
    out
}

/// Prompt used by `/init`.
pub const INIT_PROMPT: &str = "\
Analyse this codebase and write a GROK.md at the repository root.

GROK.md is read into your system prompt at the start of every session here, so \
it should contain what a competent engineer new to this repo would need and \
could not quickly infer:

- What the project is and how its pieces fit together.
- The commands that matter: build, test, lint, run.
- Conventions this codebase actually follows, taken from the code rather than \
from general best practice.
- Anything surprising: a non-obvious invariant, a gotcha, a constraint that has \
already bitten someone.

Keep it short and specific. Do not pad it with generic advice, and do not list \
the directory tree — that is discoverable. If a GROK.md already exists, improve \
it rather than replacing it wholesale.";

fn help_text(workspace: &Path) -> String {
    let mut out = String::from("Commands\n\n");
    for command in all(workspace) {
        out.push_str(&format!("  {:<22} {}\n", command.display(), command.description));
    }
    out.push_str(
        "\nKeys\n\n  \
         Enter               send\n  \
         Shift+Enter         newline\n  \
         Esc                 interrupt the current turn\n  \
         Ctrl+C              interrupt, or quit when idle\n  \
         Ctrl+D              quit\n  \
         Up / Down           scroll the transcript\n  \
         PageUp / PageDown   scroll a screen at a time\n  \
         Ctrl+L              clear the screen\n  \
         Shift+Tab           cycle permission mode\n",
    );
    out
}

/// Candidate completions for the palette, given what has been typed so far.
pub fn complete(prefix: &str, workspace: &Path) -> Vec<SlashCommand> {
    let needle = prefix.trim_start_matches('/').to_lowercase();
    let mut matches: Vec<SlashCommand> =
        all(workspace).into_iter().filter(|c| c.name.to_lowercase().starts_with(&needle)).collect();
    matches.sort_by(|a, b| a.name.cmp(&b.name));
    matches
}

/// Where `/init` writes its output.
pub fn memory_path(workspace: &Path) -> PathBuf {
    workspace.join(crate::config::MEMORY_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn empty_workspace() -> tempfile::TempDir {
        tempdir().unwrap()
    }

    #[test]
    fn plain_text_is_a_prompt_not_a_command() {
        let dir = empty_workspace();
        assert_eq!(
            dispatch("fix the auth bug", dir.path()),
            CommandAction::Prompt("fix the auth bug".into())
        );
    }

    #[test]
    fn built_ins_dispatch_to_their_actions() {
        let dir = empty_workspace();
        assert_eq!(dispatch("/quit", dir.path()), CommandAction::Quit);
        assert_eq!(dispatch("/exit", dir.path()), CommandAction::Quit);
        assert_eq!(dispatch("/clear", dir.path()), CommandAction::Clear);
        assert_eq!(dispatch("/compact", dir.path()), CommandAction::Compact);
        assert_eq!(dispatch("/resume", dir.path()), CommandAction::PickSession);
        assert_eq!(dispatch("/init", dir.path()), CommandAction::InitProject);
    }

    #[test]
    fn a_bare_model_or_mode_opens_a_picker_but_an_argument_sets_it() {
        let dir = empty_workspace();
        assert_eq!(dispatch("/model", dir.path()), CommandAction::PickModel);
        assert_eq!(dispatch("/model grok-4", dir.path()), CommandAction::SetModel("grok-4".into()));
        assert_eq!(dispatch("/mode", dir.path()), CommandAction::PickMode);
        assert_eq!(dispatch("/mode plan", dir.path()), CommandAction::SetMode(PermissionMode::Plan));
    }

    #[test]
    fn an_invalid_mode_lists_the_valid_ones() {
        let dir = empty_workspace();
        let CommandAction::Show(text) = dispatch("/mode banana", dir.path()) else {
            panic!("expected an explanation");
        };
        assert!(text.contains("Unknown mode `banana`"), "got: {text}");
        assert!(text.contains("acceptEdits"), "the valid options are listed: {text}");
    }

    #[test]
    fn an_unrecognized_command_is_reported_by_name() {
        let dir = empty_workspace();
        assert_eq!(dispatch("/nope", dir.path()), CommandAction::Unknown("nope".into()));
    }

    #[test]
    fn custom_commands_expand_their_template() {
        let dir = empty_workspace();
        let commands = dir.path().join(".grok/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(
            commands.join("review.md"),
            "---\ndescription: Review a file\nargument-hint: <path>\n---\n\nReview $ARGUMENTS carefully.\n",
        )
        .unwrap();

        assert_eq!(
            dispatch("/review src/auth.rs", dir.path()),
            CommandAction::Prompt("Review src/auth.rs carefully.".into())
        );

        let listed = all(dir.path());
        let review = listed.iter().find(|c| c.name == "review").expect("listed in the palette");
        assert_eq!(review.display(), "/review <path>");
        assert_eq!(review.description, "Review a file");
    }

    #[test]
    fn a_custom_command_can_shadow_a_built_in() {
        let dir = empty_workspace();
        let commands = dir.path().join(".grok/commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("compact.md"), "Summarize differently.").unwrap();

        assert_eq!(
            dispatch("/compact", dir.path()),
            CommandAction::Prompt("Summarize differently.".into()),
            "an explicit project command wins over the built-in"
        );
    }

    #[test]
    fn positional_placeholders_map_to_words() {
        assert_eq!(expand("compare $1 with $2", "a.rs b.rs"), "compare a.rs with b.rs");
    }

    #[test]
    fn a_template_without_a_placeholder_still_receives_the_arguments() {
        // Otherwise `/review src/a.rs` would silently drop the path.
        assert_eq!(expand("Review this file.", "src/a.rs"), "Review this file.\n\nsrc/a.rs");
        assert_eq!(expand("Review this file.", ""), "Review this file.");
    }

    #[test]
    fn a_command_file_with_no_body_is_ignored() {
        assert!(parse_custom("---\ndescription: x\n---\n\n  \n", Path::new("/x/empty.md")).is_none());
    }

    #[test]
    fn a_command_file_without_frontmatter_is_all_template() {
        let c = parse_custom("Just do the thing.", Path::new("/x/thing.md")).unwrap();
        assert_eq!(c.name, "thing");
        assert_eq!(c.template.as_deref(), Some("Just do the thing."));
        assert!(c.description.contains("thing"));
    }

    #[test]
    fn an_unterminated_frontmatter_fence_still_yields_a_usable_command() {
        let c = parse_custom("---\ndescription: oops\nBody text", Path::new("/x/a.md")).unwrap();
        assert!(c.template.as_deref().unwrap().contains("Body text"));
    }

    #[test]
    fn completion_filters_by_prefix() {
        let dir = empty_workspace();
        let names: Vec<String> = complete("/co", dir.path()).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["compact", "context", "cost"]);

        assert!(complete("/zzz", dir.path()).is_empty());
        assert!(complete("/", dir.path()).len() >= builtins().len());
    }

    #[test]
    fn help_lists_every_command_and_the_keybindings() {
        let dir = empty_workspace();
        let CommandAction::Show(text) = dispatch("/help", dir.path()) else { panic!() };
        for command in builtins() {
            assert!(text.contains(&format!("/{}", command.name)), "help omits /{}", command.name);
        }
        assert!(text.contains("Shift+Tab"), "keybindings are documented");
    }
}
