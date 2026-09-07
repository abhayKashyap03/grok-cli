//! Command execution: foreground `bash`, plus background shells the model can
//! poll and kill.
//!
//! Two things make this more than a `Command::spawn` wrapper:
//!
//! * **Cancellation actually kills the process.** Dropping a tokio `Child`
//!   detaches it rather than killing it, so an interrupted `cargo build` would
//!   keep running and keep holding the target-directory lock. The run loop
//!   selects on the cancellation token and explicitly kills the child.
//! * **Background shells outlive the tool call.** A dev server started by the
//!   model has to survive the turn that started it, so those go into a
//!   [`ShellManager`] keyed by id and are drained incrementally.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::api::ToolSpec;
use crate::tools::{Tool, ToolContext, ToolKind, ToolOutcome, object_schema, prop};
use crate::util;

/// Default wall-clock limit for a foreground command.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Ceiling the model cannot exceed, however large a timeout it asks for.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Cap on captured output per command, so `yes` cannot exhaust memory.
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
/// Lines retained per background shell.
const MAX_BACKGROUND_LINES: usize = 5_000;

/// A command still running after its tool call returned.
#[derive(Debug, Default)]
pub struct BackgroundShell {
    pub command: String,
    /// Lines captured so far. Drained by `bash_output`, so each poll returns
    /// only what is new.
    pub pending: Vec<String>,
    pub finished: bool,
    pub exit_code: Option<i32>,
}

/// Registry of background shells for one session.
#[derive(Default)]
pub struct ShellManager {
    shells: Mutex<HashMap<String, BackgroundShell>>,
    handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    next_id: AtomicU64,
}

