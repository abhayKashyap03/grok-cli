//! Small shared helpers: token estimation, diffing, path handling, timestamps.
//!
//! Each of these is used by at least two modules. Anything used by exactly one
//! module lives with that module instead.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Approximate the token count of a string.
///
/// The harness needs a token estimate *before* it sends a request, in order to
/// decide whether to compact. Running a real BPE tokenizer would mean vendoring
/// xAI's vocabulary, which is not published. Four bytes per token is the
/// standard rule of thumb for English-and-code text, and it errs high on code
/// (which is denser), which is the safe direction: over-estimating triggers
/// compaction slightly early, under-estimating triggers a context-overflow API
/// error. Real usage numbers from the API replace this estimate as soon as the
/// first response lands.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Current time as an RFC-3339 UTC string, e.g. `2026-09-07T05:24:20Z`.
///
/// Hand-rolled rather than pulling in a date library: the harness only ever
/// needs to *format* the current instant, never parse or do calendar
/// arithmetic.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format_unix_utc(secs)
}

pub fn format_unix_utc(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days-since-epoch to (year, month, day). Howard Hinnant's `civil_from_days`,
/// which is exact for the whole proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a duration for humans: `340ms`, `2.4s`, `1m12s`.
pub fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Resolve `path` against `workspace`, collapsing `.` and `..` lexically.
///
/// Deliberately *not* `canonicalize`: that requires the path to exist, which is
/// wrong for a write tool creating a new file, and it resolves symlinks, which
/// would let a symlink inside the workspace silently escape the sandbox check
/// in one direction while blocking legitimate paths in the other.
pub fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { workspace.join(raw) };
    normalize(&joined)
}

/// Lexically normalize a path, removing `.` and resolving `..` textually.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root is a no-op, matching `/..` == `/`.
                if !out.pop() {
                    out.push(comp.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True if `path` is inside `root`. Both should already be normalized.
pub fn is_within(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// Render `path` relative to `workspace` when possible, for display only.
pub fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace).unwrap_or(path).display().to_string()
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Truncate to at most `max` bytes on a character boundary, appending a note
/// about what was dropped.
///
/// Tool output goes straight into the model's context, so an unbounded result
/// from `grep` on a large repo can blow the window in a single call. Every tool
/// that can produce unbounded output routes through this.
pub fn truncate_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = text.len() - end;
    format!("{}\n\n[truncated {dropped} bytes of {} total]", &text[..end], text.len())
}

/// Heuristic binary-file detection: a NUL byte in the first 8 KiB.
///
/// Same rule `git` uses. Cheap, and wrong only for exotic text encodings the
/// model could not usefully read anyway.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Number lines in `cat -n` style, starting at `start`.
///
/// Line numbers matter: they let the model reference exact locations, and they
/// let the read and edit tools agree on what "line 42" means.
pub fn number_lines(text: &str, start: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.lines().count() * 8);
    for (i, line) in text.lines().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", start + i, line));
    }
    out
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

/// A single line of a rendered diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Added,
    Removed,
}

/// Produce a unified-style diff with `context` lines of surrounding context.
pub fn diff_lines(old: &str, new: &str, context: usize) -> Vec<DiffLine> {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for group in diff.grouped_ops(context) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => DiffKind::Context,
                    ChangeTag::Insert => DiffKind::Added,
                    ChangeTag::Delete => DiffKind::Removed,
                };
                out.push(DiffLine {
                    kind,
                    old_line: change.old_index().map(|i| i + 1),
                    new_line: change.new_index().map(|i| i + 1),
                    text: change.value().trim_end_matches('\n').to_string(),
                });
            }
        }
    }
    out
}

/// Count added and removed lines, for the `+3 -1` summary on an edit.
pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let lines = diff_lines(old, new, 0);
    let added = lines.iter().filter(|l| l.kind == DiffKind::Added).count();
    let removed = lines.iter().filter(|l| l.kind == DiffKind::Removed).count();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up_so_short_strings_never_cost_zero() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn unix_epoch_and_a_known_instant_format_correctly() {
        assert_eq!(format_unix_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_utc(1_788_758_660), "2026-09-07T05:24:20Z");
    }

    #[test]
    fn leap_days_are_handled() {
        assert_eq!(format_unix_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn relative_paths_resolve_against_the_workspace() {
        let ws = Path::new("/repo");
        assert_eq!(resolve(ws, "src/main.rs"), PathBuf::from("/repo/src/main.rs"));
        assert_eq!(resolve(ws, "./src/../lib.rs"), PathBuf::from("/repo/lib.rs"));
        assert_eq!(resolve(ws, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn dot_dot_escaping_the_workspace_is_detectable() {
        let ws = Path::new("/repo");
        let escaped = resolve(ws, "../secrets.txt");
        assert_eq!(escaped, PathBuf::from("/secrets.txt"));
        assert!(!is_within(ws, &escaped), "the sandbox check must see the escape");
        assert!(is_within(ws, &resolve(ws, "a/../b.rs")));
    }

    #[test]
    fn truncation_reports_what_it_dropped_and_respects_char_boundaries() {
        let out = truncate_text("aaaa😀bbbbbbbb", 6);
        assert!(out.starts_with("aaaa"), "got {out}");
        assert!(out.contains("truncated"));
        assert_eq!(truncate_text("short", 100), "short", "short input passes through unchanged");
    }

    #[test]
    fn binary_detection_keys_on_nul_bytes() {
        assert!(looks_binary(b"\x7fELF\0\0"));
        assert!(!looks_binary(b"fn main() {}"));
    }

    #[test]
    fn line_numbering_starts_where_it_is_told() {
        assert_eq!(number_lines("a\nb", 10), "    10\ta\n    11\tb\n");
    }

    #[test]
    fn diff_stats_count_only_changed_lines() {
        let (added, removed) = diff_stats("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(
            (added, removed),
            (2, 1),
            "b→B is one removal plus one addition; d is a further addition"
        );
    }

    #[test]
    fn identical_text_produces_no_diff() {
        assert!(diff_lines("same\n", "same\n", 3).is_empty());
        assert_eq!(diff_stats("same\n", "same\n"), (0, 0));
    }

    #[test]
    fn durations_format_at_three_scales() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(340)), "340ms");
        assert_eq!(format_duration(Duration::from_millis(2400)), "2.4s");
        assert_eq!(format_duration(Duration::from_secs(72)), "1m12s");
    }
}
