# The create-cancellation e2e asserts on winning a polling race

`crates/outrig/tests/container_cancellation_e2e.rs`'s `cancel_once` drives a creation
against `CancelOn::EngineHolds`, which resolves by polling `podman ps` until the engine
is observably holding the container. `require_in_flight` then fails the test unless at
least one of `ATTEMPTS` cancels landed while the creation was still in flight.

Nothing about the code under test decides that. If every creation returns before the
poll observes it, the run fails even though creation and cleanup did exactly what they
should -- the cancellation boundary is relative subprocess timing, not a state the test
arranges. The unchanged base branch has both passing and failing x86-64 runs, which is
the evidence this is the test and not a regression.

There is a second, quieter half. `cancel_once` computes `in_flight` from
`completed.is_none()` and then `drop(completed)` without ever inspecting the
`Result<Container>`. A creation that *failed* is therefore counted the same as one that
won the race, so an engine error reaching every attempt surfaces as
`require_in_flight`'s "the window may have moved" rather than as the error podman
actually returned.

## Sketch

Arrange the boundary instead of racing for it. The cancellation needs a point the test
controls -- a creation held at a known in-flight instant rather than one polled for --
which is what the shell-fake harness in `tests/cancellation.rs` already provides for the
non-e2e cases: a fake `podman` that blocks until the test lets it go. The e2e value is in
using a *real* engine, so the fake cannot simply be substituted; a wrapper that execs the
real podman after signalling would keep both properties.

Failing that, the weaker fix is to stop asserting on the race: report the in-flight count
without requiring it, and assert the invariant that is actually the subject -- the engine
holds nothing under the name afterwards -- on every attempt regardless of which side of
the window it landed on.

Either way, inspect the completed `Result` and fail with podman's own error when a
creation failed, so "nothing was measured" cannot stand in for "the engine refused".

## Why it is worth doing

`0002-53` turned this suite on in CI on every pull request, on two architectures. A test
that fails on subprocess timing spends that signal: it trains readers to re-run rather
than read, which is exactly how the nft defect `0002-53` found would have been missed.
