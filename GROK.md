# grok-cli

An agentic coding harness for xAI's Grok models. Rust, single binary.

Read [ARCHITECTURE.md](ARCHITECTURE.md) before changing anything structural —
it has the reasoning behind the layering, which the code does not repeat.

## Commands

```bash
cargo test                        # 281 tests; run this before every commit
cargo clippy --all-targets        # must stay clean
cargo build
cargo test --test end_to_end      # agent against a scripted mock API
cargo test --test sandbox         # security boundaries
./target/debug/grok-cli doctor    # verify XAI_API_KEY and the endpoint
```

The build and clippy are both warning-free. Do not commit with either
outstanding.

## Layout

`src/api/` wire types, SSE decoding, HTTP. `src/tools/` the Tool trait and
every built-in. `src/agent/` the loop, prompt assembly, subagents.
`src/tui/` terminal interface. `src/{permissions,session,hooks,mcp,commands}.rs`
are each one self-contained concern. `src/cli.rs` wires it all together.

## Conventions this codebase actually follows

- **Tests live beside the code** in `#[cfg(test)] mod tests`, and test names are
  full sentences describing the behaviour being pinned — `deny_rules_beat_
  bypass_permissions`, not `test_permissions_3`. Assertions carry a message
  saying why the property matters.
- **Comments explain *why*, never *what*.** A comment restating the code is
  noise; a comment recording the trade-off or the bug that motivated the line
  is the point. Several exist specifically because a live model or a real
  terminal broke the obvious implementation — do not delete those without
  understanding what they are protecting against.
- **Tool errors are returned, not propagated.** `Tool::run` returns
  `Ok(ToolOutcome::error(...))` for anything the model could recover from. A
  hard `Err` aborts the turn and strands an unanswered tool call.
- **Nothing in `agent/` may reference `tui/`.** The two meet only at
  `AgentEvent`. Breaking this makes the loop untestable.

## Non-obvious things that will bite you

- **Every tool call must produce exactly one `tool` message.** Denied, failed,
  interrupted, unknown — all still answer. Miss one and the *next* API request
  fails with an unhelpful 400, far from the actual mistake.
- **`read_file` prefixes lines with `<number>│`, not a tab.** A tab there
  caused a live model to mistake the separator for the file's indentation and
  corrupt files via `write_file`. `edit_file` has a deliberate recovery ladder
  for near-miss `old_string` values; both rungs require a *unique* match.
- **Deny rules are checked before everything, including `bypassPermissions`.**
  That ordering is the feature. Do not "simplify" it. Commands are matched
  whole *and* per shell segment; compound commands are never grantable.
- **Subagents go through the permission engine too.** Restricting a subagent's
  toolset is not a boundary on its own — an earlier version did only that, and
  delegation escaped every deny rule and plan mode. If you touch `subagent.rs`,
  keep `authorize` on the path.
- **The sandbox needs both a lexical and a physical check.** Lexical alone
  misses symlinks; canonicalizing the whole path breaks new-file creation.
- **The TUI must never lock the agent to change mode.** A turn holds that mutex
  for its whole duration; taking it from a key handler freezes the event loop,
  including the Esc that would cancel the turn.
- **`SessionStore` takes its root explicitly.** It used to read `$HOME` inside
  every call, which made three tests race under parallel execution. Do not
  reintroduce implicit global state.
- **Compaction must not split an assistant message from its tool results.**
  The split point is advanced past any leading `tool` message for this reason.
- **The TUI skips frames below 4x4.** A terminal reporting 0x0 otherwise
  redraws forever and emits megabytes of control sequences.

## Testing philosophy

Four layers, and each later one exists because the earlier ones missed real
bugs: unit tests, render tests against `ratatui::TestBackend`, end-to-end tests
against a mock API that splits SSE frames at awkward byte boundaries, and
`tests/sandbox.rs`, which probes security boundaries directly.

If you write a doc comment claiming a security property, write the test too. A
review found four bypasses that all lived in the gap between passing component
tests and a property nobody had asserted.

Behaviour discovered by running the real binary — against the live model, or in
a real pseudo-terminal — gets a regression test. If you fix something found
that way, add one.
