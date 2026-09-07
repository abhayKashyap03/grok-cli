//! Session persistence: the conversation transcript on disk.
//!
//! One session is one JSONL file under
//! `~/.grok/projects/<workspace-slug>/<uuid>.jsonl`. Records are appended as
//! they happen and never rewritten, which gives three things for free:
//!
//! * a crash mid-turn loses at most the turn in flight,
//! * `--resume` is a linear read with no repair step, and
//! * a corrupt line skips one record instead of destroying the session.
//!
//! JSONL rather than one JSON document precisely because a single document
//! would have to be rewritten in full on every message and would be
//! unrecoverable if the process died during the write.
//!
//! ```jsonl
//! {"kind":"meta","at":"2026-09-07T05:24:20Z","id":"7f3c…","workspace":"/repo","model":"grok-4-1-fast-non-reasoning","title":null}
//! {"kind":"message","at":"2026-09-07T05:24:21Z","message":{"role":"user","content":"fix the auth bug"}}
//! {"kind":"usage","at":"2026-09-07T05:24:29Z","usage":{"prompt_tokens":1200,"completion_tokens":88,"total_tokens":1288}}
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::api::{Message, Usage};
use crate::config;
use crate::util;

/// One line of a transcript file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// Written once, first. Later metadata changes append a fresh `meta`
    /// record; the last one read wins.
    Meta {
        at: String,
        id: String,
        workspace: String,
        model: String,
        #[serde(default)]
        title: Option<String>,
    },
    /// A conversation message, exactly as sent to or received from the API.
    Message { at: String, message: Message },
    /// Token accounting for one API round trip.
    Usage { at: String, usage: Usage },
    /// A compaction boundary. Everything before it was replaced by `summary`.
    Compaction { at: String, summary: String, dropped_messages: usize },
}

/// Summary of a stored session, for the resume picker.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub title: Option<String>,
    pub model: String,
    pub started_at: String,
    pub message_count: usize,
    pub modified: std::time::SystemTime,
}

impl SessionSummary {
    /// Best available label: the generated title, else the first user message.
    pub fn label(&self) -> String {
        self.title.clone().unwrap_or_else(|| format!("session {}", &self.id[..8.min(self.id.len())]))
    }
}

/// A live session, appending to its transcript as the conversation proceeds.
pub struct Session {
    pub id: String,
    pub path: Option<PathBuf>,
    pub model: String,
    pub title: Option<String>,
    /// The full conversation, including the system message at index 0.
    pub messages: Vec<Message>,
    /// Cumulative usage across every request in this session.
    pub usage: Usage,
    /// Set when persistence is unavailable (no home directory, read-only disk).
    /// The session still works; it just will not survive a restart.
    pub persistence_error: Option<String>,
}

