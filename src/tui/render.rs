//! Drawing. Pure functions of [`App`] — no I/O, no mutation except the
//! measured heights the scroll logic needs.
//!
//! The transcript is rendered to a `Vec<Line>` in full and then scrolled, which
//! costs a rebuild per frame but keeps scrolling exact: line-count arithmetic
//! over wrapped, styled content is the classic source of "the last line is
//! always cut off" bugs.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::config::PermissionMode;
use crate::tools::TodoStatus;
use crate::util::{self, DiffKind};

use super::markdown::{self, Theme};
use super::{App, NoticeLevel, Overlay, ToolState, TranscriptItem, format_tokens};

/// Diff lines shown under a tool result before it is elided.
const MAX_DIFF_LINES: usize = 14;

/// Marker before a tool's detail line.
///
/// Deliberately a box-drawing character rather than something prettier like
/// `⏵`: that codepoint is absent from Menlo, the default macOS terminal font,
/// so it rendered as a missing-glyph box on every tool line. Box-drawing is
/// present in every monospace font that can draw the borders around it.
const DETAIL_PREFIX: char = '└';

pub fn draw(frame: &mut Frame, app: &mut App) {
    let input_height = (app.textarea.visible_height() as u16 + 2).min(12);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                 // transcript
            Constraint::Length(input_height),   // input
            Constraint::Length(2),              // status bar
        ])
        .split(frame.area());

    draw_transcript(frame, app, chunks[0]);
    draw_input(frame, app, chunks[1]);
    draw_status(frame, app, chunks[2]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::Permission(request) => draw_permission(frame, app, request),
        Overlay::Palette { entries, selected } => draw_palette(frame, app, entries, *selected),
        Overlay::Picker { title, entries, selected, .. } => {
            draw_picker(frame, app, title, entries, *selected);
        }
    }
}

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_width = area.width.saturating_sub(2).max(1);
    let lines = build_transcript(app, inner_width);

    // Measure wrapped height so scrolling lands on real content.
    let height: usize =
        lines.iter().map(|line| wrapped_height(line, inner_width as usize)).sum();
    app.content_height = height.min(u16::MAX as usize) as u16;
    app.viewport_height = area.height.saturating_sub(2);

    let max_scroll = app.max_scroll();
    // `u16::MAX` is the sentinel for "pin to the bottom".
    let scroll = if app.scroll == u16::MAX || !app.scroll_locked { max_scroll } else { app.scroll.min(max_scroll) };
    app.scroll = scroll;

    let title = if app.busy {
        format!(" {} thinking… ", app.spinner())
    } else {
        " conversation ".to_string()
    };

    let more_below = scroll < max_scroll;
    let block = Block::default()
        .title(title)
        .title_bottom(if more_below {
            Line::from(Span::styled(" ↓ more ", Style::default().fg(app.theme.warning)))
                .alignment(Alignment::Right)
        } else {
            Line::default()
        })
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(app.theme.dim));

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)),
        area,
    );
}

/// How many terminal rows a line occupies once wrapped.
fn wrapped_height(line: &Line<'_>, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    if text.is_empty() {
        return 1;
    }
    // Approximate the widget's word wrapping. Exactness is not required —
    // being off by a row only shifts the scroll bound slightly — but ignoring
    // wrapping entirely makes long answers unscrollable to the end.
    text.split('\n')
        .map(|segment| {
            let w = unicode_width::UnicodeWidthStr::width(segment);
            if w == 0 { 1 } else { w.div_ceil(width) }
        })
        .sum()
}

