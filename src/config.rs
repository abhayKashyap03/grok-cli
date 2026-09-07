//! Layered configuration, model metadata, and project memory discovery.
//!
//! Settings resolve lowest-to-highest precedence:
//!
//! 1. built-in defaults
//! 2. `~/.grok/config.toml`            (user-wide)
//! 3. `<project>/.grok/config.toml`    (checked in, shared with the team)
//! 4. environment (`XAI_API_KEY`, `GROK_MODEL`, `GROK_BASE_URL`)
//! 5. command-line flags
//!
//! Later layers override earlier ones field by field, so a project file can set
//! only `model` without discarding the user's permission rules.
//!
//! ```toml
//! model = "grok-4-1-fast-non-reasoning"
//! permission_mode = "default"
//! auto_compact_threshold = 0.85
//!
//! [permissions]
//! allow = ["Bash(cargo test:*)", "Read(**)"]
//! deny  = ["Bash(rm -rf:*)"]
//!
//! [mcp_servers.filesystem]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//!
//! [[hooks.PreToolUse]]
//! matcher = "Bash"
//! command = "./scripts/audit-bash.sh"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default model. `grok-4-1-fast-non-reasoning` is the cheap, fast tool-calling
/// workhorse; reasoning variants are opt-in because they cost more per turn and
/// stream a reasoning channel the harness has to render.
pub const DEFAULT_MODEL: &str = "grok-4-1-fast-non-reasoning";

/// Directory name used for both the user-wide and project-local config roots.
pub const CONFIG_DIR_NAME: &str = ".grok";

/// Filename the agent reads for project instructions, mirroring CLAUDE.md.
pub const MEMORY_FILE_NAME: &str = "GROK.md";

// ---------------------------------------------------------------------------
// Model metadata
// ---------------------------------------------------------------------------

/// What the harness needs to know about a model to budget context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: &'static str,
    /// Total context window in tokens.
    pub context_window: u64,
    /// Whether the model streams a `reasoning_content` channel.
    pub reasoning: bool,
}

/// Known models. Unknown ids still work — [`model_info`] falls back to a
/// conservative window rather than refusing to run, because xAI ships models
/// faster than this table can be updated.
pub const MODELS: &[ModelInfo] = &[
    ModelInfo { id: "grok-4-1-fast-non-reasoning", context_window: 2_000_000, reasoning: false },
    ModelInfo { id: "grok-4-1-fast-reasoning", context_window: 2_000_000, reasoning: true },
    ModelInfo { id: "grok-4-fast-non-reasoning", context_window: 2_000_000, reasoning: false },
    ModelInfo { id: "grok-4-fast-reasoning", context_window: 2_000_000, reasoning: true },
    ModelInfo { id: "grok-4", context_window: 256_000, reasoning: true },
    ModelInfo { id: "grok-4-0709", context_window: 256_000, reasoning: true },
    ModelInfo { id: "grok-3", context_window: 131_072, reasoning: false },
    ModelInfo { id: "grok-3-mini", context_window: 131_072, reasoning: true },
    ModelInfo { id: "grok-code-fast-1", context_window: 256_000, reasoning: false },
];

/// Conservative window for models missing from [`MODELS`].
const FALLBACK_CONTEXT_WINDOW: u64 = 131_072;

pub fn model_info(id: &str) -> ModelInfo {
    MODELS.iter().copied().find(|m| m.id == id).unwrap_or(ModelInfo {
        id: "unknown",
        context_window: FALLBACK_CONTEXT_WINDOW,
        reasoning: false,
    })
}

// ---------------------------------------------------------------------------
// Permission modes
// ---------------------------------------------------------------------------

/// How aggressively the harness asks before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Reads run freely; writes and commands prompt.
    #[default]
    Default,
    /// File edits are pre-approved; commands still prompt.
    AcceptEdits,
    /// Nothing mutates. Read-only tools only — for exploration and planning.
    Plan,
    /// Everything runs unprompted. Dangerous, and the UI says so.
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "default" => Some(Self::Default),
            "acceptEdits" | "accept-edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" | "bypass-permissions" | "yolo" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Default, Self::AcceptEdits, Self::Plan, Self::BypassPermissions]
    }

    /// One-line explanation shown in the mode picker.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Default => "ask before edits and commands",
            Self::AcceptEdits => "auto-accept file edits, ask before commands",
            Self::Plan => "read-only; no edits or commands",
            Self::BypassPermissions => "run everything without asking",
        }
    }
}

// ---------------------------------------------------------------------------
// On-disk config
// ---------------------------------------------------------------------------