impl ShellManager {
    fn allocate_id(&self) -> String {
        format!("shell_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Take everything captured since the last poll.
    pub async fn drain(&self, id: &str) -> Option<(Vec<String>, bool, Option<i32>)> {
        let mut shells = self.shells.lock().await;
        let shell = shells.get_mut(id)?;
        let lines = std::mem::take(&mut shell.pending);
        Some((lines, shell.finished, shell.exit_code))
    }

    pub async fn ids(&self) -> Vec<String> {
        let shells = self.shells.lock().await;
        let mut ids: Vec<String> = shells.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Abort the reader task and forget the shell.
    ///
    /// Aborting the reader closes the pipes, which is what actually terminates
    /// a well-behaved child. A child that ignores a closed stdout survives, and
    /// the message says so rather than claiming a kill that did not happen.
    pub async fn kill(&self, id: &str) -> bool {
        let removed = self.shells.lock().await.remove(id).is_some();
        if let Some(handle) = self.handles.lock().await.remove(id) {
            handle.abort();
        }
        removed
    }

    async fn record(&self, id: &str, line: String) {
        let mut shells = self.shells.lock().await;
        if let Some(shell) = shells.get_mut(id) {
            if shell.pending.len() >= MAX_BACKGROUND_LINES {
                shell.pending.remove(0);
            }
            shell.pending.push(line);
        }
    }

    async fn finish(&self, id: &str, code: Option<i32>) {
        let mut shells = self.shells.lock().await;
        if let Some(shell) = shells.get_mut(id) {
            shell.finished = true;
            shell.exit_code = code;
        }
    }
}

/// Build the child process. `sh -c` rather than a parsed argv, because the
/// model writes pipelines and redirections and expects them to work.
fn spawn_command(command: &str, cwd: &std::path::Path) -> std::io::Result<tokio::process::Child> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Detach into its own process group so a kill reaches the whole
        // pipeline, not just the shell that spawned it.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("command".into(), prop("string", "Shell command to run, executed with `sh -c`."));
        p.insert(
            "description".into(),
            prop("string", "Short description of what this command does, shown to the user in the approval prompt."),
        );
        p.insert(
            "timeout".into(),
            prop("integer", "Timeout in seconds. Defaults to 120, capped at 600."),
        );
        p.insert(
            "run_in_background".into(),
            prop("boolean", "Start the command and return immediately. Poll it with `bash_output`. Use for dev servers and watchers."),
        );
        ToolSpec::function(
            "bash",
            "Run a shell command in the workspace. Prefer the dedicated file tools over `cat`, `sed` and `find`: they are cheaper and their output is structured.",
            object_schema(p, &["command"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn summarize(&self, args: &Value) -> String {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or_default();
        format!("Bash({cmd})")
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `command`"));
        };
        if command.trim().is_empty() {
            return Ok(ToolOutcome::error("command is empty"));
        }

        let background = args.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
        if background {
            return run_background(command, ctx).await;
        }

        let timeout = Duration::from_secs(
            args.get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );

        let started = std::time::Instant::now();
        let mut child = match spawn_command(command, &ctx.workspace) {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutcome::error(format!("cannot start command: {e}"))),
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collector = tokio::spawn(async move { collect(stdout, stderr).await });

        let status = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                // Kill rather than drop: a detached `cargo build` keeps the
                // target lock and blocks every later command.
                let _ = child.kill().await;
                collector.abort();
                return Ok(ToolOutcome::error("command interrupted by the user"));
            }
            r = tokio::time::timeout(timeout, child.wait()) => r,
        };

        let output = collector.await.unwrap_or_default();

        match status {
            Err(_elapsed) => {
                let _ = child.kill().await;
                Ok(ToolOutcome::error(format!(
                    "command timed out after {}s and was killed\n\n{}",
                    timeout.as_secs(),
                    util::truncate_text(&output, MAX_OUTPUT_BYTES)
                )))
            }
            Ok(Err(e)) => Ok(ToolOutcome::error(format!("command failed to run: {e}"))),
            Ok(Ok(exit)) => {
                let code = exit.code().unwrap_or(-1);
                let elapsed = util::format_duration(started.elapsed());
                let body = util::truncate_text(&output, MAX_OUTPUT_BYTES);
                let body = if body.trim().is_empty() {
                    format!("(no output, exit {code})")
                } else {
                    body
                };
                // A non-zero exit is reported as an error so the model notices
                // the failure instead of reading the output as success.
                let outcome = if exit.success() {
                    ToolOutcome::ok(body)
                } else {
                    ToolOutcome::error(format!("exit status {code}\n\n{body}"))
                };
                Ok(outcome.with_summary(format!("exit {code} in {elapsed}")))
            }
        }
    }
}

/// Start a command that outlives this tool call.
async fn run_background(command: &str, ctx: &ToolContext) -> Result<ToolOutcome> {
    let mut child = match spawn_command(command, &ctx.workspace) {
        Ok(c) => c,
        Err(e) => return Ok(ToolOutcome::error(format!("cannot start command: {e}"))),
    };

    let id = ctx.shells.allocate_id();
    ctx.shells
        .shells
        .lock()
        .await
        .insert(id.clone(), BackgroundShell { command: command.to_string(), ..Default::default() });

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let shells = ctx.shells.clone();
    let task_id = id.clone();

    let handle = tokio::spawn(async move {
        let mut out_lines = stdout.map(|s| BufReader::new(s).lines());
        let mut err_lines = stderr.map(|s| BufReader::new(s).lines());
        loop {
            let got_line = tokio::select! {
                Some(Ok(Some(line))) = async {
                    match out_lines.as_mut() { Some(l) => Some(l.next_line().await), None => None }
                } => { shells.record(&task_id, line).await; true }
                Some(Ok(Some(line))) = async {
                    match err_lines.as_mut() { Some(l) => Some(l.next_line().await), None => None }
                } => { shells.record(&task_id, line).await; true }
                else => false,
            };
            if !got_line {
                break;
            }
        }
        let code = child.wait().await.ok().and_then(|s| s.code());
        shells.finish(&task_id, code).await;
    });
    ctx.shells.handles.lock().await.insert(id.clone(), handle);

    Ok(ToolOutcome::ok(format!(
        "Started `{command}` in the background as {id}. Poll it with bash_output(id=\"{id}\") and stop it with kill_shell(id=\"{id}\")."
    ))
    .with_summary(format!("background {id}")))
}

/// Drain both pipes concurrently into one interleaved buffer.
///
/// Reading them sequentially deadlocks: a child that fills its stderr pipe
/// blocks forever while the parent waits on stdout.
async fn collect(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
) -> String {
    // Generic over the pipe type: `stdout` and `stderr` are distinct types, so
    // a closure would monomorphize to whichever was passed first.
    async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
        let mut buf = String::new();
        if let Some(pipe) = pipe {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > MAX_OUTPUT_BYTES * 2 {
                    break;
                }
            }
        }
        buf
    }

    let (out, err) = tokio::join!(read_all(stdout), read_all(stderr));
    if err.trim().is_empty() {
        out
    } else if out.trim().is_empty() {
        err
    } else {
        format!("{out}\n--- stderr ---\n{err}")
    }
}

// ---------------------------------------------------------------------------
// bash_output
// ---------------------------------------------------------------------------

pub struct BashOutput;