fn build_transcript(app: &App, width: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.transcript.is_empty() {
        lines.extend(welcome(app));
        return lines;
    }

    for item in &app.transcript {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        match item {
            TranscriptItem::User(text) => {
                for (i, segment) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(theme.user).add_modifier(Modifier::BOLD)),
                        Span::styled(segment.to_string(), Style::default().fg(theme.user)),
                    ]));
                }
            }

            TranscriptItem::Assistant { text, done } => {
                let mut rendered = markdown::render(text, theme);
                if !done {
                    // A block caret marks the live edge of the stream.
                    match rendered.last_mut() {
                        Some(last) => last
                            .spans
                            .push(Span::styled("▌", Style::default().fg(theme.accent))),
                        None => rendered
                            .push(Line::from(Span::styled("▌", Style::default().fg(theme.accent)))),
                    }
                }
                lines.extend(rendered);
            }

            TranscriptItem::Reasoning { text, collapsed } => {
                let style = Style::default().fg(theme.dim).add_modifier(Modifier::ITALIC);
                lines.push(Line::from(Span::styled("thinking".to_string(), style)));
                if !collapsed {
                    for segment in text.lines().take(40) {
                        lines.push(Line::from(Span::styled(format!("  {segment}"), style)));
                    }
                }
            }

            TranscriptItem::Tool { summary, state, .. } => {
                lines.extend(tool_lines(theme, summary, state, width));
            }

            TranscriptItem::Notice { text, level } => {
                let (glyph, colour) = match level {
                    NoticeLevel::Info => ("·", theme.dim),
                    NoticeLevel::Success => ("✓", theme.success),
                    NoticeLevel::Warning => ("!", theme.warning),
                    NoticeLevel::Error => ("✗", theme.error),
                };
                for (i, segment) in text.lines().enumerate() {
                    let prefix = if i == 0 { format!("{glyph} ") } else { "  ".to_string() };
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(colour)),
                        Span::styled(segment.to_string(), Style::default().fg(colour)),
                    ]));
                }
            }
        }
    }

    lines
}

fn tool_lines(
    theme: &Theme,
    summary: &str,
    state: &ToolState,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let (glyph, colour) = match state {
        ToolState::Running { .. } => ("●", theme.warning),
        ToolState::Finished { is_error: false, .. } => ("●", theme.tool),
        ToolState::Finished { is_error: true, .. } => ("●", theme.error),
        ToolState::Denied { .. } => ("○", theme.dim),
    };

    let header = util::truncate_text(summary, width.saturating_sub(4) as usize * 2);
    lines.push(Line::from(vec![
        Span::styled(format!("{glyph} "), Style::default().fg(colour)),
        Span::styled(header.replace('\n', " "), Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
    ]));

    let detail_style = Style::default().fg(theme.dim);
    match state {
        ToolState::Running { started } => {
            lines.push(Line::from(Span::styled(
                format!("  {DETAIL_PREFIX} running… {}", util::format_duration(started.elapsed())),
                detail_style,
            )));
        }
        ToolState::Denied { reason } => {
            lines.push(Line::from(Span::styled(format!("  {DETAIL_PREFIX} {reason}"), detail_style)));
        }
        ToolState::Finished { summary, display, is_error, duration } => {
            let mut detail = summary.clone().unwrap_or_else(|| {
                if *is_error { "failed".to_string() } else { "done".to_string() }
            });
            // Only mention timing when it is long enough for the user to have
            // noticed it; "12ms" is noise on every line.
            if duration.as_millis() > 500 {
                detail.push_str(&format!(" · {}", util::format_duration(*duration)));
            }
            let style = if *is_error { Style::default().fg(theme.error) } else { detail_style };
            lines.push(Line::from(Span::styled(format!("  {DETAIL_PREFIX} {detail}"), style)));

            for diff_line in display.diff.iter().take(MAX_DIFF_LINES) {
                let (marker, colour) = match diff_line.kind {
                    DiffKind::Added => ("+", theme.added),
                    DiffKind::Removed => ("-", theme.removed),
                    DiffKind::Context => (" ", theme.dim),
                };
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(marker.to_string(), Style::default().fg(colour)),
                    Span::styled(diff_line.text.clone(), Style::default().fg(colour)),
                ]));
            }
            if display.diff.len() > MAX_DIFF_LINES {
                lines.push(Line::from(Span::styled(
                    format!("    … {} more lines", display.diff.len() - MAX_DIFF_LINES),
                    detail_style,
                )));
            }
        }
    }

    lines
}

