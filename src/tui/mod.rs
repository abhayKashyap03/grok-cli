//! The terminal interface.
//!
//! The old prototype's fatal bug was calling blocking `event::read()` in the
//! render loop: a streamed response could not appear until the user pressed a
//! key. Here the loop selects over three async sources — terminal input, agent
//! events, and permission requests — so tokens paint as they arrive and Esc is
//! responsive while the model is mid-sentence.
//!
//! The agent lives behind an `Arc<Mutex<_>>` and a turn runs in its own task.
//! The UI never blocks on that lock during a turn: everything it needs to draw
//! is mirrored into [`App`] from the event stream. That is the difference
//! between an interface that stays responsive under load and one that freezes
//! whenever the model is thinking.

pub mod input;
pub mod markdown;
pub mod render;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, PermissionRequest, PermissionResponse, StopReason};
use crate::api::{Role, Usage};
use crate::commands::{self, CommandAction};
use crate::config::{Config, MODELS, PermissionMode};
use crate::session::SessionStore;
use crate::tools::{TodoItem, ToolDisplay};

use input::TextArea;
use markdown::Theme;

/// One entry in the scrollback.
#[derive(Debug, Clone)]
pub enum TranscriptItem {
    User(String),
    /// `done` flips when the turn ends, which stops the caret being drawn.
    Assistant { text: String, done: bool },
    Reasoning { text: String, collapsed: bool },
    Tool {
        id: String,
        name: String,
        summary: String,
        state: ToolState,
    },
    /// Harness output: warnings, command results, notices.
    Notice { text: String, level: NoticeLevel },
}

#[derive(Debug, Clone)]
pub enum ToolState {
    Running { started: Instant },
    Finished { summary: Option<String>, display: Box<ToolDisplay>, is_error: bool, duration: Duration },
    Denied { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
    Success,
}

/// A modal covering the transcript.
pub enum Overlay {
    None,
    /// Waiting for the user to approve a tool call.
    Permission(Box<PermissionRequest>),
    /// A list to choose from.
    Picker { kind: PickerKind, title: String, entries: Vec<PickerEntry>, selected: usize },
    /// Slash-command completion.
    Palette { entries: Vec<commands::SlashCommand>, selected: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Mode,
    Session,
}

#[derive(Debug, Clone)]
pub struct PickerEntry {
    pub label: String,
    pub detail: String,
    /// Opaque value the action handler interprets.
    pub value: String,
}

/// Everything the renderer needs.
pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub transcript: Vec<TranscriptItem>,
    pub textarea: TextArea,
    pub overlay: Overlay,

    /// Lines scrolled up from the bottom. Zero means pinned to the newest
    /// output, which is what the user wants unless they deliberately scrolled.
    pub scroll: u16,
    /// Set once the user scrolls up, so incoming tokens stop yanking the view.
    pub scroll_locked: bool,
    /// Total rendered height of the transcript, updated each draw.
    pub content_height: u16,
    pub viewport_height: u16,

    pub busy: bool,
    pub spinner_frame: usize,
    pub status: String,
    pub usage: Usage,
    pub model: String,
    pub mode: PermissionMode,
    pub todos: Vec<TodoItem>,
    pub session_id: String,
    pub context_tokens: u64,
    pub should_quit: bool,
    /// Set by a first Ctrl+C so a second one within the window exits.
    quit_armed_at: Option<Instant>,
}

/// Spinner frames. Braille dots animate smoothly in most terminal fonts.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// How long a Ctrl+C stays "armed" before a second press stops meaning quit.
const QUIT_WINDOW: Duration = Duration::from_secs(2);

impl App {
    pub fn new(config: Config, session_id: String) -> Self {
        let theme = Theme::named(&config.theme);
        let model = config.model.clone();
        let mode = config.permission_mode;
        Self {
            config,
            theme,
            transcript: Vec::new(),
            textarea: TextArea::new(),
            overlay: Overlay::None,
            scroll: 0,
            scroll_locked: false,
            content_height: 0,
            viewport_height: 0,
            busy: false,
            spinner_frame: 0,
            status: String::new(),
            usage: Usage::default(),
            model,
            mode,
            todos: Vec::new(),
            session_id,
            context_tokens: 0,
            should_quit: false,
            quit_armed_at: None,
        }
    }

    pub fn spinner(&self) -> &'static str {
        SPINNER[self.spinner_frame % SPINNER.len()]
    }

    pub fn notice(&mut self, text: impl Into<String>, level: NoticeLevel) {
        self.transcript.push(TranscriptItem::Notice { text: text.into(), level });
        self.follow_tail();
    }

    /// Scroll to the newest output unless the user has scrolled away.
    fn follow_tail(&mut self) {
        if !self.scroll_locked {
            self.scroll = u16::MAX;
        }
    }

