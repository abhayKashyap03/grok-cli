//! Renders a representative session through the real TUI renderer and dumps
//! the resulting cell grid as JSON, so a screenshot is generated from the
//! actual drawing code rather than mocked up by hand.
//!
//! Run with: cargo run --example screenshot -- <cols> <rows>

use std::collections::BTreeMap;
use std::time::Duration;

use grok_cli::config::{Config, PermissionMode, PermissionRules};
use grok_cli::tools::{ToolDisplay, TodoItem, TodoStatus};
use grok_cli::tui::{App, NoticeLevel, ToolState, TranscriptItem, render};
use grok_cli::util;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

fn config() -> Config {
    Config {
        api_key: String::new(),
        model: "grok-4-1-fast-non-reasoning".into(),
        base_url: grok_cli::api::DEFAULT_BASE_URL.into(),
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
        workspace: std::path::PathBuf::from("~/projects/auth-service"),
    }
}

fn tool(name: &str, summary: &str, detail: &str, diff: Vec<util::DiffLine>) -> TranscriptItem {
    TranscriptItem::Tool {
        id: name.into(),
        name: name.into(),
        summary: summary.into(),
        state: ToolState::Finished {
            summary: Some(detail.into()),
            display: Box::new(ToolDisplay { diff, ..Default::default() }),
            is_error: false,
            duration: Duration::from_millis(120),
        },
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cols: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(150);
    let rows: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(24);

    let mut app = App::new(config(), "demo".into());
    app.model = "grok-4-1-fast-non-reasoning".into();
    app.mode = PermissionMode::Default;
    app.context_tokens = 18_400;
    app.usage.total_tokens = 24_910;
    app.todos = vec![
        TodoItem { content: "locate the expiry comparison".into(), status: TodoStatus::Completed },
        TodoItem { content: "fix the boundary".into(), status: TodoStatus::Completed },
        TodoItem { content: "add a regression test".into(), status: TodoStatus::InProgress },
    ];

    app.transcript = vec![
        TranscriptItem::User(
            "tokens are being rejected at the exact expiry instant — find it and fix it, then prove it with a test".into(),
        ),
        TranscriptItem::Assistant {
            text: "Searching for the expiry comparison.".into(),
            done: true,
        },
        tool("grep", "Grep(now < expiry)", "2 matches in 1 file", vec![]),
        tool("read_file", "Read(src/auth.rs)", "142 lines", vec![]),
        tool(
            "edit_file",
            "Edit(src/auth.rs)",
            "+1 -1",
            util::diff_lines(
                "pub fn is_valid(now: u64, expiry: u64) -> bool {\n    now < expiry\n}\n",
                "pub fn is_valid(now: u64, expiry: u64) -> bool {\n    now <= expiry\n}\n",
                1,
            ),
        ),
        tool("bash", "Bash(cargo test auth)", "exit 0 in 2.4s", vec![]),
        TranscriptItem::Assistant {
            text: "Fixed. `src/auth.rs:42` used a strict `<`, so a token was invalid at the \
                   exact instant it expired — an off-by-one on the boundary, not on the clock.\n\n\
                   Changed it to `<=` and added `valid_at_the_expiry_instant` to pin the \
                   boundary. All 38 auth tests pass."
                .into(),
            done: true,
        },
        TranscriptItem::Notice {
            text: "Edited 1 file · 38 tests passed".into(),
            level: NoticeLevel::Success,
        },
    ];

    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).expect("backend");
    terminal.draw(|frame| render::draw(frame, &mut app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();

    // Emit rows of [char, fg, bold] so the rasterizer needs no ratatui types.
    let mut out = String::from("[\n");
    for y in 0..buffer.area.height {
        out.push('[');
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let ch = cell.symbol();
            let fg = hex(cell.style().fg.unwrap_or(Color::Reset));
            let bold = cell.style().add_modifier.contains(Modifier::BOLD);
            out.push_str(&format!(
                "[{},\"{}\",{}]",
                serde_json::to_string(ch).unwrap(),
                fg,
                if bold { 1 } else { 0 }
            ));
            if x + 1 < buffer.area.width {
                out.push(',');
            }
        }
        out.push(']');
        if y + 1 < buffer.area.height {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    println!("{out}");
}

/// Map the palette the theme uses onto concrete RGB for rasterizing.
fn hex(color: Color) -> &'static str {
    match color {
        Color::Reset | Color::White | Color::Gray => "d8dee9",
        Color::DarkGray => "6b7280",
        Color::Cyan | Color::LightCyan => "6cc7e6",
        Color::Magenta | Color::LightMagenta => "c39ae6",
        Color::Green | Color::LightGreen => "8fce7a",
        Color::Red | Color::LightRed => "e88388",
        Color::Yellow | Color::LightYellow => "e3b341",
        Color::Blue | Color::LightBlue => "6ea8fe",
        _ => "d8dee9",
    }
}
