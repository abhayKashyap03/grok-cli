# Architecture

How grok-cli is put together, and why. This is the document to read before
changing anything structural; the code has the details, this has the reasons.

## The shape

```
cli ──▶ agent ──▶ api          talk to the model
         │  └───▶ tools        act on the machine
         │  └───▶ permissions  decide whether an action is allowed
         │  └───▶ hooks        let the user veto or observe actions
         │  └───▶ mcp          borrow tools from external servers
         └──────▶ session      persist the transcript
tui  ──▶ agent                 drive it, render its events
headless ▶ agent               drive it, print its events
```

One rule holds the whole thing together: **the agent never touches the
terminal, and the terminal never touches the API.** They meet at
`agent::AgentEvent`, a flat owned enum describing what the agent is doing.

That seam is why the same loop backs both front ends, and why nearly every
component is testable without a TTY or a network. The end-to-end suite drives
the real agent against a scripted mock server; the render tests drive the real
renderer against an in-memory terminal.

## The agent loop

`agent::Agent::drive` is the centre of the program.

```
append the user message
loop {
    stream a completion
    if it made no tool calls -> done
    for each tool call:
        PreToolUse hooks    -> may deny
        permission check    -> may deny, or ask the UI
        run the tool
        PostToolUse hooks
        append the result as a `tool` message
}
```

Two invariants matter more than they look, because violating either fails on
the *next* request rather than visibly here:

1. **Every tool call gets exactly one `tool` message in reply.** Denied,
   failed, interrupted, unknown, unparseable — all of them still answer. An
   assistant message carrying an unanswered `tool_call` is rejected outright.
2. **The assistant message is appended before its tool results.** Ordering is
   part of the wire contract.

`tests/end_to_end.rs` pins both by inspecting the second request the mock
server receives, which is the only place the violation would show up.

A per-turn iteration budget (`max_tool_iterations`, default 60) stops runaway
loops. When it trips the agent says so explicitly rather than stopping quietly —
an agent that abandons a task without mentioning it is worse than one that
fails loudly.

## Streaming

`api::sse` splits into two testable halves:

- `SseDecoder` turns arbitrarily-chunked bytes into complete `data:` payloads.
  Network chunks do not respect event boundaries, so it buffers until a blank
  line. The mock server in the test suite deliberately writes 7 bytes at a time
  to keep this honest.
- `DeltaAccumulator` folds chunks into one `Completion`. The hard part is tool
  calls: `id` and `name` arrive once, then `arguments` dribbles in over many
  fragments carrying nothing but an `index`. Index is the only reliable
  correlation key, and parallel calls interleave arbitrarily.

Cancellation returns the *partial* completion rather than an error, because the
caller still needs it: any tool calls already emitted must be answered to keep
the transcript valid.

## Tools

A `Tool` bundles a JSON schema, an async `run`, and — critically — its own
`ToolKind` classification. Classification lives on the tool rather than in a
table inside the permission engine, so a new tool cannot be added without
declaring how dangerous it is.

Every filesystem tool resolves its path through one function,
`fs::resolve_in_workspace`, which is the only thing standing between a confused
model and the rest of the disk. Resolution alone is not enough — `../../etc/passwd`
resolves fine — so containment is checked explicitly. Paths are resolved
lexically rather than with `canonicalize`, which would fail for a file being
created and would resolve symlinks in ways that break the check in both
directions.

Output is bounded everywhere. A tool result goes straight into the model's
context, so an unbounded `grep` across a monorepo does not merely run slowly —
it silently costs the user their entire context window.

### Read-before-edit

`ToolContext` records a modification time whenever a file is read, and the edit
tools refuse to write a file that changed since. Without it the model happily
clobbers whatever the user changed in their editor between the read and the
write.

### Why `edit_file` recovers instead of refusing

This one was found by running the real model, not by a unit test.

`read_file` originally numbered lines as `<number><tab>`. The model took the
display tab for the file's own indentation, put a phantom tab into
`old_string`, failed five times, then fell back to `write_file` — producing a
file where every line had gained a leading tab. The task cost 27,060 tokens and
a corrupted file.

Three changes followed:

- the separator is now `│`, which cannot begin a line of source code and so
  cannot be mistaken for indentation;