    /// Append streamed text to the open assistant message, starting one if the
    /// last item is not an assistant message still in progress.
    fn append_assistant(&mut self, chunk: &str) {
        match self.transcript.last_mut() {
            Some(TranscriptItem::Assistant { text, done: false }) => text.push_str(chunk),
            _ => self
                .transcript
                .push(TranscriptItem::Assistant { text: chunk.to_string(), done: false }),
        }
        self.follow_tail();
    }

    fn append_reasoning(&mut self, chunk: &str) {
        match self.transcript.last_mut() {
            Some(TranscriptItem::Reasoning { text, .. }) => text.push_str(chunk),
            _ => self
                .transcript
                .push(TranscriptItem::Reasoning { text: chunk.to_string(), collapsed: false }),
        }
        self.follow_tail();
    }

    /// Mark any in-progress assistant message finished.
    fn close_assistant(&mut self) {
        for item in self.transcript.iter_mut().rev() {
            if let TranscriptItem::Assistant { done, .. } = item {
                *done = true;
                return;
            }
        }
    }

    /// Fold one agent event into the view.
    pub fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(chunk) => self.append_assistant(&chunk),
            AgentEvent::Reasoning(chunk) => self.append_reasoning(&chunk),
            AgentEvent::ToolStarted { id, name, summary } => {
                self.close_assistant();
                self.transcript.push(TranscriptItem::Tool {
                    id,
                    name,
                    summary,
                    state: ToolState::Running { started: Instant::now() },
                });
                self.follow_tail();
            }
            AgentEvent::ToolFinished { id, outcome_summary, display, is_error, duration, .. } => {
                if let Some(TranscriptItem::Tool { state, .. }) =
                    self.transcript.iter_mut().rev().find(
                        |i| matches!(i, TranscriptItem::Tool { id: tool_id, .. } if *tool_id == id),
                    )
                {
                    *state = ToolState::Finished {
                        summary: outcome_summary,
                        display,
                        is_error,
                        duration,
                    };
                }
                self.follow_tail();
            }
            AgentEvent::ToolDenied { id, name, reason } => {
                match self.transcript.iter_mut().rev().find(
                    |i| matches!(i, TranscriptItem::Tool { id: tool_id, .. } if *tool_id == id),
                ) {
                    Some(TranscriptItem::Tool { state, .. }) => {
                        *state = ToolState::Denied { reason };
                    }
                    _ => {
                        // The call was refused before it was ever announced.
                        self.close_assistant();
                        self.transcript.push(TranscriptItem::Tool {
                            id,
                            name: name.clone(),
                            summary: name,
                            state: ToolState::Denied { reason },
                        });
                    }
                }
                self.follow_tail();
            }
            AgentEvent::Usage(usage) => {
                self.usage.add(&usage);
                // The provider's own prompt count is ground truth for how full
                // the window is; the local estimate is only a stand-in.
                if usage.prompt_tokens > 0 {
                    self.context_tokens = usage.prompt_tokens + usage.completion_tokens;
                }
            }
            AgentEvent::Compacted { dropped, .. } => {
                self.notice(
                    format!("Compacted the conversation: {dropped} earlier messages replaced by a summary."),
                    NoticeLevel::Info,
                );
            }
            AgentEvent::SubagentProgress { agent, text } => {
                self.status = format!("{agent}: {text}");
            }
            AgentEvent::Warning(text) => self.notice(text, NoticeLevel::Warning),
            AgentEvent::TurnComplete { stop_reason } => {
                self.close_assistant();
                self.busy = false;
                self.status.clear();
                match stop_reason {
                    StopReason::Complete => {}
                    StopReason::Interrupted => self.notice("Interrupted.", NoticeLevel::Warning),
                    StopReason::ToolLimitReached { limit } => self.notice(
                        format!(
                            "Stopped after {limit} tool calls in one turn. That usually means a loop — check the last few steps, then say how to continue."
                        ),
                        NoticeLevel::Warning,
                    ),
                    StopReason::Error(e) => self.notice(e, NoticeLevel::Error),
                }
                self.follow_tail();
            }
        }
    }

    /// Scroll up, which also pins the view so streaming output stops chasing it.
    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll_locked = true;
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        let max = self.max_scroll();
        self.scroll = (self.scroll.saturating_add(lines)).min(max);
        // Reaching the bottom re-attaches to the tail.
        if self.scroll >= max {
            self.scroll_locked = false;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_locked = false;
        self.scroll = u16::MAX;
    }

    pub fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport_height)
    }

    /// Clamp the pending scroll once the rendered height is known.
    pub fn clamp_scroll(&mut self) {
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// Cycle permission mode, as Shift+Tab does.
    pub fn cycle_mode(&mut self) -> PermissionMode {
        let modes = PermissionMode::all();
        let index = modes.iter().position(|m| *m == self.mode).unwrap_or(0);
        // Skip bypassPermissions in the cycle: turning off every safety check
        // should be a deliberate `/mode` invocation, not one stray keystroke.
        let mut next = (index + 1) % modes.len();
        if modes[next] == PermissionMode::BypassPermissions {
            next = (next + 1) % modes.len();
        }
        self.mode = modes[next];
        self.mode
    }
}

