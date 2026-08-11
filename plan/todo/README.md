# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Queue

Empty -- every numbered task through 0113 has landed.

What each one settled is recorded in its own `## Decisions` section under `plan/done/`, which
is the authoritative record. This file deliberately does not summarize them: a second copy of
a design call is one that can disagree with the first.

Follow-up work discovered mid-execution collects in `plan/next/` as unnumbered entries.
`/groom-plan` folds them into the numbered queue when there is an ordering worth maintaining
-- a dependency between two buffer entries is a note, while a dependency between two numbered
tasks is an invariant this file holds.

## Not queued, deliberately

Regenerating `crates/outrig/public-api.txt` is each task's own deliverable rather than a task
of its own; a queued step would only be a second place to forget it.
`plan/next/public-api-snapshot-gate.md` -- an opt-in check so the snapshot cannot silently rot
-- stays in the buffer as post-release process hygiene.
