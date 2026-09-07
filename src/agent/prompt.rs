//! System prompt assembly.
//!
//! The prompt is built fresh each turn from live state — the actual tool list,
//! the actual permission mode, the actual `GROK.md` files — rather than being a
//! static string. A prompt that claims the agent can run commands while plan
//! mode hides `bash` produces confident, wrong behaviour, so the two are
//! generated from the same source.
//!
//! Ordering matters: identity first, then environment, then user instructions.
//! `GROK.md` content goes last because later text wins conflicts, and the
//! user's project rules should beat the harness's defaults.

use crate::config::Config;
use crate::tools::ToolRegistry;

use super::subagent::SubagentDefinition;

/// Core identity and operating rules.
const IDENTITY: &str = "\
You are grok-cli, an agentic coding assistant running in a terminal on the \
user's machine. You have direct access to their filesystem and shell through \
tools, and you act on their code rather than only describing what to do.

# Working style

- Act rather than propose. If the user asks for a change, make it. Explain \
afterwards, briefly.
- Find things by searching, not by guessing. Use `grep` and `glob` to locate \
code; read a file before you edit it.
- Prefer `edit_file` over `write_file` for existing files. Rewriting a whole \
file to change three lines risks losing everything else in it.
- Verify your work. If the project has tests or a type checker, run them after \
a change and fix what breaks.
- Match the surrounding code. Follow the file's existing naming, comment \
density, error handling and idiom instead of importing your own conventions.
- Never invent APIs, flags, or file paths. Check that they exist.

# Communicating

- Be concise. The user is reading in a terminal, not a document.
- Reference code as `path/to/file.rs:42` so it is clickable.
- Report outcomes honestly. If tests fail, say so and show the failure. If you \
skipped part of the task, say which part and why. Do not claim something works \
when you have not checked.
- Do not narrate tool calls. The interface already shows them.

# Tasks

Use `todo_write` for any task with three or more steps: it is how the user \
follows a long run. Keep exactly one task in progress, and mark each one \
completed as soon as it is done, not in a batch at the end.";

/// Prompt used for the compaction summary call.
pub const COMPACTION_SYSTEM: &str = "\
You are summarizing the earlier part of a coding session so it can be dropped \
from context while the work continues.

Write a dense summary that preserves everything needed to carry on:

- What the user asked for, in their own terms, including anything they \
corrected or ruled out.
- Files examined or changed, by path, and what changed in each.
- Decisions made and the reasoning behind them.
- Commands run and what they showed — especially failures.
- What is done, what is in progress, and what remains.
- Any constraint, preference or gotcha that would cause a mistake if forgotten.

Prefer specifics over description. `src/auth.rs:42 expiry check used < instead \
of <=, fixed` is useful; `worked on authentication` is not. Do not add \
commentary about the summary itself.";

/// Build the full system prompt.
pub fn build(
    config: &Config,
    tools: &ToolRegistry,
    subagents: &[SubagentDefinition],
    extra_context: &[String],
) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(IDENTITY);

    out.push_str("\n\n# Environment\n\n");
    out.push_str(&format!("- Working directory: {}\n", config.workspace.display()));
    out.push_str(&format!("- Platform: {}\n", std::env::consts::OS));
    out.push_str(&format!("- Model: {}\n", config.model));
    out.push_str(&format!("- Today's date: {}\n", &crate::util::now_rfc3339()[..10]));

    // Tell the model the truth about what it may do. Claiming capabilities the
    // permission mode has revoked produces confident failures.
    out.push_str(&format!(
        "- Permission mode: {} ({})\n",
        config.permission_mode.as_str(),
        config.permission_mode.describe()
    ));
    if config.permission_mode == crate::config::PermissionMode::Plan {
        out.push_str(
            "\nYou are in plan mode: you can read and search, but you cannot edit files or run \
             commands. Investigate, then present a concrete plan and let the user approve it.\n",
        );
    }

    if is_git_repo(&config.workspace) {
        out.push_str("- This directory is a git repository.\n");
    }

    out.push_str("\n# Tools\n\n");
    for tool in tools.iter() {
        let spec = tool.spec();
        // One line each: the model already has the full JSON schema, and
        // repeating it here would waste the context the schema already costs.
        out.push_str(&format!(
            "- `{}` — {}\n",
            spec.function.name,
            first_sentence(&spec.function.description)
        ));
    }

    if !subagents.is_empty() {
        out.push_str("\n# Subagents\n\nDelegate with the `task` tool when a job is \
                      self-contained and would otherwise fill this conversation with \
                      intermediate output.\n\n");
        for agent in subagents {
            out.push_str(&format!("- `{}` — {}\n", agent.name, agent.description));
        }
    }

    // User instructions last: later text wins, and the user's project rules
    // should override the harness's defaults.
    let memory = crate::config::discover_memory(&config.workspace);
    if !memory.is_empty() {
        out.push_str("\n# Project instructions\n\n");
        out.push_str(
            "These come from GROK.md files in this project. They take precedence over the \
             general guidance above.\n",
        );
        for file in memory {
            out.push_str(&format!("\n<instructions from=\"{}\">\n", file.path.display()));
            out.push_str(file.content.trim());
            out.push_str("\n</instructions>\n");
        }
    }

    if !extra_context.is_empty() {
        out.push_str("\n# Session context\n\n");
        for item in extra_context {
            out.push_str(item.trim());
            out.push('\n');
        }
    }

    out
}

