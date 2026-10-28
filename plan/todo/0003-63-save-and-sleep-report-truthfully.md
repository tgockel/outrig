# 0003-63 -- Save and sleep report truthfully

## Context

`0003-62` pauses a session, takes a cut and hands an immutable staged package to a storage adapter
under an identity, and releases the pause. What a session's stop reports exists: `shutdown`
returns a `ShutdownReport` whose `stopped` is `Proven` or `NotProven { reason }` and whose
`verdict` is `Clean`, `StoppedWithUnknown` or `NotProvenStopped`
(`crates/outrig/src/harness/report.rs:19-76`, `:155-164`), published as `session.report` before
`reported` (`crates/outrig/src/harness/session.rs:493-507`). The CLI's `run-new` stops on `/quit`
or EOF through `close_admission()` and `shutdown(DEFAULT_DRAIN)` and renders the report
(`crates/outrig-cli/src/cli/run_new.rs:310-330`), and its session directory holds `session.json`
and `logs/` (`crates/outrig-cli/src/session.rs:1-14`). The stream is lossy and the report is
what is authoritative (`observability.md`, "One stream, not one file per subject"; the validation's
constraint 6), so no acknowledgement can come from an event's having been published.

OR-SUS4 asks that a save and a sleep share machinery but differ: a sleep "stays quiescent through
persistence and resource release" and reports success "only after required
preservation/publication acknowledgement"; that staged capture, committed save, resource-stop
proof, failure and uncertain acknowledgement are distinguished; that "failed publication must not
silently discard the only recoverable selected state"; that a forced stop "exposes reliance on an
older save and possible loss"; and, as a recommendation, that once an immutable local cut exists
continuation during upload is allowed, with "stable checkpoint identity/order, bounded in-flight
work and monotonic backend publication so delayed older uploads cannot replace newer saves", owner
fencing staying the owner's. Its acceptance: a lost acknowledgement is resolved by stable
identity; a delayed older save cannot become the latest; cancelling preparation reopens and
explains effects rather than claiming rollback.

The validation found that "acknowledged persistence" must come from the adapter's acknowledgement
and not from an event (constraint 6), and the phase's exit criteria ask that "a delayed older
publication cannot replace a newer one" and that "the CLI saves and resumes a session through the
same mechanism an embedder uses". `continuation.md`, "Save and sleep outcomes", designs what this
task builds.

## Goal

A save and a sleep report staged, committed, stop-proven, failed or uncertain separately from
cleanup, a save continues once its staged package is immutable, checkpoints carry a stable
identity and order so an older publication cannot replace a newer one, and a forced stop says what
it relies on.

## Deliverables

- **The outcomes**, public and `#[non_exhaustive]`: `staged`, with the checkpoint identity;
  `committed`, once the adapter acknowledged it; `stop proven`, a sleep's, from the report's
  `stopped`; `failed`, with the cause -- the cut refused or degraded past policy, the adapter
  refused or failed -- and whether a staged package was kept; `uncertain acknowledgement`, with
  the identity to resolve it by. Each is reported apart from the cleanup after it.
- **Checkpoint identity and order.**
  `Checkpoint { session, sequence, since, digest, resumed_from }`: the session's id, a sequence the
  session increments per cut, the stream sequence the cut was taken at, the package's digest, and
  the identity of the checkpoint the session resumed from, if any, so a backend can order one
  durable task's checkpoints across sessions by following the lineage without owner state; two
  sessions resumed from one checkpoint are siblings the lineage cannot order, and the owner's
  fencing does. The adapter's interface gains `publish(checkpoint, package)` answering acknowledged,
  refused or failed, and `acknowledged(checkpoint)` for a lost answer; its documentation states the
  contract a backend keeps -- publish monotonically by `sequence` within a session and along a
  lineage, never let an older identity replace a newer one -- and the test adapter enforces it.
- **A save continues.** `Session::save` returns `staged` once the pause releases, with a future
  that resolves to `committed`, `failed` or `uncertain` as publication ends; saves in flight are
  bounded to one, and a save requested while one stages is coalesced into it (fork 2). A failed
  publication keeps the staged package until a newer committed one replaces it or the owner
  discards it; nothing is discarded by a failure.
- **A sleep.** `Session::sleep(options)` runs the preparation round, the pause and the cut as a
  save does, stays paused through publication, and only after the adapter acknowledges the
  committed checkpoint closes admission and runs `shutdown`, returning the checkpoint, the
  publication's outcome and the `ShutdownReport`; a sleep whose publication fails or is uncertain
  returns without closing, the session paused, and the owner decides: retry, continue, or stop.
  A sleep requested while a save publishes waits for that staging and takes its own cut.
- **Forced stop is a policy.** `close_admission()` and `shutdown` after a failed or uncertain
  sleep proceed as they always do, and the `ShutdownReport` and `session.report` gain a
  `continuation` field: the last committed checkpoint by identity, and what has happened since it
  -- the turns committed, the designated files whose digests changed, the open work -- so the
  report says what a resume from that checkpoint will not know. A session that never saved reports
  `None`.
- **An optional cadence**, which OR-SUS5 makes OutRig's policy and not an application loop: a
  builder option (fork 5) that has the session take a save every so many minutes or after so
  many rounds, off by default, each save the ordinary operation with `0003-61`'s reuse option and
  the same outcomes, coalesced with any save in flight; the last successful save, and a cadence
  save that failed or was delayed by a long execution, are observable in the assessment's facts
  and in `continuation.published`.
