//! Rendering assistant markdown as styled terminal lines.
//!
//! Models write markdown, and a terminal that prints the raw asterisks looks
//! broken. This walks `pulldown_cmark` events and emits `ratatui` lines.
//!
//! Two decisions worth stating:
//!
//! * **Streaming-safe.** Text arrives token by token, so the renderer is called
//!   on a partial document constantly. Unterminated code fences and dangling
//!   emphasis are normal, not errors, and must render as something reasonable
//!   rather than swallowing the rest of the response.
//! * **No syntax highlighting.** A real highlighter means vendoring grammars
//!   for every language and a large binary, to colour code the user is about to
//!   read in their editor anyway. Code blocks get one distinct colour and an
//!   indent, which is what actually aids scanning.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Colours for the whole interface, not just markdown.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    pub user: Color,
    pub tool: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub code: Color,
    pub added: Color,
    pub removed: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            text: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Cyan,
            user: Color::Cyan,
            tool: Color::Magenta,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            code: Color::LightGreen,
            added: Color::Green,
            removed: Color::Red,
        }
    }

    pub fn light() -> Self {
        Self {
            text: Color::Reset,
            dim: Color::Gray,
            accent: Color::Blue,
            user: Color::Blue,
            tool: Color::Magenta,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Rgb(180, 120, 0),
            code: Color::Rgb(0, 110, 60),
            added: Color::Green,
            removed: Color::Red,
        }
    }

    pub fn named(name: &str) -> Self {
        if name.eq_ignore_ascii_case("light") { Self::light() } else { Self::dark() }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Render markdown into styled lines.
pub fn render(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut state = Renderer::new(theme);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    for event in Parser::new_ext(source, options) {
        state.handle(event);
    }
    state.finish()
}

struct Renderer<'a> {
    theme: &'a Theme,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    /// Nested emphasis and code spans compose, so styles are a stack.
    style_stack: Vec<Style>,
    /// `Some(indent)` while inside a fenced or indented code block.
    in_code_block: bool,
    /// Bullet markers for nested lists; length is the nesting depth.
    list_stack: Vec<Option<u64>>,
    /// Set when the next text event should be prefixed with a list marker.
    pending_marker: Option<String>,
    quote_depth: usize,
}