/// First sentence of a tool description, for the one-line summary.
fn first_sentence(text: &str) -> &str {
    match text.find(". ") {
        Some(i) => &text[..=i],
        None => text,
    }
}

fn is_git_repo(workspace: &std::path::Path) -> bool {
    workspace.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PermissionMode, PermissionRules};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn config_at(workspace: PathBuf, mode: PermissionMode) -> Config {
        Config {
            api_key: String::new(),
            model: "grok-4-1-fast-non-reasoning".into(),
            base_url: crate::api::DEFAULT_BASE_URL.into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            permission_mode: mode,
            auto_compact_threshold: 0.85,
            max_tool_iterations: 60,
            theme: "dark".into(),
            permissions: PermissionRules::default(),
            mcp_servers: BTreeMap::new(),
            hooks: BTreeMap::new(),
            workspace,
        }
    }

    #[test]
    fn the_prompt_lists_the_tools_that_actually_exist() {
        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins();
        let prompt = build(&config_at(dir.path().into(), PermissionMode::Default), &tools, &[], &[]);

        for name in tools.names() {
            assert!(prompt.contains(&format!("`{name}`")), "prompt omits {name}");
        }
    }

    #[test]
    fn plan_mode_says_so_and_omits_the_tools_it_hides() {
        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins().read_only();
        let prompt = build(&config_at(dir.path().into(), PermissionMode::Plan), &tools, &[], &[]);

        assert!(prompt.contains("plan mode"), "the model must know it cannot act");
        assert!(!prompt.contains("`bash`"), "advertising a hidden tool causes confident failures");
        assert!(prompt.contains("`read_file`"));
    }

    #[test]
    fn project_instructions_come_last_so_they_win_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("GROK.md"), "Always use tabs.").unwrap();

        let prompt = build(
            &config_at(dir.path().into(), PermissionMode::Default),
            &ToolRegistry::with_builtins(),
            &[],
            &[],
        );

        let instructions_at = prompt.find("Always use tabs.").expect("GROK.md is included");
        let identity_at = prompt.find("You are grok-cli").expect("identity present");
        assert!(identity_at < instructions_at, "later text wins, so user rules go last");
        assert!(prompt.contains("take precedence"), "and the model is told they do");
    }

    #[test]
    fn a_project_without_grok_md_gets_no_instructions_section() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = build(
            &config_at(dir.path().into(), PermissionMode::Default),
            &ToolRegistry::with_builtins(),
            &[],
            &[],
        );
        assert!(!prompt.contains("# Project instructions"));
    }

    #[test]
    fn hook_context_is_appended_as_session_context() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = build(
            &config_at(dir.path().into(), PermissionMode::Default),
            &ToolRegistry::with_builtins(),
            &[],
            &["current branch: main".into()],
        );
        assert!(prompt.contains("# Session context"));
        assert!(prompt.contains("current branch: main"));
    }

    #[test]
    fn subagents_are_advertised_only_when_some_are_defined() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_at(dir.path().into(), PermissionMode::Default);
        let tools = ToolRegistry::with_builtins();

        assert!(!build(&config, &tools, &[], &[]).contains("# Subagents"));

        let agents = vec![SubagentDefinition {
            name: "reviewer".into(),
            description: "reviews a diff".into(),
            prompt: "You review code.".into(),
            tools: None,
            model: None,
        }];
        let with_agents = build(&config, &tools, &agents, &[]);
        assert!(with_agents.contains("# Subagents"));
        assert!(with_agents.contains("`reviewer` — reviews a diff"));
    }

    #[test]
    fn git_repositories_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_at(dir.path().into(), PermissionMode::Default);
        let tools = ToolRegistry::with_builtins();
        assert!(!build(&config, &tools, &[], &[]).contains("git repository"));

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(build(&config, &tools, &[], &[]).contains("git repository"));
    }

    #[test]
    fn tool_summaries_are_trimmed_to_one_sentence() {
        assert_eq!(first_sentence("Does a thing. And then another."), "Does a thing.");
        assert_eq!(first_sentence("No trailing period"), "No trailing period");
    }
}
