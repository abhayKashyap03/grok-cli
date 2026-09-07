//! Filesystem tools: read, write, edit, list, glob, grep.
//!
//! Every path argument is resolved against the workspace and then checked for
//! escape. That check is the only thing standing between a confused model and
//! the rest of the disk, so it lives in one function, [`resolve_in_workspace`],
//! which every tool here calls first.
//!
//! Output size is bounded everywhere. A tool result goes straight into the
//! model's context, so an unbounded `grep` across a monorepo is not merely slow
//! — it silently costs the user their whole context window.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::api::ToolSpec;
use crate::tools::{Tool, ToolContext, ToolDisplay, ToolKind, ToolOutcome, object_schema, prop};
use crate::util;

/// Largest single file the read tool will return.
const MAX_READ_BYTES: usize = 256 * 1024;
/// Largest result any search tool will return.
const MAX_SEARCH_BYTES: usize = 64 * 1024;
/// Default number of lines returned by a read with no explicit limit.
const DEFAULT_READ_LINES: usize = 2000;
/// Hard cap on files walked by glob/grep, so a runaway pattern terminates.
const MAX_WALK_ENTRIES: usize = 200_000;

/// Resolve `path` inside the workspace, rejecting escapes.
///
/// Two checks, and both are necessary:
///
/// 1. **Lexical.** Catches `../../etc/passwd` even when nothing on that path
///    exists yet, which matters for a tool creating a new file.
/// 2. **Physical.** Resolves symlinks and re-checks containment. Without this a
///    symlink inside the workspace — trivially created by an earlier `bash`
///    call, or simply checked into a cloned repository — reads and writes
///    anywhere on the disk while passing the lexical check cleanly.
///
/// The physical check cannot simply `canonicalize` the whole path: that fails
/// when the file does not exist yet. It canonicalizes the deepest *existing*
/// ancestor instead, then rebuilds the remainder on top, so a new file inherits
/// the containment verdict of the directory it will live in.
fn resolve_in_workspace(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    let outside = |resolved: &Path| {
        format!(
            "{path} resolves to {}, which is outside the workspace ({}). Tools may only touch files under the workspace root.",
            resolved.display(),
            ctx.workspace.display()
        )
    };

    let lexical = util::resolve(&ctx.workspace, path);
    if !util::is_within(&ctx.workspace, &lexical) {
        return Err(outside(&lexical));
    }

    // Walk up to the deepest component that exists on disk. `exists()` follows
    // symlinks, so a link is "existing" and gets canonicalized — which is
    // exactly what surfaces an escape.
    let mut existing: &Path = &lexical;
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                trailing.push(name.to_owned());
                existing = parent;
            }
            // Ran out of ancestors without finding anything real.
            _ => return Err(outside(&lexical)),
        }
    }

    let Ok(canonical) = existing.canonicalize() else { return Err(outside(&lexical)) };
    if !util::is_within(&ctx.workspace, &canonical) {
        return Err(format!(
            "{path} resolves through a symlink to {}, which is outside the workspace ({}). Tools may only touch files under the workspace root.",
            canonical.display(),
            ctx.workspace.display()
        ));
    }

    let mut resolved = canonical;
    for name in trailing.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Pull a required string argument, with a message the model can act on.
fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

