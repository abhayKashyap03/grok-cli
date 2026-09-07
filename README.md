# grok-cli

An agentic coding harness for xAI's Grok models. It reads your code, edits it,
runs your tests, and iterates — in a terminal, with permission controls you set.

Written in Rust. Single binary, no runtime.

```
┌─ conversation ───────────────────────────────────┐
│ › the token check rejects tokens at expiry, fix  │
│                                                  │
│ ● Read(src/auth.rs)                              │
│   ⏵ 142 lines                                    │
│                                                  │
│ ● Edit(src/auth.rs)                              │
│   ⏵ +1 -1                                        │
│     -    now < expiry                            │
│     +    now <= expiry                           │
│                                                  │
│ Fixed. src/auth.rs:42 used a strict comparison,  │
│ so a token was invalid at the exact expiry       │
│ instant.                                         │
└──────────────────────────────────────────────────┘
┌─ Message ────────────────────────────────────────┐
│ ▌                                                │
└──────────────────────────────────────────────────┘
 default · grok-4-1-fast-non-reasoning · 7.3k (0%)
```

## Install

```bash
git clone https://github.com/abhayKashyap03/grok-cli.git
cd grok-cli
cargo install --path .
```

Get an API key from [console.x.ai](https://console.x.ai/) and export it, or put
it in a `.env` file:

```bash
export XAI_API_KEY=...
```

Check it works:

```bash
grok doctor
```

## Use it

```bash
grok                                    # interactive
grok -p "add retry to the http client"  # one-shot, prints the answer
echo "review this diff" | grok -p -     # read the prompt from stdin
grok -c                                 # continue the last session
grok --resume a1b2c3                    # resume a specific one
```

### Interactive keys

| Key | Does |
| --- | --- |
| `Enter` | send |
| `Shift+Enter` | newline |
| `Esc` | interrupt the current turn |
| `Ctrl+C` | interrupt, or press twice to exit |
| `Shift+Tab` | cycle permission mode |
| `↑` / `↓` | move within the prompt, or recall history at the edges |
| `PageUp` / `PageDown` | scroll the transcript |
| `Tab` | complete a slash command |

### Slash commands

`/help` `/model` `/mode` `/clear` `/compact` `/context` `/cost` `/tools`
`/mcp` `/agents` `/resume` `/init` `/quit`

`/init` writes a `GROK.md` describing your project, which is loaded into every
future session in that directory.

## Permission modes

Nothing runs a command or edits a file without your say-so, unless you say so
once.

| Mode | Behaviour |
| --- | --- |
| `default` | reads run freely; edits and commands ask |
| `acceptEdits` | edits auto-approved; commands still ask |
| `plan` | read-only — the model can investigate and propose, not act |
| `bypassPermissions` | everything runs unprompted |

Approval prompts for edits show a **diff**, so you approve a change rather than
a filename. "Always allow" is scoped to the tool *and* its argument —
approving `cargo test` never approves `rm -rf /`.

## Tools

`read_file` `write_file` `edit_file` `list_files` `glob` `grep` `bash`
`bash_output` `kill_shell` `todo_write` `web_fetch`

All file access is confined to the working directory. `bash` supports
background processes for dev servers and watchers. Every output is size-capped
so one bad `grep` cannot consume your context window.

## Configuring it

`.grok/config.toml` in your project, or `~/.grok/config.toml` for every project.
Scalars override; permission rules accumulate.

```toml
model = "grok-4-1-fast-non-reasoning"
permission_mode = "default"
auto_compact_threshold = 0.85

[permissions]
allow = ["Bash(cargo test:*)", "Bash(cargo check:*)"]
deny  = ["Bash(rm -rf:*)", "Read(.env)"]
ask   = ["Bash(git push:*)"]
```

Rules are `Tool(pattern)`. `cmd:*` is a prefix match, anything else is a glob,
and a bare `Tool` matches every call. **Deny always wins** — including over
`bypassPermissions`, and including inside subagents.

Shell commands are matched whole *and* per segment, so `true; rm -rf /` cannot
slip past a rule by not starting with the denied text. Matching is still
textual, though: `rm -fr` is not `rm -rf`, and a command built from a variable
cannot be inspected. Treat deny rules as a guardrail against mistakes, not a
sandbox — `plan` mode with an explicit allow-list is the stronger control.

`grok config` prints what actually resolved.

### Extending it

**MCP servers** — borrow tools from any Model Context Protocol server:

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

**Hooks** — run a program before or after any tool call. It can veto.

```toml
[[hooks.PreToolUse]]
matcher = "bash"
command = "./scripts/audit.sh"
```

The hook gets event JSON on stdin and answers with
`{"decision": "deny", "reason": "..."}`, or just exits 2 with a reason on
stderr. A hook that crashes never becomes a silent approval.

**Subagents** — `.grok/agents/reviewer.md`, for work that would otherwise fill
the conversation with intermediate output:

```markdown
---
name: reviewer
description: Reviews a diff for correctness bugs
tools: read_file, grep, glob
---

You review code for correctness. Report only defects you can demonstrate.
```

A subagent's tools are always a subset of the parent's, so delegating can never
escape a restriction.

**Custom commands** — `.grok/commands/review.md` becomes `/review`:

```markdown
---
description: Review a file
argument-hint: <path>
---

Review $ARGUMENTS for correctness bugs. Be specific.
```

## Non-interactive use

```bash
grok -p "run the tests and fix what fails" --yes
```

The answer goes to stdout alone; progress goes to stderr. Exit codes: `0` ok,
`1` error, `2` tool limit reached, `3` everything was refused, `130`
interrupted.

Without `--yes` nothing prompts — the run stays inside your configured
permissions and stops if it needs more, rather than proceeding unsupervised.

## Project instructions

A `GROK.md` at the repo root is loaded into the system prompt every session.
Put the things a new engineer would need and could not infer: the build and
test commands, real conventions, the gotcha that has already bitten someone.
`/init` will draft one.

## Development

```bash
cargo test           # 281 tests
cargo clippy --all-targets
cargo build --release
```

Four layers: unit tests beside the code, render tests driving the real UI
against an in-memory terminal, end-to-end tests driving the real agent against
a scripted mock of the xAI API, and sandbox tests probing the security
boundaries directly.

[ARCHITECTURE.md](ARCHITECTURE.md) explains how the pieces fit and why they are
arranged that way.
