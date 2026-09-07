//! Subagents: delegating a self-contained job to a nested agent loop.
//!
//! A subagent is a markdown file in `.grok/agents/`:
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews a diff for correctness bugs
//! tools: read_file, grep, glob, bash
//! model: grok-4
//! ---
//!
//! You review code for correctness. Report only defects you can demonstrate.
//! ```
//!
//! The point is context isolation, not parallelism. A search that reads thirty
//! files to answer one question should not leave thirty files in the parent's
//! context — the subagent burns its own window and returns a paragraph.
//!
//! Two safety properties matter and are enforced here rather than trusted:
//!
//! * a subagent's tools are a **subset** of the parent's, never a superset, so
//!   delegation can not be used to escape a restriction; and
//! * subagents cannot spawn subagents, so a recursive definition cannot fork
//!   until the machine falls over.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::api::{ApiClient, Message, RequestOptions, ToolSpec};
use crate::config::Config;
use crate::tools::{Tool, ToolContext, ToolKind, ToolOutcome, ToolRegistry, object_schema, prop};
use crate::util;

/// Maximum tool round trips inside one subagent run.
const SUBAGENT_TOOL_LIMIT: u32 = 30;

/// A subagent as defined on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentDefinition {
    pub name: String,
    pub description: String,
    /// The markdown body: the subagent's system prompt.
    pub prompt: String,
    /// Tool names it may use. `None` means "everything the parent has".
    pub tools: Option<Vec<String>>,
    /// Model override.
    pub model: Option<String>,
}

/// Read every subagent definition under `.grok/agents/`.
///
/// A malformed file is skipped with a log line rather than failing startup:
/// these are user-authored notes, and one bad file should not cost the user
/// their other agents.
pub fn discover(workspace: &Path) -> Vec<SubagentDefinition> {
    let mut found = Vec::new();
    for dir in agent_dirs(workspace) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            match parse(&text, &path) {
                Some(def) => {
                    // A project definition shadows a user-wide one of the same
                    // name, which is why project directories come last.
                    found.retain(|d: &SubagentDefinition| d.name != def.name);
                    found.push(def);
                }
                None => tracing::warn!(path = %path.display(), "skipping malformed subagent"),
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// User-wide first, then project, so project definitions win.
fn agent_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user) = crate::config::user_config_dir() {
        dirs.push(user.join("agents"));
    }
    dirs.push(crate::config::project_config_dir(workspace).join("agents"));
    dirs
}

/// Parse a definition file. The filename is the fallback name.
fn parse(text: &str, path: &Path) -> Option<SubagentDefinition> {
    let (frontmatter, body) = split_frontmatter(text)?;

    let mut name = path.file_stem()?.to_string_lossy().into_owned();
    let mut description = String::new();
    let mut tools = None;
    let mut model = None;

    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => name = value.to_string(),
            "description" => description = value.to_string(),
            "model" => model = Some(value.to_string()),
            "tools" => {
                // Accept both `a, b, c` and `[a, b, c]`.
                let list: Vec<String> = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|t| t.trim().trim_matches('"').to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                tools = (!list.is_empty()).then_some(list);
            }
            _ => {}
        }
    }

    // A subagent with no prompt has nothing to contribute.
    if body.trim().is_empty() {
        return None;
    }
    if description.is_empty() {
        description = format!("custom agent from {}", path.display());
    }

    Some(SubagentDefinition { name, description, prompt: body.trim().to_string(), tools, model })
}

/// Split `---`-fenced YAML frontmatter from the markdown body.
fn split_frontmatter(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        // No frontmatter: the whole file is the prompt.
        return Some((String::new(), text.to_string()));
    }
    let after = trimmed.strip_prefix("---")?;
    let end = after.find("\n---")?;
    Some((after[..end].to_string(), after[end + 4..].to_string()))
}

/// The `task` tool: hands a job to a subagent and returns its final answer.
pub struct TaskTool {
    definitions: Vec<SubagentDefinition>,
    config: Config,
    /// The parent's tools. A subagent gets a subset of these, never more.
    parent_tools: ToolRegistry,
    client: ApiClient,
    /// Progress events, so a long delegation is not a silent pause.
    events: mpsc::Sender<super::AgentEvent>,
}

impl TaskTool {
    pub fn new(
        definitions: Vec<SubagentDefinition>,
        config: Config,
        parent_tools: ToolRegistry,
        events: mpsc::Sender<super::AgentEvent>,
    ) -> Result<Self> {
        let client = ApiClient::new(&config.api_key, &config.base_url)?;
        Ok(Self { definitions, config, parent_tools, client, events })
    }