- both tool descriptions state explicitly that the prefix is not file content;
- `edit_file` recovers rather than refusing. It strips a pasted-back
  line-number prefix, and failing that matches ignoring leading whitespace,
  re-indenting the replacement onto the file's real indentation.

Both recoveries require a **unique** match, so an ambiguous edit is still
refused rather than guessed. The same task afterwards: two tool calls, correct
file, 7,193 tokens.

The general lesson is worth keeping: being strict with a model's slightly-wrong
input did not make edits safer. It pushed the model toward `write_file`, which
is far more destructive. Strictness is only a virtue where the *intent* is
ambiguous.

## Permissions

Four inputs, checked in this order:

1. **deny rules** — never overridable, not even by `bypassPermissions`. If a
   user writes `deny = ["Bash(rm -rf:*)"]` they mean it, and a mode flag should
   not quietly undo it.
2. **plan mode** — refuses everything that mutates.
3. **allow rules and session grants**
4. **the mode's default for the tool's kind**

Ordering is the design. Putting deny first is what makes a deny rule a
guardrail rather than a suggestion.

Session grants ("always allow") are keyed by tool *and* argument, so approving
`cargo test` never silently approves `rm -rf /`. For commands the key is the
first two words — the granularity users actually mean, without re-prompting on
every changed flag.

Plan mode **hides** mutating tools rather than advertising and refusing them. A
tool the model can see, it will try, and burning a turn on a guaranteed refusal
helps nobody. The system prompt is generated from the same state, so it can
never claim a capability the mode has revoked.

Malformed rules are collected and reported rather than dropped: a silently
broken guardrail is worse than no guardrail.

## Sessions

Append-only JSONL under `~/.grok/projects/<workspace-slug>/<uuid>.jsonl`. One
record per line, never rewritten. That buys three things: a crash mid-turn
loses at most the turn in flight, `--resume` is a linear read with no repair
step, and a corrupt line costs one record rather than the session.

A single JSON document would have to be rewritten in full on every message and
would be unrecoverable if the process died mid-write.

`SessionStore` takes its root explicitly rather than reading `$HOME` at call
time. Reading the environment inside every list/find call makes the API
implicitly global — untestable in parallel, and surprising for any caller who
wants a session stored elsewhere.

Persistence failures are recorded on the session, not returned. Losing the
ability to resume later is not a reason to refuse to run now.

## Context compaction

When the estimated context passes `auto_compact_threshold` of the window, the
agent summarizes the older half and replaces it with one message.

Two details matter. The most recent turns are kept **verbatim** — recent
context is what the model is actively working from, and summarizing it is what
makes compaction feel like amnesia. And the split point is advanced past any
leading `tool` message, because a tool result whose assistant message was
summarized away is rejected by the API.

Token estimation is four bytes per token. A real BPE tokenizer would mean
vendoring xAI's vocabulary, which is not published. The estimate errs high on
code, which is the safe direction: over-estimating compacts slightly early,
under-estimating overflows the window. Real usage from the API replaces the
estimate as soon as the first response lands.

## Hooks

A hook is a program. It gets event JSON on stdin and may answer with JSON on
stdout; exit code 2 also means deny, with stderr as the reason, so a hook can
be a two-line shell script with no JSON handling.

The important property: a hook that crashes or times out **falls through to the
normal permission path** and never becomes a silent approval. It also does not
abort the session. An invalid matcher regex matches nothing rather than
everything, because matching everything is the dangerous reading of a typo.

Only `PreToolUse` can block. A `PostToolUse` deny would be meaningless — the
side effect already happened.

## MCP

JSON-RPC 2.0 over a child process's stdio. A single reader task demultiplexes
responses back to their waiters; without it, two concurrent requests race to
read each other's replies off the same pipe.

Discovered tools are namespaced `mcp__<server>__<tool>` so a server cannot
shadow a built-in — a server named `fs` exposing `read_file` must not silently
replace the sandboxed local one. Every MCP tool is classified `Execute`,
because the harness cannot see what a remote tool does and "assume it can do
anything" is the safe reading of unknown.

A server that fails to start is reported and skipped. A misconfigured MCP
server must not stop the harness from running.

## Subagents

