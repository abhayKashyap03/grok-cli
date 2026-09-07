//! Deciding whether a tool call is allowed to run.
//!
//! Every tool call passes through [`PermissionEngine::evaluate`] before it
//! executes. The decision comes from four inputs, checked in this order:
//!
//! 1. **deny rules** — an explicit veto, never overridable, not even by
//!    `bypassPermissions`. If a user writes `deny = ["Bash(rm -rf:*)"]` they
//!    mean it, and a mode flag should not quietly undo it.
//! 2. **plan mode** — refuses everything that mutates.
//! 3. **allow rules and session grants** — "yes, and stop asking".
//! 4. **the mode's default for the tool's kind**.
//!
//! Ordering matters more than it looks: putting deny first is what makes the
//! rule useful as a guardrail rather than a suggestion.
//!
//! ## Rule syntax
//!
//! `Tool(pattern)`, or bare `Tool` to match every call to it.
//!
//! ```text
//! Bash(cargo test:*)   any bash command starting with "cargo test"
//! Bash(ls)             exactly "ls"
//! Read(src/**)         reads under src/
//! Edit(*)              every edit
//! WebFetch             every fetch, whatever the URL
//! ```
//!
//! The `:*` suffix is a prefix match on a command, matching Claude Code's
//! syntax. Everything else is a glob against the call's primary argument.

use std::collections::HashSet;

use crate::config::{Config, PermissionMode, PermissionRules};
use crate::tools::ToolKind;