    fn find(&self, name: &str) -> Option<&SubagentDefinition> {
        self.definitions.iter().find(|d| d.name == name)
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn spec(&self) -> ToolSpec {
        let names: Vec<&str> = self.definitions.iter().map(|d| d.name.as_str()).collect();
        let mut p = Map::new();
        p.insert(
            "agent".into(),
            prop("string", &format!("Which subagent to use. Available: {}", names.join(", "))),
        );
        p.insert(
            "prompt".into(),
            prop("string", "The complete task. The subagent cannot see this conversation, so include every detail it needs."),
        );
        ToolSpec::function(
            "task",
            "Delegate a self-contained job to a subagent. Use it when the work would fill this conversation with intermediate output that you do not need to keep — a broad search, an audit, a survey. The subagent returns only its final answer.",
            object_schema(p, &["agent", "prompt"]),
        )
    }

    fn kind(&self) -> ToolKind {
        // A subagent inherits a subset of the parent's tools, so it can do at
        // most what the parent could. Classified Execute because that subset
        // may include command execution.
        ToolKind::Execute
    }

    fn summarize(&self, args: &Value) -> String {
        let agent = args.get("agent").and_then(Value::as_str).unwrap_or("?");
        let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("");
        format!("Task({agent}: {})", util::truncate_text(prompt, 60))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(agent_name) = args.get("agent").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `agent`"));
        };
        let Some(task) = args.get("prompt").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `prompt`"));
        };
        let Some(definition) = self.find(agent_name) else {
            let available: Vec<&str> = self.definitions.iter().map(|d| d.name.as_str()).collect();
            return Ok(ToolOutcome::error(format!(
                "no subagent named `{agent_name}`. Available: {}",
                if available.is_empty() { "none defined".to_string() } else { available.join(", ") }
            )));
        };

        // A subagent's toolset is intersected with the parent's, never unioned:
        // delegation must not be a way around a restriction. `task` itself is
        // excluded, so subagents cannot spawn subagents.
        let tools = match &definition.tools {
            Some(allowed) => self.parent_tools.subset(allowed),
            None => self.parent_tools.clone(),
        }
        .without("task");

        let options = RequestOptions {
            model: definition.model.clone().unwrap_or_else(|| self.config.model.clone()),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            reasoning_effort: self.config.reasoning_effort.clone(),
        };

        let mut messages = vec![
            Message::system(format!(
                "{}\n\nYou are running as a subagent of grok-cli in {}. You cannot ask questions: \
                 finish the task and report what you found. Your final message is the entire \
                 answer the caller receives, so make it complete and self-contained.",
                definition.prompt,
                self.config.workspace.display()
            )),
            Message::user(task.to_string()),
        ];

        let mut iterations = 0u32;
        loop {
            if ctx.cancel.is_cancelled() {
                return Ok(ToolOutcome::error("subagent interrupted by the user"));
            }
            if iterations >= SUBAGENT_TOOL_LIMIT {
                return Ok(ToolOutcome::error(format!(
                    "subagent `{agent_name}` hit its {SUBAGENT_TOOL_LIMIT}-step limit without finishing"
                )));
            }
            iterations += 1;

            let completion = match self
                .client
                .complete(messages.clone(), tools.specs(), &options, &ctx.cancel)
                .await
            {
                Ok(c) => c,
                Err(e) => return Ok(ToolOutcome::error(format!("subagent `{agent_name}` failed: {e}"))),
            };

            messages.push(completion.message.clone());

            let Some(calls) = completion.message.tool_calls.filter(|c| !c.is_empty()) else {
                let answer = completion.message.content.unwrap_or_default();
                let _ = self
                    .events
                    .send(super::AgentEvent::SubagentProgress {
                        agent: agent_name.to_string(),
                        text: format!("finished after {iterations} steps"),
                    })
                    .await;
                return Ok(ToolOutcome::ok(if answer.trim().is_empty() {
                    format!("subagent `{agent_name}` returned no answer")
                } else {
                    answer
                })
                .with_summary(format!("{agent_name}, {iterations} steps")));
            };

            // Subagents run without prompting. They are confined to a subset of
            // already-permitted tools, and a prompt from a nested loop the user
            // cannot see the context for is worse than no prompt at all.
            for call in calls {
                let result = match tools.get(&call.function.name) {
                    None => format!("unknown tool `{}`", call.function.name),
                    Some(tool) => match call.parsed_arguments() {
                        Err(e) => format!("could not parse arguments: {e}"),
                        Ok(args) => match tool.run(args, ctx).await {
                            Ok(o) => o.content,
                            Err(e) => format!("{} failed: {e}", call.function.name),
                        },
                    },
                };
                messages.push(Message::tool_result(&call.id, result));
            }

            let _ = self
                .events
                .send(super::AgentEvent::SubagentProgress {
                    agent: agent_name.to_string(),
                    text: format!("step {iterations}"),
                })
                .await;
        }
    }
}

