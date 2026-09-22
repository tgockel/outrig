# `outrig clean`'s removal check sits below its own test seam

## Context

0002-53 fixed `outrig clean` claiming to have removed containers it had not
(`plan/next/clean-batch-removal-fidelity.md`). The fix works and is measured, but it was
installed in the wrong place.

`execute_with` (`crates/outrig-cli/src/cli/clean.rs`) is deliberately parameterized over its
engine reads -- it takes `running`, `labeled`, and `build` as arguments so the sweep is testable
without podman. The verification reaches around that: `still_present` calls
`list_all_containers()` directly from inside the removal hook, on the production side of the
seam. So the batch -> list -> retry -> list sequence has no test. The one test of the new
behavior (`clean_reports_a_stray_the_engine_still_has`) fakes the hook's *return value*, which
exercises the reporting and none of the mechanism behind it.

Two further consequences:

- **The hook's contract leaks a strategy.** `D: Future<Output = Result<Vec<String>>>` -- "the
  names still present" -- is an implementation detail of one removal approach promoted into a
  signature twelve tests depend on. It also broke symmetry with the build-container hook, which
  still returns `Result<()>`; that asymmetry is the whole reason `no_build_removals` exists
  beside `no_removals`.
- **The retry discards its errors** (`let _ = engine::remove_batch(...)`), which is the only
  place in the file that drops an engine error, and it is invisible from the call site.

## There is a cheaper signal, already paid for and thrown away

`engine::remove_batch` runs `.output()`, so podman's stdout is captured -- and then dropped.
`podman rm` prints one line per container it actually acted on, and a name it did not act on is
simply absent. Measured: `podman rm -f <nonexistent>` exits 0 and prints nothing. That is exactly
the "exited zero having skipped one" discriminator the current code spends an extra `podman ps`
to recover, and it costs nothing.

Worth confirming against the real race -- a container mid-teardown, not a missing one -- before
building on it, which the same reproduction that produced the original finding answers in one
run.

## Goal

The removal's outcome is established where the rest of the sweep's engine state is, so it is
covered by the tests that cover the rest of the sweep.

## Deliverables

- **Verification above the seam.** Keep `D` as `Result<()>` -- both hooks stay "do the removal" --
  and have `execute_with` learn what survived the same way it learns everything else. Once that
  is true, whether the removal was batched becomes a pure performance question with no
  correctness content, and the batch/retry split dissolves.
- **Tests for the mechanism**, which is what moving it above the seam buys.
- **A decision on keeping the batch.** 0002-09 made batching an explicit deliverable; dropping it
  wants a sentence, not silence. Retrying survivors one at a time is currently sequential
  (`for name in &left`) at ~50-100 ms each, which is only worth parallelizing if the batch stays.

## Dependencies

- After `plan/next/clean-batch-removal-fidelity.md`, which landed in 0002-53 and is what this
  refines.