Markdown definitions in `.grok/agents/`. The point is **context isolation**,
not parallelism: a search that reads thirty files to answer one question should
not leave thirty files in the parent's context.

Two properties are enforced rather than trusted:

- a subagent's tools are **intersected** with the parent's, never unioned, so
  delegation cannot be used to escape a restriction;
- `task` is excluded from the subset, so subagents cannot spawn subagents and
  a recursive definition cannot fork until the machine falls over.

Subagents run without prompting. They are confined to already-permitted tools,
and a prompt from a nested loop whose context the user cannot see is worse than
no prompt.

## The terminal

The previous implementation called blocking `event::read()` inside the render
loop, so a streamed response could not appear until the user pressed a key —
and the transcript was never drawn at all.

The loop now selects over three async sources: terminal input, agent events,
and permission requests. Agent events are `biased` first so heavy streaming
keeps flowing rather than starving behind key polling, and queued events are
drained before redrawing so a fast stream does not cost one full render per
token.

The agent sits behind an `Arc<Mutex<_>>` and a turn runs in its own task. The
UI never blocks on that lock during a turn — everything it draws is mirrored
into `App` from the event stream. That is the difference between an interface
that stays responsive under load and one that freezes whenever the model
thinks.

Other decisions worth knowing:

- **Scrolling up pins the view.** Incoming output stops chasing it until the
  user returns to the bottom. Reading scrollback while the model streams is a
  normal thing to do.
- **Shift+Tab skips `bypassPermissions`.** Turning off every safety check
  should be a deliberate `/mode` invocation, not one stray keystroke.
- **Permission prompts for edits show a diff**, so the user approves a change
  rather than a filename.
- **Markdown rendering is streaming-safe.** Unterminated code fences and
  dangling emphasis are the normal state of a half-arrived response, not
  errors, and must not swallow the rest of the output.
- **The input box is hand-rolled** because it has to compose exactly with the
  rest: Enter submits, Shift+Enter inserts a newline, and history recall only
  fires at the first and last line. Editing is character-wise, not byte-wise —
  byte indices split multi-byte characters the moment someone types an accent.
- **Frames below 4x4 are skipped.** A terminal reporting 0x0 (transient during
  resize, persistent under some multiplexers) otherwise redraws forever,
  emitting megabytes of control sequences and nothing else.

## Headless mode

`grok -p` follows pipeline conventions: the answer alone on stdout, progress
and diagnostics on stderr, distinct exit codes for error / interrupt / tool
limit / everything-refused.

Nothing prompts. Without a permission channel the agent refuses anything that
would ask, so an unattended run either stays inside its configured permissions
or stops and says what it wanted. Defaulting to yes would silently turn an
unattended run into an unsupervised one; that is what `--yes` is for.

## What was deliberately not built

- **Tree-sitter repo indexing.** The original plan called for parsing files to
  extract signatures into a context map. That is the 2023 approach. It means
  grammar crates, FFI, per-language queries and a staleness problem, to produce
  a blob the model did not ask for. Agentic `grep` and `glob` beat it for a
  fraction of the code, and the tokens go further.
- **Syntax highlighting.** Vendoring grammars for every language to colour code
  the user is about to open in their editor. Code blocks get one distinct
  colour and an indent, which is what actually aids scanning.
- **OS-level sandboxing.** Worth doing, and the natural next step. The
  workspace containment check is a real boundary but a process-level one; it
  does not stop a command the user approved from doing whatever it likes.

## Testing

264 tests, in three layers:

- **Unit tests** next to the code, covering the awkward cases directly: SSE
  frames split mid-JSON, parallel tool calls interleaved, `..` escaping the
  workspace, ambiguous edits, malformed permission rules.
- **Render tests** driving the real renderer against `ratatui::TestBackend` and
  asserting on the resulting character grid — including that a conversation
  actually appears, which is the bug the previous version shipped with.
- **End-to-end tests** driving the real agent against a scripted mock of the
  xAI API. This is where the tool-call pairing invariant is pinned, by
  inspecting what the second request actually contained.

Behaviour found only by running the real thing — the `edit_file` prefix
corruption, the 0x0 terminal spin, the missing-TTY error — has a regression
test each. Neither would have been caught by testing components in isolation,
which is the argument for doing both.
