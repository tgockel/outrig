# Nothing proves the panic sweep issues what it builds

`crates/outrig/src/container/mod.rs` splits the last-resort cleanup in three:
`pending_removals` builds one label-scoped removal per outstanding obligation, `sweep`
detaches each of them, and two triggers decide when `sweep` runs -- `with_panic_sweep`, for
a panic that unwinds out of `main`'s body, and `install_panic_hook`, at the panic site in a
`panic = "abort"` build (#349).

Unit tests cover the mapping exactly, and the unwinding trigger through `on_unwind`, which
takes the sweep as a closure so a test can watch it fire under real panics -- a task's, a
re-raised one, a caught one. Nothing covers the wiring between them: that
`with_panic_sweep` hands `on_unwind` the real `sweep`, that `sweep` detaches what
`pending_removals` returns, or that `outrig-cli`'s `run()` wraps `dispatch` at all. A
wrapper that swept nothing would pass the whole suite.

## Sketch

A test binary of its own, so `TRACKED` is not shared with other tests: start a container
through the fake `podman` from `tests/cancellation.rs` (on `PATH`, journal variable set via
`fake_runtime()`), `mem::forget` the handle so `Drop` cannot discharge it, then
`catch_unwind(|| with_panic_sweep(|| panic!()))` and assert the journal records a
`podman rm -f --filter label=org.outrig.attempt=<token>`. No re-exec is needed any more:
the trigger is a scope, not a process-wide hook.

The abort trigger stays untestable from `cargo test`, whose builds unwind. A re-exec of a
binary built with `-C panic=abort` would reach it; `grep current_exe` across the tree still
returns nothing, so that pattern would be new.

## Why it is worth doing

This is the layer that runs when the process is already failing, so it is the one least
likely to be noticed if it silently stops working. It is also the layer whose defects
(`#147`, `#349`) survived precisely because no test ever drove it.