fn arg_str_opt(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert(
            "path".into(),
            prop("string", "Path to the file, relative to the workspace root or absolute."),
        );
        p.insert(
            "offset".into(),
            prop("integer", "1-based line number to start from. Use with `limit` to page through a large file."),
        );
        p.insert("limit".into(), prop("integer", "Maximum number of lines to return."));
        ToolSpec::function(
            "read_file",
            "Read a text file. Always read a file before editing it. Each line is prefixed with `<line number>│` for reference only — that prefix is NOT part of the file, so never include it in `edit_file`'s old_string. Everything after the `│` is the file's exact content, including its indentation.",
            object_schema(p, &["path"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, args: &Value) -> String {
        format!("Read({})", arg_str_opt(args, "path").unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let raw = match arg_str(&args, "path") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let path = match resolve_in_workspace(ctx, &raw) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutcome::error(format!("cannot read {}: {e}", path.display()))),
        };

        if util::looks_binary(&bytes) {
            return Ok(ToolOutcome::error(format!(
                "{} is a binary file ({} bytes) and cannot be read as text.",
                path.display(),
                bytes.len()
            )));
        }

        let text = String::from_utf8_lossy(&bytes).into_owned();
        let all: Vec<&str> = text.lines().collect();
        let total = all.len();
        let start = arg_usize(&args, "offset").unwrap_or(1).max(1);
        let limit = arg_usize(&args, "limit").unwrap_or(DEFAULT_READ_LINES);

        if start > total && total > 0 {
            return Ok(ToolOutcome::error(format!(
                "offset {start} is past the end of {} ({total} lines)",
                path.display()
            )));
        }

        let slice: Vec<&str> = all.iter().skip(start - 1).take(limit).copied().collect();
        let shown = slice.len();
        let numbered = util::number_lines(&slice.join("\n"), start);

        // Record the read so a later edit can verify the file has not changed.
        ctx.mark_read(&path).await;

        let mut body = util::truncate_text(&numbered, MAX_READ_BYTES);
        if shown > 0 && start + shown - 1 < total {
            body.push_str(&format!(
                "\n[showing lines {}-{} of {total}; call again with offset={} for more]\n",
                start,
                start + shown - 1,
                start + shown
            ));
        }
        if total == 0 {
            body = format!("{} is empty.\n", path.display());
        }

        Ok(ToolOutcome::ok(body).with_display(ToolDisplay {
            path: Some(util::display_path(&ctx.workspace, &path)),
            summary: Some(format!("{shown} line{}", if shown == 1 { "" } else { "s" })),
            ..Default::default()
        }))
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("path".into(), prop("string", "Path to write."));
        p.insert(
            "content".into(),
            prop("string", "Full contents to write. Replaces the file entirely."),
        );
        ToolSpec::function(
            "write_file",
            "Create a file, or overwrite an existing one in full. To change part of an existing file prefer `edit_file`, which is safer and cheaper.",
            object_schema(p, &["path", "content"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn summarize(&self, args: &Value) -> String {
        format!("Write({})", arg_str_opt(args, "path").unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let raw = match arg_str(&args, "path") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let content = match arg_str(&args, "content") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let path = match resolve_in_workspace(ctx, &raw) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };

        if let Some(err) = ctx.staleness_error(&path).await {
            return Ok(ToolOutcome::error(err));
        }

        let previous = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let existed = path.exists();

        if let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(ToolOutcome::error(format!("cannot create {}: {e}", parent.display())));
        }
        if let Err(e) = tokio::fs::write(&path, &content).await {
            return Ok(ToolOutcome::error(format!("cannot write {}: {e}", path.display())));
        }
        ctx.mark_read(&path).await;

        let (added, removed) = util::diff_stats(&previous, &content);
        let shown = util::display_path(&ctx.workspace, &path);
        Ok(ToolOutcome::ok(format!(
            "{} {} ({} lines)",
            if existed { "Overwrote" } else { "Created" },
            shown,
            content.lines().count()
        ))
        .with_display(ToolDisplay {
            path: Some(shown),
            diff_stats: Some((added, removed)),
            diff: util::diff_lines(&previous, &content, 3),
            summary: Some(format!("+{added} -{removed}")),
        }))
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert("path".into(), prop("string", "File to edit."));
        p.insert(
            "old_string".into(),
            prop("string", "Exact text to replace, including indentation. Must appear exactly once unless `replace_all` is true."),
        );
        p.insert("new_string".into(), prop("string", "Replacement text."));
        p.insert(
            "replace_all".into(),
            prop("boolean", "Replace every occurrence instead of requiring exactly one. Defaults to false."),
        );
        ToolSpec::function(
            "edit_file",
            "Replace exact text in a file. The file must have been read first. `old_string` must match the file byte for byte, including its leading whitespace — but WITHOUT the `<line number>│` prefixes that read_file adds for display.",
            object_schema(p, &["path", "old_string", "new_string"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn summarize(&self, args: &Value) -> String {
        format!("Edit({})", arg_str_opt(args, "path").unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let raw = match arg_str(&args, "path") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let old = match arg_str(&args, "old_string") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let new = arg_str_opt(&args, "new_string").unwrap_or_default();
        let replace_all = arg_bool(&args, "replace_all");

        if old == new {
            return Ok(ToolOutcome::error("old_string and new_string are identical; nothing to do"));
        }

        let path = match resolve_in_workspace(ctx, &raw) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        if let Some(err) = ctx.staleness_error(&path).await {
            return Ok(ToolOutcome::error(err));
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutcome::error(format!("cannot read {}: {e}", path.display()))),
        };

        // Recovery ladder. Each rung handles a way models get `old_string`
        // slightly wrong while their *intent* stays unambiguous. Being strict
        // here does not make edits safer — it just costs round trips and pushes
        // the model toward `write_file`, which is far more destructive.
        let mut old = old;
        let mut recovery: Option<&str> = None;

        // 1. The display prefix from read_file was pasted back in.
        if !content.contains(&old)
            && let Some(stripped) = strip_line_number_prefixes(&old)
            && content.contains(&stripped)
        {
            old = stripped;
            recovery = Some("line-number prefixes were stripped from old_string");
        }

        // 2. The indentation was reconstructed and got it wrong.
        let mut indentation_range = None;
        if !content.contains(&old) {
            let candidate = strip_line_number_prefixes(&old).unwrap_or_else(|| old.clone());
            if let Some(range) = find_ignoring_indentation(&content, &candidate) {
                indentation_range = Some(range);
                recovery = Some("old_string matched apart from leading whitespace");
            }
        }

        let matches = if indentation_range.is_some() { 1 } else { content.matches(&old).count() };
        if matches == 0 {
            return Ok(ToolOutcome::error(format!(
                "old_string not found in {}. It must match the file exactly, including indentation and line breaks, and must not include the `<line number>{}` prefixes that read_file adds for display.",
                path.display(),
                util::LINE_NUMBER_SEPARATOR
            )));
        }
        // Ambiguity is an error, not a coin flip: replacing the wrong one of
        // three identical lines produces a subtle bug the model cannot see.
        if matches > 1 && !replace_all {
            return Ok(ToolOutcome::error(format!(
                "old_string appears {matches} times in {}. Include more surrounding context to make it unique, or set replace_all=true.",
                path.display()
            )));
        }

        let updated = match indentation_range {
            // Re-indent the replacement to match what the file actually had, so
            // recovering from bad whitespace does not introduce bad whitespace.
            Some(range) => {
                let actual = &content[range.clone()];
                let indent: String = actual
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let reindented = reindent(&new, &indent);
                let mut updated = content.clone();
                updated.replace_range(range, &reindented);
                updated
            }
            None if replace_all => content.replace(&old, &new),
            None => content.replacen(&old, &new, 1),
        };

        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return Ok(ToolOutcome::error(format!("cannot write {}: {e}", path.display())));
        }
        ctx.mark_read(&path).await;

        let (added, removed) = util::diff_stats(&content, &updated);
        let shown = util::display_path(&ctx.workspace, &path);
        let note = recovery
            .map(|r| format!(" ({r}; match the file exactly next time)"))
            .unwrap_or_default();
        Ok(ToolOutcome::ok(format!(
            "Edited {shown}: {matches} replacement{} (+{added} -{removed}){note}",
            if matches == 1 { "" } else { "s" }
        ))
        .with_display(ToolDisplay {
            path: Some(shown),
            diff_stats: Some((added, removed)),
            diff: util::diff_lines(&content, &updated, 3),
            summary: Some(format!("+{added} -{removed}")),
        }))
    }
}

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

pub struct ListFiles;

#[async_trait]
impl Tool for ListFiles {
    fn name(&self) -> &str {
        "list_files"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert(
            "path".into(),
            prop("string", "Directory to list. Defaults to the workspace root."),
        );
        ToolSpec::function(
            "list_files",
            "List the immediate contents of a directory. Directories are suffixed with '/'. For recursive search use `glob`.",
            object_schema(p, &[]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, args: &Value) -> String {
        format!("List({})", arg_str_opt(args, "path").unwrap_or_else(|| ".".into()))
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let raw = arg_str_opt(&args, "path").unwrap_or_else(|| ".".into());
        let dir = match resolve_in_workspace(ctx, &raw) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };

        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => return Ok(ToolOutcome::error(format!("cannot list {}: {e}", dir.display()))),
        };

        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            names.push(if is_dir { format!("{name}/") } else { name });
        }
        // Directories first, then alphabetical — the ordering a human scans.
        names.sort_by(|a, b| {
            let (ad, bd) = (a.ends_with('/'), b.ends_with('/'));
            bd.cmp(&ad).then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });

        let count = names.len();
        let body = if names.is_empty() {
            format!("{} is empty\n", dir.display())
        } else {
            format!("{}\n", names.join("\n"))
        };

        Ok(ToolOutcome::ok(util::truncate_text(&body, MAX_SEARCH_BYTES)).with_display(ToolDisplay {
            path: Some(util::display_path(&ctx.workspace, &dir)),
            summary: Some(format!("{count} entr{}", if count == 1 { "y" } else { "ies" })),
            ..Default::default()
        }))
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert(
            "pattern".into(),
            prop("string", "Glob pattern, e.g. `**/*.rs` or `src/**/mod.rs`."),
        );
        p.insert(
            "path".into(),
            prop("string", "Directory to search under. Defaults to the workspace root."),
        );
        p.insert(
            "hidden".into(),
            prop("boolean", "Include dotfiles and files excluded by .gitignore. Defaults to false."),
        );
        ToolSpec::function(
            "glob",
            "Find files by name pattern, respecting .gitignore. Results are sorted by modification time, newest first, so recently touched files come up first.",
            object_schema(p, &["pattern"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, args: &Value) -> String {
        format!("Glob({})", arg_str_opt(args, "pattern").unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let pattern = match arg_str(&args, "pattern") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let root =
            match resolve_in_workspace(ctx, &arg_str_opt(&args, "path").unwrap_or_else(|| ".".into())) {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutcome::error(e)),
            };
        let hidden = arg_bool(&args, "hidden");

        let matcher = match globset::Glob::new(&pattern) {
            Ok(g) => g.compile_matcher(),
            Err(e) => return Ok(ToolOutcome::error(format!("invalid glob `{pattern}`: {e}"))),
        };

        // The walk is blocking; keep it off the async runtime's worker threads.
        let workspace = ctx.workspace.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(!hidden)
                .git_ignore(!hidden)
                // Honour .gitignore even outside a git repository. Without this
                // the ignore crate silently skips the file whenever there is no
                // .git directory, which includes freshly scaffolded projects.
                .require_git(false)
                .build();
            for (seen, entry) in walker.enumerate() {
                if seen > MAX_WALK_ENTRIES {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let path = entry.path();
                // Match against the workspace-relative path so patterns like
                // `src/**/*.rs` behave as the user expects.
                let rel = path.strip_prefix(&workspace).unwrap_or(path);
                if matcher.is_match(rel) || matcher.is_match(path) {
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    hits.push((path.to_path_buf(), mtime));
                }
            }
            hits.sort_by(|a, b| b.1.cmp(&a.1));
            hits
        })
        .await
        .unwrap_or_default();

        let count = found.len();
        let listing: Vec<String> =
            found.iter().map(|(p, _)| util::display_path(&ctx.workspace, p)).collect();
        let body = if listing.is_empty() {
            format!("No files match `{pattern}`\n")
        } else {
            format!("{}\n", listing.join("\n"))
        };

        Ok(ToolOutcome::ok(util::truncate_text(&body, MAX_SEARCH_BYTES))
            .with_summary(format!("{count} file{}", if count == 1 { "" } else { "s" })))
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn spec(&self) -> ToolSpec {
        let mut p = Map::new();
        p.insert(
            "pattern".into(),
            prop("string", "Rust-flavoured regular expression to search for."),
        );
        p.insert(
            "path".into(),
            prop("string", "Directory or file to search. Defaults to the workspace root."),
        );
        p.insert("glob".into(), prop("string", "Only search files matching this glob, e.g. `*.rs`."));
        p.insert("case_insensitive".into(), prop("boolean", "Ignore case. Defaults to false."));
        p.insert(
            "output_mode".into(),
            json!({
                "type": "string",
                "enum": ["content", "files_with_matches", "count"],
                "description": "`content` returns matching lines (default), `files_with_matches` returns only paths, `count` returns per-file counts."
            }),
        );
        p.insert(
            "context".into(),
            prop("integer", "Lines of context around each match. Only for output_mode=content."),
        );
        ToolSpec::function(
            "grep",
            "Search file contents by regular expression, respecting .gitignore. Prefer this over reading many files: it is the main way to find code.",
            object_schema(p, &["pattern"]),
        )
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn summarize(&self, args: &Value) -> String {
        format!("Grep({})", arg_str_opt(args, "pattern").unwrap_or_default())
    }

    async fn run(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let pattern = match arg_str(&args, "pattern") {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutcome::error(e)),
        };
        let root =
            match resolve_in_workspace(ctx, &arg_str_opt(&args, "path").unwrap_or_else(|| ".".into())) {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutcome::error(e)),
            };
        let mode = arg_str_opt(&args, "output_mode").unwrap_or_else(|| "content".into());
        let context_lines = arg_usize(&args, "context").unwrap_or(0);

        let regex = match regex::RegexBuilder::new(&pattern)
            .case_insensitive(arg_bool(&args, "case_insensitive"))
            .build()
        {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutcome::error(format!("invalid regex `{pattern}`: {e}"))),
        };

        let file_filter = match arg_str_opt(&args, "glob") {
            Some(g) => match globset::Glob::new(&g) {
                Ok(g) => Some(g.compile_matcher()),
                Err(e) => return Ok(ToolOutcome::error(format!("invalid glob: {e}"))),
            },
            None => None,
        };

        let workspace = ctx.workspace.clone();
        let mode_for_task = mode.clone();
        let (body, total_matches, files_hit) = tokio::task::spawn_blocking(move || {
            search(&root, &workspace, &regex, file_filter.as_ref(), &mode_for_task, context_lines)
        })
        .await
        .unwrap_or_else(|_| (String::from("search was interrupted"), 0, 0));

        let summary = match mode.as_str() {
            "files_with_matches" => {
                format!("{files_hit} file{}", if files_hit == 1 { "" } else { "s" })
            }
            _ => format!(
                "{total_matches} match{} in {files_hit} file{}",
                if total_matches == 1 { "" } else { "es" },
                if files_hit == 1 { "" } else { "s" }
            ),
        };

        Ok(ToolOutcome::ok(util::truncate_text(&body, MAX_SEARCH_BYTES)).with_summary(summary))
    }
}

/// Strip a leading `<spaces><digits><separator>` from every line, if every line
/// has one.
///
/// Returns `None` when the text is not uniformly prefixed, so ordinary code
/// that merely happens to start with a number is never mangled. Requiring
/// *every* line to match is what makes this safe: a single stray match cannot
/// silently rewrite the model's intent.
fn strip_line_number_prefixes(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());

    for (i, line) in lines.iter().enumerate() {
        // Tolerate a trailing blank line from a copied block.
        if line.is_empty() && i == lines.len() - 1 {
            out.push(String::new());
            continue;
        }
        let trimmed = line.trim_start_matches(' ');
        let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let rest = &trimmed[digits.len()..];
        let stripped = rest
            .strip_prefix(util::LINE_NUMBER_SEPARATOR)
            // Accept a tab too: models trained on `cat -n` output emit it, and
            // rejecting it would just cost another failed round trip.
            .or_else(|| rest.strip_prefix('\t'))?;
        out.push(stripped.to_string());
    }
    Some(out.join("\n"))
}

/// Re-indent `text` so its first line carries `indent`, shifting the rest by
/// the same amount relative to their own original indentation.
///
/// Used when recovering from a whitespace-mismatched edit: the replacement was
/// written against the model's idea of the indentation, so it has to be moved
/// onto the file's real indentation or the fix introduces the very problem it
/// was recovering from.
fn reindent(text: &str, indent: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(first) = lines.first() else { return text.to_string() };
    let original: String = first.chars().take_while(|c| *c == ' ' || *c == '\t').collect();

    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return (*line).to_string();
            }
            // Replace the model's leading whitespace with the file's, keeping
            // any deeper nesting the line had relative to the first.
            match line.strip_prefix(original.as_str()) {
                Some(rest) => format!("{indent}{rest}"),
                None => (*line).to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Locate `needle` in `haystack` ignoring each line's leading whitespace.
///
/// Returns the byte range of the *actual* text in `haystack`, so the caller
/// replaces real file content rather than the model's approximation of it.
///
/// This exists because models reconstruct indentation from memory and get it
/// wrong — inventing a tab, or normalizing four spaces to two. The change they
/// intend is unambiguous; only the whitespace is wrong. A match is accepted
/// only when it is **unique**, so this can never silently pick the wrong one of
/// several similar blocks.
fn find_ignoring_indentation(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    let needle_lines: Vec<&str> = needle.split('\n').collect();
    if needle_lines.iter().all(|l| l.trim().is_empty()) {
        return None;
    }

    // Byte offset and trimmed content of every line in the haystack.
    let mut offsets = Vec::new();
    let mut cursor = 0usize;
    for line in haystack.split('\n') {
        offsets.push((cursor, line));
        cursor += line.len() + 1; // +1 for the '\n'
    }

    let trimmed_needle: Vec<&str> = needle_lines.iter().map(|l| l.trim()).collect();
    let window = trimmed_needle.len();
    if window == 0 || window > offsets.len() {
        return None;
    }

    let mut found: Option<std::ops::Range<usize>> = None;
    for start in 0..=(offsets.len() - window) {
        let matches = (0..window).all(|k| offsets[start + k].1.trim() == trimmed_needle[k]);
        if !matches {
            continue;
        }
        // Ambiguity means give up rather than guess.
        if found.is_some() {
            return None;
        }
        let from = offsets[start].0;
        let last = &offsets[start + window - 1];
        found = Some(from..last.0 + last.1.len());
    }
    found
}

/// Blocking search worker. Returns `(rendered output, match count, file count)`.
fn search(
    root: &Path,
    workspace: &Path,
    regex: &regex::Regex,
    file_filter: Option<&globset::GlobMatcher>,
    mode: &str,
    context_lines: usize,
) -> (String, usize, usize) {
    let mut out = String::new();
    let mut total = 0usize;
    let mut files = 0usize;

    let walker =
        ignore::WalkBuilder::new(root).hidden(true).git_ignore(true).require_git(false).build();
    for (seen, entry) in walker.enumerate() {
        if seen > MAX_WALK_ENTRIES || out.len() > MAX_SEARCH_BYTES {
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(workspace).unwrap_or(path);
        if let Some(f) = file_filter {
            let name_matches =
                path.file_name().map(Path::new).is_some_and(|n| f.is_match(n));
            if !f.is_match(rel) && !name_matches {
                continue;
            }
        }

        let Ok(bytes) = std::fs::read(path) else { continue };
        if util::looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();

        let hits: Vec<usize> =
            lines.iter().enumerate().filter(|(_, l)| regex.is_match(l)).map(|(i, _)| i).collect();
        if hits.is_empty() {
            continue;
        }

        files += 1;
        total += hits.len();
        let shown = util::display_path(workspace, path);

        match mode {
            "files_with_matches" => out.push_str(&format!("{shown}\n")),
            "count" => out.push_str(&format!("{shown}: {}\n", hits.len())),
            _ => {
                for &i in &hits {
                    let lo = i.saturating_sub(context_lines);
                    let hi = (i + context_lines).min(lines.len().saturating_sub(1));
                    for (offset, line) in lines[lo..=hi].iter().enumerate() {
                        let j = lo + offset;
                        // ':' marks the match, '-' marks context — the same
                        // convention grep uses, so the model reads it correctly.
                        let sep = if j == i { ':' } else { '-' };
                        out.push_str(&format!("{shown}{sep}{}{sep}{line}\n", j + 1));
                    }
                }
            }
        }
    }

    if out.is_empty() {
        out.push_str("No matches\n");
    }
    (out, total, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx_at(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf(), CancellationToken::new())
    }

    #[tokio::test]
    async fn reads_number_lines_and_record_the_read() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        let ctx = ctx_at(dir.path());

        let out = ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("     1\u{2502}fn main() {}"), "got: {}", out.content);
        assert_eq!(out.display.summary.as_deref(), Some("2 lines"));
        assert!(
            ctx.staleness_error(&dir.path().join("a.rs")).await.is_none(),
            "reading must register the freshness stamp"
        );
    }

    #[tokio::test]
    async fn reads_page_with_offset_and_limit() {
        let dir = tempdir().unwrap();
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();

        let out = ReadFile
            .run(json!({"path": "a.txt", "offset": 3, "limit": 2}), &ctx_at(dir.path()))
            .await
            .unwrap();
        assert!(out.content.contains("     3\u{2502}line3"));
        assert!(out.content.contains("     4\u{2502}line4"));
        assert!(!out.content.contains("line5"));
        assert!(out.content.contains("offset=5"), "must tell the model how to continue");
    }

    #[tokio::test]
    async fn path_escapes_are_refused_by_every_path_taking_tool() {
        let dir = tempdir().unwrap();
        let ctx = ctx_at(dir.path());

        for out in [
            ReadFile.run(json!({"path": "../../etc/passwd"}), &ctx).await.unwrap(),
            WriteFile.run(json!({"path": "../evil.txt", "content": "x"}), &ctx).await.unwrap(),
            ListFiles.run(json!({"path": "../.."}), &ctx).await.unwrap(),
        ] {
            assert!(out.is_error, "escape must be refused");
            assert!(out.content.contains("outside the workspace"), "got: {}", out.content);
        }
    }

    #[tokio::test]
    async fn binary_files_are_refused_rather_than_mangled() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("bin"), [0x7f, b'E', b'L', b'F', 0, 0]).unwrap();
        let out = ReadFile.run(json!({"path": "bin"}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("binary"));
    }

    #[tokio::test]
    async fn writing_creates_missing_parent_directories() {
        let dir = tempdir().unwrap();
        let out = WriteFile
            .run(json!({"path": "a/b/c.txt", "content": "hi\n"}), &ctx_at(dir.path()))
            .await
            .unwrap();
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "hi\n");
    }

    #[tokio::test]
    async fn editing_requires_a_prior_read() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "let x = 1;\n").unwrap();
        let ctx = ctx_at(dir.path());

        let out = EditFile
            .run(json!({"path": "a.rs", "old_string": "1", "new_string": "2"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("has not been read"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn an_ambiguous_edit_is_refused_instead_of_guessing() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\nx\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(json!({"path": "a.rs", "old_string": "x", "new_string": "y"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("appears 2 times"), "got: {}", out.content);

        // replace_all makes the intent explicit and is accepted.
        let out = EditFile
            .run(
                json!({"path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "y\ny\n");
    }

    #[tokio::test]
    async fn an_edit_pasted_back_with_line_numbers_still_applies() {
        // Observed against the live model: it copied read_file's display prefix
        // into old_string, failed five times, then clobbered the file with
        // write_file. Recovering here costs nothing and saves the round trips.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn f() {\n    now < expiry\n}\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(
                json!({
                    "path": "a.rs",
                    "old_string": "     2\t    now < expiry",
                    "new_string": "    now <= expiry"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn f() {\n    now <= expiry\n}\n",
            "the file must not gain a stray tab"
        );
        assert!(out.content.contains("match the file exactly"), "the model is told: {}", out.content);
    }

    #[tokio::test]
    async fn an_edit_with_hallucinated_indentation_still_applies_correctly() {
        // Observed live: the model decided the file was tab-indented when it was
        // space-indented, and every exact-match edit failed.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn f() {\n    now < expiry\n}\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(
                json!({
                    "path": "a.rs",
                    "old_string": "\tnow < expiry",
                    "new_string": "\tnow <= expiry"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn f() {\n    now <= expiry\n}\n",
            "the file keeps its real indentation; no phantom tab is introduced"
        );
    }

    #[tokio::test]
    async fn a_whitespace_recovery_is_refused_when_it_would_be_ambiguous() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "  x = 1\n    x = 1\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(json!({"path": "a.rs", "old_string": "\tx = 1", "new_string": "\tx = 2"}), &ctx)
            .await
            .unwrap();

        assert!(out.is_error, "two equally good candidates must not be guessed between");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "  x = 1\n    x = 1\n",
            "the file is untouched"
        );
    }

    #[test]
    fn prefix_stripping_only_fires_when_every_line_is_numbered() {
        assert_eq!(
            strip_line_number_prefixes("     1│fn f() {\n     2│    x\n").as_deref(),
            Some("fn f() {\n    x\n")
        );
        // A tab separator is accepted too, for models trained on `cat -n`.
        assert_eq!(strip_line_number_prefixes("  1\tone").as_deref(), Some("one"));
        // Ordinary code that merely starts with a digit must be left alone.
        assert!(strip_line_number_prefixes("42\nlet x = 1;").is_none());
        assert!(strip_line_number_prefixes("     1│numbered\nnot numbered").is_none());
        assert!(strip_line_number_prefixes("no numbers here").is_none());
    }

    #[test]
    fn indentation_insensitive_search_requires_a_unique_match() {
        let file = "fn a() {\n    work()\n}\nfn b() {\n    other()\n}\n";
        let range = find_ignoring_indentation(file, "\twork()").expect("unique match");
        assert_eq!(&file[range], "    work()");

        // Two candidates: refuse rather than pick.
        let ambiguous = "  same\n    same\n";
        assert!(find_ignoring_indentation(ambiguous, "same").is_none());

        // No candidate at all.
        assert!(find_ignoring_indentation(file, "missing()").is_none());
        // Blank needles never match.
        assert!(find_ignoring_indentation(file, "   \n  ").is_none());
    }

    #[test]
    fn reindenting_moves_a_block_onto_the_files_real_indentation() {
        assert_eq!(reindent("\tone\n\t\ttwo", "    "), "    one\n    \ttwo");
        assert_eq!(reindent("no indent", "  "), "  no indent");
        // Blank lines stay blank rather than gaining trailing whitespace.
        assert_eq!(reindent("\ta\n\n\tb", "  "), "  a\n\n  b");
    }

    #[tokio::test]
    async fn code_that_looks_numbered_is_not_mangled() {
        let dir = tempdir().unwrap();
        // A real tab-indented file whose content could be mistaken for a prefix.
        std::fs::write(dir.path().join("a.txt"), "1\tone\n2\ttwo\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.txt"}), &ctx).await.unwrap();

        // An exact match must win before any stripping is attempted.
        let out = EditFile
            .run(json!({"path": "a.txt", "old_string": "1\tone", "new_string": "1\tONE"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error, "got: {}", out.content);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "1\tONE\n2\ttwo\n");
        assert!(!out.content.contains("omit them"), "no recovery should have been needed");
    }

    #[tokio::test]
    async fn a_missing_edit_target_reports_why() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "hello\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(json!({"path": "a.rs", "old_string": "goodbye", "new_string": "x"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not found"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn edits_report_diff_stats_for_the_ui() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "a\nb\nc\n").unwrap();
        let ctx = ctx_at(dir.path());
        ReadFile.run(json!({"path": "a.rs"}), &ctx).await.unwrap();

        let out = EditFile
            .run(json!({"path": "a.rs", "old_string": "b", "new_string": "B"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.display.diff_stats, Some((1, 1)));
        assert!(!out.display.diff.is_empty(), "the UI needs the rendered diff");
    }

    #[tokio::test]
    async fn glob_respects_gitignore_and_sorts_newest_first() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("old.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join("new.rs"), "").unwrap();

        let out = Glob.run(json!({"pattern": "*.rs"}), &ctx_at(dir.path())).await.unwrap();
        assert!(!out.content.contains("ignored.rs"), "gitignored files must be skipped");
        let new_at = out.content.find("new.rs").expect("new.rs present");
        let old_at = out.content.find("old.rs").expect("old.rs present");
        assert!(new_at < old_at, "newest first");
    }

    #[tokio::test]
    async fn grep_reports_path_line_and_text() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

        let out =
            Grep.run(json!({"pattern": "fn (alpha|beta)"}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.content.contains("a.rs:1:fn alpha() {}"), "got: {}", out.content);
        assert!(out.content.contains("a.rs:2:fn beta() {}"));
        assert_eq!(out.display.summary.as_deref(), Some("2 matches in 1 file"));
    }

    #[tokio::test]
    async fn grep_modes_change_the_shape_of_the_output() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "hit\nhit\n").unwrap();
        let ctx = ctx_at(dir.path());

        let files = Grep
            .run(json!({"pattern": "hit", "output_mode": "files_with_matches"}), &ctx)
            .await
            .unwrap();
        assert_eq!(files.content.trim(), "a.rs");

        let counts =
            Grep.run(json!({"pattern": "hit", "output_mode": "count"}), &ctx).await.unwrap();
        assert_eq!(counts.content.trim(), "a.rs: 2");
    }

    #[tokio::test]
    async fn an_invalid_regex_is_reported_not_panicked_on() {
        let dir = tempdir().unwrap();
        let out = Grep.run(json!({"pattern": "("}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("invalid regex"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn missing_required_arguments_produce_actionable_errors() {
        let dir = tempdir().unwrap();
        let out = ReadFile.run(json!({}), &ctx_at(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("missing required string argument `path`"));
    }

    #[tokio::test]
    async fn listing_puts_directories_first() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("afile"), "").unwrap();

        let out = ListFiles.run(json!({}), &ctx_at(dir.path())).await.unwrap();
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines[0], "zdir/", "directories sort before files");
        assert_eq!(lines[1], "afile");
    }
}
