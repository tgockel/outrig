# 0003-64 -- A session resumes and reconciles

## Context

A session starts in one call from the builder: `start` resolves the model, starts the bindings,
launches the container, starts the interpreter and builds the primary (`crates/outrig/src/
harness/builder.rs:377-457`; `:679-693`, `assemble`), and the primary's first round opens on the
announcer's line (`crates/outrig/src/agent/orientation.rs:433-445`, `opening`) where an agent
class's child opens on `ORIENTING` (`:358-362`). `0003-58` imports a history on the builder;
`0003-43` provisions application code before construction and records its identity; `0003-42`
binds facilities by name; `0003-62`'s manifest lists the designated set, the restore obligations
and the uncertain operations; `0003-63` gives a checkpoint its identity. A host program starts a
session with `session.start` over the owner connection (`crates/outrig/src/owner/mod.rs:12-28`;
`crates/outrig/src/owner/session.rs:116-122`), and `run-new` takes its arguments in `RunNewArgs`
(`crates/outrig-cli/src/cli/run_new.rs:83-106`). Nothing resumes anything: the code map (section
4) finds no restore code in the crate.

OR-SUS6 asks that a resume "re-provisions supported application code, resources and facility
associations with fresh authority, imports history/data, and integrates [the owner's] current
application records"; that "no saved proxy or token revives an obsolete claim"; that "the world
may be ahead of saved history after a crash" and nothing is rolled back; that a
"reset/reconciliation notice" states "restored versus omitted state, lost names/handles, changed
services, outstanding obligations and uncertain effects"; that missing required data prevents a
faithful resume or gives an explicitly accepted degradation, "summary restart ... not silently
equivalent"; and that durable identities "reconcile authoritative newer facts rather than repost
questions or replay unknown actions". Its acceptance: an older save predates an answered question
and an uncertain upload; the owner's records settle the question, the model receives the upload's
uncertainty and the preserved ticket, runtime resources are fresh, the task identity is unchanged,
and the CLI follows the same mechanism.

The validation found that resume is "a new session plus import plus the reset notice
`lifecycle.md` already prescribes" (constraint 7; `lifecycle.md`, "No restart in 0.3"), which
`continuation.md`, "One continuation: a new session", absorbs from
`plan/next/interpreter-restart-with-a-reset-notice.md` as this page's rule. The adversarial
review (issue 1) requires that "the model must never be shown a replay or a silently missing
result", and names restart-from-prompt a degraded fallback that needs an explicit decision.
`continuation.md`, "Resume", designs what this task builds.

## Goal

A new session built from a package re-provisions application code, rebinds facilities by name
with fresh authority, restores designated files and data, imports history, and opens its first
round with a notice that says what came back, what did not, and what the owner's records say
happened since.

## Deliverables

- **`SessionBuilder::resume(package, reconciliation)`** (the spelling is this task's), which reads a
  package from the storage adapter by checkpoint identity or takes one given, refuses an unknown
  format version by name, validates the manifest, and makes the start run five steps in this order:
  application code re-provisioned by the identity the manifest names, through `application-code.md`,
  with a mismatch handled per fork 3; facilities, channels and MCP services the manifest names bound
  by name through `facilities.md` from what the owner supplies again, with fresh authority -- a name
  the owner does not supply again is a lost name; designated files written into the owner-supplied
  workspace from the host side before the interpreter starts, a present-and-different path handled
  per fork 2, and designated data made available to the agent per fork 4; history imported as
  `0003-58` does, before the agent is built, attached to the root's `ConstructSpec` by agent id when
  the root is owner-constructed (`0003-46`); and each external channel's registry seeded from the
  manifest before the first round (`0003-53`, `0003-65`) -- the consumed and acknowledged
  watermarks, each unread item id as awaiting redelivery under its saved delivery id, the
  received-and-unanswered inbound request ids and the outstanding outbound request ids with their
  backend references -- so the first push of an id awaiting redelivery is queued under its saved
  delivery id and a later one is a repeat, a push of an item consumed before the save is a repeat,
  `pending()` counts the window alone, the notice names the ids awaiting redelivery, and a `settle`
  for a suspended request is accepted and delivered. No proxy, client, token or socket from the
  package is used for anything.
- **The reconciliation the owner supplies**: a public value, `#[non_exhaustive]`, holding the
  facts the owner's records know that the save does not -- requests answered since, by durable
  id and with the answer where the owner has it; messages consumed since, by id; effects it knows
  completed or failed, by the ticket the agent designated; and free notes -- plus a per-fact
  source. In Rust it is a value; a host program sends it as JSON in `session.start`'s new
  `resume` field, and the engine receives it.
- **The notice**, the first round's opening (fork 1), written by the host as `ORIENTING` is and
  so a turn of history, in this order: this is a new interpreter and nothing was run again; the
  declared state restored, by path and name; the declared state omitted or unsupported, with the
  reason, and that undesignated state was never promised; the names lost -- every facility,
  channel and service not supplied again, and every variable of the old namespace; the services
  whose identity changed; the obligations outstanding -- each disposition the manifest holds,
  each uncertain operation with its ticket where one was designated; and the owner's
  reconciliation, fact by fact, as authoritative newer facts -- a question answered is stated
  with its answer and never reposted, an action uncertain is stated as uncertain and never
  replayed. The notice is bounded as a round's opening is, and a notice that would exceed the
  bound says how many entries it left out and that the manifest holds them. Draft of its first
  lines, for the task to tune:

  > [outrig] you are continuing in a new interpreter from a save taken at <time>; nothing you ran
  > before has been run again, and every variable you held is gone. Restored: `index.json`, the
  > datum `ticket`. Omitted: `scratch/` (not under a mount). Lost names: `upload_task`, `repo`
  > (binding not supplied again). Outstanding: upload `u-42` was in flight at the save and its
  > outcome is unknown; do not resend it without checking. Since the save, the owner reports:
  > question 42 was answered "yes".

- **Two lines read from `0003-58`'s continuation-metadata section**, both tasks landing before
  this one: the agent's spent model-request count, and the limit the owner set at resume through
  `0003-50`'s operation, or the saved one when it set none (OR-A4-2); and the alias member
  restored by name, or the fallback to the head when the member is no longer present
  (`0003-59`, OR-A5-2), which the owner is also told through the resume's outcome.
- **A summary restart is a different operation.** Nothing here starts a session from the prompt
  alone and calls it a resume; an owner that wants that starts a session as it always could.
- **A resume that fails ends as a start failure** -- a missing required part, a code identity
  refused, a file that cannot be written -- with what it had made stopped as `0003-29` stops an
  interrupted start, and no retry loop.
- **The Python owner**: `outrig.Session(resume=checkpoint_or_package, reconciliation=...)`, sent
  with `session.start`; the package the program holds is a Python object and JSON crosses the
  connection.
- **The CLI**: `run-new --resume <checkpoint directory>` reads the CLI's directory adapter
  (`0003-63`), builds the session through `resume` with an empty reconciliation, and shows the
  notice as the first round's opening; a directory that is not a checkpoint fails before anything
  starts, naming it.
- **Events**: `continuation.resumed` with the checkpoint identity, what was restored, omitted and
  lost, by count and name; and the notice as a bounded record. Added to `doc/reference/events.md`.
- **`lifecycle.md`'s close table** gains a row: a resume's restoration interrupted by the owner's
  abandonment of the start is cleaned up as any interrupted start is, and the workspace is left
  with whatever had been written, which the start's error names.
- **`## Decisions` records** the file-conflict policy, the code-mismatch policy, the notice's
  placement, and the restored data's surface.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **The round trip.** Session A, against a mock model, writes `index.json`, designates it with a
  datum `ticket`, saves through `0003-63`'s test adapter, and is shut down; session B resumes
  from the checkpoint: `runtime.history.turns` matches A's; `index.json` is in B's workspace with
  A's bytes before B's first execution; the datum is reachable per fork 4; and B's first round
  opens on the notice.
- **The notice says what came back and what did not.** With one designated path outside every
  mount, one binding not supplied again, one service supplied under another identity, one hosted
  call recorded uncertain with its ticket, and a reconciliation saying request 42 was answered
  "yes": the notice lists the file restored, the path omitted with its reason, the binding as a
  lost name, the service as changed, the upload as uncertain with its ticket and no instruction to
  resend, and the answer to request 42, in that order; no turn of A's is re-executed, and no
  `exec.submitted` precedes the first round.
- **Fresh authority.** A fixture binding whose factory counts constructions is at count one in
  B; no object of A's is reachable; a proxy the package would hold is not in the package.
- **The metadata lines.** A session saved with 7 of 20 model requests spent and its alias pinned on
  `b`: resumed with no new limit, the notice says 7 spent of 20; resumed with the owner's limit of
  50, it says 7 spent of 50 and the fourth request after the first three goes out; resumed with the
  alias reordered, the first call goes to `b` and the notice says so; resumed with `b` removed, the
  first call goes to the head and the notice and the outcome name the fallback.
- **The world ahead.** A workspace that already holds `index.json` with other content is handled
  as fork 2 decides, and the notice says which; nothing is rolled back.
- **Code identity.** A package naming application code whose identity the owner's supply does
  not match is handled as fork 3 decides, and the notice or the refusal names the difference.
- **Missing required data** -- a package without its history part -- fails the start naming the
  part, and nothing is left running.
- **The Python owner.** A host program resumes with `outrig.Session(resume=...)` and reads the
  same `runtime.history.turns` through its root agent's rounds against a mock model.
- **The CLI.** `run-new --resume <dir>` against a mock model opens on the notice; a directory that
  is not a checkpoint fails before any container starts.
- **Gated e2e, real containers, no model**: `run-new` saves through `/save` and a second
  `run-new --resume` restores the designated file into a fresh workspace, which is the phase's
  exit criterion for the CLI.
- **Seeded positions.** A session saved with two items unread on `inbox`, one consumed and
  unacknowledged, and one outbound request outstanding: after the resume and before any push,
  `pending()` reads 0, the channel handle lists the two unread ids as awaiting redelivery, and the
  notice names them; the first push of each is queued under its saved delivery id and `pending()`
  then reads 2, a second push of either is a repeat that queues nothing, a push of the consumed item
  is a repeat, a push of a new item is queued, and `settle` for the outstanding request is accepted
  and delivered (`0003-65`'s fixture).
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the notice goes -- Recommended: the first round's opening**, which costs context once
   and is a turn of history the model can read back; with one sentence in the preamble saying the
   session was resumed and that the first round's opening holds the notice. The alternatives are
   a user-channel message, which the model's code reads only if it chooses to, and a preamble
   section, which costs every call.
2. **A designated path that exists with other content -- Open; recommended: kept as it is and
   reported, with `overwrite` in the options to replace it.** After a crash the workspace may be
   ahead of the save, and a resume that silently replaced newer bytes would roll the world back.
   The alternative overwrites by default and reports what it replaced.
3. **A code identity that differs -- Open; recommended: refuse, with `allow_changed` in the
   options to proceed with the difference named in the notice.** OR-SUS7 prescribes no upgrade
   policy; refusing by default keeps "faithful or explicitly accepted degradation" true.
4. **Where restored data lives -- Recommended: `runtime.continuation.restored`, a read-only
   mapping by name**, beside `0003-61`'s API, named in the notice. The alternative binds each
   datum as a variable, which would collide with names the application code or the imports take.
5. **The CLI's spelling -- Recommended: `run-new --resume <path>`**, one flag on the command that
   has the session directory already. The alternative, `outrig resume`, duplicates `run-new`'s
   setup.

## Dependencies

- **Hard: `0003-42`, `0003-43`.** Facilities bound by name, and application code provisioned by
  identity.
- **Hard: `0003-58`.** The import on the builder.
- **Hard: `0003-62`, `0003-63`.** The package, its manifest, the adapter's `read` and the
  checkpoint identity to choose by.
- **Hard: `0003-19`, `0003-29`.** The builder's start and the Python owner's `session.start`.
- **Soft: `0003-46`.** The owner-constructed root re-provisioned as the root agent.
- **Soft: `0003-51`, `0003-54`.** External channels and MCP services bound again by name; without
  them the names are reported lost.
- **Soft: `0003-60`, `0003-61`.** The standing designation and `runtime.continuation`, which the
  restored data joins.
- **Soft: `0003-50`, `0003-59`.** The model-request limit set at resume and the alias member
  restored by name; without either, its notice line is absent.

## See also

- `plan/phase/0003-python/continuation.md` -- "Resume", "One continuation: a new session".
- `plan/phase/0003-python/application-code.md` and `facilities.md` -- what is provisioned
  and bound again.
- `plan/phase/0003-python/lifecycle.md` -- "The interpreter's death ends the session", "Rejected
  alternatives".
- `plan/next/interpreter-restart-with-a-reset-notice.md` -- absorbed by `continuation.md`; the
  notice's rule.
- `crates/outrig/src/harness/builder.rs`; `crates/outrig/src/agent/orientation.rs`;
  `crates/outrig/src/owner/session.rs`; `crates/outrig-cli/src/cli/run_new.rs`.