/// Run the interface until the user quits.
pub async fn run(mut agent: Agent, store: Option<SessionStore>) -> Result<()> {
    let config = agent.config.clone();

    // Seed the UI from the agent, including a resumed transcript.
    let mut app = App::new(config.clone(), agent.session.id.clone());
    app.model = agent.session.model.clone();
    app.mode = agent.permissions.mode();
    replay_session(&mut app, &agent);

    if !agent.permissions.invalid.is_empty() {
        app.notice(
            format!(
                "Ignoring malformed permission rules: {}",
                agent.permissions.invalid.join(", ")
            ),
            NoticeLevel::Warning,
        );
    }
    if let Some(err) = &agent.session.persistence_error {
        app.notice(format!("This session will not be saved: {err}"), NoticeLevel::Warning);
    }

    // Grabbed before the agent moves behind the mutex, so mode changes never
    // need that lock.
    let mode_cell = agent.permissions.mode_cell();

    let (perm_tx, mut perm_rx) = mpsc::channel::<PermissionRequest>(8);
    agent = agent.with_permission_channel(perm_tx);
    agent.refresh_system_prompt(&[]);

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(512);
    // The `task` tool was built at startup with a placeholder channel; give it
    // the real one so subagent progress reaches the status bar.
    agent.rewire_subagent_events(event_tx.clone());

    let agent = Arc::new(Mutex::new(agent));

    let mut terminal = setup_terminal()?;
    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(90));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut turn: Option<tokio::task::JoinHandle<()>> = None;
    let mut cancel = CancellationToken::new();

    let result = loop {
        // A terminal can report a degenerate size — transiently while being
        // resized, and persistently under some multiplexers. Drawing into it
        // emits a stream of control sequences and nothing else, so skip the
        // frame rather than spinning. Events are still processed, so the
        // session recovers the moment a real size arrives.
        let drawable = terminal
            .size()
            .map(|size| size.width >= 4 && size.height >= 4)
            .unwrap_or(false);
        if drawable {
            terminal.draw(|frame| render::draw(frame, &mut app))?;
            app.clamp_scroll();
        }

        if app.should_quit {
            break Ok(());
        }

        tokio::select! {
            // Agent events first: under heavy streaming this keeps output
            // flowing rather than starving behind key polling.
            biased;

            Some(event) = event_rx.recv() => {
                app.apply(event);
                // Drain anything else already queued before redrawing, so a
                // fast stream does not cost one full render per token.
                while let Ok(next) = event_rx.try_recv() {
                    app.apply(next);
                }
                if !app.busy {
                    sync_from_agent(&mut app, &agent).await;
                }
            }

            Some(request) = perm_rx.recv() => {
                app.overlay = Overlay::Permission(Box::new(request));
            }

            maybe_key = keys.next() => {
                match maybe_key {
                    None => break Ok(()),
                    Some(Err(e)) => break Err(anyhow::anyhow!("terminal input failed: {e}")),
                    Some(Ok(event)) => {
                        handle_terminal_event(
                            event,
                            &mut app,
                            &agent,
                            &event_tx,
                            &mut turn,
                            &mut cancel,
                            store.as_ref(),
                            &mode_cell,
                        )
                        .await;
                    }
                }
            }

            _ = ticker.tick() => {
                if app.busy {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
            }
        }
    };

    // Stop any in-flight turn before tearing the terminal down, so a running
    // command cannot scribble over the restored screen.
    cancel.cancel();
    if let Some(handle) = turn {
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Rebuild the transcript from a resumed session's messages.
fn replay_session(app: &mut App, agent: &Agent) {
    for message in &agent.session.messages {
        let Some(content) = message.content.as_deref().filter(|c| !c.trim().is_empty()) else {
            continue;
        };
        if message.is_role(Role::User) {
            // Skip the compaction marker: it is machinery, not conversation.
            if content.starts_with("<summary-of-earlier-conversation>") {
                app.notice("Resumed after an earlier compaction.", NoticeLevel::Info);
                continue;
            }
            app.transcript.push(TranscriptItem::User(content.to_string()));
        } else if message.is_role(Role::Assistant) {
            app.transcript
                .push(TranscriptItem::Assistant { text: content.to_string(), done: true });
        }
    }

    // Seed input history from what the user typed before.
    let prompts: Vec<String> = agent
        .session
        .messages
        .iter()
        .filter(|m| m.is_role(Role::User))
        .filter_map(|m| m.content.clone())
        .filter(|c| !c.starts_with("<summary-of-earlier-conversation>"))
        .collect();
    app.textarea.load_history(prompts);

    if !app.transcript.is_empty() {
        app.notice(
            format!("Resumed session {} ({} messages).", &agent.session.id[..8], agent.session.messages.len()),
            NoticeLevel::Info,
        );
    }
    app.scroll_to_bottom();
}

/// Copy state the UI mirrors out of the agent. Only called while idle.
async fn sync_from_agent(app: &mut App, agent: &Arc<Mutex<Agent>>) {
    let Ok(guard) = agent.try_lock() else { return };
    app.model = guard.session.model.clone();
    app.mode = guard.permissions.mode();
    app.session_id = guard.session.id.clone();
    if app.context_tokens == 0 {
        app.context_tokens = guard.session.estimated_context_tokens();
    }
    if let Ok(todos) = guard.tool_context().todos.try_lock() {
        app.todos = todos.clone();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_event(
    event: Event,
    app: &mut App,
    agent: &Arc<Mutex<Agent>>,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &mut CancellationToken,
    store: Option<&SessionStore>,
    mode_cell: &crate::permissions::ModeCell,
) {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_up(3),
            MouseEventKind::ScrollDown => app.scroll_down(3),
            _ => {}
        },
        Event::Paste(text) => app.textarea.insert_str(&text),
        Event::Resize(..) => app.scroll_to_bottom(),
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(key, app, agent, event_tx, turn, cancel, store, mode_cell).await;
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    app: &mut App,
    agent: &Arc<Mutex<Agent>>,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &mut CancellationToken,
    store: Option<&SessionStore>,
    mode_cell: &crate::permissions::ModeCell,
) {
    // An open overlay takes every key: a modal that leaks keystrokes to the
    // input box behind it is how people accidentally send half a prompt.
    if !matches!(app.overlay, Overlay::None) {
        handle_overlay_key(key, app, mode_cell).await;
        return;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        // -- interrupt and quit ---------------------------------------------
        KeyCode::Esc => {
            if app.busy {
                cancel.cancel();
                app.status = "interrupting…".into();
            } else {
                app.textarea.clear();
            }
        }
        KeyCode::Char('c') if ctrl => {
            if app.busy {
                cancel.cancel();
                app.status = "interrupting…".into();
                app.quit_armed_at = None;
                return;
            }
            if !app.textarea.is_empty() {
                app.textarea.clear();
                return;
            }
            // Two presses within the window to leave, so a stray Ctrl+C in a
            // long session does not discard it.
            match app.quit_armed_at {
                Some(at) if at.elapsed() < QUIT_WINDOW => app.should_quit = true,
                _ => {
                    app.quit_armed_at = Some(Instant::now());
                    app.notice("Press Ctrl+C again to exit.", NoticeLevel::Info);
                }
            }
        }
        KeyCode::Char('d') if ctrl && app.textarea.is_empty() => app.should_quit = true,

        // -- scrolling ------------------------------------------------------
        KeyCode::PageUp => app.scroll_up(app.viewport_height.max(1)),
        KeyCode::PageDown => app.scroll_down(app.viewport_height.max(1)),
        KeyCode::Home if ctrl => app.scroll_up(u16::MAX),
        KeyCode::End if ctrl => app.scroll_to_bottom(),
        KeyCode::Char('l') if ctrl => {
            app.transcript.clear();
            app.scroll_to_bottom();
        }

        // -- mode -----------------------------------------------------------
        KeyCode::BackTab => {
            // No lock: a turn task holds the agent's mutex for its whole
            // duration, so awaiting it here would freeze the entire event loop
            // — including the Esc that would have cancelled the turn. The mode
            // lives behind an atomic precisely so this key stays live.
            let mode = app.cycle_mode();
            mode_cell.store(mode);
            app.notice(format!("Mode: {} — {}", mode.as_str(), mode.describe()), NoticeLevel::Info);
        }

        // -- submission -----------------------------------------------------
        KeyCode::Enter if shift || alt => app.textarea.insert_newline(),
        KeyCode::Enter => {
            if app.busy || app.textarea.is_empty() {
                return;
            }
            let text = app.textarea.take();
            submit(text, app, agent, event_tx, turn, cancel, store, mode_cell).await;
        }

        // -- editing --------------------------------------------------------
        KeyCode::Backspace if ctrl || alt => app.textarea.delete_word_before(),
        KeyCode::Backspace => app.textarea.backspace(),
        KeyCode::Delete => app.textarea.delete(),
        KeyCode::Char('w') if ctrl => app.textarea.delete_word_before(),
        KeyCode::Char('k') if ctrl => app.textarea.kill_to_end(),
        KeyCode::Char('u') if ctrl => app.textarea.kill_to_start(),
        KeyCode::Char('a') if ctrl => app.textarea.move_home(),
        KeyCode::Char('e') if ctrl => app.textarea.move_end(),
        KeyCode::Left if ctrl || alt => app.textarea.move_word_left(),
        KeyCode::Right if ctrl || alt => app.textarea.move_word_right(),
        KeyCode::Left => app.textarea.move_left(),
        KeyCode::Right => app.textarea.move_right(),
        KeyCode::Home => app.textarea.move_home(),
        KeyCode::End => app.textarea.move_end(),

        // Up and Down edit within a multi-line prompt, and fall through to
        // history only at the boundaries.
        KeyCode::Up => {
            if !app.textarea.move_up() {
                app.textarea.history_prev();
            }
        }
        KeyCode::Down => {
            if !app.textarea.move_down() {
                app.textarea.history_next();
            }
        }

        KeyCode::Tab => {
            // Tab completes a slash command.
            let text = app.textarea.text();
            if text.starts_with('/') && !text.contains(char::is_whitespace) {
                let entries = commands::complete(&text, &app.config.workspace);
                match entries.len() {
                    0 => {}
                    1 => app.textarea.set_text(&format!("/{} ", entries[0].name)),
                    _ => app.overlay = Overlay::Palette { entries, selected: 0 },
                }
            }
        }

        KeyCode::Char(c) => {
            app.textarea.insert_char(c);
            // Opening a slash on an empty line offers the palette immediately.
            if c == '/' && app.textarea.text() == "/" {
                let entries = commands::all(&app.config.workspace);
                app.overlay = Overlay::Palette { entries, selected: 0 };
            }
        }
        _ => {}
    }

    // Any key other than a second Ctrl+C disarms the quit prompt.
    if !matches!(key.code, KeyCode::Char('c')) {
        app.quit_armed_at = None;
    }
}

async fn handle_overlay_key(
    key: KeyEvent,
    app: &mut App,
    mode_cell: &crate::permissions::ModeCell,
) {
    match &mut app.overlay {
        Overlay::None => {}

        Overlay::Permission(_) => {
            let answer = match key.code {
                KeyCode::Char('y' | 'Y' | '1') | KeyCode::Enter => Some(PermissionResponse::Once),
                KeyCode::Char('a' | 'A' | '2') => Some(PermissionResponse::Always),
                KeyCode::Char('n' | 'N' | '3') | KeyCode::Esc => Some(PermissionResponse::Reject),
                _ => None,
            };
            if let Some(answer) = answer {
                let Overlay::Permission(request) = std::mem::replace(&mut app.overlay, Overlay::None)
                else {
                    return;
                };
                let label = match answer {
                    PermissionResponse::Once => "approved",
                    PermissionResponse::Always => "approved for this session",
                    PermissionResponse::Reject => "declined",
                };
                app.notice(format!("{} — {label}", request.summary), NoticeLevel::Info);
                // The receiver is gone only if the turn was cancelled meanwhile.
                let _ = request.respond.send(answer);
            }
        }

        Overlay::Palette { entries, selected } => match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(entries.len().saturating_sub(1)),
            KeyCode::Esc => app.overlay = Overlay::None,
            KeyCode::Enter | KeyCode::Tab => {
                let name = entries.get(*selected).map(|c| c.name.clone());
                app.overlay = Overlay::None;
                if let Some(name) = name {
                    app.textarea.set_text(&format!("/{name} "));
                }
            }
            KeyCode::Backspace => {
                app.overlay = Overlay::None;
                app.textarea.backspace();
            }
            KeyCode::Char(c) => {
                app.textarea.insert_char(c);
                let text = app.textarea.text();
                let matches = commands::complete(&text, &app.config.workspace);
                let typed = text.trim_start_matches('/');

                // Once the typed text *is* a command, the palette has nothing
                // left to offer and must get out of the way — otherwise Enter
                // is consumed as "complete the selection" and every slash
                // command needs Enter pressed twice.
                if matches.is_empty() || matches.iter().any(|m| m.name == typed) {
                    app.overlay = Overlay::None;
                } else {
                    app.overlay = Overlay::Palette { entries: matches, selected: 0 };
                }
            }
            _ => {}
        },

        Overlay::Picker { kind, entries, selected, .. } => match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(entries.len().saturating_sub(1)),
            KeyCode::Esc => app.overlay = Overlay::None,
            KeyCode::Enter => {
                let kind = *kind;
                let chosen = entries.get(*selected).cloned();
                app.overlay = Overlay::None;
                let Some(chosen) = chosen else { return };
                match kind {
                    PickerKind::Model => {
                        // Recorded on the App and pushed to the agent when the
                        // next turn starts, which is the only point the lock is
                        // already being taken. Locking here would block the
                        // whole event loop if a turn were in flight.
                        app.model = chosen.value.clone();
                        app.notice(format!("Model: {}", chosen.value), NoticeLevel::Success);
                    }
                    PickerKind::Mode => {
                        if let Some(mode) = PermissionMode::parse(&chosen.value) {
                            app.mode = mode;
                            mode_cell.store(mode);
                            app.notice(
                                format!("Mode: {} — {}", mode.as_str(), mode.describe()),
                                NoticeLevel::Success,
                            );
                        }
                    }
                    PickerKind::Session => {
                        // Switching sessions mid-process would mean rebuilding
                        // the agent's whole conversation; restarting is both
                        // simpler and less surprising.
                        app.notice(
                            format!(
                                "Run `grok --resume {}` to continue that session.",
                                &chosen.value
                            ),
                            NoticeLevel::Info,
                        );
                    }
                }
            }
            _ => {}
        },
    }
}

/// Handle a submitted line: a slash command, or a prompt for the model.
#[allow(clippy::too_many_arguments)]
async fn submit(
    text: String,
    app: &mut App,
    agent: &Arc<Mutex<Agent>>,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &mut CancellationToken,
    store: Option<&SessionStore>,
    mode_cell: &crate::permissions::ModeCell,
) {
    let action = commands::dispatch(&text, &app.config.workspace);

    // Commands needing live state are answered here, where that state exists.
    // `try_lock` rather than `lock`: a turn holds the agent's mutex for its
    // whole duration, and blocking here would freeze the event loop.
    let action = match &action {
        CommandAction::Report(name) => match agent.try_lock() {
            Ok(guard) => match name.as_str() {
                "context" => CommandAction::Show(context_report(app, &guard)),
                "cost" => CommandAction::Show(cost_report(app)),
                "tools" => CommandAction::Show(tools_report(&guard)),
                "mcp" => CommandAction::Show(mcp_report(&guard)),
                "agents" => CommandAction::Show(agents_report(&guard)),
                other => CommandAction::Show(format!("`/{other}` has no report")),
            },
            Err(_) => CommandAction::Show(format!(
                "/{name} needs the agent, which is busy with the current turn. Try again once it finishes."
            )),
        },
        _ => action,
    };

    match action {
        CommandAction::Show(body) => {
            app.transcript.push(TranscriptItem::User(text));
            app.notice(body, NoticeLevel::Info);
        }
        CommandAction::Quit => app.should_quit = true,
        CommandAction::Clear => {
            agent.lock().await.session.clear();
            app.transcript.clear();
            app.usage = Usage::default();
            app.context_tokens = 0;
            app.notice("Started a fresh conversation.", NoticeLevel::Success);
        }
        CommandAction::SetModel(model) => {
            // Applied at the start of the next turn; see PickerKind::Model.
            app.model = model.clone();
            app.notice(format!("Model: {model}"), NoticeLevel::Success);
        }
        CommandAction::SetMode(mode) => {
            mode_cell.store(mode);
            app.mode = mode;
            app.notice(format!("Mode: {} — {}", mode.as_str(), mode.describe()), NoticeLevel::Success);
        }
        CommandAction::PickModel => {
            let current = app.model.clone();
            let mut entries: Vec<PickerEntry> = MODELS
                .iter()
                .map(|m| PickerEntry {
                    label: m.id.to_string(),
                    detail: format!(
                        "{}k context{}",
                        m.context_window / 1000,
                        if m.reasoning { ", reasoning" } else { "" }
                    ),
                    value: m.id.to_string(),
                })
                .collect();
            // A model set by hand may not be in the table; keep it selectable.
            if !entries.iter().any(|e| e.value == current) {
                entries.insert(
                    0,
                    PickerEntry {
                        label: current.clone(),
                        detail: "configured".into(),
                        value: current.clone(),
                    },
                );
            }
            let selected = entries.iter().position(|e| e.value == current).unwrap_or(0);
            app.overlay =
                Overlay::Picker { kind: PickerKind::Model, title: "Model".into(), entries, selected };
        }
        CommandAction::PickMode => {
            let entries: Vec<PickerEntry> = PermissionMode::all()
                .iter()
                .map(|m| PickerEntry {
                    label: m.as_str().to_string(),
                    detail: m.describe().to_string(),
                    value: m.as_str().to_string(),
                })
                .collect();
            let selected = entries.iter().position(|e| e.value == app.mode.as_str()).unwrap_or(0);
            app.overlay = Overlay::Picker {
                kind: PickerKind::Mode,
                title: "Permission mode".into(),
                entries,
                selected,
            };
        }
        CommandAction::PickSession => {
            let sessions = store.map(SessionStore::list).unwrap_or_default();
            if sessions.is_empty() {
                app.notice("No earlier sessions in this project.", NoticeLevel::Info);
                return;
            }
            let entries: Vec<PickerEntry> = sessions
                .iter()
                .take(30)
                .map(|s| PickerEntry {
                    label: s.label(),
                    detail: format!("{} messages · {}", s.message_count, s.model),
                    value: s.id.clone(),
                })
                .collect();
            app.overlay = Overlay::Picker {
                kind: PickerKind::Session,
                title: "Sessions".into(),
                entries,
                selected: 0,
            };
        }
        CommandAction::Compact => {
            app.transcript.push(TranscriptItem::User(text));
            app.busy = true;
            app.status = "compacting…".into();
            let agent = Arc::clone(agent);
            let events = event_tx.clone();
            *turn = Some(tokio::spawn(async move {
                let mut guard = agent.lock().await;
                if let Err(e) = guard.compact(&events).await {
                    let _ = events.send(AgentEvent::Warning(format!("compaction failed: {e}"))).await;
                }
                let _ = events
                    .send(AgentEvent::TurnComplete { stop_reason: StopReason::Complete })
                    .await;
            }));
        }
        CommandAction::InitProject => {
            app.transcript.push(TranscriptItem::User(text));
            start_turn(commands::INIT_PROMPT.to_string(), app, agent, event_tx, turn, cancel).await;
        }
        CommandAction::Prompt(prompt) => {
            app.transcript.push(TranscriptItem::User(text.clone()));
            start_turn(prompt, app, agent, event_tx, turn, cancel).await;
        }
        CommandAction::Unknown(name) => {
            app.notice(
                format!("Unknown command `/{name}`. Type /help to see what is available."),
                NoticeLevel::Error,
            );
        }
        // Rewritten above into a Show once the agent could be inspected;
        // reaching here would mean that lookup was skipped.
        CommandAction::Report(name) => {
            app.notice(format!("`/{name}` could not be produced."), NoticeLevel::Error);
        }
    }
}

/// Spawn a turn.
async fn start_turn(
    prompt: String,
    app: &mut App,
    agent: &Arc<Mutex<Agent>>,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &mut CancellationToken,
) {
    app.busy = true;
    app.status.clear();
    app.scroll_to_bottom();

    // A fresh token per turn: interrupting one turn must not poison the next.
    *cancel = CancellationToken::new();
    {
        let mut guard = agent.lock().await;
        guard.reset_cancel(cancel.clone());
        // Model changes made while idle are applied here, where the lock is
        // taken anyway.
        guard.session.model.clone_from(&app.model);
        guard.refresh_system_prompt(&[]);
    }

    let agent = Arc::clone(agent);
    let events = event_tx.clone();
    *turn = Some(tokio::spawn(async move {
        let mut guard = agent.lock().await;
        guard.run_turn(&prompt, &events).await;
    }));
}

// ---------------------------------------------------------------------------
// Reports for the state-dependent slash commands
// ---------------------------------------------------------------------------

fn context_report(app: &App, agent: &Agent) -> String {
    let window = crate::config::model_info(&app.model).context_window;
    let used = if app.context_tokens > 0 {
        app.context_tokens
    } else {
        agent.session.estimated_context_tokens()
    };
    let pct = (used as f64 / window as f64 * 100.0).min(999.0);

    let mut by_role = std::collections::BTreeMap::<&str, (usize, u64)>::new();
    for message in &agent.session.messages {
        let entry = by_role.entry(message.role.as_str()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += (message.char_len() as u64).div_ceil(4);
    }

    let mut out = format!(
        "Context: {used} of {window} tokens ({pct:.1}%)\nCompaction triggers at {} tokens\n\nBy role:\n",
        agent.config.compact_at()
    );
    for (role, (count, tokens)) in by_role {
        out.push_str(&format!("  {role:<10} {count:>4} messages  ~{tokens} tokens\n"));
    }
    out
}

fn cost_report(app: &App) -> String {
    let cached = app.usage.cached_tokens();
    format!(
        "Session usage\n\n  input      {:>9}\n  cached     {:>9}\n  output     {:>9}\n  total      {:>9}\n\nModel: {}",
        app.usage.prompt_tokens, cached, app.usage.completion_tokens, app.usage.total_tokens, app.model
    )
}

fn tools_report(agent: &Agent) -> String {
    let mut out = format!("{} tools available\n\n", agent.tools.len());
    for tool in agent.tools.iter() {
        out.push_str(&format!("  {:<24} {:?}\n", tool.name(), tool.kind()));
    }
    if agent.permissions.mode() == PermissionMode::Plan {
        out.push_str("\nPlan mode is on, so mutating tools are hidden from the model.\n");
    }
    out
}

fn mcp_report(agent: &Agent) -> String {
    let mcp_tools: Vec<&str> =
        agent.tools.iter().map(|t| t.name()).filter(|n| n.starts_with("mcp__")).collect();
    if agent.config.mcp_servers.is_empty() {
        return "No MCP servers configured. Add them under [mcp_servers] in .grok/config.toml."
            .to_string();
    }
    let mut out = String::from("MCP servers\n\n");
    for (name, config) in &agent.config.mcp_servers {
        let count = mcp_tools.iter().filter(|t| t.starts_with(&format!("mcp__{name}__"))).count();
        let state = if config.disabled {
            "disabled".to_string()
        } else if count > 0 {
            format!("{count} tools")
        } else {
            "not connected".to_string()
        };
        out.push_str(&format!("  {name:<20} {state}\n"));
    }
    out
}

fn agents_report(agent: &Agent) -> String {
    if agent.subagents.is_empty() {
        return "No subagents defined. Add markdown files under .grok/agents/.".to_string();
    }
    let mut out = String::from("Subagents\n\n");
    for definition in &agent.subagents {
        out.push_str(&format!("  {:<20} {}\n", definition.name, definition.description));
    }
    out
}

/// Human-readable token count for the status bar: `12.4k`.
pub fn format_tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return format!("{:.1}k", n as f64 / 1000.0);
    }
    format!("{:.1}M", n as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests_support::config_at;

    fn app() -> App {
        App::new(config_at(std::env::temp_dir()), "test-session".into())
    }

    #[test]
    fn streamed_text_accumulates_into_one_assistant_message() {
        let mut a = app();
        a.apply(AgentEvent::Text("Hel".into()));
        a.apply(AgentEvent::Text("lo".into()));

        assert_eq!(a.transcript.len(), 1, "chunks must not each become an entry");
        let TranscriptItem::Assistant { text, done } = &a.transcript[0] else { panic!() };
        assert_eq!(text, "Hello");
        assert!(!done, "still streaming");
    }

    #[test]
    fn a_tool_call_closes_the_open_assistant_message() {
        let mut a = app();
        a.apply(AgentEvent::Text("Let me look.".into()));
        a.apply(AgentEvent::ToolStarted {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "Read(a.rs)".into(),
        });

        let TranscriptItem::Assistant { done, .. } = &a.transcript[0] else { panic!() };
        assert!(done, "text after a tool call is a new message, not a continuation");
        assert!(matches!(a.transcript[1], TranscriptItem::Tool { .. }));
    }

    #[test]
    fn a_finished_tool_updates_the_entry_it_started() {
        let mut a = app();
        a.apply(AgentEvent::ToolStarted {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "Read(a.rs)".into(),
        });
        a.apply(AgentEvent::ToolFinished {
            id: "t1".into(),
            name: "read_file".into(),
            outcome_summary: Some("142 lines".into()),
            display: Box::default(),
            is_error: false,
            duration: Duration::from_millis(12),
        });

        assert_eq!(a.transcript.len(), 1, "the finish updates in place");
        let TranscriptItem::Tool { state: ToolState::Finished { summary, .. }, .. } = &a.transcript[0]
        else {
            panic!("expected a finished tool");
        };
        assert_eq!(summary.as_deref(), Some("142 lines"));
    }

    #[test]
    fn a_denial_before_the_tool_was_announced_still_appears() {
        let mut a = app();
        a.apply(AgentEvent::ToolDenied {
            id: "t9".into(),
            name: "bash".into(),
            reason: "blocked".into(),
        });
        assert!(matches!(
            &a.transcript[0],
            TranscriptItem::Tool { state: ToolState::Denied { .. }, .. }
        ));
    }

    #[test]
    fn scrolling_up_pins_the_view_and_reaching_the_bottom_releases_it() {
        let mut a = app();
        a.content_height = 100;
        a.viewport_height = 10;
        a.scroll = 90;

        a.scroll_up(5);
        assert!(a.scroll_locked, "the view must stop chasing new output");

        a.scroll_down(u16::MAX);
        assert!(!a.scroll_locked, "returning to the bottom re-attaches");
    }

    #[test]
    fn new_output_does_not_move_a_pinned_view() {
        let mut a = app();
        a.content_height = 100;
        a.viewport_height = 10;
        a.scroll_up(20);
        let pinned = a.scroll;

        a.apply(AgentEvent::Text("more output".into()));
        assert_eq!(a.scroll, pinned, "reading scrollback must not be interrupted");
    }

    #[test]
    fn mode_cycling_skips_bypass_permissions() {
        let mut a = app();
        a.mode = PermissionMode::Default;

        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(a.cycle_mode());
        }
        assert!(
            !seen.contains(&PermissionMode::BypassPermissions),
            "one stray keystroke must not disable every safety check: {seen:?}"
        );
        assert!(seen.contains(&PermissionMode::Plan));
        assert!(seen.contains(&PermissionMode::AcceptEdits));
    }

    #[test]
    fn a_turn_ending_marks_the_message_done_and_clears_busy() {
        let mut a = app();
        a.busy = true;
        a.apply(AgentEvent::Text("answer".into()));
        a.apply(AgentEvent::TurnComplete { stop_reason: StopReason::Complete });

        assert!(!a.busy);
        let TranscriptItem::Assistant { done, .. } = &a.transcript[0] else { panic!() };
        assert!(done);
    }

    #[test]
    fn hitting_the_tool_limit_explains_itself_rather_than_stopping_silently() {
        let mut a = app();
        a.apply(AgentEvent::TurnComplete { stop_reason: StopReason::ToolLimitReached { limit: 60 } });

        let TranscriptItem::Notice { text, level } = a.transcript.last().unwrap() else {
            panic!("expected a notice")
        };
        assert_eq!(*level, NoticeLevel::Warning);
        assert!(text.contains("60 tool calls"), "got: {text}");
        assert!(text.contains("loop"), "the likely cause is named: {text}");
    }

    #[test]
    fn usage_accumulates_across_round_trips() {
        let mut a = app();
        a.apply(AgentEvent::Usage(Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_tokens_details: None,
        }));
        a.apply(AgentEvent::Usage(Usage {
            prompt_tokens: 300,
            completion_tokens: 40,
            total_tokens: 340,
            prompt_tokens_details: None,
        }));

        assert_eq!(a.usage.total_tokens, 460);
        assert_eq!(a.context_tokens, 340, "context tracks the latest prompt, not the sum");
    }

    #[test]
    fn token_counts_format_compactly() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_400), "12.4k");
        assert_eq!(format_tokens(2_000_000), "2.0M");
    }
}
