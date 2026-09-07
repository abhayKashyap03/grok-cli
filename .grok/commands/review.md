---
description: Review the uncommitted diff for correctness bugs
argument-hint: [path]
---

Run `git diff HEAD` (restricted to $ARGUMENTS if anything was given) and review
the change.

Delegate the review itself to the `reviewer` subagent so the intermediate
reading does not fill this conversation — pass it the diff and enough context
to work without seeing our exchange.

Then report its findings, most serious first. If nothing real turned up, say so
in one line rather than padding.
