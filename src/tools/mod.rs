//! The tool abstraction: what the model is allowed to do to the machine.
//!
//! A [`Tool`] is three things bundled together:
//!
//! * a JSON-schema [`ToolSpec`] advertised to the model,
//! * a [`ToolKind`] classification the permission engine uses to decide whether
//!   the call needs approval, and
//! * an async `run` that does the work.
//!
//! Classification lives on the tool rather than in a lookup table in the
//! permission engine so that a new tool cannot be added without declaring how
//! dangerous it is.

pub mod fs;
pub mod misc;
pub mod shell;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::api::ToolSpec;

/// How much damage a tool can do. Drives the default permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Observes the filesystem without changing it.
    Read,
    /// Creates or modifies files.
    Edit,
    /// Runs arbitrary commands. The most dangerous class.
    Execute,
    /// Reaches the network.
    Network,
    /// Changes only harness-internal state (todo list, plan).
    Meta,
}

impl ToolKind {
    /// Whether this class mutates anything outside the harness. Plan mode
    /// refuses everything for which this is true.
    pub fn mutates(self) -> bool {
        matches!(self, Self::Edit | Self::Execute)
    }
}

/// Structured detail for the UI, alongside the plain text sent to the model.
///
/// The model gets prose; the human gets a rendered diff or a file list. Keeping
/// them separate means improving the display never risks changing what the
/// model sees.
#[derive(Debug, Clone, Default)]
pub struct ToolDisplay {
    /// Path the operation touched, for the header line.
    pub path: Option<String>,
    /// `(added, removed)` line counts for edits.
    pub diff_stats: Option<(usize, usize)>,
    /// Unified diff to render under the header.
    pub diff: Vec<crate::util::DiffLine>,
    /// One-line summary, e.g. "142 lines" or "7 matches in 3 files".
    pub summary: Option<String>,
}

/// The result of running a tool.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Text handed back to the model as the tool result.
    pub content: String,
    /// Extra detail for rendering. Never seen by the model.
    pub display: ToolDisplay,
    /// True when the tool failed. Errors are still returned to the model as
    /// tool results rather than aborting the turn — the model usually recovers
    /// by fixing its arguments, and aborting would strand the conversation with
    /// an unanswered tool call, which the API rejects on the next request.
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), display: ToolDisplay::default(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), display: ToolDisplay::default(), is_error: true }
    }

    pub fn with_display(mut self, display: ToolDisplay) -> Self {
        self.display = display;
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.display.summary = Some(summary.into());
        self
    }
}

/// A tracked task in the model's to-do list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "☐",
            Self::InProgress => "◐",
            Self::Completed => "☑",
        }
    }
}

/// Shared state every tool invocation can reach.
#[derive(Clone)]
pub struct ToolContext {
    /// Root the agent is confined to.
    pub workspace: PathBuf,
    /// Modification times captured when a file was last read.
    ///
    /// The edit tools refuse to write a file that changed since the model read
    /// it. Without this the model happily clobbers edits the user made in their
    /// editor between the read and the write.
    pub read_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
    /// Long-running background shells, keyed by id.
    pub shells: Arc<shell::ShellManager>,
    /// The model's current to-do list.
    pub todos: Arc<Mutex<Vec<TodoItem>>>,
    /// Fires when the user interrupts the turn.
    pub cancel: CancellationToken,
}

impl ToolContext {
    pub fn new(workspace: PathBuf, cancel: CancellationToken) -> Self {
        // Canonicalize the root once, here, so every containment check compares
        // like with like. On macOS `/var` is itself a symlink to `/private/var`,
        // so an un-canonicalized root makes every legitimate path look like an
        // escape the moment symlinks are resolved.
        let workspace = workspace.canonicalize().unwrap_or(workspace);
        Self {
            workspace,
            read_files: Arc::new(Mutex::new(HashMap::new())),
            shells: Arc::new(shell::ShellManager::default()),
            todos: Arc::new(Mutex::new(Vec::new())),
            cancel,
        }
    }

    /// Canonical key for the freshness map.
    ///
    /// Paths reach these methods both canonicalized (from the sandbox
    /// resolver) and raw, and two spellings of the same file must not produce
    /// two entries — that would silently disable the read-before-edit guard.
    fn stamp_key(path: &std::path::Path) -> std::path::PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    /// Record that `path` was read, so a later edit can verify freshness.
    pub async fn mark_read(&self, path: &std::path::Path) {
        if let Ok(meta) = tokio::fs::metadata(path).await
            && let Ok(mtime) = meta.modified()
        {
            self.read_files.lock().await.insert(Self::stamp_key(path), mtime);
        }
    }

    /// Error message if `path` was never read, or changed since it was.
    /// Returns `None` when the edit is safe to proceed.
    pub async fn staleness_error(&self, path: &std::path::Path) -> Option<String> {
        let Ok(meta) = tokio::fs::metadata(path).await else {
            // File does not exist yet: creating it is always safe.
            return None;
        };
        let stamps = self.read_files.lock().await;
        match stamps.get(&Self::stamp_key(path)) {
            None => Some(format!(
                "{} has not been read in this session. Read it first so the edit applies to its current contents.",
                path.display()
            )),
            Some(&seen) => match meta.modified() {
                Ok(now) if now > seen => Some(format!(
                    "{} changed on disk after it was read. Read it again before editing, or the change will be overwritten.",
                    path.display()
                )),
                _ => None,
            },
        }
    }
}

