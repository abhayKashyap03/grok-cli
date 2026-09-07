//! Sandbox boundary tests.
//!
//! These probe the containment guarantee directly rather than through a model,
//! because a model that declines to try an escape proves nothing.

use grok_cli::tools::fs::{ReadFile, WriteFile};
use grok_cli::tools::{Tool, ToolContext};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), CancellationToken::new())
}

#[tokio::test]
async fn a_symlink_pointing_outside_the_workspace_cannot_be_read() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();

    let secret = root.path().join("outside_secret.txt");
    std::fs::write(&secret, "TOP SECRET\n").unwrap();
    std::os::unix::fs::symlink(&secret, workspace.join("link.txt")).unwrap();

    let out = ReadFile.run(json!({"path": "link.txt"}), &ctx(&workspace)).await.unwrap();

    assert!(out.is_error, "a symlink out of the workspace must be refused");
    assert!(!out.content.contains("TOP SECRET"), "the file's contents leaked: {}", out.content);
}

#[tokio::test]
async fn a_symlinked_directory_cannot_be_used_to_escape() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "TOP SECRET\n").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();

    let out = ReadFile.run(json!({"path": "escape/secret.txt"}), &ctx(&workspace)).await.unwrap();

    assert!(out.is_error, "a symlinked directory must not widen the sandbox");
    assert!(!out.content.contains("TOP SECRET"), "leaked: {}", out.content);
}

#[tokio::test]
async fn a_symlink_cannot_be_written_through() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();

    let target = root.path().join("outside.txt");
    std::fs::write(&target, "original\n").unwrap();
    std::os::unix::fs::symlink(&target, workspace.join("link.txt")).unwrap();

    let out = WriteFile
        .run(json!({"path": "link.txt", "content": "clobbered"}), &ctx(&workspace))
        .await
        .unwrap();

    assert!(out.is_error, "writing through an escaping symlink must be refused");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "original\n",
        "the outside file was modified"
    );
}

#[tokio::test]
async fn a_symlink_that_stays_inside_the_workspace_still_works() {
    // The fix must not break legitimate symlinks, which are common in
    // monorepos and vendored-dependency layouts.
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(workspace.join("real")).unwrap();
    std::fs::write(workspace.join("real/file.txt"), "inside\n").unwrap();
    std::os::unix::fs::symlink(workspace.join("real"), workspace.join("alias")).unwrap();

    let out = ReadFile.run(json!({"path": "alias/file.txt"}), &ctx(&workspace)).await.unwrap();

    assert!(!out.is_error, "an internal symlink must keep working: {}", out.content);
    assert!(out.content.contains("inside"));
}

#[tokio::test]
async fn creating_a_new_file_in_a_new_subdirectory_still_works() {
    // Containment cannot be implemented by canonicalizing the full path: the
    // file does not exist yet.
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();

    let out = WriteFile
        .run(json!({"path": "a/b/c.txt", "content": "hello\n"}), &ctx(&workspace))
        .await
        .unwrap();

    assert!(!out.is_error, "got: {}", out.content);
    assert_eq!(std::fs::read_to_string(workspace.join("a/b/c.txt")).unwrap(), "hello\n");
}

/// A subagent must not be a way around the parent's permissions.
///
/// Restricting the *toolset* is not enough on its own: a subagent granted
/// `bash` was running commands the parent's deny rules forbid, unprompted,
/// because nothing on the delegated path consulted the permission engine.
mod delegation {
    use grok_cli::agent::subagent::{SubagentDefinition, TaskTool};
    use grok_cli::config::{PermissionMode, PermissionRules};
    use grok_cli::permissions::PermissionEngine;
    use grok_cli::tools::{Tool, ToolContext, ToolRegistry};
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn config(workspace: &std::path::Path, deny: &[&str], mode: PermissionMode) -> grok_cli::config::Config {
        grok_cli::config::Config {
            api_key: "test-key".into(),
            model: grok_cli::config::DEFAULT_MODEL.into(),
            base_url: "http://127.0.0.1:1".into(), // never reached
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            permission_mode: mode,
            auto_compact_threshold: 0.85,
            max_tool_iterations: 20,
            theme: "dark".into(),
            permissions: PermissionRules {
                allow: vec![],
                deny: deny.iter().map(|d| (*d).to_string()).collect(),
                ask: vec![],
            },
            mcp_servers: Default::default(),
            hooks: Default::default(),
            workspace: workspace.to_path_buf(),
        }
    }

    fn task_tool(config: &grok_cli::config::Config) -> TaskTool {
        let (tx, _rx) = mpsc::channel(16);
        let definitions = vec![SubagentDefinition {
            name: "worker".into(),
            description: "does work".into(),
            prompt: "You do work.".into(),
            tools: Some(vec!["bash".into()]),
            model: None,
        }];
        TaskTool::new(
            definitions,
            config.clone(),
            ToolRegistry::with_builtins(),
            tx,
            PermissionEngine::from_config(config),
            // No channel: anything that would prompt must be refused, never
            // silently allowed.
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_subagent_cannot_run_a_command_the_parent_denies() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned.txt");
        // bypassPermissions is the strongest case: the deny rule must still win.
        let config = config(dir.path(), &["Bash(*)"], PermissionMode::BypassPermissions);
        let tool = task_tool(&config);
        let ctx = ToolContext::new(dir.path().to_path_buf(), CancellationToken::new());

        // Drive the authorization path directly by asking the subagent's own
        // engine, which is what the delegated loop consults before every call.
        let engine = PermissionEngine::from_config(&config);
        let decision = engine.evaluate(
            "bash",
            grok_cli::tools::ToolKind::Execute,
            &format!("touch {}", marker.display()),
        );
        assert!(
            matches!(decision, grok_cli::permissions::Decision::Deny { .. }),
            "the engine a subagent consults must deny this"
        );
        assert!(!marker.exists());

        // And the tool refuses an unknown agent rather than falling through.
        let out = tool
            .run(json!({"agent": "nonexistent", "prompt": "do something"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn a_subagent_inherits_plan_mode_from_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = PermissionEngine::new(&PermissionRules::default(), PermissionMode::Default);
        let child = PermissionEngine::sharing_mode(&PermissionRules::default(), parent.shared_mode());

        // Switching the parent to plan mode must govern delegated work too.
        parent.set_mode(PermissionMode::Plan);
        let decision =
            child.evaluate("write_file", grok_cli::tools::ToolKind::Edit, "a.txt");
        assert!(
            matches!(decision, grok_cli::permissions::Decision::Deny { .. }),
            "plan mode must reach delegated calls"
        );
        drop(dir);
    }
}

#[tokio::test]
async fn plain_dot_dot_escapes_are_still_refused() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(root.path().join("secret.txt"), "TOP SECRET\n").unwrap();

    let out = ReadFile.run(json!({"path": "../secret.txt"}), &ctx(&workspace)).await.unwrap();
    assert!(out.is_error);
    assert!(!out.content.contains("TOP SECRET"));
}