/// One layer of configuration as it appears in a TOML file. Every field is
/// optional so that merging can distinguish "unset" from "set to the default".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: Option<String>,
    pub auto_compact_threshold: Option<f32>,
    pub max_tool_iterations: Option<u32>,
    pub theme: Option<String>,
    #[serde(default)]
    pub permissions: Option<PermissionRules>,
    #[serde(default)]
    pub mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
    #[serde(default)]
    pub hooks: Option<BTreeMap<String, Vec<HookConfig>>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Executable to spawn. Only stdio transport is supported.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Keep the server configured but do not start it.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Regex matched against the tool name. Absent means "every tool".
    #[serde(default)]
    pub matcher: Option<String>,
    /// Shell command to run. Receives the event JSON on stdin.
    pub command: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

fn default_hook_timeout() -> u64 {
    30
}

/// Fully resolved configuration used at runtime.
#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: PermissionMode,
    /// Fraction of the context window at which compaction triggers.
    pub auto_compact_threshold: f32,
    /// Safety valve on runaway tool loops within a single user turn.
    pub max_tool_iterations: u32,
    pub theme: String,
    pub permissions: PermissionRules,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub hooks: BTreeMap<String, Vec<HookConfig>>,
    /// Directory the agent treats as the project root.
    pub workspace: PathBuf,
}

impl Config {
    pub fn context_window(&self) -> u64 {
        model_info(&self.model).context_window
    }

    /// Token count at which the agent should compact before the next request.
    pub fn compact_at(&self) -> u64 {
        (self.context_window() as f64 * f64::from(self.auto_compact_threshold)) as u64
    }
}

/// Overrides supplied on the command line. Applied last.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub max_tokens: Option<u32>,
}

/// Merge `next` over `base`, field by field.
fn merge(base: &mut ConfigFile, next: ConfigFile) {
    macro_rules! take {
        ($($field:ident),* $(,)?) => {
            $(if next.$field.is_some() { base.$field = next.$field; })*
        };
    }
    take!(
        model,
        base_url,
        max_tokens,
        temperature,
        reasoning_effort,
        permission_mode,
        auto_compact_threshold,
        max_tool_iterations,
        theme
    );

    // Collections concatenate rather than replace: a project file adding one
    // allow-rule should not silently drop the user's global rules.
    if let Some(next_perms) = next.permissions {
        let perms = base.permissions.get_or_insert_with(PermissionRules::default);
        perms.allow.extend(next_perms.allow);
        perms.deny.extend(next_perms.deny);
        perms.ask.extend(next_perms.ask);
    }
    if let Some(next_servers) = next.mcp_servers {
        base.mcp_servers.get_or_insert_with(BTreeMap::new).extend(next_servers);
    }
    if let Some(next_hooks) = next.hooks {
        let hooks = base.hooks.get_or_insert_with(BTreeMap::new);
        for (event, list) in next_hooks {
            hooks.entry(event).or_default().extend(list);
        }
    }
}

pub fn user_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(CONFIG_DIR_NAME))
}

pub fn project_config_dir(workspace: &Path) -> PathBuf {
    workspace.join(CONFIG_DIR_NAME)
}

/// Where session transcripts live for a given workspace.
pub fn sessions_dir(workspace: &Path) -> Option<PathBuf> {
    user_config_dir().map(|d| d.join("projects").join(slugify_path(workspace)))
}

/// Turn an absolute path into a filesystem-safe directory name.
///
/// Collisions are possible in principle (two paths differing only in separator
/// characters) but the slug is only a human-readable bucket; session ids are
/// UUIDs, so a collision merely co-locates two projects' transcripts.
pub fn slugify_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn read_layer(path: &Path) -> Result<Option<ConfigFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: ConfigFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

