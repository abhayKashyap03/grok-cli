//! Tools that are neither filesystem nor shell: task tracking and web fetch.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::api::ToolSpec;
use crate::tools::{
    Tool, ToolContext, ToolKind, ToolOutcome, TodoItem, TodoStatus, object_schema, prop,
};
use crate::util;

/// Cap on fetched page text handed to the model.
const MAX_FETCH_BYTES: usize = 96 * 1024;
/// Cap on bytes read off the wire, applied *while* reading.
///
/// Larger than the text cap because markup compresses away, but bounded: the
/// model picks the URL from untrusted context, so the server may be hostile or
/// simply enormous.
const MAX_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// todo_write
// ---------------------------------------------------------------------------

/// Lets the model keep an explicit plan.
///
/// This exists for the *user's* benefit as much as the model's: a visible
/// checklist is how someone watching a long autonomous run knows what is
/// happening and how much is left. It writes harness state only, so it is
/// classified [`ToolKind::Meta`] and never needs approval.
pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert(
            "todos".into(),
            json!({
                "type": "array",
                "description": "The complete to-do list, replacing any previous one.",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "What needs doing." },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed"],
                            "description": "Keep exactly one task in_progress at a time."
                        }
                    },
                    "required": ["content", "status"]
                }
            }),
        );
        ToolSpec::function(
            "todo_write",
            "Record or update the task list for the current piece of work. Use it for anything with three or more steps, and mark each task completed as soon as it is done.",
            object_schema(p, &["todos"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Meta
    }

    fn summarize(&self, args: &Value) -> String {
        let n = args.get("todos").and_then(Value::as_array).map_or(0, Vec::len);
        format!("TodoWrite({n} tasks)")
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(raw) = args.get("todos").and_then(Value::as_array) else {
            return Ok(ToolOutcome::error("missing required array argument `todos`"));
        };

        let mut parsed = Vec::with_capacity(raw.len());
        for (i, item) in raw.iter().enumerate() {
            let Some(content) = item.get("content").and_then(Value::as_str) else {
                return Ok(ToolOutcome::error(format!("todos[{i}] is missing `content`")));
            };
            let status = match item.get("status").and_then(Value::as_str).unwrap_or("pending") {
                "pending" => TodoStatus::Pending,
                "in_progress" => TodoStatus::InProgress,
                "completed" => TodoStatus::Completed,
                other => {
                    return Ok(ToolOutcome::error(format!(
                        "todos[{i}] has unknown status `{other}`; expected pending, in_progress or completed"
                    )));
                }
            };
            parsed.push(TodoItem { content: content.to_string(), status });
        }

        let in_progress = parsed.iter().filter(|t| t.status == TodoStatus::InProgress).count();
        let done = parsed.iter().filter(|t| t.status == TodoStatus::Completed).count();
        let total = parsed.len();

        let rendered: String =
            parsed.iter().map(|t| format!("{} {}\n", t.status.glyph(), t.content)).collect();

        *ctx.todos.lock().await = parsed;

        // A nudge rather than an error: more than one in-progress task is a
        // planning smell, not a reason to fail the call.
        let note = if in_progress > 1 {
            "\nNote: more than one task is in_progress. Keep exactly one active at a time.\n"
        } else {
            ""
        };

        Ok(ToolOutcome::ok(format!("Task list updated ({done}/{total} done)\n{rendered}{note}"))
            .with_summary(format!("{done}/{total} done")))
    }
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

/// Fetches a URL and returns it as plain text.
///
/// Deliberately dumb: no JavaScript, no LLM summarization pass. The model is
/// perfectly capable of reading a stripped page, and adding a summarization
/// call would double the cost of every fetch and lose detail the model needs.
pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("url".into(), prop("string", "Absolute http or https URL to fetch."));
        ToolSpec::function(
            "web_fetch",
            "Fetch a web page and return its text content with HTML markup stripped. Useful for reading documentation and API references.",
            object_schema(p, &["url"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Network
    }

    fn summarize(&self, args: &Value) -> String {
        format!("WebFetch({})", args.get("url").and_then(Value::as_str).unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return Ok(ToolOutcome::error("missing required string argument `url`"));
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolOutcome::error(format!(
                "`{url}` is not an http or https URL. Only web URLs can be fetched."
            )));
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("grok-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;

        let response = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Ok(ToolOutcome::error("fetch interrupted by the user")),
            r = client.get(url).send() => r,
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutcome::error(format!("cannot fetch {url}: {e}"))),
        };

        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutcome::error(format!("{url} returned HTTP {status}")));
        }

        // Read incrementally with a hard cap rather than calling `.text()`,
        // which materializes the whole body first. The model chooses the URL
        // from untrusted context, so the server on the other end may be hostile
        // — or merely enormous — and truncating after the fact is too late.
        let mut collected: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        let mut truncated = false;
        loop {
            let next = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    return Ok(ToolOutcome::error("fetch interrupted by the user"));
                }
                chunk = futures_util::StreamExt::next(&mut stream) => chunk,
            };
            let Some(chunk) = next else { break };
            match chunk {
                Err(e) => return Ok(ToolOutcome::error(format!("cannot read body of {url}: {e}"))),
                Ok(bytes) => {
                    collected.extend_from_slice(&bytes);
                    if collected.len() >= MAX_DOWNLOAD_BYTES {
                        collected.truncate(MAX_DOWNLOAD_BYTES);
                        truncated = true;
                        break;
                    }
                }
            }
        }

        let body = String::from_utf8_lossy(&collected).into_owned();
        if truncated {
            tracing::warn!(url, "response exceeded the fetch limit and was truncated");
        }

        let text = strip_html(&body);
        Ok(ToolOutcome::ok(util::truncate_text(&text, MAX_FETCH_BYTES))
            .with_summary(format!("{} chars from {url}", text.len())))
    }
}

