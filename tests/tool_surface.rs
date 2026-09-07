//! Every built-in tool, exercised on its success path and its failure paths.
//!
//! The unit tests beside each tool cover their interesting cases. This file
//! covers the *surface*: that every registered tool has a usable schema, that
//! every one handles junk arguments without panicking, and that none can be
//! made to escape the workspace. Those are properties of the set, and a new
//! tool added without them would otherwise slip through.

use grok_cli::tools::{Tool, ToolContext, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), CancellationToken::new())
}

/// A plausible, valid argument set for each built-in.
fn happy_args(name: &str) -> Option<Value> {
    Some(match name {
        "read_file" => json!({"path": "sample.txt"}),
        "write_file" => json!({"path": "created.txt", "content": "hello\n"}),
        "edit_file" => json!({"path": "sample.txt", "old_string": "alpha", "new_string": "ALPHA"}),
        "list_files" => json!({"path": "."}),
        "glob" => json!({"pattern": "*.txt"}),
        "grep" => json!({"pattern": "alpha"}),
        "bash" => json!({"command": "echo hello"}),
        "bash_output" => json!({"id": "shell_1"}),
        "kill_shell" => json!({"id": "shell_1"}),
        "todo_write" => json!({"todos": [{"content": "a task", "status": "pending"}]}),
        // web_fetch would make a real network call; covered by its own tests.
        _ => return None,
    })
}

#[test]
fn every_tool_advertises_a_well_formed_schema() {
    for tool in ToolRegistry::with_builtins().iter() {
        let spec = tool.spec();
        let name = tool.name();

        assert_eq!(spec.kind, "function", "{name}: wrong spec kind");
        assert_eq!(spec.function.name, name, "{name}: spec name disagrees with tool name");
        assert!(
            spec.function.description.len() > 20,
            "{name}: description is too short to guide a model"
        );
        assert_eq!(
            spec.function.parameters["type"], "object",
            "{name}: parameters must be an object"
        );
        assert!(
            spec.function.parameters["properties"].is_object(),
            "{name}: properties must be an object"
        );

        let required = spec.function.parameters["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: required must be an array"));
        let properties = spec.function.parameters["properties"].as_object().unwrap();

        // Every required field must actually be declared, or the model is being
        // asked for something the schema does not describe.
        for field in required {
            let field = field.as_str().expect("required entries are strings");
            assert!(
                properties.contains_key(field),
                "{name}: requires `{field}` but does not declare it"
            );
        }

        // Every declared property needs a description; an undocumented
        // parameter gets guessed at.
        for (property, schema) in properties {
            assert!(
                schema.get("description").and_then(Value::as_str).is_some_and(|d| !d.is_empty()),
                "{name}.{property} has no description"
            );
        }
    }
}

#[tokio::test]
async fn no_tool_panics_on_empty_arguments() {
    // Tool arguments come from a model and are attacker-adjacent: a confused or
    // steered model can send anything. Every tool must answer, never panic.
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());

    for tool in ToolRegistry::with_builtins().iter() {
        let outcome = tool.run(json!({}), &ctx).await;
        let outcome = outcome.unwrap_or_else(|e| panic!("{} returned Err: {e}", tool.name()));
        // Missing required arguments is a failure the model can fix, so it must
        // come back as an error result rather than as a success.
        if !tool.spec().function.parameters["required"].as_array().unwrap().is_empty() {
            assert!(outcome.is_error, "{} accepted empty arguments", tool.name());
            assert!(!outcome.content.trim().is_empty(), "{} gave no reason", tool.name());
        }
    }
}

#[tokio::test]
async fn no_tool_panics_on_wrongly_typed_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());

    // Numbers and arrays where strings are expected, and vice versa.
    for junk in
        [json!({"path": 42}), json!({"command": []}), json!({"pattern": null}), json!(null)]
    {
        for tool in ToolRegistry::with_builtins().iter() {
            if tool.name() == "web_fetch" {
                continue; // would attempt a network call
            }
            let result = tool.run(junk.clone(), &ctx).await;
            assert!(result.is_ok(), "{} returned Err on {junk}", tool.name());
        }
    }
}

