//! User-defined hooks: shell commands that observe and can veto agent actions.
//!
//! A hook is a program. It receives a JSON event on stdin and may reply with
//! JSON on stdout:
//!
//! ```json
//! {"event":"PreToolUse","tool_name":"bash","tool_input":{"command":"rm -rf /"},"cwd":"/repo"}
//! ```
//! ```json
//! {"decision":"deny","reason":"destructive command blocked by policy"}
//! ```
//!
//! Exit code 2 also means deny, with stderr as the reason — that convention
//! lets a hook be a two-line shell script with no JSON handling at all.
//!
//! Hooks run with a timeout and their failures are non-fatal *except* for an
//! explicit deny. A hook that crashes or times out must not silently become an
//! approval, so `PreToolUse` failures fall through to the normal permission
//! path rather than auto-allowing; but they also must not brick the session, so
//! they do not abort the turn.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::HookConfig;

/// Points in the agent's lifecycle where hooks can fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Once, when the session starts. Output is added to the system prompt.
    SessionStart,
    /// Before each user prompt is sent. Output is appended to the prompt.
    UserPromptSubmit,
    /// Before a tool runs. Can deny.
    PreToolUse,
    /// After a tool runs. Cannot deny; output is shown to the model.
    PostToolUse,
    /// When the agent finishes a turn.
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
        }
    }

    /// Only `PreToolUse` can block. A `PostToolUse` deny would be meaningless:
    /// the side effect already happened.
    pub fn can_block(self) -> bool {
        self == Self::PreToolUse
    }
}

/// The JSON handed to a hook on stdin.
#[derive(Debug, Clone, Serialize)]
pub struct HookPayload {
    pub event: String,
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// The JSON a hook may return on stdout.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HookReply {
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Text to inject into the conversation.
    #[serde(default)]
    pub context: Option<String>,
}

/// What the hooks collectively decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Nothing objected. `context` is any text hooks asked to inject.
    Proceed { context: Vec<String> },
    /// A hook explicitly approved, skipping the permission prompt.
    Allow { reason: String },
    /// A hook vetoed. `reason` goes to the model as the tool result.
    Deny { reason: String },
}

/// Runs the hooks configured for a session.
pub struct HookRunner {
    hooks: BTreeMap<String, Vec<HookConfig>>,
    session_id: String,
    workspace: std::path::PathBuf,
}

impl HookRunner {
    pub fn new(
        hooks: BTreeMap<String, Vec<HookConfig>>,
        session_id: impl Into<String>,
        workspace: &Path,
    ) -> Self {
        Self { hooks, session_id: session_id.into(), workspace: workspace.to_path_buf() }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(Vec::is_empty)
    }

