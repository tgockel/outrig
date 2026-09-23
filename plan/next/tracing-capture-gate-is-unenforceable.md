# The tracing capture gate depends on an obligation nothing checks

## Context

`crates/outrig/src/process_tests.rs` has three tests that install a thread-local capturing
subscriber and assert on what it recorded. `tracing` keeps callsite state process-globally, so
those tests capture nothing when another test in the same binary reaches the same callsites
concurrently -- measured at 6 failures in 40 runs of the compiled lib binary before the gate,
always `run_streamed_forwards_stderr_to_tracing`.

`TRACING_CALLSITES` closes it: observers take an `RwLock`'s write side, emitters the read side.
That works, and the rate is 0 in 250+ runs. What it does not do is make itself true.

## The residual

Correctness now rests on *every* subscriber-less caller of `try_capture_logged*`,
`run_capture_logged*` and `run_streamed` taking `emitting()` -- including the ones that reach
those callsites several frames down, through a production helper, where nothing at the call site
suggests tracing is involved. Nothing enforces it. It was found and re-found by review:

| Round        | Missed emitters                                                              |
|--------------|------------------------------------------------------------------------------|
| First gate   | 5 direct callers in `process_tests.rs` -- all that was guarded                |
| Review 1     | 24 `network` tests via `run_step`, 2 `container` tests via `Container::stop`  |
| Review 2     | 1 more `container` test via `stop_or_keep`, 4 `outrig_` tests via `unwound`   |

**Measurement cannot find these.** The gate as it stood after round one measured 0 failures in
140 runs while 5 emitters were still unguarded. The exposure is real and fires far below any rate
a local run count would establish, which is exactly the property that makes an obligation-based
design the wrong one to leave standing.

A `#[cfg(test)]` guard inside the production helpers is not the answer and must not be tried: an
observer holds the write side *while calling those same helpers*, so a read acquired underneath
it deadlocks.

## Goal

The capture tests stop depending on what every other test in the binary remembers to do.

## Sketch

Give them a process of their own, which is the only arrangement where the global state is not
shared. They assert on `pub(crate)` items, so an integration test binary cannot reach them as
things stand. Options, cheapest first:

- **A test-only feature that exposes the seam**, the shape `outrig-cli` already uses for
  `internal-test-api`. The snapshot is generated without it, so `public-api.txt` does not move.
- **Assert on a seam that is not global tracing.** `run_streamed` and `try_capture_logged_until`
  could take a sink the test supplies, leaving `tracing` as one implementation of it. Larger, and
  it changes production signatures for a test's benefit, which wants its own argument.

Either way the gate and its obligation come out, along with `run_step_gated` and the nine
scattered `emitting()` calls.

## Why it is filed rather than done

Both options widen or reshape a frozen surface, and `0002-54` gates on `public-api.txt` not
moving since rc.3. The gate holds in the meantime and its comment carries the measurements.

## See also

- `plan/next/relocate-unit-shaped-tests.md` -- the same "unit tests that want to be integration
  tests" shape, in `outrig-cli`.
- `plan/next/test-helper-consolidation.md` -- other `init_tracing` / subscriber duplication.