fn welcome(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let accent = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let value = Style::default().fg(theme.text);
    let label_w = 12;

    // Key/value row, with an optional right-hand hint naming the command that
    // changes it — the thing a new user most wants to know.
    let row = |label: &str, val: Vec<Span<'static>>, hint: &str| {
        let mut spans = vec![
            Span::styled("   ", dim),
            Span::styled(format!("{label:<label_w$}"), dim),
        ];
        spans.extend(val);
        if !hint.is_empty() {
            spans.push(Span::styled(format!("  {hint}"), dim));
        }
        Line::from(spans)
    };

    let window = crate::config::model_info(&app.model).context_window;
    let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
    let cwd = {
        let full = app.config.workspace.display().to_string();
        match (!home.is_empty()).then(|| full.strip_prefix(&home)).flatten() {
            Some(rest) => format!("~{rest}"),
            None => full,
        }
    };

    let mut tools = vec![Span::styled(format!("{} built-in", app.tool_count), value)];
    if app.mcp_count > 0 {
        tools.push(Span::styled(format!(" · {} from MCP", app.mcp_count), value));
    }
    if app.subagent_count > 0 {
        tools.push(Span::styled(
            format!(" · {} subagent{}", app.subagent_count, if app.subagent_count == 1 { "" } else { "s" }),
            value,
        ));
    }

    let mut location = vec![Span::styled(cwd, value)];
    if let Some(branch) = &app.branch {
        location.push(Span::styled("  ▸ ", Style::default().fg(theme.accent)));
        location.push(Span::styled(branch.clone(), Style::default().fg(theme.accent)));
    }

    vec![
        Line::default(),
        Line::from(vec![
            Span::styled("  ▄▀▄  ", accent),
            Span::styled(format!("{} ", crate::APP_NAME), accent),
            Span::styled(format!("v{}", crate::VERSION), dim),
        ]),
        Line::from(vec![
            Span::styled("  ▀▄▀  ", accent),
            Span::styled("an agentic coding harness for xAI's Grok models", dim),
        ]),
        Line::default(),
        row("model", vec![
            Span::styled(app.model.clone(), value),
            Span::styled(format!("  {} context", format_tokens(window)), dim),
        ], "/model"),
        row("directory", location, ""),
        row("permissions", vec![
            Span::styled(app.mode.as_str().to_string(), Style::default().fg(mode_colour(theme, app.mode))),
            Span::styled(format!(" — {}", app.mode.describe()), dim),
        ], "shift+tab"),
        row("tools", tools, "/tools"),
        Line::default(),
        Line::from(vec![
            Span::styled("   Ask for a change and it will make it. ", dim),
            Span::styled("/help", Style::default().fg(theme.accent)),
            Span::styled(" for commands · ", dim),
            Span::styled("Ctrl+C", Style::default().fg(theme.accent)),
            Span::styled(" twice to exit", dim),
        ]),
    ]
}