#[tokio::test]
async fn every_tool_summarizes_without_executing_anything() {
    // `summarize` renders the permission prompt. If it had side effects, the
    // prompt itself would perform the action it is asking about.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("must_not_exist.txt");

    for tool in ToolRegistry::with_builtins().iter() {
        let args = json!({
            "path": marker.to_string_lossy(),
            "command": format!("touch {}", marker.display()),
            "content": "x",
            "pattern": "x",
            "url": "https://example.invalid",
            "id": "shell_1",
        });
        let summary = tool.summarize(&args);
        assert!(!summary.trim().is_empty(), "{} produced an empty summary", tool.name());
    }

    assert!(!marker.exists(), "summarize() had a side effect");
}

#[tokio::test]
async fn every_path_taking_tool_refuses_to_escape_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(root.path().join("secret.txt"), "TOP SECRET\n").unwrap();
    let ctx = ctx(&workspace);

    for tool in ToolRegistry::with_builtins().iter() {
        let takes_path = tool.spec().function.parameters["properties"]
            .as_object()
            .unwrap()
            .contains_key("path");
        if !takes_path {
            continue;
        }

        let outcome = tool
            .run(
                json!({"path": "../secret.txt", "content": "x", "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            !outcome.content.contains("TOP SECRET"),
            "{} leaked a file outside the workspace",
            tool.name()
        );
    }

    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "TOP SECRET\n",
        "a tool wrote outside the workspace"
    );
}

#[tokio::test]
async fn every_tool_succeeds_on_its_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.txt"), "alpha\nbeta\n").unwrap();
    let ctx = ctx(dir.path());

    let registry = ToolRegistry::with_builtins();

    // Read first, so edit_file's read-before-edit guard is satisfied.
    registry.get("read_file").unwrap().run(json!({"path": "sample.txt"}), &ctx).await.unwrap();

    for tool in registry.iter() {
        let Some(args) = happy_args(tool.name()) else { continue };

        // The shell-inspection tools need a live shell id; start one first.
        if tool.name() == "bash_output" || tool.name() == "kill_shell" {
            registry
                .get("bash")
                .unwrap()
                .run(json!({"command": "echo started", "run_in_background": true}), &ctx)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let outcome = tool.run(args.clone(), &ctx).await.unwrap();
        assert!(
            !outcome.is_error,
            "{} failed on its happy path with {args}: {}",
            tool.name(),
            outcome.content
        );
        assert!(!outcome.content.trim().is_empty(), "{} returned nothing", tool.name());
    }

    // The edit actually landed.
    assert!(std::fs::read_to_string(dir.path().join("sample.txt")).unwrap().contains("ALPHA"));
    assert!(dir.path().join("created.txt").exists());
}

#[tokio::test]
async fn cancellation_is_honoured_by_a_tool_that_can_block() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx(dir.path());
    ctx.cancel.cancel();

    // A pre-cancelled context must make a long command return promptly rather
    // than run to completion.
    let started = std::time::Instant::now();
    let outcome = ToolRegistry::with_builtins()
        .get("bash")
        .unwrap()
        .run(json!({"command": "sleep 10"}), &ctx)
        .await
        .unwrap();

    assert!(outcome.is_error);
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "cancellation was ignored");
}

#[test]
fn tool_classifications_are_consistent_with_what_the_tools_do() {
    use grok_cli::tools::ToolKind;
    let registry = ToolRegistry::with_builtins();

    let expected = [
        ("read_file", ToolKind::Read),
        ("list_files", ToolKind::Read),
        ("glob", ToolKind::Read),
        ("grep", ToolKind::Read),
        ("bash_output", ToolKind::Read),
        ("write_file", ToolKind::Edit),
        ("edit_file", ToolKind::Edit),
        ("bash", ToolKind::Execute),
        ("kill_shell", ToolKind::Execute),
        ("todo_write", ToolKind::Meta),
        ("web_fetch", ToolKind::Network),
    ];

    for (name, kind) in expected {
        assert_eq!(
            registry.get(name).unwrap_or_else(|| panic!("{name} is missing")).kind(),
            kind,
            "{name} is classified wrongly, which changes when it prompts"
        );
    }

    // And every registered tool is covered by that table, so a new tool cannot
    // be added without deciding how dangerous it is.
    for tool in registry.iter() {
        assert!(
            expected.iter().any(|(n, _)| *n == tool.name()),
            "{} is registered but not classified in this test",
            tool.name()
        );
    }
}