/// Reduce HTML to readable text.
///
/// A real parser would be better, but the failure mode of this one is "some
/// stray angle brackets survive", which the model handles fine. `script` and
/// `style` bodies are dropped wholesale because leaving them in is the
/// difference between a readable page and a wall of minified JavaScript.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0usize;

    'scan: while i < html.len() {
        let rest = &html[i..];

        if rest.starts_with('<') {
            // Drop the entire body of a script or style element. Without this
            // the output is a wall of minified JavaScript, and `1 < 2` inside a
            // script would be mistaken for a tag.
            for (open, close) in [("<script", "</script>"), ("<style", "</style>")] {
                if starts_with_ci(rest, open) {
                    i = match find_ci(rest, close) {
                        Some(end) => i + end + close.len(),
                        // Unclosed script: everything after it is unreadable.
                        None => html.len(),
                    };
                    continue 'scan;
                }
            }
            // An ordinary tag: skip to its '>' and emit a line break, so the
            // document's block structure survives into the text.
            i = match rest.find('>') {
                Some(end) => i + end + 1,
                None => html.len(),
            };
            out.push('\n');
            continue 'scan;
        }

        let ch = rest.chars().next().expect("non-empty remainder");
        out.push(ch);
        i += ch.len_utf8();
    }

    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    // Collapse the run of blank lines that tag-stripping leaves behind.
    let mut result = String::with_capacity(decoded.len());
    let mut blanks = 0;
    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        result.push_str(trimmed);
        result.push('\n');
    }
    result.trim().to_string()
}

/// ASCII-case-insensitive `starts_with`.
///
/// Comparing against a lowercased copy of the whole document would be simpler
/// but wrong: some characters change byte length when lowercased, which
/// desynchronizes indices between the copy and the original.
fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len()
        && haystack.as_bytes()[..needle.len()].eq_ignore_ascii_case(needle.as_bytes())
}

/// ASCII-case-insensitive `find`, returning a byte offset into `haystack`.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), CancellationToken::new())
    }

    #[tokio::test]
    async fn todo_write_replaces_the_list_and_reports_progress() {
        let ctx = ctx();
        let out = TodoWrite
            .run(
                json!({"todos": [
                    {"content": "write the parser", "status": "completed"},
                    {"content": "wire up the loop", "status": "in_progress"},
                    {"content": "add tests", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(out.display.summary.as_deref(), Some("1/3 done"));
        assert!(out.content.contains("☑ write the parser"));
        assert!(out.content.contains("◐ wire up the loop"));
        assert_eq!(ctx.todos.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn todo_write_flags_more_than_one_active_task_without_failing() {
        let out = TodoWrite
            .run(
                json!({"todos": [
                    {"content": "a", "status": "in_progress"},
                    {"content": "b", "status": "in_progress"}
                ]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "a planning smell is a nudge, not a failure");
        assert!(out.content.contains("Keep exactly one active"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn todo_write_rejects_an_unknown_status() {
        let out = TodoWrite
            .run(json!({"todos": [{"content": "a", "status": "almost"}]}), &ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("unknown status `almost`"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn web_fetch_refuses_non_web_schemes() {
        let out = WebFetch.run(json!({"url": "file:///etc/passwd"}), &ctx()).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not an http or https URL"), "got: {}", out.content);
    }

    #[test]
    fn html_stripping_drops_scripts_and_keeps_prose() {
        let html = "<html><head><style>body{color:red}</style>\
            <script>var x = 1 < 2;</script></head>\
            <body><h1>Title</h1><p>Hello &amp; welcome</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"), "entities are decoded: {text}");
        assert!(!text.contains("color:red"), "style bodies are dropped: {text}");
        assert!(!text.contains("var x"), "script bodies are dropped: {text}");
    }

    #[test]
    fn html_stripping_collapses_blank_line_runs() {
        let text = strip_html("<div></div><div></div><p>only line</p>");
        assert_eq!(text, "only line");
    }
}