/// Where transcripts live for one workspace.
///
/// The root is passed in rather than read from the environment at call time.
/// Reading `$HOME` inside every list/find call would make the whole API
/// implicitly global, which is untestable in parallel and surprising when a
/// caller wants a session stored somewhere else.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default store for `workspace`, under the user's home directory.
    /// `None` when there is no home directory to write to.
    pub fn for_workspace(workspace: &Path) -> Option<Self> {
        config::sessions_dir(workspace).map(Self::new)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Start a new session and begin its transcript.
    ///
    /// Failure to open the transcript is recorded on the session rather than
    /// returned: losing the ability to resume later is not a reason to refuse
    /// to run now.
    pub fn create(&self, workspace: &Path, model: &str) -> Session {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = Session::in_memory(&id, model);

        match std::fs::create_dir_all(&self.root) {
            Err(e) => {
                session.persistence_error = Some(format!("cannot create {}: {e}", self.root.display()));
            }
            Ok(()) => {
                session.path = Some(self.root.join(format!("{id}.jsonl")));
                let meta = TranscriptEntry::Meta {
                    at: util::now_rfc3339(),
                    id,
                    workspace: workspace.display().to_string(),
                    model: model.to_string(),
                    title: None,
                };
                if let Err(e) = session.append(&meta) {
                    session.persistence_error = Some(e.to_string());
                }
            }
        }

        session
    }

    /// Stored sessions, newest first. Sessions with no messages are omitted:
    /// they are aborted starts and offering them to resume is noise.
    pub fn list(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };

        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(text) = std::fs::read_to_string(&path) else { continue };

            let mut summary = SessionSummary {
                id: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                path: path.clone(),
                title: None,
                model: String::new(),
                started_at: String::new(),
                message_count: 0,
                modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            };
            let mut first_user_text: Option<String> = None;

            for line in text.lines() {
                match serde_json::from_str::<TranscriptEntry>(line) {
                    Ok(TranscriptEntry::Meta { at, id, model, title, .. }) => {
                        summary.id = id;
                        summary.model = model;
                        if summary.started_at.is_empty() {
                            summary.started_at = at;
                        }
                        if title.is_some() {
                            summary.title = title;
                        }
                    }
                    Ok(TranscriptEntry::Message { message, .. }) => {
                        summary.message_count += 1;
                        if first_user_text.is_none() && message.is_role(crate::api::Role::User) {
                            first_user_text = message.content.clone();
                        }
                    }
                    _ => {}
                }
            }

            // Fall back to the opening line of the conversation as a label.
            if summary.title.is_none()
                && let Some(text) = first_user_text
            {
                let first_line = text.lines().next().unwrap_or_default();
                summary.title = Some(util::truncate_text(first_line, 72));
            }

            if summary.message_count > 0 {
                out.push(summary);
            }
        }

        out.sort_by(|a, b| b.modified.cmp(&a.modified));
        out
    }

    /// The most recent session. Backs `--continue`.
    pub fn most_recent(&self) -> Option<SessionSummary> {
        self.list().into_iter().next()
    }

    /// Find a session by id, or by an unambiguous id prefix.
    pub fn find(&self, id_or_prefix: &str) -> Option<SessionSummary> {
        let all = self.list();
        if let Some(exact) = all.iter().find(|s| s.id == id_or_prefix) {
            return Some(exact.clone());
        }
        let mut matches = all.into_iter().filter(|s| s.id.starts_with(id_or_prefix));
        let first = matches.next()?;
        // An ambiguous prefix resolves to nothing rather than to an arbitrary one.
        matches.next().is_none().then_some(first)
    }
}

