# Nothing proves the panic hook issues the sweep it builds

`crates/outrig/src/container/mod.rs` splits the last-resort cleanup in two:
`pending_removals` builds one label-scoped removal per outstanding obligation, and
`install_panic_hook` detaches each of them before delegating to the previous hook.
Unit tests cover the first half exactly -- that is what letting the sweep be asserted
on without a panic bought -- and nothing at all covers the second.

So the wiring is unproven. A hook that built the right commands and then dropped them,
or one that was never installed, would pass the whole suite.

## Sketch

Needs a child process that panics with the fake `podman` from `tests/cancellation.rs` on
`PATH` and that fake's journal variable inherited, then an assertion on the journal. Both
env vars are already process-global via `fake_runtime()`, so the harness is most of the
way there.

What it needs and the repo does not have is a re-exec-self pattern: `grep current_exe`
across the tree returns nothing. A test binary that re-runs itself with a marker
argument, panics under it, and reads the journal back is the usual shape, and it is worth
writing once rather than per test -- `plan/next/relocate-unit-shaped-tests.md` and the
cancellation harness would both have uses for it.

## Why it is worth doing

This is the layer that runs when the process is already failing, so it is the one least
likely to be noticed if it silently stops working. It is also the layer whose defect
(`#147`) survived from 0.1 through three release candidates precisely because no test
ever drove it.