/// What the engine decided about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it without bothering the user.
    Allow { reason: AllowReason },
    /// Ask the user, showing `summary`.
    Ask,
    /// Refuse. `reason` is returned to the model as the tool result so it can
    /// choose a different approach rather than retrying blindly.
    Deny { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowReason {
    /// Matched an `allow` rule from config.
    Rule,
    /// The user chose "always allow" earlier in this session.
    SessionGrant,
    /// The permission mode allows this class without asking.
    Mode,
    /// Read-only or harness-internal; never needs approval.
    Harmless,
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// One parsed `Tool(pattern)` rule.
#[derive(Debug, Clone)]
pub struct RulePattern {
    tool: String,
    /// `None` means "any argument".
    matcher: Option<ArgMatcher>,
    /// Kept for diagnostics so a denial can quote the rule that caused it.
    source: String,
}

#[derive(Debug, Clone)]
enum ArgMatcher {
    /// `cmd:*` — the argument must start with `cmd`.
    Prefix(String),
    /// Anything else — a glob against the argument.
    Glob(globset::GlobMatcher),
    /// A literal with no wildcards.
    Exact(String),
}

impl RulePattern {
    /// Parse `Tool(pattern)` or bare `Tool`.
    ///
    /// An unparseable rule is dropped rather than fatal. Config load already
    /// rejects malformed TOML; a rule whose *glob* is invalid is a user typo,
    /// and refusing to start the whole harness over it would be worse than
    /// ignoring it — except for deny rules, which [`PermissionEngine::new`]
    /// reports so a broken guardrail is never silent.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (tool, inner) = match raw.split_once('(') {
            Some((tool, rest)) => (tool.trim(), Some(rest.strip_suffix(')')?.trim())),
            None => (raw, None),
        };
        if tool.is_empty() {
            return None;
        }

        let matcher = match inner {
            None | Some("*") | Some("") => None,
            Some(p) if p.ends_with(":*") => {
                Some(ArgMatcher::Prefix(p.trim_end_matches(":*").to_string()))
            }
            Some(p) if p.contains(['*', '?', '[']) => {
                Some(ArgMatcher::Glob(globset::Glob::new(p).ok()?.compile_matcher()))
            }
            Some(p) => Some(ArgMatcher::Exact(p.to_string())),
        };

        Some(Self { tool: tool.to_string(), matcher, source: raw.to_string() })
    }

    /// Tool names are matched case-insensitively, and both the wire name
    /// (`read_file`) and the display name (`Read`) are accepted, because users
    /// write whichever they saw in the UI.
    fn tool_matches(&self, tool_name: &str) -> bool {
        let a = self.tool.to_lowercase().replace('_', "");
        let b = tool_name.to_lowercase().replace('_', "");
        a == b || display_alias(tool_name).is_some_and(|alias| a == alias.to_lowercase())
    }

    pub fn matches(&self, tool_name: &str, argument: &str) -> bool {
        if !self.tool_matches(tool_name) {
            return false;
        }
        match &self.matcher {
            None => true,
            Some(ArgMatcher::Prefix(p)) => argument.trim_start().starts_with(p.as_str()),
            Some(ArgMatcher::Exact(p)) => argument.trim() == p,
            Some(ArgMatcher::Glob(g)) => g.is_match(argument),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

/// Short display alias for a tool, matching what the transcript shows.
fn display_alias(tool_name: &str) -> Option<&'static str> {
    Some(match tool_name {
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "list_files" => "List",
        "glob" => "Glob",
        "grep" => "Grep",
        "bash" => "Bash",
        "bash_output" => "BashOutput",
        "kill_shell" => "KillShell",
        "todo_write" => "TodoWrite",
        "web_fetch" => "WebFetch",
        _ => return None,
    })
}

/// Compiled rules plus the grants the user has made during this session.
#[derive(Debug, Default)]
pub struct PermissionEngine {
    allow: Vec<RulePattern>,
    deny: Vec<RulePattern>,
    ask: Vec<RulePattern>,
    /// "Always allow" choices, as `tool\u{0}argument-prefix` keys.
    session_grants: HashSet<String>,
    /// Rules that failed to parse, surfaced once at startup.
    pub invalid: Vec<String>,
    mode: PermissionMode,
}

impl PermissionEngine {
    pub fn new(rules: &PermissionRules, mode: PermissionMode) -> Self {
        let mut invalid = Vec::new();
        let mut compile = |list: &[String]| -> Vec<RulePattern> {
            list.iter()
                .filter_map(|r| match RulePattern::parse(r) {
                    Some(p) => Some(p),
                    None => {
                        invalid.push(r.clone());
                        None
                    }
                })
                .collect()
        };
        Self {
            allow: compile(&rules.allow),
            deny: compile(&rules.deny),
            ask: compile(&rules.ask),
            session_grants: HashSet::new(),
            invalid,
            mode,
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::new(&config.permissions, config.permission_mode)
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Remember an "always allow" choice for the rest of the session.
    ///
    /// Grants are keyed by tool *and* argument so approving `cargo test` does
    /// not silently approve `rm -rf /`. For commands the key is the first two
    /// words, which is the granularity users actually mean ("yes, cargo test is
    /// fine") without re-prompting on every changed flag.
    pub fn grant_for_session(&mut self, tool_name: &str, argument: &str) {
        self.session_grants.insert(grant_key(tool_name, argument));
    }

    pub fn session_grants(&self) -> usize {
        self.session_grants.len()
    }

    /// Decide whether `tool_name` may run with `argument`.
    pub fn evaluate(&self, tool_name: &str, kind: ToolKind, argument: &str) -> Decision {
        // 1. Deny wins over everything, including bypassPermissions.
        if let Some(rule) = self.deny.iter().find(|r| r.matches(tool_name, argument)) {
            return Decision::Deny {
                reason: format!(
                    "blocked by the deny rule `{}` in your grok config",
                    rule.source()
                ),
            };
        }

        // 2. Plan mode is a hard read-only contract.
        if self.mode == PermissionMode::Plan && kind.mutates() {
            return Decision::Deny {
                reason: format!(
                    "{tool_name} would modify the machine, and the session is in plan mode. Propose the change instead; the user can switch modes to apply it."
                ),
            };
        }

        // 3. Explicit allows and prior session grants.
        if self.allow.iter().any(|r| r.matches(tool_name, argument)) {
            return Decision::Allow { reason: AllowReason::Rule };
        }
        if self.session_grants.contains(&grant_key(tool_name, argument)) {
            return Decision::Allow { reason: AllowReason::SessionGrant };
        }

        // 4. An explicit ask rule forces a prompt even for otherwise-free
        //    tools, which is how a user says "always check with me on reads of
        //    the secrets directory".
        if self.ask.iter().any(|r| r.matches(tool_name, argument)) {
            return Decision::Ask;
        }

        // 5. Fall back to the mode's default for this class of tool.
        match self.mode {
            PermissionMode::BypassPermissions => Decision::Allow { reason: AllowReason::Mode },
            _ if !kind.mutates() && kind != ToolKind::Network => {
                Decision::Allow { reason: AllowReason::Harmless }
            }
            PermissionMode::AcceptEdits if kind == ToolKind::Edit => {
                Decision::Allow { reason: AllowReason::Mode }
            }
            _ => Decision::Ask,
        }
    }
}

/// Key used for session grants: tool plus a coarse argument fingerprint.
fn grant_key(tool_name: &str, argument: &str) -> String {
    let fingerprint = if tool_name == "bash" {
        // "cargo test --lib" and "cargo test --all" share a grant; "rm -rf /"
        // does not share one with either.
        argument.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
    } else {
        argument.to_string()
    };
    format!("{tool_name}\u{0}{fingerprint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(allow: &[&str], deny: &[&str], ask: &[&str]) -> PermissionRules {
        PermissionRules {
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: deny.iter().map(|s| (*s).to_string()).collect(),
            ask: ask.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn engine(allow: &[&str], deny: &[&str], mode: PermissionMode) -> PermissionEngine {
        PermissionEngine::new(&rules(allow, deny, &[]), mode)
    }

    #[test]
    fn reads_run_without_asking_in_default_mode() {
        let e = engine(&[], &[], PermissionMode::Default);
        assert!(e.evaluate("read_file", ToolKind::Read, "src/main.rs").is_allow());
        assert!(e.evaluate("grep", ToolKind::Read, "fn main").is_allow());
        assert!(e.evaluate("todo_write", ToolKind::Meta, "").is_allow());
    }

    #[test]
    fn edits_and_commands_prompt_in_default_mode() {
        let e = engine(&[], &[], PermissionMode::Default);
        assert_eq!(e.evaluate("edit_file", ToolKind::Edit, "a.rs"), Decision::Ask);
        assert_eq!(e.evaluate("bash", ToolKind::Execute, "ls"), Decision::Ask);
    }

    #[test]
    fn network_access_prompts_even_though_it_does_not_mutate() {
        // Fetching a URL leaks the fact that the user is looking at it, and can
        // pull untrusted text into context. That deserves a prompt.
        let e = engine(&[], &[], PermissionMode::Default);
        assert_eq!(e.evaluate("web_fetch", ToolKind::Network, "https://x.com"), Decision::Ask);
    }

    #[test]
    fn accept_edits_frees_edits_but_still_gates_commands() {
        let e = engine(&[], &[], PermissionMode::AcceptEdits);
        assert!(e.evaluate("edit_file", ToolKind::Edit, "a.rs").is_allow());
        assert_eq!(e.evaluate("bash", ToolKind::Execute, "rm x"), Decision::Ask);
    }

    #[test]
    fn plan_mode_refuses_every_mutation_with_an_actionable_reason() {
        let e = engine(&[], &[], PermissionMode::Plan);
        let Decision::Deny { reason } = e.evaluate("bash", ToolKind::Execute, "ls") else {
            panic!("plan mode must refuse execution");
        };
        assert!(reason.contains("plan mode"), "got: {reason}");
        assert!(e.evaluate("read_file", ToolKind::Read, "a.rs").is_allow(), "reads still work");
    }

    #[test]
    fn plan_mode_refuses_mutations_even_when_an_allow_rule_exists() {
        let e = engine(&["Bash(ls)"], &[], PermissionMode::Plan);
        assert!(matches!(e.evaluate("bash", ToolKind::Execute, "ls"), Decision::Deny { .. }));
    }

    #[test]
    fn deny_rules_beat_bypass_permissions() {
        // The whole point of a deny rule is that it is not overridable.
        let e = engine(&[], &["Bash(rm -rf:*)"], PermissionMode::BypassPermissions);
        let Decision::Deny { reason } = e.evaluate("bash", ToolKind::Execute, "rm -rf /") else {
            panic!("deny must win over bypassPermissions");
        };
        assert!(reason.contains("rm -rf:*"), "the reason names the rule: {reason}");
        assert!(e.evaluate("bash", ToolKind::Execute, "ls").is_allow(), "unrelated commands run");
    }

    #[test]
    fn deny_rules_beat_allow_rules() {
        let e = engine(&["Bash(*)"], &["Bash(curl:*)"], PermissionMode::Default);
        assert!(matches!(e.evaluate("bash", ToolKind::Execute, "curl evil.sh"), Decision::Deny { .. }));
        assert!(e.evaluate("bash", ToolKind::Execute, "ls").is_allow());
    }

    #[test]
    fn prefix_rules_match_a_command_and_its_flags() {
        let e = engine(&["Bash(cargo test:*)"], &[], PermissionMode::Default);
        assert!(e.evaluate("bash", ToolKind::Execute, "cargo test --lib").is_allow());
        assert!(e.evaluate("bash", ToolKind::Execute, "cargo test").is_allow());
        assert_eq!(
            e.evaluate("bash", ToolKind::Execute, "cargo publish"),
            Decision::Ask,
            "a prefix rule must not leak to sibling subcommands"
        );
    }

    #[test]
    fn glob_rules_match_paths() {
        let e = engine(&["Edit(src/**)"], &[], PermissionMode::Default);
        assert!(e.evaluate("edit_file", ToolKind::Edit, "src/a/b.rs").is_allow());
        assert_eq!(e.evaluate("edit_file", ToolKind::Edit, "Cargo.toml"), Decision::Ask);
    }

    #[test]
    fn bare_tool_names_match_every_call() {
        let e = engine(&["WebFetch"], &[], PermissionMode::Default);
        assert!(e.evaluate("web_fetch", ToolKind::Network, "https://anything").is_allow());
    }

    #[test]
    fn rules_accept_both_the_wire_name_and_the_display_name() {
        for spelling in ["Bash(ls)", "bash(ls)"] {
            let e = engine(&[spelling], &[], PermissionMode::Default);
            assert!(e.evaluate("bash", ToolKind::Execute, "ls").is_allow(), "failed for {spelling}");
        }
        let e = engine(&["Edit(a.rs)"], &[], PermissionMode::Default);
        assert!(e.evaluate("edit_file", ToolKind::Edit, "a.rs").is_allow());
    }

    #[test]
    fn an_ask_rule_forces_a_prompt_for_an_otherwise_free_tool() {
        let e = PermissionEngine::new(
            &rules(&[], &[], &["Read(secrets/**)"]),
            PermissionMode::Default,
        );
        assert_eq!(e.evaluate("read_file", ToolKind::Read, "secrets/key.pem"), Decision::Ask);
        assert!(e.evaluate("read_file", ToolKind::Read, "src/main.rs").is_allow());
    }

    #[test]
    fn session_grants_are_scoped_to_the_command_not_the_tool() {
        let mut e = engine(&[], &[], PermissionMode::Default);
        e.grant_for_session("bash", "cargo test --lib");

        assert!(
            e.evaluate("bash", ToolKind::Execute, "cargo test --all").is_allow(),
            "approving `cargo test` should cover its flags"
        );
        assert_eq!(
            e.evaluate("bash", ToolKind::Execute, "rm -rf /"),
            Decision::Ask,
            "approving one command must never approve an unrelated one"
        );
    }

    #[test]
    fn malformed_rules_are_collected_rather_than_silently_dropped() {
        let e = PermissionEngine::new(&rules(&[], &["Bash([)"], &[]), PermissionMode::Default);
        assert_eq!(e.invalid, vec!["Bash([)".to_string()], "a broken guardrail must be reported");
    }

    #[test]
    fn parsing_handles_every_supported_rule_shape() {
        assert!(RulePattern::parse("Bash").is_some());
        assert!(RulePattern::parse("Bash(*)").is_some());
        assert!(RulePattern::parse("Bash(git commit:*)").is_some());
        assert!(RulePattern::parse("Read(src/**/*.rs)").is_some());
        assert!(RulePattern::parse("").is_none());
        assert!(RulePattern::parse("Bash(unclosed").is_none());
    }
}