- **The Python owner.** `await session.save(...)` and `await session.sleep(...)` on
  `outrig.Session`, requests on the owner connection answered with the outcome as JSON; the
  adapter's `publish` and `acknowledged` reach a host program's adapter object the same way.
- **The CLI.** `run-new` gains `/save` and `/sleep`, each rendering its outcome in the terminal in
  the words above, and a directory adapter in `outrig-cli` that writes each checkpoint under the
  session directory beside `logs/` (fork 4) and publishes by renaming a staged directory into
  place, which is atomic on one filesystem; `/sleep` ends the session with the report as `/quit`
  does, and the exit status is the report's.
- **Events**: `continuation.published` with the identity and the acknowledgement; `session.report`
  gains `continuation`. Added to `doc/reference/events.md`.
- **`lifecycle.md`'s close table** gains rows: a publication in flight at the close is awaited
  until the drain deadline and reported `uncertain` if it has not answered, the staged package
  kept; a sleep waiting for acknowledgement when the owner calls `shutdown` instead is a forced
  stop and reports as one.
- **`## Decisions` records** the identity's fields, the concurrency policy, where the CLI writes,
  the forced-stop spelling, and the cadence's spelling.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **Five outcomes.** With the test adapter acknowledging, a save reports `staged` then
  `committed`; with it refusing, `failed`, the staged package kept and the session running; with
  it never answering, `uncertain` after the deadline, and `acknowledged(checkpoint)` later
  resolves it to `committed`; a sleep whose adapter acknowledges reports `committed` and then a
  report whose `stopped` is `Proven`; a cut refused for a running writer reports `failed` naming
  it, with no package.
- **Order is monotonic.** Two saves staged in order whose publications complete out of order
  leave the test adapter's latest at the higher sequence, and a publication of the lower one that
  arrives last is refused by the adapter and recorded; OutRig never publishes a lower sequence
  after a higher one it has committed.
- **Bounded and coalesced.** Three saves requested at once produce one staging, and all three
  resolve to the same checkpoint; a save requested while one publishes stages a second cut and
  no third until one of the two completes.
- **A sleep waits for acknowledgement.** With the adapter delaying its answer, the session stays
  paused -- no round opens, no submission runs -- until the answer, and `shutdown` runs only then;
  with the adapter failing, `sleep` returns `failed`, the session is paused, and a later `save`
  succeeds against a fixed adapter.
- **Forced stop exposes reliance.** After a failed sleep the owner calls `shutdown`: the report's
  `continuation` names the last committed checkpoint, counts the turns committed since it, lists
  the designated files whose digests changed, and lists the open work; `session.report` carries
  the same.
- **Cancellation reopens and explains.** A sleep abandoned during its preparation round reopens
  the session and reports the round's effects without claiming a rollback (the `0003-62` test,
  run through `sleep`).
- **The cadence.** With a cadence of two rounds, a save is taken after the second round and
  the fourth and none between; one whose cut waits on a long execution past its interval is
  reported delayed once it completes, and the assessment's last-save fact names the one that
  committed.
- **The CLI.** `/save` in a `run-new` session against a mock model writes a checkpoint directory
  under the session directory and prints `committed` with its identity; `/sleep` prints the
  outcome, stops the session, and exits with the report's status; a second `/save` writes the
  next sequence and the first stays.
- **Gated e2e, a real container, no model**: `/save` captures a designated file through the CLI's
  adapter and the directory holds it.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What `save` returns -- Recommended: `staged` at once, with a future for the publication**,
   so an owner can continue driving rounds while the upload runs, which is the recommendation's
   point. The alternative blocks until `committed`, which holds the owner's loop for the upload.
2. **Saves requested together -- Recommended: coalesced into the one staging in progress**, since
   two cuts a moment apart differ by nothing the owner asked for. The alternative refuses the
   second with the first's identity.
3. **Forced stop -- Recommended: the ordinary `close_admission()` and `shutdown`, with the
   report's `continuation` field saying what the stop relies on.** No second spelling of stopping
   is added. The alternative, `sleep(force=true)`, puts a stop inside an operation that otherwise
   never stops without acknowledgement.
4. **Where the CLI writes -- Recommended: `<session dir>/continuation/<sequence>/`**, beside
   `logs/`, the staged directory renamed into place on publication. The alternative, a user-wide
   directory, needs a layout `outrig ls` does not have.
5. **The cadence's spelling -- Open; recommended: a builder option, with `[continuation]` keys
   the CLI reads into it**, off by default, in minutes and in rounds. A key alone would leave an
   embedder with no way in but configuration; an option alone would leave the CLI none.

## Dependencies

- **Hard: `0003-62`.** The pause, the cut, the staged package and the adapter's interface.
- **Hard: `0003-19`, `0003-29`.** The report and the Python owner.
- **Soft: `0003-58`.** The history part the CLI's directory holds.

## See also

- `plan/phase/0003-python/continuation.md` -- "Save and sleep outcomes".
- `plan/phase/0003-python/lifecycle.md` -- "The report", its three rules.
- `plan/phase/0003-python/embedding.md` -- "What stays the embedder's".
- `crates/outrig/src/harness/report.rs`, `session.rs`; `crates/outrig-cli/src/cli/run_new.rs`,
  `crates/outrig-cli/src/session.rs`.