/// Something the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Name as it appears on the wire. Must be unique within a registry.
    fn name(&self) -> &str;

    /// Schema advertised to the model.
    fn spec(&self) -> ToolSpec;

    /// Danger classification, consumed by the permission engine.
    fn kind(&self) -> ToolKind;

    /// One-line description of what *this specific call* will do, shown in the
    /// permission prompt and the transcript. Must never execute anything.
    fn summarize(&self, args: &Value) -> String;

    /// Do the work.
    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome>;

    /// Whether concurrent invocations are safe. Read-only tools run in parallel
    /// when the model requests several at once; anything that mutates is
    /// serialized so two edits to the same file cannot interleave.
    fn parallel_safe(&self) -> bool {
        self.kind() == ToolKind::Read
    }
}

/// The set of tools available to one agent.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every built-in tool. MCP tools are registered on top of these at startup.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(fs::ReadFile));
        r.register(Arc::new(fs::WriteFile));
        r.register(Arc::new(fs::EditFile));
        r.register(Arc::new(fs::ListFiles));
        r.register(Arc::new(fs::Glob));
        r.register(Arc::new(fs::Grep));
        r.register(Arc::new(shell::Bash));
        r.register(Arc::new(shell::BashOutput));
        r.register(Arc::new(shell::KillShell));
        r.register(Arc::new(misc::TodoWrite));
        r.register(Arc::new(misc::WebFetch));
        r
    }

    /// Add a tool. A later registration with the same name replaces the earlier
    /// one, which is how a project can shadow a built-in.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == tool.name()) {
            *slot = tool;
            return;
        }
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Specs to advertise to the model.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    /// Restrict to a named subset, for subagents with a narrower remit.
    ///
    /// An unknown name is ignored rather than erroring: subagent definitions
    /// are user-authored markdown, and a typo there should narrow the toolset,
    /// not crash the run.
    pub fn subset(&self, allowed: &[String]) -> Self {
        Self {
            tools: self.tools.iter().filter(|t| allowed.iter().any(|a| a == t.name())).cloned().collect(),
        }
    }

    /// Drop every tool that can mutate the machine. Used by plan mode.
    pub fn read_only(&self) -> Self {
        Self { tools: self.tools.iter().filter(|t| !t.kind().mutates()).cloned().collect() }
    }

    /// Drop one tool by name. Used to keep `task` out of a subagent's toolset,
    /// so subagents cannot spawn subagents.
    pub fn without(&self, name: &str) -> Self {
        Self { tools: self.tools.iter().filter(|t| t.name() != name).cloned().collect() }
    }
}

/// Build a JSON-schema object for a tool's parameters.
pub fn object_schema(props: serde_json::Map<String, Value>, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// Shorthand for one property in a schema.
pub fn prop(ty: &str, description: &str) -> Value {
    serde_json::json!({ "type": ty, "description": description })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct Dummy(&'static str, ToolKind);

    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &str {
            self.0
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec::function(self.0, "dummy", object_schema(serde_json::Map::new(), &[]))
        }
        fn kind(&self) -> ToolKind {
            self.1
        }
        fn summarize(&self, _: &Value) -> String {
            self.0.to_string()
        }
        async fn run(&self, _: Value, _: &ToolContext) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok("ok"))
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), CancellationToken::new())
    }

    #[test]
    fn builtins_have_unique_names() {
        let r = ToolRegistry::with_builtins();
        let mut names = r.names();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate tool names would shadow each other silently");
        assert!(before >= 10, "expected the full built-in suite, got {before}");
    }

    #[test]
    fn re_registering_a_name_replaces_rather_than_duplicates() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(Dummy("x", ToolKind::Read)));
        r.register(Arc::new(Dummy("x", ToolKind::Execute)));
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("x").unwrap().kind(), ToolKind::Execute, "the later registration wins");
    }

    #[test]
    fn read_only_view_drops_every_mutating_tool() {
        let r = ToolRegistry::with_builtins().read_only();
        assert!(r.get("bash").is_none(), "plan mode must not expose command execution");
        assert!(r.get("write_file").is_none());
        assert!(r.get("edit_file").is_none());
        assert!(r.get("read_file").is_some(), "reads stay available in plan mode");
        assert!(r.get("grep").is_some());
    }

    #[test]
    fn subset_ignores_unknown_names() {
        let r = ToolRegistry::with_builtins().subset(&["read_file".into(), "not_a_real_tool".into()]);
        assert_eq!(r.names(), vec!["read_file"]);
    }

    #[test]
    fn only_reads_are_declared_parallel_safe() {
        let r = ToolRegistry::with_builtins();
        assert!(r.get("read_file").unwrap().parallel_safe());
        assert!(!r.get("edit_file").unwrap().parallel_safe(), "concurrent edits could interleave");
        assert!(!r.get("bash").unwrap().parallel_safe());
    }

    #[tokio::test]
    async fn editing_an_unread_file_is_refused() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let ctx = ctx();
        let err = ctx.staleness_error(&file).await.expect("unread file must be refused");
        assert!(err.contains("has not been read"), "got: {err}");
    }

    #[tokio::test]
    async fn editing_a_file_that_changed_since_the_read_is_refused() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let ctx = ctx();
        ctx.mark_read(&file).await;
        assert!(ctx.staleness_error(&file).await.is_none(), "a fresh read permits the edit");

        // Simulate the user editing the file in their own editor. The mtime is
        // set explicitly rather than relying on filesystem timestamp
        // granularity, which is one second on some filesystems.
        std::fs::write(&file, "v2").unwrap();
        let f = std::fs::File::options().write(true).open(&file).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(2)).unwrap();

        let err = ctx.staleness_error(&file).await.expect("stale file must be refused");
        assert!(err.contains("changed on disk"), "got: {err}");
    }

    #[tokio::test]
    async fn creating_a_new_file_needs_no_prior_read() {
        let dir = tempdir().unwrap();
        let ctx = ctx();
        assert!(ctx.staleness_error(&dir.path().join("new.txt")).await.is_none());
    }
}