impl Session {
    /// A session that exists only in memory. Used when there is nowhere to
    /// persist to, and as the starting point for a persisted session.
    pub fn in_memory(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            path: None,
            model: model.to_string(),
            title: None,
            messages: Vec::new(),
            usage: Usage::default(),
            persistence_error: None,
        }
    }

    /// Load an existing session from its transcript.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let mut session = Self {
            id: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            path: Some(path.to_path_buf()),
            model: config::DEFAULT_MODEL.to_string(),
            title: None,
            messages: Vec::new(),
            usage: Usage::default(),
            persistence_error: None,
        };

        for (lineno, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // A truncated final line is the normal result of a crash mid-write.
            // Skip it and keep the rest rather than failing the whole resume.
            let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) else {
                tracing::warn!(path = %path.display(), lineno = lineno + 1, "skipping unreadable transcript line");
                continue;
            };
            session.apply(entry);
        }

        Ok(session)
    }

    fn apply(&mut self, entry: TranscriptEntry) {
        match entry {
            TranscriptEntry::Meta { id, model, title, .. } => {
                self.id = id;
                self.model = model;
                if title.is_some() {
                    self.title = title;
                }
            }
            TranscriptEntry::Message { message, .. } => self.messages.push(message),
            TranscriptEntry::Usage { usage, .. } => self.usage.add(&usage),
            TranscriptEntry::Compaction { summary, .. } => {
                // Replay of a compaction: everything before it is already gone
                // from the transcript's logical state, so drop what we have and
                // start from the summary.
                self.messages.retain(|m| m.is_role(crate::api::Role::System));
                self.messages.push(Message::user(summary));
            }
        }
    }

    fn append(&self, entry: &TranscriptEntry) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(entry)?)
            .with_context(|| format!("writing to {}", path.display()))?;
        Ok(())
    }

    /// Add a message to the conversation and persist it.
    pub fn push(&mut self, message: Message) {
        let entry = TranscriptEntry::Message { at: util::now_rfc3339(), message: message.clone() };
        self.messages.push(message);
        self.persist(&entry);
    }

    /// Record usage from one API round trip.
    pub fn record_usage(&mut self, usage: &Usage) {
        self.usage.add(usage);
        self.persist(&TranscriptEntry::Usage { at: util::now_rfc3339(), usage: *usage });
    }

    /// Replace the conversation with a compacted version.
    pub fn record_compaction(&mut self, summary: String, dropped: usize) {
        self.persist(&TranscriptEntry::Compaction {
            at: util::now_rfc3339(),
            summary,
            dropped_messages: dropped,
        });
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        let title = title.into();
        self.title = Some(title.clone());
        self.persist(&TranscriptEntry::Meta {
            at: util::now_rfc3339(),
            id: self.id.clone(),
            workspace: String::new(),
            model: self.model.clone(),
            title: Some(title),
        });
    }

    /// Write an entry, downgrading a failure to a recorded warning.
    fn persist(&mut self, entry: &TranscriptEntry) {
        if let Err(e) = self.append(entry) {
            // Record once; a failing disk will fail on every subsequent write
            // and the user does not need to be told repeatedly.
            if self.persistence_error.is_none() {
                self.persistence_error = Some(e.to_string());
            }
        }
    }

    /// Drop everything except the system prompt. Backs `/clear`.
    pub fn clear(&mut self) {
        self.messages.retain(|m| m.is_role(crate::api::Role::System));
        self.usage = Usage::default();
    }

    /// Number of user-visible turns, for the status line.
    pub fn turn_count(&self) -> usize {
        self.messages.iter().filter(|m| m.is_role(crate::api::Role::User)).count()
    }

    /// Estimated tokens currently in context.
    ///
    /// Uses the last reported prompt token count when available, because that
    /// is ground truth from the provider; falls back to a character estimate
    /// before the first response lands.
    pub fn estimated_context_tokens(&self) -> u64 {
        // Four bytes per token, plus a small per-message overhead for the role
        // and structural tokens the provider adds around each message.
        const PER_MESSAGE_OVERHEAD: u64 = 4;
        self.messages
            .iter()
            .map(|m| (m.char_len() as u64).div_ceil(4) + PER_MESSAGE_OVERHEAD)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A store and a workspace in scratch directories. No process-global state,
    /// so these tests are safe to run in parallel.
    fn scratch() -> (tempfile::TempDir, tempfile::TempDir, SessionStore) {
        let store_dir = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let store = SessionStore::new(store_dir.path().join("sessions"));
        (store_dir, workspace, store)
    }

    #[test]
    fn a_new_session_writes_a_meta_record_and_round_trips() {
        let (_store_dir, workspace, store) = scratch();

        let mut session = store.create(workspace.path(), "grok-4-1-fast-non-reasoning");
        assert!(session.persistence_error.is_none(), "{:?}", session.persistence_error);
        session.push(Message::user("fix the auth bug"));
        session.push(Message::assistant("Looking at it."));
        session.record_usage(&Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            total_tokens: 14,
            prompt_tokens_details: None,
        });

        let path = session.path.clone().unwrap();
        let reloaded = Session::load(&path).unwrap();

        assert_eq!(reloaded.id, session.id);
        assert_eq!(reloaded.model, "grok-4-1-fast-non-reasoning");
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content.as_deref(), Some("fix the auth bug"));
        assert_eq!(reloaded.usage.total_tokens, 14, "usage survives a reload");
    }

    #[test]
    fn a_truncated_final_line_does_not_destroy_the_session() {
        let (_store_dir, workspace, store) = scratch();

        let mut session = store.create(workspace.path(), "grok-4");
        session.push(Message::user("first"));
        let path = session.path.clone().unwrap();

        // Simulate a crash partway through appending the next record.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{{\"kind\":\"message\",\"at\":\"2026-09").unwrap();
        drop(f);

        let reloaded = Session::load(&path).unwrap();
        assert_eq!(reloaded.messages.len(), 1, "the intact record is still recovered");
    }

    #[test]
    fn listing_labels_sessions_by_their_opening_message() {
        let (_store_dir, workspace, store) = scratch();

        let mut s = store.create(workspace.path(), "grok-4");
        s.push(Message::user("add retry to the http client"));

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title.as_deref(), Some("add retry to the http client"));
        assert_eq!(listed[0].message_count, 1);
    }

    #[test]
    fn an_explicit_title_wins_over_the_opening_message() {
        let (_store_dir, workspace, store) = scratch();

        let mut s = store.create(workspace.path(), "grok-4");
        s.push(Message::user("some rambling opening line"));
        s.set_title("HTTP retry work");

        assert_eq!(store.list()[0].title.as_deref(), Some("HTTP retry work"));
    }

    #[test]
    fn empty_sessions_are_not_offered_for_resume() {
        let (_store_dir, workspace, store) = scratch();

        let _abandoned = store.create(workspace.path(), "grok-4");
        assert!(store.list().is_empty(), "a session with no messages is not resumable");
        assert!(store.most_recent().is_none());
    }

    #[test]
    fn listing_orders_the_most_recent_session_first() {
        let (_store_dir, workspace, store) = scratch();

        let mut older = store.create(workspace.path(), "grok-4");
        older.push(Message::user("older"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut newer = store.create(workspace.path(), "grok-4");
        newer.push(Message::user("newer"));

        assert_eq!(store.most_recent().unwrap().id, newer.id);
    }

    #[test]
    fn sessions_resolve_by_id_prefix_but_never_ambiguously() {
        let (_store_dir, workspace, store) = scratch();

        let mut s = store.create(workspace.path(), "grok-4");
        s.push(Message::user("hello"));
        let id = s.id.clone();

        assert!(store.find(&id).is_some(), "exact id resolves");
        assert!(store.find(&id[..8]).is_some(), "unique prefix resolves");
        assert!(store.find("zzzz").is_none());
    }

    #[test]
    fn clear_keeps_the_system_prompt_and_drops_the_rest() {
        let mut s = Session {
            id: "x".into(),
            path: None,
            model: "grok-4".into(),
            title: None,
            messages: vec![Message::system("you are grok"), Message::user("hi"), Message::assistant("hello")],
            usage: Usage { total_tokens: 99, ..Default::default() },
            persistence_error: None,
        };
        s.clear();
        assert_eq!(s.messages.len(), 1);
        assert!(s.messages[0].is_role(crate::api::Role::System));
        assert_eq!(s.usage.total_tokens, 0);
    }

    #[test]
    fn a_session_that_cannot_be_persisted_still_runs() {
        let dir = tempdir().unwrap();
        // A file where the session directory should go makes create_dir_all fail.
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, "").unwrap();
        let store = SessionStore::new(blocker.join("sessions"));

        let workspace = tempdir().unwrap();
        let mut s = store.create(workspace.path(), "grok-4");
        assert!(s.persistence_error.is_some(), "the failure is recorded");
        assert!(s.path.is_none());

        s.push(Message::user("still works"));
        assert_eq!(s.messages.len(), 1, "the conversation continues in memory");
    }
}
