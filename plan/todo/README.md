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
- [0003 -- python](../phase/0003-python/README.md) -- an agent acts by writing Python into a
  persistent interpreter in its container rather than by calling MCP tools, and the agent loop
  moves into `outrig`. The code lands on `version/0.3.x`.

## Recently completed

- [0001 -- bootstrap](../done/phase/0001-bootstrap/README.md) -- closed 2026-06-26.

## Per-step index

### Phase 0002 -- sidecars

**The queue is empty.** Every task of this phase has landed, and the 0.2.0 release gate --
derived from an external release-readiness audit of `trunk` at `adee4f61` that returned no-go
for 0.2.0 final -- is closed by `0002-54`. The report's recommended sequencing is what the
numbering followed: security semantics first (`0002-37` and `0002-38`), then lifecycle
(`0002-39` and `0002-40`), then the public-surface changes (`0002-41` through `0002-47`) that
had to happen before the freeze, then release engineering. Each task carries its own evidence;
the report is not in the tree.

`0002-55` sits above `0002-54` in `plan/done/` although it landed first, which inverts the
"lands lowest-numbered first" rule above. That was deliberate, and its `## Decisions` records
why: the maintainer read `0002-52`'s blocker class as not forcing another candidate for issue
#147, so rc.3 stood and the release kept its number. The dependency invariant holds either way
-- `0002-55` depends on `0002-39` alone, and `0002-54` never depended on `0002-55`.

Cross-cutting notes the individual tasks carry rather than this file:

- `0002-43` and `0002-47` between them settled where rmcp stops being an implementation detail:
  an rmcp type is public only where the item exists to participate in rmcp's own machinery. What
  survives is eleven lines under `outrig::mcp_proxy` and nothing else -- the fact `0002-49`'s
  migration guide has to carry, and the invariant `crates/outrig/tests/public_api_boundary.rs`
  now holds. `0002-47` also froze `container::enter`'s two functions and sealed `IoPathExt`.
  Both `## Decisions` sections are in `plan/done/phase/0002-sidecars/tasks/`.
- `0002-46` resolved `0002-42`'s fork 3, which had deferred it: `Model` gains no `ConfigSource`,
  and a relative `[models.<n>].model-path` stays repo-root-relative -- now for the load as well as
  the existence check. It is the one documented exception to the declaring-file rule.
- `0002-48` landed the gate: `scripts/check-public-api.py`, the pins in
  `[workspace.metadata.public-api]`, and a `public-api` CI job. Regenerating under that pin moved
  no item, so `0002-41` through `0002-47` had each regenerated correctly and the whole diff was
  the `std::io::error` / `core::io::error` compiler drift plus a generated header on both files.
  The drift is now settled by the pin rather than tracked, so `0002-51` and `0002-54` could
  treat the snapshots as current.
- `0002-53` turned the e2e suite on: a `live-e2e` job runs it against a live podman on
  `ubuntu-24.04` and `ubuntu-24.04-arm` per PR, and the compile-only row is gone. The first
  execution found that the network interceptor installed **no rules at all** on nft 1.0.9 --
  `audit` recorded nothing, `filter` refused nothing -- which is a security-semantics defect
  present in rc.3 and therefore in the class `0002-52` recorded as forcing another candidate.
  `0002-54` decided on that and waived it, recording the waiver and what it costs rather than
  claiming the class was satisfied. Its `## Decisions` also carry the `outrig clean`
  false-removal fix and what the ARM row does and does not now cover.
- `0002-49` wrote only what was false *then*; `0002-54` wrote the version-bearing docs and
  folded every candidate section into one `[0.2.0]` measured against 0.1.0, because that can be
  written only once. `0002-52` -> `0002-54` was a loop: another RC would have returned to
  `0002-52`.
- `0002-52` landed the RC, strict rustdoc in CI, and a `package` job that packages both crates
  under `OUTRIG_REQUIRE_ENTER=1`, so an archive missing a source `build.rs` compiles fails
  rather than shipping. It first built two release-gate scripts for this and removed them again;
  its `## Decisions` records what `cargo publish` already covers, so they are not re-proposed.
  There is no soak window -- `0002-52` considered one and recorded why it measures nothing --
  and what `0002-54` checked instead is the blocker class recorded there. rc.3 stayed a
  library-only cut, as rc.1 and rc.2 were.
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

### Phase 0003 -- python

`0003-01` through `0003-16` cover the phase's ten user-visible deliverables. The ordering is
chosen so a person can use the thing early: `0003-01` through `0003-05` are the shortest path to
an interactive `outrig run-new`, and everything after it is additive. A first draft of this queue
was built from the phase's exit criteria instead and silently omitted discovery, history, and
observability -- the exit criteria describe the first milestone, the deliverables describe the
phase.

