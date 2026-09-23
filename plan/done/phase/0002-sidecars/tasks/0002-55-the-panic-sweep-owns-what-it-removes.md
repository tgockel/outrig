# 0002-55 -- The panic sweep owns what it removes

## Context

`crates/outrig/src/container/mod.rs` defends container cleanup in four layers: an explicit
`Container::stop`, the `NameGuard` start guard, `Drop for Container`, and a process-wide panic
hook. 0002-39 moved every layer *except* the panic hook onto the per-attempt
`org.outrig.attempt` label, on the principle its own doc comment states: "A name is a request,
not a claim."

The hook did not move. It kept `static TRACKED: Mutex<BTreeSet<String>>` -- container *names* --
and swept them with `podman rm -f <name>`. 0002-39's `## Decisions` records the deferral
verbatim ("Both are unchanged from before this task and reachable only through a different
trigger, so they stay out of a diff that has already been through four rounds"), and filed
`plan/next/panic-sweep-removes-by-requested-name.md`. Upstream that is
<https://github.com/tgockel/outrig/issues/147>.

This is not a worry about a shape. Three concrete paths reach it:

1. **A collision.** `podman run --name N` fails when N is in use, which is an ordinary outcome.
   A panic anywhere between `NameGuard::reserve(N)` and the guard dropping made the hook
   `podman rm -f N` against whatever already held it -- another session, a stray, a container
   made by hand.
2. **A concurrent reservation.** `track` inserted and `untrack` removed by name, so two attempts
   reserving one name collapsed into one entry and whichever finished first discharged the
   other's obligation, leaving a container outrig made with nothing coming for it.
3. **A successful start, with no collision at all.** `start_named` passes `--rm`, and
   `stop_inner`'s `TimedOut`/`Failed` arms deliberately do not discharge. So after a stop whose
   removal timed out, the container is very likely gone and its name free -- and a panic in that
   window swept whatever had since taken it. The issue text does not mention this one; it fell
   out of tracing the failure paths while planning.

## Goal

Every removal outrig issues for a container it created selects that container, and a panic
arriving at any point cannot remove one outrig did not create.

## Deliverables

- **`TRACKED` is keyed by the attempt token**, `BTreeMap<attempt, name>`. The token is already
  128 random bits minted per `NameGuard::reserve`, so it *is* an obligation id and no counter
  was added. The recorded name is for `is_tracked` and diagnostics and is never a selector.
- **`removal_by_attempt(attempt)`** -- takes no name, so nothing built from it can reach a
  container this process did not create. `removal_cmd`'s `Some` arm delegates to it.
- **`NameGuard` holds no name at all**, `{ attempt, released }`. `release` is `#[must_use]` and
  leaves the registry entry standing, so the obligation changes owner rather than lapsing.
  `Drop` detaches before discharging, never the reverse.
- **`pending_removals() -> Option<Vec<Removal>>`**, and the hook replays it. `None` is "could
  not read the registry", which is not the same answer as "nothing is owed".
- **The hook cannot deadlock or spawn under the lock.** `try_lock`, poison read rather than
  refused, commands built under the lock and detached outside it.
- **The session watcher reaps by label.** Each sidecar carries `org.outrig.instance`, derived
  once as `<per-session salt>-<config name>`; `watcher::reap` filters on it through a new
  `engine::remove_by_label`, awaited so a failure is reported.
- Module header, `NameGuard` rationale, `containers.md`, and both changelogs.

## Acceptance

- Six new unit tests in `container/mod.rs` and five in `outrig-cli`, the `container/mod.rs` ones
  scoped to process-unique tokens so they cannot flake on a shared static.
- **Negative controls, run rather than assumed.** Reverting the sweep to by-name fails three
  tests; making `discharge` collapse by name fails exactly one, and it is the one written for
  that defect; putting the reap back in front of `died.cancel()` fails
  `the_primary_death_signal_does_not_wait_for_the_reaping` and nothing else.
- Nothing on the primary-death path can wait unboundedly: the signal precedes the cleanup, each
  reap is bounded, and a given-up-on or aborted reap kills its child rather than orphaning it.
- `crates/outrig/public-api.txt` and its `outrig-cli` counterpart do not move.
- The 19 tests in `tests/cancellation.rs` pass **untouched** -- the change is argv-neutral for
  every path they drive.

## Dependencies

- **Hard**: 0002-39, which built `ATTEMPT_LABEL`, `NameGuard`, and `Reissue`.
- **Before**: nothing. Deliberately not a predecessor of 0002-54 -- see `## Decisions`.

## See also

- `plan/next/panic-sweep-removes-by-requested-name.md` (removed here; this task is its record)
- `plan/next/panic-hook-sweep-is-never-driven-by-a-panic.md`
- `plan/next/removal-cmd-has-an-arm-nothing-reaches.md`
- `plan/next/a-forked-child-inherits-the-parents-cleanup-obligations.md`
- `plan/next/a-container-handle-should-hold-the-id-podman-gave-it.md` (its ruling on
  `force_remove_detached` is corrected here)

## Decisions

- **Numbered above 0002-54 although it landed before it.** The queue's rule is that a task
  depends only on lower-numbered predecessors; this one depends on 0002-39 and on nothing in
  0002-54, and 0002-54 does not depend on it. Renumbering the release was considered and
  rejected by the maintainer: 0002-52's blocker class was read as *not* forcing another
  candidate for this defect, so rc.3 still stands and the release keeps its number. What is
  true and worth stating plainly is that 0002-55 landing first breaks the "lands lowest-numbered
  first" reading of `plan/todo/README.md`, which is why it is recorded here rather than left for
  a reader to notice.

- **The attempt token is the obligation id; no new field, no new counter.** It is minted per
  reserve, is 128 random bits, and `Container` already carried it as `attempt: Option<String>` --
  which `EngineIdentity` structurally guarantees is `Some` exactly when ownership is `Owned`. So
  defect (2) is fixed by the choice of key rather than by any logic that has to be remembered.

- **`NameGuard` keeps no name, and that is the load-bearing half.** The first draft kept
  `name: Option<String>` as the released-marker, leaving `release` to `mem::take` the token --
  so `attempt` was a real token in one state and `""` in another, with a second field saying
  which. Once `Drop` reads `attempt`, that is the correlated-fields shape `EngineIdentity` was
  introduced to keep out of this module. A `released: bool` keeps the token valid in every state,
  and the guard now has no name to remove by even if someone tried.

- **`pending_removals` returns `Option`, decided by a test failure rather than by design.** It
  was a `Vec`, and `the_panic_sweep_..._never_by_name` passed alone and failed in the parallel
  suite: `try_lock` had declined because another test held the registry, and an empty `Vec` read
  as "owes nothing". The production behavior was right -- a panic hook must never wait on a lock
  -- but the type was lying about it. `None` now means "could not tell", the hook treats it as
  "sweep nothing", and the test helper spins for the real answer. 25 consecutive runs of the
  container suite were clean afterwards.

- **The watcher reaps by a CLI-minted label, not by the attempt token.** The maintainer asked for
  token-scoping. Persisting `Container`'s *attempt* token would need a new `pub fn` on
  `Container` -- there is no accessor -- which moves `public-api.txt` and trips 0002-54's first
  exit criterion, and it would make a deliberately per-attempt value durable. Stamping through
  the already-public `ContainerLaunchSpec::labels` gives the same 128-bit machine-unique
  selector for free. `LABEL_SESSION` would not have done: a session id is a timestamp plus
  sixteen bits, so two sessions starting in one second can share it.

- **One per-session salt, not a nonce per container.** A per-container random value would have
  had to be minted deep inside `sidecar_launch_base` and carried back out through two start
  functions whose return types would both have had to change. `<salt>-<config name>` is unique
  across the machine because the salt is and unique within the session because the config name
  is, and `instance_label_value` is the single place it is derived -- so what is stamped at
  launch and what is selected at reap cannot drift.

- **The instance is not an `Option`, and the session record gained no field.** Both were in the
  approved plan and both were removed during execution, for the same reason. The watcher is
  always armed by the process that started the containers -- there is no resume path anywhere in
  either crate -- so it takes its refs from the live handles, and a record field would have been
  written and never read. With no record to be missing the value, `instance: Option<String>` had
  no reachable `None`, and its by-name fallback was dead code with a test pinning it and a
  changelog sentence describing a compatibility story that did not exist. Making it a `String`
  deleted all three. The cost is that `force_remove_detached` now has no caller in the tree; it
  is public API, so it stays, and that is noted in the `a-container-handle` entry rather than
  acted on here.

- **`cancellation.rs` gained no tests, deliberately.** It is `#![cfg]`-free while `is_tracked` is
  `#[cfg(any(test, feature = "e2e"))]`, so referencing it there would break the default CI row.
  Its podman fake also keys `removed.<subject>` by container name, so it cannot tell two
  reservations of one name apart -- for the same reason podman cannot. That its 19 tests pass
  unmodified is the evidence that this change alters no argv they observe.

- **What is still not covered, stated rather than implied.** `pending_removals` proves what the
  hook would issue; nothing proves the hook issues it. That needs a re-exec-self child that
  panics with the fake `podman` on `PATH`, a pattern that exists nowhere in this repo.
  Filed as `plan/next/panic-hook-sweep-is-never-driven-by-a-panic.md`. The issue's fourth
  criterion asks for a sweep testable *without* a panic, which the extraction satisfies.

- **`pending_removals` lost its `Option` in the cleanup pass, and the test stopped spinning.**
  The `Option` was introduced to fix a flake and then defended in five lines of doc -- but the
  only production caller did `.unwrap_or_default()`, collapsing exactly the distinction the doc
  said must not be collapsed, and the only consumer that cared was a test helper spinning up to
  10,000 times to turn `None` back into `Some`. The mapping is now `removals_for(&map)` (pure)
  and `pending_removals()` (the hook's `try_lock`, `WouldBlock` -> empty). The test takes the
  blocking lock, because a test has no reason to inherit a panic hook's refusal to wait.

- **One poison policy, because the four accessors disagreed.** `track` and `discharge` skipped
  on a poisoned lock while the sweep deliberately read through it -- so after any panic
  elsewhere, obligations would silently stop being recorded *and* stop being discharged while
  the sweep went on acting. Every mutation is a single `insert` or `remove`, so there is no
  invariant for poisoning to protect; `tracked()` now states that once and all four use it.

- **The reap was serialized and gated the session's own end.** Awaiting each `podman rm -f` in
  a `for` loop put a full podman startup per sidecar in front of `died.cancel()` -- the token
  that tells the session its primary is gone -- and printed "reaping N sidecar container(s)"
  *after* the reaping. Now: announce, `join_all`, then cancel. `join_all` is already the idiom
  two functions over in this file's own sidecar start path.

- **The awaited reap was a regression, caught in review, and the order is now the contract.**
  Making the reap awaited (so a failure could be reported) put it *in front of*
  `died.cancel()` -- the token the REPL and the MCP session select on to learn their primary
  is gone. `remove_by_label` had no deadline, so one `podman rm` that never answered was a
  session that never ended, which is strictly worse than the fire-and-forget form it replaced.
  A timeout alone would not have been enough either: the raw `Command` had no `kill_on_drop`,
  so `SessionWatcher::shutdown`'s `task.abort()` would have orphaned the child rather than
  killed it.

  Three things settle it. The signal now goes first, which is safe because cleanup was never
  this task's to finish -- `teardown` stops every sidecar through its own handle and reports
  what it could not, and already carried a comment anticipating the watcher having got there
  first. Each reap is bounded by `REAP_BUDGET` and its child is `kill_on_drop`, so neither a
  wedged engine nor an aborted task leaves anything behind. And the ordering is now a seam,
  `on_primary_died`, testable without an engine or an events stream.

  `remove_by_label` consequently stopped delegating to `remove_batch`, which an earlier
  cleanup pass had merged them into. They render the same argv but no longer do the same
  thing: `clean`'s is unbounded because the sweep is what its user is waiting for, and this
  one must not hold up a session that is already ending.

- **Claims narrowed after a review caught them being false.** The doc and changelog asserted
  that no removal outrig issues for a container it created is addressed by name any more.
  `outrig clean` removes strays by name, and a stray *is* a container outrig created -- so the
  claim was wrong as written. It now covers the session-lifetime layers and says why `clean`
  is different in kind: a stray's session is over, so the per-attempt label it was started with
  died with the process that chose it. The deeper answer there is podman's own id, which
  `plan/next/a-container-handle-should-hold-the-id-podman-gave-it.md` already owns.

- **Not taken: collapsing `org.outrig.instance` into the library's attempt token.** Review
  argued the CLI's label is a parallel mechanism and that `Container::removal_selector()` would
  delete it along with the salt and its plumbing. That is the better design and it is recorded
  here rather than done, for one reason: it adds a `pub fn`, which moves
  `crates/outrig/public-api.txt` and trips `0002-54`'s first exit criterion. The maintainer's
  decision on this task was explicitly that it must not force another candidate. The conditions
  to revisit are simply "the next time the surface moves anyway".

  Two consequences of keeping the split are stated in the code rather than left implicit:
  `LABEL_INSTANCE` lives in `outrig-cli` while every other `org.outrig.*` key lives in the
  library, and `reject_reserved_labels` guards only the library's own key -- so nothing stops a
  library caller stamping this one.

- **Not taken: the `instance_salt: _` destructuring arms.** Review called them noise fixable by
  partial-move. They are the exhaustive-destructure forcing function this codebase uses
  deliberately -- two characters that make the next added field a decision rather than an
  oversight.

- **`Container::unstoppable()` was reintroducing the bug in the scaffolding.** It hardcoded
  `attempt: "outrig-test-never-created"`, one token shared by every such handle in the binary --
  so the first `discharge` would have cancelled the rest. Now `attempt_token()`.

## Verification

- `rustc 1.95.0 (59807616e 2026-04-14)`.
- `cargo test --workspace --no-fail-fast`: every binary green except three pre-existing
  failures, each confirmed on `ad59179e` with nothing applied rather than assumed.
  `network::tests::the_accept_loop_*` fail identically on the stashed tree
  (`plan/next/accept-loop-reaps-only-on-accept.md`).
  `process::process_tests::run_streamed_forwards_stderr_to_tracing` is the global
  tracing-callsite flake in `plan/next/lib-unit-test-flake.md`: it passes alone, and running
  the `process_tests` module by itself reproduces at 8 failures in 12 on pristine trunk versus
  9 in 12 here. That entry named only the one test first diagnosed, and now records that this
  is a second in the same class.
- `cargo test -p outrig --test cancellation`: 19 passed, file unmodified.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`: clean.
- A four-angle cleanup pass (reuse / simplification / efficiency / altitude) ran against the
  finished diff; what it changed and what it did not are in `## Decisions` above.
- `python3 scripts/check-public-api.py`: both snapshots `OK`.
- `cargo rustdoc -p outrig --lib`: zero warnings. Three `private_intra_doc_links` were
  introduced and removed during the work -- inlining `spawn_detached_rm` moved its links to
  `crate::supervise`, `Reissue::Safe`, and `removal_by_attempt` into a *public* doc comment.
  Same class as 0001-52 and 0002-52; all three became plain backticked text.
- `python3 scripts/audit-doc-style.py`: Width / Links / Spelling all OK.
- `kill_on_drop` checked rather than trusted: exact `sleep 60` processes counted before and
  after the bounded-removal test, zero both times.
- A code review rejected the first cut for the unbounded reap in front of `died.cancel()`; what
  that was and how it is settled is in `## Decisions`. The same review flagged a pre-existing
  timing dependency in `container_cancellation_e2e.rs`, in a file this task does not touch,
  filed as `plan/next/create-cancellation-e2e-depends-on-winning-a-poll-race.md`.
- The e2e tier (`--features outrig/e2e`) was **not** run here: podman is installed but this
  environment mounts `/run/user/1000/libpod` read-only, so the engine cannot start. 0002-53's
  `live-e2e` CI job covers it on both architectures.
