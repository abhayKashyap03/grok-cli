//! Every slash command, exercised through the real dispatch path.
//!
//! Written after `/agents` shipped printing "is unavailable right now": the
//! command dispatched to a `Show` placeholder, and because a placeholder is a
//! perfectly valid `Show`, nothing anywhere noticed. Five commands were broken
//! the same way.
//!
//! The rule these tests enforce is therefore not "each command works" but the
//! stronger "no command resolves to something that only *looks* like an
//! answer".

use grok_cli::commands::{self, CommandAction};

/// Phrases that mean the harness gave up rather than answered.
const PLACEHOLDER_MARKERS: [&str; 6] = [
    "unavailable",
    "not implemented",
    "todo",
    "coming soon",
    "could not be produced",
    "has no report",
];

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn every_builtin_command_dispatches_to_a_real_action() {
    let dir = workspace();

    for command in commands::builtins() {
        let action = commands::dispatch(&format!("/{}", command.name), dir.path());

        assert!(
            !matches!(action, CommandAction::Unknown(_)),
            "/{} is advertised in the palette but dispatches to Unknown",
            command.name
        );

        if let CommandAction::Show(body) = &action {
            let lowered = body.to_lowercase();
            for marker in PLACEHOLDER_MARKERS {
                assert!(
                    !lowered.contains(marker),
                    "/{} returned a placeholder rather than an answer: {body}",
                    command.name
                );
            }
            assert!(!body.trim().is_empty(), "/{} returned an empty body", command.name);
        }
    }
}

#[test]
fn state_dependent_commands_ask_the_front_end_rather_than_faking_an_answer() {
    // These five cannot be answered without the running agent. They must say so
    // structurally — as a Report the front end has to handle — not by returning
    // prose that reads like a result.
    let dir = workspace();

    for name in ["context", "cost", "tools", "mcp", "agents"] {
        let action = commands::dispatch(&format!("/{name}"), dir.path());
        assert!(
            matches!(&action, CommandAction::Report(n) if n == name),
            "/{name} must dispatch to Report, got {action:?}"
        );
    }
}

#[test]
fn every_advertised_command_is_reachable_by_its_own_name() {
    // A command in the palette that dispatch does not recognise is dead UI.
    let dir = workspace();
    for command in commands::all(dir.path()) {
        let action = commands::dispatch(&format!("/{}", command.name), dir.path());
        assert!(
            !matches!(action, CommandAction::Unknown(_)),
            "/{} appears in the palette but is not dispatchable",
            command.name
        );
    }
}

#[test]
fn every_command_has_a_description_and_a_sensible_display_form() {
    for command in commands::builtins() {
        assert!(!command.description.trim().is_empty(), "/{} has no description", command.name);
        assert!(
            command.display().starts_with(&format!("/{}", command.name)),
            "/{} renders as {:?}",
            command.name,
            command.display()
        );
        assert!(
            !command.name.contains(char::is_whitespace),
            "/{} contains whitespace and could never be typed",
            command.name
        );
    }
}

#[test]
fn command_names_are_unique() {
    let dir = workspace();
    let mut names: Vec<String> = commands::all(dir.path()).into_iter().map(|c| c.name).collect();
    let before = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), before, "duplicate command names would shadow each other");
}

#[test]
fn help_documents_every_command_that_exists() {
    let dir = workspace();
    let CommandAction::Show(help) = commands::dispatch("/help", dir.path()) else {
        panic!("/help must produce text");
    };

    for command in commands::all(dir.path()) {
        assert!(help.contains(&format!("/{}", command.name)), "/help omits /{}", command.name);
    }
}

#[test]
fn aliases_resolve_to_the_same_action_as_their_primary_name() {
    let dir = workspace();
    for (alias, primary) in
        [("/q", "/quit"), ("/exit", "/quit"), ("/new", "/clear"), ("/?", "/help")]
    {
        let a = commands::dispatch(alias, dir.path());
        let b = commands::dispatch(primary, dir.path());
        assert_eq!(
            std::mem::discriminant(&a),
            std::mem::discriminant(&b),
            "{alias} should behave like {primary}"
        );
    }
}

#[test]
fn arguments_are_accepted_by_the_commands_that_take_them() {
    let dir = workspace();

    assert_eq!(
        commands::dispatch("/model grok-4", dir.path()),
        CommandAction::SetModel("grok-4".into())
    );
    assert_eq!(
        commands::dispatch("/mode acceptEdits", dir.path()),
        CommandAction::SetMode(grok_cli::config::PermissionMode::AcceptEdits)
    );

    // Extra arguments on a command that takes none must not break it.
    assert!(matches!(commands::dispatch("/clear extra words", dir.path()), CommandAction::Clear));
}

#[test]
fn leading_and_trailing_whitespace_does_not_break_dispatch() {
    let dir = workspace();
    assert!(matches!(commands::dispatch("   /help   ", dir.path()), CommandAction::Show(_)));
    assert!(matches!(commands::dispatch("/quit  ", dir.path()), CommandAction::Quit));
}

#[test]
fn a_lone_slash_is_not_treated_as_a_command() {
    let dir = workspace();
    // The palette opens on "/" in the UI; dispatch should not claim it works.
    assert!(matches!(commands::dispatch("/", dir.path()), CommandAction::Unknown(_)));
}

#[test]
fn completion_offers_only_real_commands() {
    let dir = workspace();
    let all: Vec<String> = commands::all(dir.path()).into_iter().map(|c| c.name).collect();

    for prefix in ["/", "/c", "/m", "/t"] {
        for suggestion in commands::complete(prefix, dir.path()) {
            assert!(
                all.contains(&suggestion.name),
                "completion offered /{}, which is not a real command",
                suggestion.name
            );
        }
    }
}
