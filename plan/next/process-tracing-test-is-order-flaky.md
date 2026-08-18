# `try_capture_logged_traces_spawn_and_exit_at_debug` is order-flaky

`crates/outrig/src/process_tests.rs:223` failed once during unrelated work, then passed five
consecutive full-suite runs with no change to it. The assertion that failed was the first one:

```
debug output should name the full command line, got:
DEBUG outrig::process: exit program="/bin/echo" code=Some(0) elapsed_ms=3
```

The `exit` event was captured; the `spawn` event on the same code path was not. The test installs
its subscriber with `tracing::subscriber::set_default`, which is thread-local, but `tracing` caches
callsite interest **globally**. A sibling test that evaluates the `spawn` callsite first, under a
subscriber that is not interested in it, can leave that callsite cached as uninteresting for the
process -- so a later test on another thread silently drops the event. Adding two unrelated tests
elsewhere in the crate was enough to perturb scheduling and surface it.

The failure is in the test harness, not in `process.rs`: the events themselves are emitted
correctly, and the sibling test `run_streamed_forwards_stderr_lines_to_tracing` shares the pattern
and the same exposure.

Options, roughly in order of preference: give the tracing-capture tests a dedicated integration
test binary so they get their own process; or serialize them behind a mutex; or drop the
subscriber-capture approach and assert on a seam that does not involve global tracing state.
Whichever is chosen, `run_streamed_forwards_stderr_lines_to_tracing` wants the same treatment.