    /// Hooks registered for `event` whose matcher accepts `tool_name`.
    fn matching(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<&HookConfig> {
        let Some(list) = self.hooks.get(event.as_str()) else { return Vec::new() };
        list.iter()
            .filter(|h| match (&h.matcher, tool_name) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(pattern), Some(name)) => regex::Regex::new(pattern)
                    // An invalid matcher matches nothing. Matching *everything*
                    // would be the dangerous reading of a typo.
                    .map(|re| re.is_match(name))
                    .unwrap_or(false),
            })
            .collect()
    }

    /// Fire every hook for `event` and combine their answers.
    ///
    /// The first explicit deny wins and short-circuits; otherwise an explicit
    /// allow is remembered but later hooks still run, so a logging hook after
    /// an approving hook still sees the event.
    pub async fn run(&self, event: HookEvent, payload: HookPayload) -> HookDecision {
        let matched = self.matching(event, payload.tool_name.as_deref());
        if matched.is_empty() {
            return HookDecision::Proceed { context: Vec::new() };
        }

        let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        let mut context = Vec::new();
        let mut allow_reason: Option<String> = None;

        for hook in matched {
            match self.invoke(hook, &body).await {
                Err(e) => {
                    // A broken hook must not become a silent approval; fall
                    // through to the normal permission path instead.
                    tracing::warn!(command = %hook.command, error = %e, "hook failed");
                    context.push(format!("[hook `{}` failed: {e}]", hook.command));
                }
                Ok(reply) => {
                    if let Some(text) = reply.context.filter(|t| !t.trim().is_empty()) {
                        context.push(text);
                    }
                    match reply.decision.as_deref() {
                        Some("deny" | "block") if event.can_block() => {
                            return HookDecision::Deny {
                                reason: reply.reason.unwrap_or_else(|| {
                                    format!("blocked by hook `{}`", hook.command)
                                }),
                            };
                        }
                        Some("allow" | "approve") => {
                            allow_reason = Some(
                                reply
                                    .reason
                                    .unwrap_or_else(|| format!("approved by hook `{}`", hook.command)),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }

        match allow_reason {
            Some(reason) => HookDecision::Allow { reason },
            None => HookDecision::Proceed { context },
        }
    }

    /// Run one hook process to completion.
    async fn invoke(&self, hook: &HookConfig, stdin_body: &str) -> anyhow::Result<HookReply> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(&hook.command)
            .current_dir(&self.workspace)
            .env("GROK_SESSION_ID", &self.session_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            // A hook that ignores stdin closes the pipe early; that is a broken
            // pipe, not a hook failure, so the error is discarded.
            let _ = stdin.write_all(stdin_body.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }

        let output = tokio::time::timeout(
            Duration::from_secs(hook.timeout_secs.clamp(1, 300)),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", hook.timeout_secs))??;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        // Exit code 2 is the no-JSON path: block, with stderr as the reason.
        if output.status.code() == Some(2) {
            return Ok(HookReply {
                decision: Some("deny".into()),
                reason: Some(if stderr.is_empty() { "blocked by hook".into() } else { stderr }),
                context: None,
            });
        }

        if stdout.is_empty() {
            return Ok(HookReply::default());
        }

        // Treat stdout as a reply only if it is an object carrying at least one
        // field this protocol defines. Deserializing straight into `HookReply`
        // would be wrong: every field is optional, so *any* JSON — including a
        // hook that simply echoes the event payload — would parse into an
        // all-`None` reply and be silently discarded.
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&stdout)
            && map.keys().any(|k| matches!(k.as_str(), "decision" | "reason" | "context"))
            && let Ok(reply) = serde_json::from_str::<HookReply>(&stdout)
        {
            return Ok(reply);
        }

        // Anything else is plain output to inject as context, so a hook can be
        // `echo "current branch: $(git branch --show-current)"`.
        Ok(HookReply { decision: None, reason: None, context: Some(stdout) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(event: &str, hooks: Vec<HookConfig>) -> HookRunner {
        let mut map = BTreeMap::new();
        map.insert(event.to_string(), hooks);
        HookRunner::new(map, "test-session", Path::new("."))
    }

    fn hook(command: &str, matcher: Option<&str>) -> HookConfig {
        HookConfig {
            matcher: matcher.map(str::to_string),
            command: command.to_string(),
            timeout_secs: 10,
        }
    }

    fn tool_payload(tool: &str, command: &str) -> HookPayload {
        HookPayload {
            event: "PreToolUse".into(),
            session_id: "test-session".into(),
            cwd: ".".into(),
            tool_name: Some(tool.into()),
            tool_input: Some(serde_json::json!({ "command": command })),
            tool_output: None,
            prompt: None,
        }
    }

    #[tokio::test]
    async fn no_configured_hooks_means_proceed() {
        let r = HookRunner::new(BTreeMap::new(), "s", Path::new("."));
        assert!(r.is_empty());
        assert_eq!(
            r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await,
            HookDecision::Proceed { context: vec![] }
        );
    }

    #[tokio::test]
    async fn a_hook_can_deny_a_tool_call_with_json() {
        let r = runner(
            "PreToolUse",
            vec![hook(r#"echo '{"decision":"deny","reason":"nope"}'"#, None)],
        );
        assert_eq!(
            r.run(HookEvent::PreToolUse, tool_payload("bash", "rm -rf /")).await,
            HookDecision::Deny { reason: "nope".into() }
        );
    }

    #[tokio::test]
    async fn exit_code_two_denies_with_stderr_as_the_reason() {
        let r = runner("PreToolUse", vec![hook("echo 'policy violation' 1>&2; exit 2", None)]);
        assert_eq!(
            r.run(HookEvent::PreToolUse, tool_payload("bash", "x")).await,
            HookDecision::Deny { reason: "policy violation".into() }
        );
    }

    #[tokio::test]
    async fn a_hook_can_pre_approve_a_call() {
        let r = runner("PreToolUse", vec![hook(r#"echo '{"decision":"allow"}'"#, None)]);
        let decision = r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await;
        assert!(matches!(decision, HookDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn matchers_scope_a_hook_to_specific_tools() {
        let r = runner(
            "PreToolUse",
            vec![hook(r#"echo '{"decision":"deny","reason":"no shells"}'"#, Some("^bash$"))],
        );
        assert!(matches!(
            r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await,
            HookDecision::Deny { .. }
        ));
        assert!(
            matches!(
                r.run(HookEvent::PreToolUse, tool_payload("read_file", "a.rs")).await,
                HookDecision::Proceed { .. }
            ),
            "a non-matching tool must be unaffected"
        );
    }

    #[tokio::test]
    async fn an_invalid_matcher_matches_nothing_rather_than_everything() {
        let r = runner("PreToolUse", vec![hook(r#"echo '{"decision":"deny"}'"#, Some("([unclosed"))]);
        assert!(
            matches!(
                r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await,
                HookDecision::Proceed { .. }
            ),
            "a typo in a matcher must not block every tool"
        );
    }

    #[tokio::test]
    async fn plain_stdout_becomes_injected_context() {
        let r = runner("SessionStart", vec![hook("echo 'branch: main'", None)]);
        let payload = HookPayload {
            event: "SessionStart".into(),
            session_id: "s".into(),
            cwd: ".".into(),
            tool_name: None,
            tool_input: None,
            tool_output: None,
            prompt: None,
        };
        let HookDecision::Proceed { context } = r.run(HookEvent::SessionStart, payload).await else {
            panic!("expected Proceed");
        };
        assert_eq!(context, vec!["branch: main".to_string()]);
    }

    #[tokio::test]
    async fn post_tool_use_hooks_cannot_block() {
        let mut map = BTreeMap::new();
        map.insert(
            "PostToolUse".to_string(),
            vec![hook(r#"echo '{"decision":"deny","reason":"too late"}'"#, None)],
        );
        let r = HookRunner::new(map, "s", Path::new("."));

        let decision = r.run(HookEvent::PostToolUse, tool_payload("bash", "ls")).await;
        assert!(
            matches!(decision, HookDecision::Proceed { .. }),
            "the side effect already happened; a deny here is meaningless"
        );
    }

    #[tokio::test]
    async fn a_crashing_hook_does_not_become_an_approval() {
        let r = runner("PreToolUse", vec![hook("exit 1", None)]);
        let decision = r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await;
        assert!(
            matches!(decision, HookDecision::Proceed { .. }),
            "a failed hook falls through to normal permissions, never to Allow"
        );
    }

    #[tokio::test]
    async fn a_hanging_hook_times_out_instead_of_wedging_the_session() {
        let mut h = hook("sleep 30", None);
        h.timeout_secs = 1;
        let r = runner("PreToolUse", vec![h]);

        let started = std::time::Instant::now();
        let decision = r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await;
        assert!(started.elapsed() < Duration::from_secs(5), "must not wait for the hook");

        let HookDecision::Proceed { context } = decision else { panic!("expected Proceed") };
        assert!(context.iter().any(|c| c.contains("failed")), "the failure is surfaced: {context:?}");
    }

    #[tokio::test]
    async fn hooks_receive_the_event_payload_on_stdin() {
        let r = runner("PreToolUse", vec![hook("cat", None)]);
        let HookDecision::Proceed { context } =
            r.run(HookEvent::PreToolUse, tool_payload("bash", "ls -la")).await
        else {
            panic!("expected Proceed");
        };
        let echoed = context.join("");
        assert!(echoed.contains("\"tool_name\":\"bash\""), "got: {echoed}");
        assert!(echoed.contains("ls -la"), "got: {echoed}");
    }

    #[tokio::test]
    async fn the_first_deny_wins_over_a_later_allow() {
        let r = runner(
            "PreToolUse",
            vec![
                hook(r#"echo '{"decision":"deny","reason":"first"}'"#, None),
                hook(r#"echo '{"decision":"allow"}'"#, None),
            ],
        );
        assert_eq!(
            r.run(HookEvent::PreToolUse, tool_payload("bash", "ls")).await,
            HookDecision::Deny { reason: "first".into() }
        );
    }
}
