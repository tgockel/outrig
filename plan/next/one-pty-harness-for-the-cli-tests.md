# One pty harness for the CLI's tests

## Context

Two test binaries start `outrig` on a terminal of its own: `tests/run_program.rs`
(`OnTerminal`, for `outrig run <script.py>`) and `tests/run_new_e2e.rs` (`OnTerminal`, for
`run-new`'s screen). Each opens a pty, dups its terminal side onto stdio, and runs the same
`unsafe` `pre_exec` block -- `setsid`, then `TIOCSCTTY` -- in its own copy. Surfaced by the
review of the change that gave `run-new` its screen, which kept its copy rather than reach into
`run_program.rs`.

The copies have already drifted. `openpty` returns descriptors without close-on-exec, and
`run_new_e2e.rs` sets it, after its tests hung when run side by side: a child another test
started meanwhile inherited this test's terminal side, so its reader never saw the end of
output. `run_program.rs` does not set it, and has the same exposure whenever two of its terminal
tests, or one of them and another spawning test in the binary, run at once.

## Goal

One helper in `crates/outrig-cli/tests/common/mod.rs` that starts a command on a fresh pty --
taking the window size, setting close-on-exec on both ends, and leaving the caller the master,
the terminal side, and the child -- used by both binaries.

## Acceptance

- Both `OnTerminal`s start their child through the shared helper, and no other `pre_exec`
  `setsid`/`TIOCSCTTY` block remains under `crates/outrig-cli/tests/`.
- `run_program.rs`'s terminal tests pass with `--test-threads` at its default and at 1.

## Dependencies

None.