/// Register `task` when at least one subagent is defined.
///
/// Advertising a delegation tool with nothing to delegate to just invites a
/// wasted turn.
pub fn register_if_available(
    registry: &mut ToolRegistry,
    definitions: &[SubagentDefinition],
    config: &Config,
    events: mpsc::Sender<super::AgentEvent>,
) -> Result<()> {
    if definitions.is_empty() {
        return Ok(());
    }
    let task = TaskTool::new(definitions.to_vec(), config.clone(), registry.clone(), events)?;
    registry.register(Arc::new(task));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_parses_its_frontmatter_and_body() {
        let text = "---\nname: reviewer\ndescription: Reviews a diff\ntools: read_file, grep\nmodel: grok-4\n---\n\nYou review code.\n";
        let def = parse(text, Path::new("/x/reviewer.md")).unwrap();

        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews a diff");
        assert_eq!(def.tools.as_deref(), Some(&["read_file".to_string(), "grep".to_string()][..]));
        assert_eq!(def.model.as_deref(), Some("grok-4"));
        assert_eq!(def.prompt, "You review code.");
    }

    #[test]
    fn a_yaml_array_tool_list_parses_the_same_as_a_comma_list() {
        let text = "---\ntools: [read_file, \"grep\"]\n---\nbody\n";
        let def = parse(text, Path::new("/x/a.md")).unwrap();
        assert_eq!(def.tools.as_deref(), Some(&["read_file".to_string(), "grep".to_string()][..]));
    }

    #[test]
    fn the_filename_supplies_a_missing_name() {
        let def = parse("---\ndescription: x\n---\nbody\n", Path::new("/x/searcher.md")).unwrap();
        assert_eq!(def.name, "searcher");
    }

    #[test]
    fn a_file_without_frontmatter_is_all_prompt() {
        let def = parse("Just a prompt.\n", Path::new("/x/plain.md")).unwrap();
        assert_eq!(def.name, "plain");
        assert_eq!(def.prompt, "Just a prompt.");
        assert!(def.tools.is_none(), "no restriction means inherit the parent's tools");
    }

    #[test]
    fn a_definition_with_no_body_is_rejected() {
        assert!(parse("---\nname: empty\n---\n\n   \n", Path::new("/x/empty.md")).is_none());
    }

    #[test]
    fn discovery_reads_project_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".grok/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("reviewer.md"),
            "---\ndescription: reviews\n---\nYou review.\n",
        )
        .unwrap();
        std::fs::write(agents.join("notes.txt"), "ignored, not markdown").unwrap();

        let found = discover(dir.path());
        assert_eq!(found.len(), 1, "only .md files are definitions");
        assert_eq!(found[0].name, "reviewer");
    }

    #[test]
    fn a_malformed_definition_does_not_cost_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join(".grok/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("broken.md"), "---\nname: broken\n---\n").unwrap();
        std::fs::write(agents.join("good.md"), "---\nname: good\n---\nWorks.\n").unwrap();

        let found = discover(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "good");
    }

    #[test]
    fn task_is_not_registered_when_no_subagents_exist() {
        let mut registry = ToolRegistry::with_builtins();
        let (tx, _rx) = mpsc::channel(1);
        let config = crate::agent::tests_support::config();

        register_if_available(&mut registry, &[], &config, tx).unwrap();
        assert!(registry.get("task").is_none(), "nothing to delegate to");
    }

    #[test]
    fn task_is_registered_once_a_subagent_exists() {
        let mut registry = ToolRegistry::with_builtins();
        let (tx, _rx) = mpsc::channel(1);
        let config = crate::agent::tests_support::config();
        let defs = vec![SubagentDefinition {
            name: "reviewer".into(),
            description: "reviews".into(),
            prompt: "You review.".into(),
            tools: None,
            model: None,
        }];

        register_if_available(&mut registry, &defs, &config, tx).unwrap();
        let task = registry.get("task").expect("task is available");
        assert!(task.spec().function.description.contains("Delegate"));
        assert!(task.spec().function.parameters["properties"]["agent"]["description"]
            .as_str()
            .unwrap()
            .contains("reviewer"));
    }
}
