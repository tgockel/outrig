# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Queue

`0116`-`0129` are the 0.2.0 release gate, derived from an external release-readiness audit of
`trunk` at `adee4f61` that returned no-go for 0.2.0 final. The ordering below is that report's
recommended sequencing: security semantics first (`0114`-`0115`, both landed), then lifecycle,
then the public-surface changes that have to happen before the freeze, then release engineering. Each task carries its own evidence;
the report is not in the tree.

**Process and attachment lifecycle**

| Task   | What it settles                                                     |
| ------ | ------------------------------------------------------------------- |
| `0116` | An internally owned subprocess dies when its future is dropped      |
| `0117` | Interceptor attach rolls back; detach ends every bridge it started  |

**Public surface, before the freeze**

| Task   | What it settles                                                     |
| ------ | ------------------------------------------------------------------- |
| `0118` | `LaunchSpec::from_config` lowers the network policy it was handed   |
| `0119` | A provenance-bearing path and its base directory move together      |
| `0120` | MCP content is canonical data, or the reduction is written down     |
| `0121` | Every lossy tool-name sanitization is collision-resistant           |
| `0122` | `McpServerSpec` can build the named-sidecar entrypoint shape        |
| `0123` | What the deprecated local-LLM surface does in 0.2.0                 |
| `0124` | Narrow or explicitly freeze the rmcp-coupled and low-level surfaces |

**Release engineering**

| Task   | What it settles                                                     |
| ------ | ------------------------------------------------------------------- |
| `0125` | The public-API snapshots are gated, not trusted                     |
| `0126` | Documentation contracts, and a drafted 0.1 -> 0.2 migration guide   |
| `0127` | A fresh RC, and a packaging check that can catch a reused version   |
| `0128` | The e2e suite runs for real, on both architectures                  |
| `0129` | 0.2.0 ships, once rc.3's exit criteria are met                      |

Cross-cutting notes the individual tasks carry rather than this file:

- `0120` records where rmcp stops being an implementation detail, and `0124` applies that answer
  to the surfaces `0120` does not touch. The decision sits in `0120` because the queue runs one
  task at a time and `0120` is the first that has to commit; neutral content types do not on
  their own decouple the rmcp types in `OutrigError`.
- `0119`'s fork 3 and `0123`'s fork 2 both decide whether `Model` gains path provenance.
- `0125` regenerates both `public-api.txt` files, so it wants to follow `0118`-`0124`.
- `0126` writes only what is false *now*; `0129` writes the version-bearing docs, because those
  can be written only once. `0127` -> `0129` is a loop: another RC returns to `0127`.
- `0127` records the soak parameters before rc.3 ships; `0129` checks that record rather than
  composing one after the fact.
- Gate item 17 ships **partially met**: `0125` enforces the API snapshots, and the downstream
  runtime-core surface test (`plan/next/container-surface-test.md`) is a deliberate, recorded
  exception rather than an omission.

This file deliberately does not summarize what each task decides. What one settled is recorded in
its own `## Decisions` section under `plan/done/`, which is the authoritative record: a second
copy of a design call is one that can disagree with the first.

Follow-up work discovered mid-execution collects in `plan/next/` as unnumbered entries.
`/groom-plan` folds them into the numbered queue when there is an ordering worth maintaining
-- a dependency between two buffer entries is a note, while a dependency between two numbered
tasks is an invariant this file holds.

## Not queued, deliberately

Regenerating `crates/outrig/public-api.txt` is each task's own deliverable rather than a task
of its own; a queued step would only be a second place to forget it. `0125` adds the enforcement
that catches a task which forgets anyway.

The audit's gate item 8 -- "add deterministic regressions for every item above" -- is likewise
not a task. It is each task's own `## Acceptance`, for the same reason.

The component audits behind the report surfaced more than the 18-item gate. The rest is
**post-0.2.0 by decision**, not by omission; none is a release blocker, and each has, or should
get, a `plan/next/` entry.

- MCP shutdown may consume multiple grace periods and return success without a confirmed reap.
  Adjacent to `0116`'s cooperative-reap work; not folded in because it is a different owner.
- Post-fork use of `std::net::*::bind` is a residual async-signal-safety risk in `nsfork`.
- The four-site `[security]` lowering (`plan/next/launch-spec-security-lowering.md`) is the same
  silent-drop class as `0118` and stays buffered: `0118` fixes a block that is not lowered at
  all, which is the bug; the four sites are ergonomics.
- `Transcript` is a concrete public sink and therefore an extension-point commitment that `0124`
  does not cover.
- Dynamic sidecar add has no removal or handle-lifecycle contract.
- Device colon grammar, mount lexical normalization, the silent Anthropic ceiling, and
  startup-banner testing all remain buffered -- though `0126` pulls the banner item forward if it
  can, since three of its documentation corrections are what the banner would regress.
- Workspace rustdoc output-name collision, and local-LLM build-warning cleanliness, have no
  entry yet and should get one.
