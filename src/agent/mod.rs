//! The agent loop: the thing the old prototype was missing.
//!
//! One user turn drives a loop:
//!
//! ```text
//! append user message
//! loop {
//!     stream a completion
//!     if it made no tool calls -> done
//!     for each tool call:
//!         hooks (PreToolUse)  -> may deny
//!         permissions         -> may deny, or ask the UI
//!         run the tool
//!         hooks (PostToolUse)
//!         append the result as a `tool` message
//! }
//! ```
//!
//! Two invariants hold the whole thing together, and violating either produces
//! an API error on the *next* request rather than a visible failure here:
//!
//! 1. **Every tool call gets exactly one `tool` message in reply.** Denied,
//!    failed, interrupted, unknown — all of them still answer. An assistant
//!    message carrying an unanswered `tool_call` is rejected by the API.
//! 2. **The assistant message is appended before its tool results.** Order is
//!    part of the contract.
//!
//! The loop never touches the terminal. It emits [`AgentEvent`]s and, when a
//! tool needs approval, hands a [`PermissionRequest`] to whoever is driving it.
//! That is what lets the same loop back both the TUI and headless mode.

pub mod prompt;
pub mod subagent;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::api::{ApiClient, Completion, Message, RequestOptions, StreamEvent, ToolCall, Usage};
use crate::config::{Config, PermissionMode};
use crate::hooks::{HookDecision, HookEvent, HookPayload, HookRunner};
use crate::permissions::{Decision, PermissionEngine};
use crate::session::Session;
use crate::tools::{ToolContext, ToolDisplay, ToolOutcome, ToolRegistry};
use crate::util;

