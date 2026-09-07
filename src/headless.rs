//! Non-interactive mode: `grok -p "..."`.
//!
//! Same agent, no terminal. Built for pipelines and CI, so it follows the rules
//! a pipeline expects:
//!
//! * the model's answer goes to **stdout**, alone, so `grok -p … | grep` works;
//! * progress and diagnostics go to **stderr**;
//! * the exit code reflects the outcome, so `&&` chains behave.
//!
//! Nothing prompts. Without a permission channel the agent refuses anything
//! that would ask, which means an unattended run either stays inside its
//! configured permissions or stops and says what it wanted. Turning that off is
//! an explicit `--yes`.

use std::io::Write;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, StopReason};
use crate::config::PermissionMode;

/// Exit codes, chosen so a caller can tell the failure modes apart.
pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_INTERRUPTED: i32 = 130;
pub const EXIT_TOOL_LIMIT: i32 = 2;
pub const EXIT_BLOCKED: i32 = 3;

/// Run one prompt and print the result.
pub async fn run(agent: &mut Agent, prompt: &str, mcp_failures: &[(String, String)]) -> Result<()> {
    let mut stderr = std::io::stderr();

    for (name, error) in mcp_failures {
        let _ = writeln!(stderr, "warning: MCP server `{name}` unavailable: {error}");
    }
    if !agent.permissions.invalid.is_empty() {
        let _ = writeln!(
            stderr,
            "warning: ignoring malformed permission rules: {}",
            agent.permissions.invalid.join(", ")
        );
    }
    if agent.permissions.mode() == PermissionMode::BypassPermissions {
        let _ = writeln!(stderr, "warning: running with permissions bypassed");
    }

    agent.refresh_system_prompt(&[]);

    // Interrupting an unattended run should still stop it cleanly.
    let cancel = agent.cancel_token();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
    // Same rewiring as the TUI: the `task` tool starts with a placeholder
    // channel, so subagent progress would otherwise never reach stderr.
    agent.rewire_subagent_events(tx.clone());

    // Collect the answer while streaming progress to stderr, so a long run
    // shows signs of life without polluting stdout.
    let printer = tokio::spawn(async move {
        let mut stderr = std::io::stderr();
        let mut answer = String::new();
        let mut blocked: Vec<String> = Vec::new();
        let mut stop = StopReason::Complete;

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Text(chunk) => answer.push_str(&chunk),
                AgentEvent::ToolStarted { summary, .. } => {
                    let _ = writeln!(stderr, "· {summary}");
                }
                AgentEvent::ToolFinished { name, outcome_summary, is_error, .. } => {
                    if is_error {
                        let _ = writeln!(
                            stderr,
                            "  {name} failed{}",
                            outcome_summary.map(|s| format!(": {s}")).unwrap_or_default()
                        );
                    } else if let Some(summary) = outcome_summary {
                        let _ = writeln!(stderr, "  {summary}");
                    }
                }
                AgentEvent::ToolDenied { name, reason, .. } => {
                    let _ = writeln!(stderr, "  {name} refused: {reason}");
                    blocked.push(format!("{name}: {reason}"));
                }
                AgentEvent::Compacted { dropped, .. } => {
                    let _ = writeln!(stderr, "· compacted {dropped} earlier messages");
                }
                AgentEvent::Warning(text) => {
                    let _ = writeln!(stderr, "warning: {text}");
                }
                AgentEvent::TurnComplete { stop_reason } => stop = stop_reason,
                AgentEvent::Reasoning(_) | AgentEvent::Usage(_) | AgentEvent::SubagentProgress { .. } => {}
            }
        }

        (answer, blocked, stop)
    });

    agent.run_turn(prompt, &tx).await;
    drop(tx);

    let (answer, blocked, stop) = printer.await.unwrap_or_default();

    // The answer is the only thing on stdout.
    let mut stdout = std::io::stdout();
    let trimmed = answer.trim();
    if !trimmed.is_empty() {
        writeln!(stdout, "{trimmed}")?;
    }
    stdout.flush()?;

    let usage = agent.session.usage;
    if usage.total_tokens > 0 {
        let _ = writeln!(
            stderr,
            "\n{} in, {} out, {} total",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
    }

    match stop {
        StopReason::Complete => {
            // A run that produced nothing because every action was refused is
            // not a success, even though the model "finished".
            if trimmed.is_empty() && !blocked.is_empty() {
                let _ = writeln!(
                    stderr,
                    "error: every action was refused. Adjust permissions in .grok/config.toml, or pass --yes."
                );
                std::process::exit(EXIT_BLOCKED);
            }
            Ok(())
        }
        StopReason::Interrupted => {
            let _ = writeln!(stderr, "interrupted");
            std::process::exit(EXIT_INTERRUPTED);
        }
        StopReason::ToolLimitReached { limit } => {
            let _ = writeln!(stderr, "error: stopped after {limit} tool calls in one turn");
            std::process::exit(EXIT_TOOL_LIMIT);
        }
        StopReason::Error(message) => {
            let _ = writeln!(stderr, "error: {message}");
            std::process::exit(EXIT_ERROR);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_distinct_so_callers_can_tell_failures_apart() {
        let codes = [EXIT_OK, EXIT_ERROR, EXIT_TOOL_LIMIT, EXIT_BLOCKED, EXIT_INTERRUPTED];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "duplicate exit codes lose information");
    }

    #[test]
    fn interruption_uses_the_conventional_signal_exit_code() {
        // 128 + SIGINT(2), which is what a shell reports for Ctrl+C.
        assert_eq!(EXIT_INTERRUPTED, 130);
    }
}