**The path to a running CLI**

| Task      | What it settles                                                    |
| --------- | ------------------------------------------------------------------ |
| `0003-04` | The model's only tool submits Python, and `outrig` drives a round  |
| `0003-05` | `outrig run-new` holds a session; `run-legacy` names the old one   |

**Surviving what the agent writes**

| Task      | What it settles                                                    |
| --------- | ------------------------------------------------------------------ |
| `0003-06` | A runaway execution is survivable, and the user can end one        |
| `0003-07` | An agent's memory has a ceiling that reports rather than kills     |

**How the agent is addressed, and what it can see**

| Task      | What it settles                                                    |
| --------- | ------------------------------------------------------------------ |
| `0003-08` | The user reaches the agent through a channel, not a prompt         |
| `0003-09` | `runtime.wait` mirrors `asyncio.wait` and watches the channels     |
| `0003-10` | The agent can ask what it holds and what it cannot import          |
| `0003-11` | The full conversation lives in Python; a view goes to the provider |
| `0003-12` | The view is budgeted, and promotion has settled semantics          |

**What a human can see afterwards, and finishing the loop**

| Task      | What it settles                                                    |
| --------- | ------------------------------------------------------------------ |
| `0003-13` | A session records what its agents did, by category                 |
| `0003-14` | A script renders a session directory to one page                   |
| `0003-15` | Retry and failover come across from the 0.2.x loop                 |
| `0003-16` | The docs describe the system that now exists                       |

Cross-cutting notes the individual tasks carry rather than this file:

- `0003-05` ships a CLI that passes a typed line to the model as an ordinary prompt. The `user`
  channel that replaces that arrangement is `0003-08`; the staging is deliberate, and gating the
  first runnable CLI on the channel would have bought no verification the prompt does not give.
- `0003-04` copies the minimum of the agent loop that one round needs and `0003-15` finishes it.
  Between them `run-new` is less resilient than `run`, which is a stated cost rather than an
  oversight.
- `0003-06` is not optional polish. `runtime-protection.md` records that without it the first
  `while True:` ends the session, which is why it lands immediately after the CLI that makes it
  reachable.
- The phase's deferred subjects -- credential isolation, MCP as Python objects, subagents and the
  work API -- have design pages and no tasks. They are out of scope by the phase README's own
  list, not forgotten.

## Not queued, deliberately

Regenerating `crates/outrig/public-api.txt` is each task's own deliverable rather than a task
of its own; a queued step would only be a second place to forget it. `0002-48` added the
enforcement that catches a task which forgets anyway.

The audit's gate item 8 -- "add deterministic regressions for every item above" -- is likewise
not a task. It is each task's own `## Acceptance`, for the same reason.

The component audits behind the report surfaced more than the 18-item gate. The rest is
**post-0.2.0 by decision**, not by omission; none is a release blocker, and each has, or should
get, a `plan/next/` entry.

- MCP shutdown may consume multiple grace periods and return success without a confirmed reap.
  Adjacent to `0002-39`'s cooperative-reap work; not folded in because it is a different owner.
- Post-fork use of `std::net::*::bind` is a residual async-signal-safety risk in `nsfork`.
- The panic-hook sweep removing by requested name is **no longer buffered** -- `0002-55` fixed it,
  and what that task could not reach is refiled as
  `plan/next/panic-hook-sweep-is-never-driven-by-a-panic.md`,
  `plan/next/removal-cmd-has-an-arm-nothing-reaches.md`, and
  `plan/next/a-forked-child-inherits-the-parents-cleanup-obligations.md`.
- The four-site `[security]` lowering (`plan/next/launch-spec-security-lowering.md`) is the same
  silent-drop class as `0002-41` and stays buffered: `0002-41` fixed a block that was not lowered
  at all, which was the bug; the four sites are ergonomics. It is *not* the same conversion --
  `ContainerLaunchSpec` has no network field -- and its header was corrected to say so.
- `Transcript` is a concrete public sink and therefore an extension-point commitment that `0002-47`
  does not cover.
- Dynamic sidecar add has no removal or handle-lifecycle contract.
- Device colon grammar, mount lexical normalization, and the silent Anthropic ceiling remain
  buffered. Startup-banner testing no longer does: `0002-49` pulled it in, since three of its
  documentation corrections are what the banner would have regressed. What it could not reach --
  the mid-turn failover move announcement -- is refiled as
  `plan/next/failover-move-announcement-untested.md`.
- The workspace rustdoc output-name collision now has one, filed by `0002-52` while adding the
  `cargo rustdoc` gate whose `-p` and `--lib` work around it:
  `plan/next/workspace-rustdoc-output-collision.md`.