impl<'a> Renderer<'a> {
    fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: vec![Style::default().fg(theme.text)],
            in_code_block: false,
            list_stack: Vec::new(),
            pending_marker: None,
            quote_depth: 0,
        }
    }

    fn style(&self) -> Style {
        *self.style_stack.last().expect("the base style is never popped")
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let next = f(self.style());
        self.style_stack.push(next);
    }

    fn pop_style(&mut self) {
        // Never pop the base: a malformed or truncated document would otherwise
        // leave the renderer with no style at all and panic.
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
    }

    /// End the current line only if something is on it.
    fn break_line(&mut self) {
        if !self.current.is_empty() {
            self.flush_line();
        }
    }

    /// A blank line, collapsed so consecutive blocks do not stack them.
    fn blank_line(&mut self) {
        self.break_line();
        if self.lines.last().is_some_and(|l| line_is_blank(l)) {
            return;
        }
        if !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
    }

    fn indent(&self) -> String {
        let mut prefix = String::new();
        for _ in 0..self.quote_depth {
            prefix.push_str("│ ");
        }
        // Continuation lines of a nested list align under their marker.
        for _ in 0..self.list_stack.len().saturating_sub(1) {
            prefix.push_str("  ");
        }
        prefix
    }

    fn push_text(&mut self, text: &str) {
        let style = self.style();

        for (i, segment) in text.split('\n').enumerate() {
            if i > 0 {
                self.flush_line();
            }
            if self.current.is_empty() {
                let indent = self.indent();
                if !indent.is_empty() {
                    self.current.push(Span::styled(indent, Style::default().fg(self.theme.dim)));
                }
                if let Some(marker) = self.pending_marker.take() {
                    self.current.push(Span::styled(marker, Style::default().fg(self.theme.accent)));
                }
            }
            if !segment.is_empty() {
                self.current.push(Span::styled(segment.to_string(), style));
            }
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                let style = Style::default().fg(self.theme.code);
                if self.current.is_empty() {
                    let indent = self.indent();
                    if !indent.is_empty() {
                        self.current.push(Span::styled(indent, Style::default().fg(self.theme.dim)));
                    }
                }
                self.current.push(Span::styled(code.to_string(), style));
            }
            Event::SoftBreak => {
                // A soft break is a wrap point in the source, not a paragraph
                // break; rendering it as a space lets the terminal re-wrap.
                self.push_text(" ");
            }
            Event::HardBreak => self.break_line(),
            Event::Rule => {
                self.blank_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(self.theme.dim),
                )));
                self.blank_line();
            }
            Event::TaskListMarker(done) => {
                let glyph = if done { "[x] " } else { "[ ] " };
                self.push_text(glyph);
            }
            // Raw HTML in a model's answer is noise; drop it rather than
            // printing angle brackets at the user.
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::FootnoteReference(name) => self.push_text(&format!("[^{name}]")),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.push_text(&t),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.blank_line(),
            Tag::Heading { level, .. } => {
                self.blank_line();
                let modifier = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    Modifier::BOLD | Modifier::UNDERLINED
                } else {
                    Modifier::BOLD
                };
                self.push_style(|s| s.fg(self.theme.accent).add_modifier(modifier));
            }
            Tag::BlockQuote(_) => {
                self.blank_line();
                self.quote_depth += 1;
                self.push_style(|s| s.fg(self.theme.dim).add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.blank_line();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.lines.push(Line::from(Span::styled(
                        format!("  {lang}"),
                        Style::default().fg(self.theme.dim).add_modifier(Modifier::ITALIC),
                    )));
                }
                self.push_style(|s| s.fg(self.theme.code));
            }
            Tag::List(start) => {
                if self.list_stack.is_empty() {
                    self.blank_line();
                } else {
                    self.break_line();
                }
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.break_line();
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(marker);
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.push_style(|s| s.fg(self.theme.accent).add_modifier(Modifier::UNDERLINED)),
            Tag::Table(_) | Tag::TableHead | Tag::TableRow => self.break_line(),
            Tag::TableCell => self.push_text(" │ "),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.break_line(),
            TagEnd::Heading(_) => {
                self.break_line();
                self.pop_style();
            }
            TagEnd::BlockQuote(_) => {
                self.break_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.pop_style();
            }
            TagEnd::CodeBlock => {
                self.break_line();
                self.in_code_block = false;
                self.pop_style();
            }
            TagEnd::List(_) => {
                self.break_line();
                self.list_stack.pop();
                self.pending_marker = None;
            }
            TagEnd::Item => self.break_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.pop_style();
            }
            TagEnd::TableHead | TagEnd::TableRow => self.break_line(),
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.break_line();
        // Trailing blanks are an artifact of block boundaries, not content.
        while self.lines.last().is_some_and(|l| line_is_blank(l)) {
            self.lines.pop();
        }
        self.lines
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Flatten rendered lines back to plain text. Used by tests and by `/export`.
pub fn to_plain(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str) -> String {
        to_plain(&render(source, &Theme::dark()))
    }

    #[test]
    fn plain_prose_survives_unchanged() {
        assert_eq!(plain("Hello there."), "Hello there.");
    }

    #[test]
    fn emphasis_markers_are_consumed_not_printed() {
        let out = plain("This is **bold** and *italic*.");
        assert_eq!(out, "This is bold and italic.");
        assert!(!out.contains('*'), "raw markdown would look broken in a terminal");
    }

    #[test]
    fn bold_text_actually_carries_the_bold_modifier() {
        let lines = render("**loud**", &Theme::dark());
        let bold = lines[0].spans.iter().find(|s| s.content.contains("loud")).unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn headings_are_styled_and_their_hashes_removed() {
        let lines = render("# Title", &Theme::dark());
        assert_eq!(to_plain(&lines), "Title");
        let span = &lines[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.style.fg, Some(Theme::dark().accent));
    }

    #[test]
    fn bullet_lists_get_markers() {
        let out = plain("- one\n- two");
        assert_eq!(out, "• one\n• two");
    }

    #[test]
    fn ordered_lists_number_themselves() {
        assert_eq!(plain("1. first\n2. second"), "1. first\n2. second");
    }

    #[test]
    fn inline_code_is_coloured_and_keeps_its_content() {
        let lines = render("call `foo()` now", &Theme::dark());
        assert_eq!(to_plain(&lines), "call foo() now");
        let code = lines[0].spans.iter().find(|s| s.content == "foo()").unwrap();
        assert_eq!(code.style.fg, Some(Theme::dark().code));
    }

    #[test]
    fn fenced_code_blocks_keep_their_lines_and_label_the_language() {
        let out = plain("```rust\nfn main() {}\nlet x = 1;\n```");
        assert!(out.contains("rust"), "the language is labelled: {out}");
        assert!(out.contains("fn main() {}"));
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn an_unterminated_code_fence_still_renders_its_content() {
        // This is the normal case while a response is streaming.
        let out = plain("Here you go:\n\n```rust\nfn main() {}");
        assert!(out.contains("fn main() {}"), "streaming must not swallow the tail: {out}");
    }

    #[test]
    fn dangling_emphasis_does_not_panic_or_eat_the_rest() {
        // A half-streamed `**bold` arrives constantly.
        let out = plain("Some **partially typed");
        assert!(out.contains("partially typed"), "got: {out}");
    }

    #[test]
    fn block_quotes_are_marked_in_the_gutter() {
        let out = plain("> quoted text");
        assert!(out.contains("quoted text"));
        assert!(out.contains('│'), "the quote gutter is visible: {out}");
    }

    #[test]
    fn consecutive_blank_lines_collapse() {
        let out = plain("one\n\n\n\ntwo");
        assert_eq!(out, "one\n\ntwo", "runs of blank lines waste vertical space");
    }

    #[test]
    fn trailing_blank_lines_are_trimmed() {
        let lines = render("text\n\n\n", &Theme::dark());
        assert!(!lines.last().is_some_and(line_is_blank));
    }

    #[test]
    fn raw_html_is_dropped_rather_than_shown() {
        let out = plain("before <div class=\"x\"> after");
        assert!(!out.contains("<div"), "angle brackets in a terminal are noise: {out}");
        assert!(out.contains("before"));
    }

    #[test]
    fn an_empty_document_renders_to_nothing() {
        assert!(render("", &Theme::dark()).is_empty());
        assert!(render("   \n  \n", &Theme::dark()).is_empty());
    }

    #[test]
    fn task_list_markers_render_as_checkboxes() {
        let out = plain("- [x] done\n- [ ] todo");
        assert!(out.contains("[x] done"), "got: {out}");
        assert!(out.contains("[ ] todo"), "got: {out}");
    }

    #[test]
    fn themes_resolve_by_name_and_default_to_dark() {
        assert_eq!(Theme::named("light").accent, Theme::light().accent);
        assert_eq!(Theme::named("dark").accent, Theme::dark().accent);
        assert_eq!(Theme::named("nonsense").accent, Theme::dark().accent);
    }

    #[test]
    fn a_long_realistic_answer_renders_without_panicking() {
        let source = "# Fix\n\nThe bug is in `src/auth.rs:42`.\n\n\
                      1. Read the file\n2. Change `<` to `<=`\n\n\
                      ```rust\nif now <= expiry { ok() }\n```\n\n\
                      > Note: this changes boundary behaviour.\n\n\
                      - [x] fixed\n- [ ] tested\n\n---\n\nDone.";
        let out = plain(source);
        assert!(out.contains("Fix"));
        assert!(out.contains("src/auth.rs:42"));
        assert!(out.contains("if now <= expiry"));
        assert!(out.contains("Done."));
    }
}