/// Resolve the effective configuration for `workspace`.
///
/// A malformed config file is a hard error: silently running with different
/// permission rules than the user wrote would be worse than refusing to start.
pub fn load(workspace: &Path, overrides: &CliOverrides) -> Result<Config> {
    let mut merged = ConfigFile::default();

    if let Some(dir) = user_config_dir()
        && let Some(layer) = read_layer(&dir.join("config.toml"))?
    {
        merge(&mut merged, layer);
    }
    if let Some(layer) = read_layer(&project_config_dir(workspace).join("config.toml"))? {
        merge(&mut merged, layer);
    }

    // Environment layer.
    if let Ok(m) = std::env::var("GROK_MODEL")
        && !m.is_empty()
    {
        merged.model = Some(m);
    }
    if let Ok(u) = std::env::var("GROK_BASE_URL")
        && !u.is_empty()
    {
        merged.base_url = Some(u);
    }

    // CLI layer.
    if let Some(m) = &overrides.model {
        merged.model = Some(m.clone());
    }
    if let Some(u) = &overrides.base_url {
        merged.base_url = Some(u.clone());
    }
    if let Some(t) = overrides.max_tokens {
        merged.max_tokens = Some(t);
    }

    let permission_mode = overrides
        .permission_mode
        .or_else(|| merged.permission_mode.as_deref().and_then(PermissionMode::parse))
        .unwrap_or_default();

    let api_key = std::env::var("XAI_API_KEY").unwrap_or_default();

    Ok(Config {
        api_key,
        model: merged.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        base_url: merged.base_url.unwrap_or_else(|| crate::api::DEFAULT_BASE_URL.to_string()),
        max_tokens: merged.max_tokens,
        temperature: merged.temperature,
        reasoning_effort: merged.reasoning_effort,
        permission_mode,
        auto_compact_threshold: merged.auto_compact_threshold.unwrap_or(0.85).clamp(0.1, 0.98),
        max_tool_iterations: merged.max_tool_iterations.unwrap_or(60).max(1),
        theme: merged.theme.unwrap_or_else(|| "dark".to_string()),
        permissions: merged.permissions.unwrap_or_default(),
        mcp_servers: merged.mcp_servers.unwrap_or_default(),
        hooks: merged.hooks.unwrap_or_default(),
        workspace: workspace.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// Project memory (GROK.md)
// ---------------------------------------------------------------------------

/// A discovered memory file and its contents.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub content: String,
}

/// Collect `GROK.md` files that apply to `workspace`.
///
/// Order is outermost-first (user-wide, then ancestors, then the workspace
/// itself) so the most specific instructions appear last in the system prompt
/// and therefore win when they conflict.
pub fn discover_memory(workspace: &Path) -> Vec<MemoryFile> {
    let mut found = Vec::new();

    if let Some(dir) = user_config_dir() {
        push_if_readable(&mut found, dir.join(MEMORY_FILE_NAME));
    }

    // Collect the ancestor chain, then reverse it so parents come first. Stop
    // at the home directory to avoid slurping unrelated files.
    let home = dirs::home_dir();
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(workspace.to_path_buf());
    while let Some(dir) = cursor {
        chain.push(dir.clone());
        if home.as_deref() == Some(dir.as_path()) {
            break;
        }
        cursor = dir.parent().map(Path::to_path_buf);
    }
    for dir in chain.into_iter().rev() {
        push_if_readable(&mut found, dir.join(MEMORY_FILE_NAME));
    }

    found
}

fn push_if_readable(out: &mut Vec<MemoryFile>, path: PathBuf) {
    if out.iter().any(|m| m.path == path) {
        return;
    }
    if let Ok(content) = std::fs::read_to_string(&path)
        && !content.trim().is_empty()
    {
        out.push(MemoryFile { path, content });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_layers_override_scalars_but_concatenate_collections() {
        let mut base: ConfigFile = toml::from_str(
            r#"
            model = "grok-3"
            [permissions]
            allow = ["Read(**)"]
            "#,
        )
        .unwrap();
        let next: ConfigFile = toml::from_str(
            r#"
            model = "grok-4"
            [permissions]
            allow = ["Bash(ls:*)"]
            "#,
        )
        .unwrap();

        merge(&mut base, next);

        assert_eq!(base.model.as_deref(), Some("grok-4"), "scalars are overridden");
        let allow = &base.permissions.as_ref().unwrap().allow;
        assert_eq!(allow, &["Read(**)", "Bash(ls:*)"], "rules accumulate, never silently drop");
    }

    #[test]
    fn an_unset_field_in_a_later_layer_does_not_clear_an_earlier_one() {
        let mut base = ConfigFile { model: Some("grok-4".into()), ..Default::default() };
        merge(&mut base, ConfigFile { temperature: Some(0.2), ..Default::default() });
        assert_eq!(base.model.as_deref(), Some("grok-4"));
        assert_eq!(base.temperature, Some(0.2));
    }

    #[test]
    fn unknown_models_get_a_conservative_context_window() {
        assert_eq!(model_info("grok-99-future").context_window, FALLBACK_CONTEXT_WINDOW);
        assert_eq!(model_info(DEFAULT_MODEL).context_window, 2_000_000);
    }

    #[test]
    fn permission_modes_round_trip_through_their_string_form() {
        for mode in PermissionMode::all() {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PermissionMode::parse("yolo"), Some(PermissionMode::BypassPermissions));
        assert_eq!(PermissionMode::parse("nonsense"), None);
    }

    #[test]
    fn slugify_produces_a_filesystem_safe_bucket_name() {
        let slug = slugify_path(Path::new("/Users/dev/projects/grok-cli"));
        assert_eq!(slug, "Users-dev-projects-grok-cli");
        assert!(!slug.starts_with('-'), "leading separator is trimmed");
    }

    #[test]
    fn unknown_config_keys_are_rejected_rather_than_ignored() {
        // A typo in a key must not silently disable the setting it was meant to
        // apply — especially not a permission setting.
        let err = toml::from_str::<ConfigFile>("permision_mode = \"plan\"").unwrap_err();
        assert!(err.to_string().contains("permision_mode"), "got: {err}");
    }
}
