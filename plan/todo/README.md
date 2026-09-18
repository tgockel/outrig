# Plan: the task queue

The per-step index of `plan/todo/`. A task is `PPPP-NN-short-name.md`: `PPPP` names a phase,
`NN` is its sequence within that phase. Tasks land lowest-numbered first, and a task depends
only on smaller-numbered predecessors in the same ordering. Run `/next-task` to advance one
task end-to-end on its own branch; run `/groom-plan` to maintain ordering after edits or
`plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

## Active phases

- [0002 -- sidecars](../phase/0002-sidecars/README.md) -- tools move out of the agent's
  primary image and into sidecar containers OutRig places, and the public surface of both
  crates is narrowed, sealed, and frozen for 0.2.0.

## Recently completed

- [0001 -- bootstrap](../done/phase/0001-bootstrap/README.md) -- closed 2026-06-26.

## Per-step index

### Phase 0002 -- sidecars

`0002-44` through `0002-54` are what is left of the 0.2.0 release gate, derived from an external
release-readiness audit of `trunk` at `adee4f61` that returned no-go for 0.2.0 final. The ordering
below is that report's recommended sequencing: security semantics first (`0002-37` and
`0002-38`, both landed), then lifecycle (`0002-39` and `0002-40` landed), then the
public-surface changes (`0002-41` through `0002-43` landed) that have to happen before the
freeze, then release engineering. Each task carries its own evidence; the report is not in the tree.

**Public surface, before the freeze**

| Task      | What it settles                                                     |
| --------- | ------------------------------------------------------------------- |
| `0002-44` | Every lossy tool-name sanitization is collision-resistant           |
| `0002-45` | `McpServerSpec` can build the named-sidecar entrypoint shape        |
| `0002-46` | What the deprecated local-LLM surface does in 0.2.0                 |
| `0002-47` | Narrow or explicitly freeze the rmcp-coupled and low-level surfaces |

**Release engineering**

| Task      | What it settles                                                   |
| --------- | ----------------------------------------------------------------- |
| `0002-48` | The public-API snapshots are gated, not trusted                   |
| `0002-49` | Documentation contracts, and a drafted 0.1 -> 0.2 migration guide |
| `0002-50` | A cancelled build owns the working containers buildah made for it |
| `0002-51` | `Config.Env` joins `Config.Entrypoint` and `Config.Cmd` as a read |
| `0002-52` | A fresh RC, and a packaging check that can catch a reused version |
| `0002-53` | The e2e suite runs for real, on both architectures                |
| `0002-54` | 0.2.0 ships, once rc.3's exit criteria are met                    |

Cross-cutting notes the individual tasks carry rather than this file:

- `0002-43` recorded where rmcp stops being an implementation detail -- an rmcp type is public
  only where the item exists to participate in rmcp's own machinery -- and `0002-47` applies that
  answer to the surfaces `0002-43` did not touch. It lands `ProxyServer`'s trait impl and
  `SUPPORTED_PROTOCOL_VERSIONS` on "frozen" and the three `OutrigError` variants on "narrow", so
  `0002-47` inherits a verdict rather than a principle. See its `## Decisions`, in
  `plan/done/phase/0002-sidecars/tasks/`.
- `0002-46`'s fork 2 decides whether `Model` gains path provenance; `0002-42` deferred it there.
- `0002-48` regenerates both `public-api.txt` files, so it wants to follow `0002-44` through
  `0002-47`. The `std::io::error` -> `core::io::error` compiler drift that `0002-41` left out of
  its hand-edited snapshot is absorbed: `0002-43` regenerated `outrig`'s with the tool rather than
  by hand. `outrig-cli`'s is untouched and may still carry it.
- `0002-49` writes only what is false *now*; `0002-54` writes the version-bearing docs, because
  those can be written only once. `0002-52` -> `0002-54` is a loop: another RC returns to `0002-52`.
- `0002-52` records the soak parameters before rc.3 ships; `0002-54` checks that record rather than
  composing one after the fact.
- `0002-50` is lifecycle work sitting inside the release block because it was found after the
  ordering was set: `0002-39` closed a process leak and opened a smaller engine-resource one, which
  `0002-52` should ship rather than describe. Its acceptance needs a live engine, so it shares a
  fixture with `0002-53` and runs in that harness if `0002-53` lands first.
- Gate item 17 ships **partially met**: `0002-48` enforces the API snapshots, and the downstream
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
of its own; a queued step would only be a second place to forget it. `0002-48` adds the enforcement
that catches a task which forgets anyway.

The audit's gate item 8 -- "add deterministic regressions for every item above" -- is likewise
not a task. It is each task's own `## Acceptance`, for the same reason.

The component audits behind the report surfaced more than the 18-item gate. The rest is
**post-0.2.0 by decision**, not by omission; none is a release blocker, and each has, or should
get, a `plan/next/` entry.

- MCP shutdown may consume multiple grace periods and return success without a confirmed reap.
  Adjacent to `0002-39`'s cooperative-reap work; not folded in because it is a different owner.
- Post-fork use of `std::net::*::bind` is a residual async-signal-safety risk in `nsfork`.
- The four-site `[security]` lowering (`plan/next/launch-spec-security-lowering.md`) is the same
  silent-drop class as `0002-41` and stays buffered: `0002-41` fixed a block that was not lowered
  at all, which was the bug; the four sites are ergonomics. It is *not* the same conversion --
  `ContainerLaunchSpec` has no network field -- and its header was corrected to say so.
- `Transcript` is a concrete public sink and therefore an extension-point commitment that `0002-47`
  does not cover.
- Dynamic sidecar add has no removal or handle-lifecycle contract.
- Device colon grammar, mount lexical normalization, the silent Anthropic ceiling, and
  startup-banner testing all remain buffered -- though `0002-49` pulls the banner item forward if it
  can, since three of its documentation corrections are what the banner would regress.
- Workspace rustdoc output-name collision, and local-LLM build-warning cleanliness, have no
  entry yet and should get one.