/// Everything the agent tells its driver about what it is doing.
///
/// Deliberately a flat, owned enum rather than borrowed views: it crosses a
/// channel into a render loop, and it is the seam that keeps the TUI and the
/// headless runner from needing to know anything about the agent's internals.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A chunk of assistant text.
    Text(String),
    /// A chunk of reasoning text, for models that emit it.
    Reasoning(String),
    /// A tool is about to run.
    ToolStarted { id: String, name: String, summary: String },
    /// A tool finished.
    ToolFinished {
        id: String,
        name: String,
        outcome_summary: Option<String>,
        display: Box<ToolDisplay>,
        is_error: bool,
        duration: std::time::Duration,
    },
    /// A tool call was refused, with the reason given to the model.
    ToolDenied { id: String, name: String, reason: String },
    /// Token accounting for one API round trip.
    Usage(Usage),
    /// Context was compacted; `summary` replaced `dropped` messages.
    Compacted { dropped: usize, summary: String },
    /// A subagent produced progress worth showing.
    SubagentProgress { agent: String, text: String },
    /// A non-fatal problem worth surfacing (hook failure, MCP disconnect).
    Warning(String),
    /// The turn ended.
    TurnComplete { stop_reason: StopReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StopReason {
    /// The model finished its answer.
    #[default]
    Complete,
    /// The user interrupted.
    Interrupted,
    /// The per-turn tool budget was exhausted — almost always a loop.
    ToolLimitReached { limit: u32 },
    /// The turn failed.
    Error(String),
}

/// A pending approval, handed to the driver so it can ask the human.
#[derive(Debug)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub summary: String,
    /// The argument the decision is about — the command, or the path.
    pub argument: String,
    /// A diff preview when the tool is an edit, so the user approves a change
    /// rather than a filename.
    pub preview: Option<String>,
    pub respond: oneshot::Sender<PermissionResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponse {
    /// Run it, just this once.
    Once,
    /// Run it, and stop asking for this tool and argument.
    Always,
    /// Refuse.
    Reject,
}

/// The agent. Owns the conversation and everything needed to advance it.
pub struct Agent {
    pub config: Config,
    pub session: Session,
    pub permissions: PermissionEngine,
    pub tools: ToolRegistry,
    pub hooks: HookRunner,
    pub subagents: Vec<subagent::SubagentDefinition>,
    client: ApiClient,
    tool_context: ToolContext,
    /// Channel the driver supplies to receive permission prompts.
    permission_tx: Option<mpsc::Sender<PermissionRequest>>,
}

impl Agent {
    pub fn new(
        config: Config,
        session: Session,
        tools: ToolRegistry,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let client = ApiClient::new(&config.api_key, &config.base_url)?;
        let permissions = PermissionEngine::from_config(&config);
        let hooks = HookRunner::new(config.hooks.clone(), &session.id, &config.workspace);
        let subagents = subagent::discover(&config.workspace);
        let tool_context = ToolContext::new(config.workspace.clone(), cancel);

        Ok(Self {
            config,
            session,
            permissions,
            tools,
            hooks,
            subagents,
            client,
            tool_context,
            permission_tx: None,
        })
    }

    /// Supply the channel on which the driver answers permission prompts.
    /// Without one, anything that would prompt is refused instead of hanging.
    pub fn with_permission_channel(mut self, tx: mpsc::Sender<PermissionRequest>) -> Self {
        self.permission_tx = Some(tx);
        self
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.tool_context.cancel.clone()
    }

    /// Replace the cancellation token. Each turn gets a fresh one so that
    /// interrupting one turn does not permanently poison the next.
    pub fn reset_cancel(&mut self, cancel: CancellationToken) {
        self.tool_context.cancel = cancel;
    }

    pub fn tool_context(&self) -> &ToolContext {
        &self.tool_context
    }

    /// Point the `task` tool at the driver's event channel.
    ///
    /// The tool is registered at startup, before a front end exists, so it
    /// begins life holding a placeholder channel nobody reads. A front end
    /// calls this once it has a real one; without it, subagent progress is
    /// silently discarded and a long delegation looks like a hang.
    pub fn rewire_subagent_events(&mut self, events: mpsc::Sender<AgentEvent>) {
        if self.subagents.is_empty() {
            return;
        }
        // Rebuild against the registry minus `task` itself, so the tool does
        // not end up holding a stale clone of a registry containing itself.
        let parent = self.tools.without("task");
        // Share the parent's mode cell so plan mode and any later mode switch
        // govern delegated work too, and hand over the permission channel so a
        // subagent can ask rather than silently failing.
        let permissions = PermissionEngine::sharing_mode(
            &self.config.permissions,
            self.permissions.shared_mode(),
        );
        if let Ok(task) = subagent::TaskTool::new(
            self.subagents.clone(),
            self.config.clone(),
            parent,
            events,
            permissions,
            self.permission_tx.clone(),
        ) {
            self.tools.register(std::sync::Arc::new(task));
        }
    }

    fn request_options(&self) -> RequestOptions {
        RequestOptions {
            model: self.session.model.clone(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            reasoning_effort: self.config.reasoning_effort.clone(),
        }
    }

    /// Install the system prompt, replacing any previous one.
    pub fn refresh_system_prompt(&mut self, extra_context: &[String]) {
        let text = prompt::build(&self.config, &self.tools, &self.subagents, extra_context);
        let system = Message::system(text);
        match self.session.messages.first_mut() {
            Some(first) if first.is_role(crate::api::Role::System) => *first = system,
            _ => self.session.messages.insert(0, system),
        }
    }

    /// Which tools are visible to the model right now.
    ///
    /// Plan mode hides mutating tools entirely rather than advertising them and
    /// refusing every call: a tool the model can see, it will try, and burning
    /// a turn on a guaranteed refusal helps nobody.
    fn visible_tools(&self) -> ToolRegistry {
        if self.permissions.mode() == PermissionMode::Plan {
            self.tools.read_only()
        } else {
            self.tools.clone()
        }
    }

    /// Run one user turn to completion.
    pub async fn run_turn(&mut self, user_input: &str, events: &mpsc::Sender<AgentEvent>) {
        // UserPromptSubmit hooks may enrich or veto the prompt.
        let payload = HookPayload {
            event: HookEvent::UserPromptSubmit.as_str().into(),
            session_id: self.session.id.clone(),
            cwd: self.config.workspace.display().to_string(),
            tool_name: None,
            tool_input: None,
            tool_output: None,
            prompt: Some(user_input.to_string()),
        };
        let mut prompt_text = user_input.to_string();
        match self.hooks.run(HookEvent::UserPromptSubmit, payload).await {
            HookDecision::Deny { reason } => {
                let _ = events
                    .send(AgentEvent::TurnComplete { stop_reason: StopReason::Error(reason) })
                    .await;
                return;
            }
            HookDecision::Proceed { context } if !context.is_empty() => {
                prompt_text.push_str("\n\n<hook-context>\n");
                prompt_text.push_str(&context.join("\n"));
                prompt_text.push_str("\n</hook-context>");
            }
            _ => {}
        }

        self.session.push(Message::user(prompt_text));

        let stop_reason = self.drive(events).await;

        let _ = self
            .hooks
            .run(
                HookEvent::Stop,
                HookPayload {
                    event: HookEvent::Stop.as_str().into(),
                    session_id: self.session.id.clone(),
                    cwd: self.config.workspace.display().to_string(),
                    tool_name: None,
                    tool_input: None,
                    tool_output: None,
                    prompt: None,
                },
            )
            .await;

        let _ = events.send(AgentEvent::TurnComplete { stop_reason }).await;
    }

    /// The loop proper: stream, run tools, repeat until the model stops asking.
    async fn drive(&mut self, events: &mpsc::Sender<AgentEvent>) -> StopReason {
        let mut iterations = 0u32;

        loop {
            if self.tool_context.cancel.is_cancelled() {
                return StopReason::Interrupted;
            }
            if iterations >= self.config.max_tool_iterations {
                // Answer honestly instead of silently stopping: an agent that
                // quits mid-task without saying so is worse than one that fails.
                return StopReason::ToolLimitReached { limit: self.config.max_tool_iterations };
            }
            iterations += 1;

            if let Err(e) = self.compact_if_needed(events).await {
                let _ = events.send(AgentEvent::Warning(format!("compaction failed: {e}"))).await;
            }

            let completion = match self.stream_once(events).await {
                Ok(c) => c,
                Err(e) => {
                    if self.tool_context.cancel.is_cancelled() {
                        return StopReason::Interrupted;
                    }
                    return StopReason::Error(e.to_string());
                }
            };

            self.session.record_usage(&completion.usage);
            let _ = events.send(AgentEvent::Usage(completion.usage)).await;

            // The assistant message must land before its tool results.
            self.session.push(completion.message.clone());

            let Some(calls) = completion.message.tool_calls.clone().filter(|c| !c.is_empty()) else {
                // A stream cancelled part-way still returns the text it had, so
                // the check has to happen here too. Reporting Complete for a
                // turn the user interrupted is a lie the UI then repeats.
                return if self.tool_context.cancel.is_cancelled() {
                    StopReason::Interrupted
                } else {
                    StopReason::Complete
                };
            };

            // Invariant: every call is answered, even after an interrupt.
            let interrupted = self.tool_context.cancel.is_cancelled();
            for call in calls {
                let result = if interrupted {
                    "The user interrupted this turn before the tool ran.".to_string()
                } else {
                    self.execute_call(&call, events).await
                };
                self.session.push(Message::tool_result(&call.id, result));
            }

            if interrupted {
                return StopReason::Interrupted;
            }
        }
    }

    /// One streamed completion, forwarding text to the driver as it arrives.
    async fn stream_once(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<Completion> {
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        let forward = {
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    let mapped = match event {
                        StreamEvent::Text(t) => AgentEvent::Text(t),
                        StreamEvent::Reasoning(t) => AgentEvent::Reasoning(t),
                        // Tool calls are announced when they actually run, so
                        // the transcript order matches execution order.
                        StreamEvent::ToolCallReady(_) | StreamEvent::Done(_) => continue,
                    };
                    if events.send(mapped).await.is_err() {
                        break;
                    }
                }
            })
        };

        let result = self
            .client
            .stream_chat(
                self.session.messages.clone(),
                self.visible_tools().specs(),
                &self.request_options(),
                &tx,
                &self.tool_context.cancel,
            )
            .await;

        drop(tx);
        let _ = forward.await;
        result
    }

    /// Permission-check and run one tool call, returning the text the model sees.
    async fn execute_call(&mut self, call: &ToolCall, events: &mpsc::Sender<AgentEvent>) -> String {
        let name = call.function.name.clone();

        let Some(tool) = self.tools.get(&name) else {
            // Naming the alternatives turns a dead end into a recoverable one.
            let available = self.visible_tools().names().join(", ");
            let reason = format!("unknown tool `{name}`. Available tools: {available}");
            let _ = events
                .send(AgentEvent::ToolDenied { id: call.id.clone(), name, reason: reason.clone() })
                .await;
            return reason;
        };

        let args = match call.parsed_arguments() {
            Ok(v) => v,
            Err(e) => {
                let reason = format!(
                    "could not parse arguments for `{name}` as JSON: {e}. Arguments received: {}",
                    util::truncate_text(&call.function.arguments, 400)
                );
                let _ = events
                    .send(AgentEvent::ToolDenied { id: call.id.clone(), name, reason: reason.clone() })
                    .await;
                return reason;
            }
        };

        let summary = tool.summarize(&args);
        let argument = primary_argument(&args);

        // 1. PreToolUse hooks.
        let hook_payload = HookPayload {
            event: HookEvent::PreToolUse.as_str().into(),
            session_id: self.session.id.clone(),
            cwd: self.config.workspace.display().to_string(),
            tool_name: Some(name.clone()),
            tool_input: Some(args.clone()),
            tool_output: None,
            prompt: None,
        };
        let hook_allowed = match self.hooks.run(HookEvent::PreToolUse, hook_payload).await {
            HookDecision::Deny { reason } => {
                let _ = events
                    .send(AgentEvent::ToolDenied {
                        id: call.id.clone(),
                        name,
                        reason: reason.clone(),
                    })
                    .await;
                return format!("Tool call blocked: {reason}");
            }
            HookDecision::Allow { .. } => true,
            HookDecision::Proceed { context } => {
                for warning in context.iter().filter(|c| c.contains("failed")) {
                    let _ = events.send(AgentEvent::Warning(warning.clone())).await;
                }
                false
            }
        };

        // 2. Permissions.
        if !hook_allowed {
            match self.permissions.evaluate(&name, tool.kind(), &argument) {
                Decision::Allow { .. } => {}
                Decision::Deny { reason } => {
                    let _ = events
                        .send(AgentEvent::ToolDenied {
                            id: call.id.clone(),
                            name,
                            reason: reason.clone(),
                        })
                        .await;
                    return format!("Tool call refused: {reason}");
                }
                Decision::Ask => {
                    let preview = self.preview_for(&name, &args).await;
                    match self.ask_user(&name, &summary, &argument, preview).await {
                        PermissionResponse::Reject => {
                            let reason = "the user declined this action".to_string();
                            let _ = events
                                .send(AgentEvent::ToolDenied {
                                    id: call.id.clone(),
                                    name,
                                    reason: reason.clone(),
                                })
                                .await;
                            return format!(
                                "Tool call refused: {reason}. Ask what they would prefer instead of retrying."
                            );
                        }
                        PermissionResponse::Always => {
                            self.permissions.grant_for_session(&name, &argument);
                        }
                        PermissionResponse::Once => {}
                    }
                }
            }
        }

        // 3. Run it.
        let _ = events
            .send(AgentEvent::ToolStarted {
                id: call.id.clone(),
                name: name.clone(),
                summary: summary.clone(),
            })
            .await;

        let started = std::time::Instant::now();
        let outcome = match tool.run(args.clone(), &self.tool_context).await {
            Ok(o) => o,
            // A panic-free error path: the model gets the failure and can adapt.
            Err(e) => ToolOutcome::error(format!("{name} failed: {e}")),
        };
        let duration = started.elapsed();

        let _ = events
            .send(AgentEvent::ToolFinished {
                id: call.id.clone(),
                name: name.clone(),
                outcome_summary: outcome.display.summary.clone(),
                display: Box::new(outcome.display.clone()),
                is_error: outcome.is_error,
                duration,
            })
            .await;

        // 4. PostToolUse hooks may append context for the model.
        let post = self
            .hooks
            .run(
                HookEvent::PostToolUse,
                HookPayload {
                    event: HookEvent::PostToolUse.as_str().into(),
                    session_id: self.session.id.clone(),
                    cwd: self.config.workspace.display().to_string(),
                    tool_name: Some(name),
                    tool_input: Some(args),
                    tool_output: Some(util::truncate_text(&outcome.content, 4096)),
                    prompt: None,
                },
            )
            .await;

        let mut content = outcome.content;
        if let HookDecision::Proceed { context } = post
            && !context.is_empty()
        {
            content.push_str("\n\n<hook-context>\n");
            content.push_str(&context.join("\n"));
            content.push_str("\n</hook-context>");
        }
        content
    }

    /// Render a diff preview so the user approves a change, not a filename.
    async fn preview_for(&self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let path = args.get("path").and_then(serde_json::Value::as_str)?;
        let resolved = util::resolve(&self.config.workspace, path);
        let current = tokio::fs::read_to_string(&resolved).await.unwrap_or_default();

        let proposed = match tool_name {
            "write_file" => args.get("content").and_then(serde_json::Value::as_str)?.to_string(),
            "edit_file" => {
                let old = args.get("old_string").and_then(serde_json::Value::as_str)?;
                let new = args.get("new_string").and_then(serde_json::Value::as_str).unwrap_or("");
                if args.get("replace_all").and_then(serde_json::Value::as_bool).unwrap_or(false) {
                    current.replace(old, new)
                } else {
                    current.replacen(old, new, 1)
                }
            }
            _ => return None,
        };

        let lines = util::diff_lines(&current, &proposed, 3);
        if lines.is_empty() {
            return None;
        }
        Some(
            lines
                .iter()
                .map(|l| {
                    let marker = match l.kind {
                        util::DiffKind::Added => '+',
                        util::DiffKind::Removed => '-',
                        util::DiffKind::Context => ' ',
                    };
                    format!("{marker}{}", l.text)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Ask the driver for approval.
    ///
    /// With no channel wired up — headless mode without `--yes` — the answer is
    /// no. Defaulting to yes would silently turn an unattended run into an
    /// unsupervised one.
    async fn ask_user(
        &self,
        tool_name: &str,
        summary: &str,
        argument: &str,
        preview: Option<String>,
    ) -> PermissionResponse {
        let Some(tx) = &self.permission_tx else { return PermissionResponse::Reject };
        let (respond, rx) = oneshot::channel();
        let request = PermissionRequest {
            tool_name: tool_name.to_string(),
            summary: summary.to_string(),
            argument: argument.to_string(),
            preview,
            respond,
        };
        if tx.send(request).await.is_err() {
            return PermissionResponse::Reject;
        }
        tokio::select! {
            biased;
            () = self.tool_context.cancel.cancelled() => PermissionResponse::Reject,
            answer = rx => answer.unwrap_or(PermissionResponse::Reject),
        }
    }

    /// Whether the conversation is close enough to the context limit to compact.
    pub fn needs_compaction(&self) -> bool {
        self.session.estimated_context_tokens() >= self.config.compact_at()
    }

    /// Summarize and drop old turns when the context window is nearly full.
    async fn compact_if_needed(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        if !self.needs_compaction() {
            return Ok(());
        }
        self.compact(events).await
    }

    /// Replace old history with a summary, keeping the tail intact.
    pub async fn compact(&mut self, events: &mpsc::Sender<AgentEvent>) -> Result<()> {
        // Keep the system prompt and the most recent turns verbatim. Recent
        // context is what the model is actively working from; summarizing it
        // is what makes compaction feel like amnesia.
        const KEEP_TAIL: usize = 8;

        let system: Vec<Message> = self
            .session
            .messages
            .iter()
            .filter(|m| m.is_role(crate::api::Role::System))
            .cloned()
            .collect();
        let body: Vec<Message> = self
            .session
            .messages
            .iter()
            .filter(|m| !m.is_role(crate::api::Role::System))
            .cloned()
            .collect();

        if body.len() <= KEEP_TAIL {
            return Ok(());
        }

        // Split so the tail never begins with an orphaned `tool` message: a
        // tool result whose assistant message was summarized away is rejected
        // by the API.
        let mut split = body.len() - KEEP_TAIL;
        while split < body.len() && body[split].is_role(crate::api::Role::Tool) {
            split += 1;
        }

        let (head, tail) = body.split_at(split);
        if head.is_empty() {
            return Ok(());
        }

        let transcript: String = head
            .iter()
            .map(|m| {
                format!("{}: {}\n", m.role, util::truncate_text(m.content.as_deref().unwrap_or(""), 2000))
            })
            .collect();

        let summary_request = vec![
            Message::system(prompt::COMPACTION_SYSTEM),
            Message::user(format!("Conversation to summarize:\n\n{transcript}")),
        ];

        let completion = self
            .client
            .complete(
                summary_request,
                vec![],
                &RequestOptions { max_tokens: Some(2048), ..self.request_options() },
                &self.tool_context.cancel,
            )
            .await?;

        let summary = completion.message.content.unwrap_or_default();
        if summary.trim().is_empty() {
            anyhow::bail!("the model returned an empty summary");
        }

        let dropped = head.len();
        let mut rebuilt = system;
        rebuilt.push(Message::user(format!(
            "<summary-of-earlier-conversation>\n{summary}\n</summary-of-earlier-conversation>"
        )));
        rebuilt.extend_from_slice(tail);
        self.session.messages = rebuilt;
        self.session.record_compaction(summary.clone(), dropped);

        let _ = events.send(AgentEvent::Compacted { dropped, summary }).await;
        Ok(())
    }
}

/// The argument a permission decision is really about.
///
/// Rules are written against the thing that matters — the command for `bash`,
/// the path for a file tool — not against the whole JSON blob.
pub fn primary_argument(args: &serde_json::Value) -> String {
    for key in ["command", "path", "pattern", "url", "file_path", "id"] {
        if let Some(v) = args.get(key).and_then(serde_json::Value::as_str) {
            return v.to_string();
        }
    }
    String::new()
}

/// Fixtures shared by the tests in this module tree.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::config::PermissionRules;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A config pointing at a scratch workspace, with no API access.
    pub(crate) fn config_at(workspace: PathBuf) -> Config {
        Config {
            api_key: "test-key".into(),
            model: crate::config::DEFAULT_MODEL.into(),
            base_url: crate::api::DEFAULT_BASE_URL.into(),
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            permission_mode: PermissionMode::Default,
            auto_compact_threshold: 0.85,
            max_tool_iterations: 60,
            theme: "dark".into(),
            permissions: PermissionRules::default(),
            mcp_servers: BTreeMap::new(),
            hooks: BTreeMap::new(),
            workspace,
        }
    }

    pub(crate) fn config() -> Config {
        config_at(std::env::temp_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tests_support::config_at as test_config;

    fn test_agent(workspace: PathBuf) -> Agent {
        let config = test_config(workspace.clone());
        let session = Session::in_memory("test", &config.model);
        Agent::new(config, session, ToolRegistry::with_builtins(), CancellationToken::new()).unwrap()
    }

    #[test]
    fn the_primary_argument_is_the_thing_rules_are_written_against() {
        assert_eq!(primary_argument(&serde_json::json!({"command": "ls -la"})), "ls -la");
        assert_eq!(primary_argument(&serde_json::json!({"path": "src/a.rs"})), "src/a.rs");
        assert_eq!(primary_argument(&serde_json::json!({"url": "https://x"})), "https://x");
        assert_eq!(primary_argument(&serde_json::json!({"unrelated": 1})), "");
    }

    #[test]
    fn plan_mode_hides_mutating_tools_instead_of_advertising_and_refusing_them() {
        let mut agent = test_agent(std::env::temp_dir());
        assert!(agent.visible_tools().get("bash").is_some());

        agent.permissions.set_mode(PermissionMode::Plan);
        let visible = agent.visible_tools();
        assert!(visible.get("bash").is_none(), "the model should not see a tool it cannot use");
        assert!(visible.get("write_file").is_none());
        assert!(visible.get("read_file").is_some());
    }

    #[test]
    fn the_system_prompt_is_replaced_not_duplicated_on_refresh() {
        let mut agent = test_agent(std::env::temp_dir());
        agent.refresh_system_prompt(&[]);
        agent.refresh_system_prompt(&[]);

        let system_count =
            agent.session.messages.iter().filter(|m| m.is_role(crate::api::Role::System)).count();
        assert_eq!(system_count, 1, "refreshing must not stack system prompts");
        assert!(agent.session.messages[0].is_role(crate::api::Role::System), "and it stays first");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_answered_with_the_list_of_real_ones() {
        let mut agent = test_agent(std::env::temp_dir());
        let (tx, mut rx) = mpsc::channel(16);

        let call = ToolCall::new("c1", "does_not_exist", "{}");
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("unknown tool"), "got: {result}");
        assert!(result.contains("read_file"), "the model needs to know what it can use: {result}");
        assert!(matches!(rx.recv().await, Some(AgentEvent::ToolDenied { .. })));
    }

    #[tokio::test]
    async fn malformed_tool_arguments_are_answered_rather_than_crashing_the_turn() {
        let mut agent = test_agent(std::env::temp_dir());
        let (tx, _rx) = mpsc::channel(16);

        let call = ToolCall::new("c1", "read_file", "{not json");
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("could not parse arguments"), "got: {result}");
        assert!(result.contains("{not json"), "echoing the input helps the model fix it: {result}");
    }

    #[tokio::test]
    async fn a_tool_needing_approval_is_refused_when_no_driver_can_answer() {
        // Headless with no --yes: the safe default is no, never yes.
        let dir = tempfile::tempdir().unwrap();
        let mut agent = test_agent(dir.path().to_path_buf());
        let (tx, _rx) = mpsc::channel(16);

        let call = ToolCall::new("c1", "bash", r#"{"command":"echo hi"}"#);
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("refused"), "got: {result}");
    }

    #[tokio::test]
    async fn an_approved_tool_runs_and_reports_its_outcome() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();

        let mut agent = test_agent(dir.path().to_path_buf());
        let (tx, mut rx) = mpsc::channel(32);

        // read_file is harmless and needs no approval.
        let call = ToolCall::new("c1", "read_file", r#"{"path":"a.txt"}"#);
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("hello"), "got: {result}");
        assert!(matches!(rx.recv().await, Some(AgentEvent::ToolStarted { .. })));
        assert!(matches!(rx.recv().await, Some(AgentEvent::ToolFinished { is_error: false, .. })));
    }

    #[tokio::test]
    async fn a_deny_rule_refuses_the_call_and_tells_the_model_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(dir.path().to_path_buf());
        config.permissions.deny.push("Bash(*)".into());
        config.permission_mode = PermissionMode::BypassPermissions;

        let session = Session::in_memory("test", &config.model);
        let mut agent =
            Agent::new(config, session, ToolRegistry::with_builtins(), CancellationToken::new())
                .unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let call = ToolCall::new("c1", "bash", r#"{"command":"echo hi"}"#);
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("refused"), "got: {result}");
        assert!(result.contains("deny rule"), "the reason names the cause: {result}");
    }

    #[tokio::test]
    async fn approving_always_stops_the_next_identical_call_from_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = test_agent(dir.path().to_path_buf());
        let (perm_tx, mut perm_rx) = mpsc::channel::<PermissionRequest>(4);
        agent = agent.with_permission_channel(perm_tx);

        // Answer every prompt with "always" and count how many arrive. Counting
        // rather than returning the request matters: holding an unanswered
        // `respond` sender alive would leave the agent waiting on it forever,
        // whereas a real driver either answers or drops the channel.
        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&prompts);
        let responder = tokio::spawn(async move {
            while let Some(req) = perm_rx.recv().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = req.respond.send(PermissionResponse::Always);
            }
        });

        let (tx, _rx) = mpsc::channel(32);

        // Both commands share their first two words, which is the granularity a
        // bash grant is keyed at, so the second must not re-prompt.
        let call = ToolCall::new("c1", "bash", r#"{"command":"echo alpha one"}"#);
        assert!(agent.execute_call(&call, &tx).await.contains("alpha one"));

        let call2 = ToolCall::new("c2", "bash", r#"{"command":"echo alpha two"}"#);
        assert!(agent.execute_call(&call2, &tx).await.contains("alpha two"));

        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the session grant should have suppressed the second prompt"
        );

        drop(agent);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), responder).await;
    }

    #[tokio::test]
    async fn a_grant_for_one_command_does_not_cover_an_unrelated_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = test_agent(dir.path().to_path_buf());
        let (perm_tx, mut perm_rx) = mpsc::channel::<PermissionRequest>(4);
        agent = agent.with_permission_channel(perm_tx);

        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&prompts);
        tokio::spawn(async move {
            while let Some(req) = perm_rx.recv().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = req.respond.send(PermissionResponse::Always);
            }
        });

        let (tx, _rx) = mpsc::channel(32);
        agent.execute_call(&ToolCall::new("c1", "bash", r#"{"command":"echo hello"}"#), &tx).await;
        agent.execute_call(&ToolCall::new("c2", "bash", r#"{"command":"pwd"}"#), &tx).await;

        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "approving `echo` must never silently approve a different command"
        );
    }

    #[tokio::test]
    async fn rejecting_tells_the_model_to_ask_rather_than_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = test_agent(dir.path().to_path_buf());
        let (perm_tx, mut perm_rx) = mpsc::channel::<PermissionRequest>(4);
        agent = agent.with_permission_channel(perm_tx);

        tokio::spawn(async move {
            if let Some(req) = perm_rx.recv().await {
                let _ = req.respond.send(PermissionResponse::Reject);
            }
        });

        let (tx, _rx) = mpsc::channel(16);
        let call = ToolCall::new("c1", "bash", r#"{"command":"rm -rf /"}"#);
        let result = agent.execute_call(&call, &tx).await;

        assert!(result.contains("declined"), "got: {result}");
        assert!(result.contains("Ask what they would prefer"), "got: {result}");
    }

    #[tokio::test]
    async fn an_edit_prompt_carries_a_diff_so_the_user_approves_the_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let agent = test_agent(dir.path().to_path_buf());

        let preview = agent
            .preview_for(
                "edit_file",
                &serde_json::json!({"path": "a.txt", "old_string": "two", "new_string": "TWO"}),
            )
            .await
            .expect("an edit must produce a preview");

        assert!(preview.contains("-two"), "got: {preview}");
        assert!(preview.contains("+TWO"), "got: {preview}");
    }

    #[tokio::test]
    async fn a_bash_prompt_has_no_diff_preview() {
        let agent = test_agent(std::env::temp_dir());
        assert!(agent.preview_for("bash", &serde_json::json!({"command": "ls"})).await.is_none());
    }
}