/// Colour for a permission mode, shared by the banner, input border and status.
fn mode_colour(theme: &Theme, mode: PermissionMode) -> ratatui::style::Color {
    match mode {
        PermissionMode::Plan => theme.accent,
        PermissionMode::BypassPermissions => theme.error,
        PermissionMode::AcceptEdits => theme.success,
        PermissionMode::Default => theme.success,
    }
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let (border, hint) = if app.busy {
        (theme.dim, " Esc to interrupt ")
    } else {
        match app.mode {
            PermissionMode::Plan => (theme.accent, " plan mode — read only "),
            PermissionMode::BypassPermissions => (theme.error, " permissions bypassed "),
            PermissionMode::AcceptEdits => (theme.success, " edits auto-accepted "),
            PermissionMode::Default => (theme.user, ""),
        }
    };

    let offset = app.textarea.scroll_offset();
    let visible: Vec<Line> = app
        .textarea
        .lines()
        .iter()
        .skip(offset)
        .take(super::input::MAX_VISIBLE_LINES)
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.text))))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title_bottom(
            Line::from(Span::styled(hint, Style::default().fg(border))).alignment(Alignment::Right),
        );

    frame.render_widget(Paragraph::new(visible).block(block), area);

    // Place the real terminal caret, so the cursor blinks where typing lands.
    if !app.busy && matches!(app.overlay, Overlay::None) {
        let (row, _) = app.textarea.cursor();
        let x = area.x + 1 + app.textarea.cursor_display_col() as u16;
        let y = area.y + 1 + (row.saturating_sub(offset)) as u16;
        if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
            frame.set_cursor_position((x, y));
        }
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim);
    let width = area.width as usize;

    // -- row one: what the agent is, and where -----------------------------
    let mode_style = Style::default().fg(mode_colour(theme, app.mode)).add_modifier(Modifier::BOLD);
    let mut left = vec![
        Span::styled(" ◆ ", mode_style),
        Span::styled(app.mode.as_str().to_string(), mode_style),
        Span::styled("  ", dim),
        Span::styled(short_path(&app.config.workspace), Style::default().fg(theme.text)),
    ];
    if let Some(branch) = &app.branch {
        left.push(Span::styled("  ▸ ", dim));
        left.push(Span::styled(branch.clone(), Style::default().fg(theme.accent)));
    }

    // Right side: the context meter, which is the number people actually watch.
    let window = crate::config::model_info(&app.model).context_window;
    let pct = if window == 0 { 0.0 } else { app.context_tokens as f64 / window as f64 * 100.0 };
    let meter_colour = if pct > 90.0 {
        theme.error
    } else if pct > 75.0 {
        theme.warning
    } else {
        theme.success
    };
    let right = vec![
        Span::styled(meter(pct), Style::default().fg(meter_colour)),
        Span::styled(
            format!(" {} / {} ", format_tokens(app.context_tokens), format_tokens(window)),
            Style::default().fg(theme.text),
        ),
        Span::styled(format!("({pct:.0}%) "), dim),
    ];
    frame.render_widget(
        Paragraph::new(justify(left, right, width)),
        Rect { height: 1, ..area },
    );

    // -- row two: capabilities and cost ------------------------------------
    let mut left2 = vec![
        Span::styled(" ", dim),
        Span::styled(app.model.clone(), Style::default().fg(theme.tool)),
        Span::styled(format!("  ·  {} tools", app.tool_count), dim),
    ];
    if app.mcp_count > 0 {
        left2.push(Span::styled(format!("  ·  {} MCP", app.mcp_count), dim));
    }
    if app.subagent_count > 0 {
        left2.push(Span::styled(
            format!(
                "  ·  {} agent{}",
                app.subagent_count,
                if app.subagent_count == 1 { "" } else { "s" }
            ),
            dim,
        ));
    }

    let mut right2 = Vec::new();
    let pending = app.todos.iter().filter(|t| t.status != TodoStatus::Completed).count();
    if pending > 0 {
        right2.push(Span::styled(
            format!("☐ {pending} left  ·  "),
            Style::default().fg(theme.accent),
        ));
    }
    if app.usage.total_tokens > 0 {
        right2.push(Span::styled(format!("{} used  ·  ", format_tokens(app.usage.total_tokens)), dim));
    }
    if app.busy {
        right2.push(Span::styled(
            format!("{} working ", app.spinner()),
            Style::default().fg(theme.warning),
        ));
    } else if !app.status.is_empty() {
        right2.push(Span::styled(format!("{} ", app.status), Style::default().fg(theme.warning)));
    } else {
        right2.push(Span::styled("? /help ", dim));
    }

    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(justify(left2, right2, width)),
            Rect { y: area.y + 1, height: 1, ..area },
        );
    }
}

/// An eight-cell bar for context usage.
fn meter(pct: f64) -> String {
    const CELLS: usize = 8;
    let filled = ((pct / 100.0) * CELLS as f64).round().clamp(0.0, CELLS as f64) as usize;
    // Show one filled cell as soon as anything is used, so the meter never
    // reads as empty during a conversation that has clearly started.
    let filled = if pct > 0.0 { filled.max(1) } else { 0 };
    format!("{}{}", "▮".repeat(filled), "▯".repeat(CELLS - filled))
}

/// Push `right` against the right edge, padding between the two groups.
fn justify(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = left
        .iter()
        .chain(right.iter())
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans.extend(right);
    Line::from(spans)
}

/// `~`-abbreviated workspace path.
fn short_path(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    match dirs::home_dir() {
        Some(home) => match full.strip_prefix(&home.display().to_string()) {
            Some(rest) => format!("~{rest}"),
            None => full,
        },
        None => full,
    }
}

