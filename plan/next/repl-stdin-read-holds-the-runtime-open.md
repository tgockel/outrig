# REPL: a stdin read in flight holds the process open at exit

`Repl::run` reads stdin through `tokio::io::stdin`, which reads on tokio's blocking pool. Dropping
a runtime waits for its blocking tasks. So any way out of `outrig run` that leaves a read in flight
should hold the process open until the user presses Enter or closes input. The obvious one is a
second Ctrl-C at the prompt, where the `select!` has dropped `next_line` but not the read under
it. That is inferred for `run` rather than observed; it was observed for `run-new`, below.

`error.rs` documents the same hang for a session whose monitored container went away, and
`watcher::exit_if_monitor_stopped` answers it with `std::process::exit`. `run-new` (`0003-08`)
reads while a round runs, so a read is always in flight there. It hung on a second Ctrl-C and on
the interpreter exiting until its input moved to a `std::thread`, which does not hold up the
process (`cli/run_new/converse.rs`'s `read_lines`). Its two exit-path e2e tests fail when the
reading moves back onto the blocking pool.

`run`'s REPL was left alone there, because `repl.rs` is the 0.2.x line's to change until the
merge constraint lifts.

## Sketch

Fix it once, below both commands:

- `Repl::run` adopts a shared `stdin_lines()`, the thread `run-new` uses, or
- `app.rs` gives the runtime a short `shutdown_timeout` after `block_on`. That also retires
  `exit_if_monitor_stopped`'s `process::exit`.

The second is broader: it covers the MCP stdio transport too. It also stops the runtime from
waiting on any other blocking task, so check that nothing relies on one finishing.

## Acceptance

- `outrig run`: two Ctrl-Cs at the prompt exit without further input. This is an e2e test, as
  `run_new_e2e.rs` does it: stdin left open, the exit awaited within a step timeout.
- If the runtime owns the fix: the `SessionMonitorStopped` path exits the same way without
  `process::exit`.