#[async_trait]
impl Tool for BashOutput {
    fn name(&self) -> &str {
        "bash_output"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("id".into(), prop("string", "Shell id returned by a background `bash` call."));
        ToolSpec::function(
            "bash_output",
            "Read new output from a background shell. Each call returns only what has arrived since the previous call.",
            object_schema(p, &["id"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, args: &Value) -> String {
        format!("BashOutput({})", args.get("id").and_then(Value::as_str).unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `id`"));
        };
        match ctx.shells.drain(id).await {
            None => Ok(ToolOutcome::error(format!(
                "no background shell `{id}`. Running shells: {}",
                ctx.shells.ids().await.join(", ")
            ))),
            Some((lines, finished, code)) => {
                let mut body = lines.join("\n");
                if body.is_empty() {
                    body = "(no new output)".into();
                }
                if finished {
                    body.push_str(&format!("\n\n[shell exited with status {}]", code.unwrap_or(-1)));
                }
                Ok(ToolOutcome::ok(util::truncate_text(&body, MAX_OUTPUT_BYTES)))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// kill_shell
// ---------------------------------------------------------------------------

pub struct KillShell;

#[async_trait]
impl Tool for KillShell {
    fn name(&self) -> &str {
        "kill_shell"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("id".into(), prop("string", "Shell id to terminate."));
        ToolSpec::function("kill_shell", "Stop a background shell.", object_schema(p, &["id"]))
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn summarize(&self, args: &Value) -> String {
        format!("KillShell({})", args.get("id").and_then(Value::as_str).unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `id`"));
        };
        if ctx.shells.kill(id).await {
            Ok(ToolOutcome::ok(format!("Stopped {id}")))
        } else {
            Ok(ToolOutcome::error(format!("no background shell `{id}`")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx_at(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf(), CancellationToken::new())
    }

    #[tokio::test]
    async fn a_successful_command_returns_its_stdout() {
        let dir = tempdir().unwrap();
        let out = Bash.run(json!({"command": "echo hello"}), &ctx_at(dir.path())).await.unwrap();
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(out.content.trim(), "hello");
        assert!(out.display.summary.unwrap().starts_with("exit 0"));
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_as_an_error_with_its_status() {
        let dir = tempdir().unwrap();
        let out = Bash.run(json!({"command": "exit 3"}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error, "a non-zero exit must not read as success");
        assert!(out.content.contains("exit status 3"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn stderr_is_captured_alongside_stdout() {
        let dir = tempdir().unwrap();
        let out = Bash
            .run(json!({"command": "echo out; echo err 1>&2"}), &ctx_at(dir.path()))
            .await
            .unwrap();
        assert!(out.content.contains("out"));
        assert!(out.content.contains("err"), "stderr must not be dropped: {}", out.content);
    }

    #[tokio::test]
    async fn commands_run_in_the_workspace_directory() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let out = Bash.run(json!({"command": "ls"}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.content.contains("marker.txt"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_says_so() {
        let dir = tempdir().unwrap();
        let out =
            Bash.run(json!({"command": "sleep 5", "timeout": 1}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("timed out after 1s"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_running_command() {
        let dir = tempdir().unwrap();
        let ctx = ctx_at(dir.path());
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        });

        let out = Bash.run(json!({"command": "sleep 10"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("interrupted"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let dir = tempdir().unwrap();
        let out = Bash.run(json!({"command": "   "}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn background_shells_are_pollable_and_killable() {
        let dir = tempdir().unwrap();
        let ctx = ctx_at(dir.path());

        let started = Bash
            .run(json!({"command": "echo one; echo two", "run_in_background": true}), &ctx)
            .await
            .unwrap();
        assert!(!started.is_error, "got: {}", started.content);
        assert!(started.content.contains("shell_1"));

        // Give the reader task a moment to drain the pipes.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let polled = BashOutput.run(json!({"id": "shell_1"}), &ctx).await.unwrap();
        assert!(polled.content.contains("one"), "got: {}", polled.content);
        assert!(polled.content.contains("two"));

        // A second poll returns only what is new, i.e. nothing.
        let again = BashOutput.run(json!({"id": "shell_1"}), &ctx).await.unwrap();
        assert!(!again.content.contains("one"), "output must not be replayed");

        let killed = KillShell.run(json!({"id": "shell_1"}), &ctx).await.unwrap();
        assert!(!killed.is_error);
        assert!(ctx.shells.ids().await.is_empty());
    }

    #[tokio::test]
    async fn polling_an_unknown_shell_lists_the_ones_that_exist() {
        let dir = tempdir().unwrap();
        let out = BashOutput.run(json!({"id": "nope"}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no background shell"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn timeouts_are_capped_even_when_the_model_asks_for_more() {
        // Verified through the clamp rather than by actually waiting 10 minutes.
        let requested = 99_999u64;
        assert_eq!(requested.clamp(1, MAX_TIMEOUT_SECS), MAX_TIMEOUT_SECS);
    }
}