/// A centred box `pct_x` by `pct_y` percent of the screen.
fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn draw_permission(frame: &mut Frame, app: &App, request: &crate::agent::PermissionRequest) {
    let theme = &app.theme;
    let area = centered(76, 60, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            request.summary.clone(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];

    if let Some(preview) = &request.preview {
        // The user should be approving a change, not a filename.
        let body_rows = area.height.saturating_sub(8) as usize;
        for line in preview.lines().take(body_rows) {
            let colour = match line.chars().next() {
                Some('+') => theme.added,
                Some('-') => theme.removed,
                _ => theme.dim,
            };
            lines.push(Line::from(Span::styled(line.to_string(), Style::default().fg(colour))));
        }
        lines.push(Line::default());
    } else if !request.argument.is_empty() {
        for line in request.argument.lines().take(8) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.code),
            )));
        }
        lines.push(Line::default());
    }

    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
        Span::styled(" allow once   ", Style::default().fg(theme.dim)),
        Span::styled("a", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
        Span::styled(" allow for this session   ", Style::default().fg(theme.dim)),
        Span::styled("n", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
        Span::styled(" decline", Style::default().fg(theme.dim)),
    ]));

    let block = Block::default()
        .title(" permission required ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warning));

    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_palette(frame: &mut Frame, app: &App, entries: &[crate::commands::SlashCommand], selected: usize) {
    let theme = &app.theme;
    let height = (entries.len() as u16 + 2).min(14);
    let full = frame.area();
    // Sit just above the input box rather than centred: it is a completion
    // list, and it should read as attached to what is being typed.
    let area = Rect {
        x: full.x + 2,
        y: full.height.saturating_sub(height + 4).max(1),
        width: full.width.saturating_sub(4).min(76),
        height,
    };
    frame.render_widget(Clear, area);

    let visible_from = selected.saturating_sub(height.saturating_sub(3) as usize);
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(visible_from)
        .map(|(i, command)| {
            let style = if i == selected {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            Line::from(vec![
                Span::styled(if i == selected { "▸ " } else { "  " }, style),
                Span::styled(format!("{:<24}", command.display()), style),
                Span::styled(command.description.clone(), Style::default().fg(theme.dim)),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_picker(
    frame: &mut Frame,
    app: &App,
    title: &str,
    entries: &[super::PickerEntry],
    selected: usize,
) {
    let theme = &app.theme;
    let area = centered(66, 60, frame.area());
    frame.render_widget(Clear, area);

    let rows = area.height.saturating_sub(2) as usize;
    let from = selected.saturating_sub(rows.saturating_sub(1));
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(from)
        .take(rows)
        .map(|(i, entry)| {
            let style = if i == selected {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            Line::from(vec![
                Span::styled(if i == selected { "▸ " } else { "  " }, style),
                Span::styled(format!("{:<34}", entry.label), style),
                Span::styled(entry.detail.clone(), Style::default().fg(theme.dim)),
            ])
        })
        .collect();

    let block = Block::default()
        .title(format!(" {title} "))
        .title_bottom(
            Line::from(Span::styled(" ↑↓ select · Enter confirm · Esc cancel ", Style::default().fg(theme.dim)))
                .alignment(Alignment::Center),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests_support::config_at;
    use crate::tui::{App, NoticeLevel, TranscriptItem};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new(config_at(std::env::temp_dir()), "test".into())
    }

    /// Render into an in-memory terminal and return what would be on screen.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_empty_session_shows_the_welcome_banner() {
        let mut a = app();
        let out = screen(&mut a, 80, 24);
        assert!(out.contains("grok-cli"), "got:\n{out}");
        assert!(out.contains("/help"), "the user is told how to start: \n{out}");
    }

    #[test]
    fn a_conversation_renders_both_turns() {
        // The prototype's headline bug: this drew an empty box.
        let mut a = app();
        a.transcript.push(TranscriptItem::User("fix the bug".into()));
        a.transcript.push(TranscriptItem::Assistant { text: "Fixed it.".into(), done: true });

        let out = screen(&mut a, 80, 24);
        assert!(out.contains("fix the bug"), "the user's message must appear:\n{out}");
        assert!(out.contains("Fixed it."), "the reply must appear:\n{out}");
    }

    #[test]
    fn markdown_in_a_reply_is_rendered_not_printed_raw() {
        let mut a = app();
        a.transcript.push(TranscriptItem::Assistant {
            text: "Use **bold** here.".into(),
            done: true,
        });
        let out = screen(&mut a, 80, 12);
        assert!(out.contains("Use bold here."), "got:\n{out}");
        assert!(!out.contains("**"), "raw asterisks would look broken:\n{out}");
    }

    #[test]
    fn a_running_tool_shows_as_running_and_a_finished_one_shows_its_summary() {
        let mut a = app();
        a.transcript.push(TranscriptItem::Tool {
            id: "t1".into(),
            name: "read_file".into(),
            summary: "Read(src/main.rs)".into(),
            state: ToolState::Running { started: std::time::Instant::now() },
        });
        assert!(screen(&mut a, 80, 12).contains("running"));

        let TranscriptItem::Tool { state, .. } = &mut a.transcript[0] else { panic!() };
        *state = ToolState::Finished {
            summary: Some("142 lines".into()),
            display: Box::default(),
            is_error: false,
            duration: std::time::Duration::from_millis(10),
        };
        let out = screen(&mut a, 80, 12);
        assert!(out.contains("Read(src/main.rs)"), "got:\n{out}");
        assert!(out.contains("142 lines"), "got:\n{out}");
    }

    #[test]
    fn an_edit_renders_its_diff() {
        let mut a = app();
        a.transcript.push(TranscriptItem::Tool {
            id: "t1".into(),
            name: "edit_file".into(),
            summary: "Edit(a.rs)".into(),
            state: ToolState::Finished {
                summary: Some("+1 -1".into()),
                display: Box::new(crate::tools::ToolDisplay {
                    diff: util::diff_lines("old line\n", "new line\n", 0),
                    ..Default::default()
                }),
                is_error: false,
                duration: std::time::Duration::from_millis(5),
            },
        });

        let out = screen(&mut a, 80, 20);
        assert!(out.contains("-old line"), "removals are shown:\n{out}");
        assert!(out.contains("+new line"), "additions are shown:\n{out}");
    }

    #[test]
    fn counts_in_the_status_bar_are_pluralised_correctly() {
        let mut a = app();
        a.tool_count = 12;
        a.subagent_count = 1;
        let out = screen(&mut a, 118, 14);
        assert!(out.contains("1 agent"), "got:\n{out}");
        assert!(!out.contains("1 agents"), "singular must not read as plural:\n{out}");

        a.subagent_count = 3;
        let out = screen(&mut a, 118, 14);
        assert!(out.contains("3 agents"), "got:\n{out}");
    }

    #[test]
    fn the_context_window_is_shown_in_human_units() {
        let mut a = app();
        let out = screen(&mut a, 118, 14);
        assert!(out.contains("2.0M"), "a 2M window must not render as 2000k:\n{out}");
    }

    #[test]
    fn the_banner_shows_what_a_new_user_needs() {
        let mut a = app();
        a.branch = Some("main".into());
        a.tool_count = 12;
        a.subagent_count = 2;
        a.mcp_count = 3;
        let out = screen(&mut a, 118, 20);

        for expected in ["grok-cli", "model", "directory", "permissions", "tools", "main",
                         "3 from MCP", "2 subagents", "/help"] {
            assert!(out.contains(expected), "banner omits {expected:?}:\n{out}");
        }
    }

    #[test]
    fn the_context_meter_fills_as_the_window_fills() {
        assert_eq!(meter(0.0), "▯▯▯▯▯▯▯▯");
        assert_eq!(meter(100.0), "▮▮▮▮▮▮▮▮");
        assert_eq!(meter(50.0), "▮▮▮▮▯▯▯▯");
        // Any usage at all must show, or a busy session reads as empty.
        assert_eq!(meter(0.1), "▮▯▯▯▯▯▯▯");
    }

    #[test]
    fn the_status_bar_shows_mode_model_and_context() {
        let mut a = app();
        a.mode = PermissionMode::Plan;
        a.context_tokens = 12_400;

        let out = screen(&mut a, 100, 12);
        assert!(out.contains("plan"), "got:\n{out}");
        assert!(out.contains(&a.model), "got:\n{out}");
        assert!(out.contains("12.4k"), "got:\n{out}");
    }

    #[test]
    fn bypass_mode_is_visually_loud() {
        let mut a = app();
        a.mode = PermissionMode::BypassPermissions;
        let out = screen(&mut a, 100, 12);
        assert!(
            out.contains("permissions bypassed"),
            "running without safety checks must be impossible to miss:\n{out}"
        );
    }

    #[test]
    fn a_permission_prompt_covers_the_transcript_and_lists_the_choices() {
        let mut a = app();
        a.transcript.push(TranscriptItem::User("hidden behind the modal".into()));
        let (tx, _rx) = tokio::sync::oneshot::channel();
        a.overlay = Overlay::Permission(Box::new(crate::agent::PermissionRequest {
            tool_name: "bash".into(),
            summary: "Bash(rm -rf build)".into(),
            argument: "rm -rf build".into(),
            preview: None,
            respond: tx,
        }));

        let out = screen(&mut a, 80, 20);
        assert!(out.contains("permission required"), "got:\n{out}");
        assert!(out.contains("Bash(rm -rf build)"), "got:\n{out}");
        assert!(out.contains("allow once"), "got:\n{out}");
        assert!(out.contains("decline"), "got:\n{out}");
    }

    #[test]
    fn a_permission_prompt_for_an_edit_shows_the_diff_not_just_the_path() {
        let mut a = app();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        a.overlay = Overlay::Permission(Box::new(crate::agent::PermissionRequest {
            tool_name: "edit_file".into(),
            summary: "Edit(src/auth.rs)".into(),
            argument: "src/auth.rs".into(),
            preview: Some("-if now < expiry\n+if now <= expiry".into()),
            respond: tx,
        }));

        let out = screen(&mut a, 80, 20);
        assert!(out.contains("+if now <= expiry"), "approve the change, not the filename:\n{out}");
    }

    #[test]
    fn the_command_palette_lists_matching_commands() {
        let mut a = app();
        a.overlay = Overlay::Palette {
            entries: crate::commands::complete("/co", &a.config.workspace),
            selected: 0,
        };
        let out = screen(&mut a, 90, 24);
        assert!(out.contains("/compact"), "got:\n{out}");
        assert!(out.contains("/cost"), "got:\n{out}");
    }

    #[test]
    fn every_glyph_the_ui_draws_exists_in_a_default_terminal_font() {
        // Menlo is the default macOS terminal font. `⏵` and the braille
        // spinner were absent from it, so every tool line and every "thinking"
        // frame rendered as a missing-glyph box. Codepoints added here must be
        // checked against a real font before use.
        //
        // Menlo's repertoire covers Latin-1, General Punctuation, Arrows,
        // Box Drawing, Block Elements, Geometric Shapes and Dingbats — but not
        // Miscellaneous Technical (U+2300–23FF) or Braille (U+2800–28FF).
        let forbidden = |c: char| {
            let u = c as u32;
            (0x2300..=0x23FF).contains(&u) || (0x2800..=0x28FF).contains(&u)
        };

        assert!(!forbidden(DETAIL_PREFIX), "tool detail prefix is not renderable");
        for frame in crate::tui::SPINNER {
            for c in frame.chars() {
                assert!(!forbidden(c), "spinner frame {frame:?} is not renderable");
            }
        }

        // And the same for whatever the renderer actually emits for a full
        // session, which catches a glyph added anywhere in this file.
        let mut a = app();
        a.transcript.push(TranscriptItem::Tool {
            id: "t".into(),
            name: "bash".into(),
            summary: "Bash(cargo test)".into(),
            state: ToolState::Finished {
                summary: Some("exit 0".into()),
                display: Box::default(),
                is_error: false,
                duration: std::time::Duration::from_millis(10),
            },
        });
        for c in screen(&mut a, 90, 20).chars() {
            assert!(!forbidden(c), "renderer emitted unrenderable glyph {c:?} (U+{:04X})", c as u32);
        }
    }

    #[test]
    fn rendering_is_stable_at_a_tiny_terminal_size() {
        // Panicking on a small window is a real crash people hit.
        let mut a = app();
        a.transcript.push(TranscriptItem::Assistant { text: "hello".into(), done: true });
        let _ = screen(&mut a, 20, 6);
        let _ = screen(&mut a, 4, 3);
    }

    #[test]
    fn long_content_reports_a_scrollable_height() {
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptItem::Notice {
                text: format!("line {i}"),
                level: NoticeLevel::Info,
            });
        }
        screen(&mut a, 80, 20);
        assert!(a.content_height > a.viewport_height, "the transcript must be scrollable");
        assert!(a.max_scroll() > 0);
    }

    #[test]
    fn a_pinned_scroll_position_survives_a_redraw() {
        let mut a = app();
        for i in 0..80 {
            a.transcript.push(TranscriptItem::Notice {
                text: format!("line {i}"),
                level: NoticeLevel::Info,
            });
        }
        screen(&mut a, 80, 20);
        a.scroll_up(10);
        let pinned = a.scroll;
        screen(&mut a, 80, 20);
        assert_eq!(a.scroll, pinned, "redrawing must not yank a scrolled-back view");
    }
}
