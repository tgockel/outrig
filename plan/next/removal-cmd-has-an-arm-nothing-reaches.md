# `removal_cmd`'s bare-name arm is unreachable

`crates/outrig/src/container/mod.rs`'s `removal_cmd(name, attempt)` still has two arms.
The `Some` arm delegates to `removal_by_attempt`; the `None` arm builds
`podman rm -f <name>` with `Reissue::Once` and is documented as serving an attached
container.

Nothing reaches it. All three callers guard it first: `stop_inner` returns early for
`ContainerOwnership::Attached`, `Drop for Container` does the same, and
`NameGuard::drop` goes straight to `removal_by_attempt`. Its own doc comment already
says as much -- "Nothing that reaches here removes one".

So the function is a one-armed match plus dead code, and
`a_removal_without_an_attempt_falls_back_to_the_name` pins a branch no caller can take.

## Sketch

Collapse `removal_cmd` into `removal_by_attempt` and have the two `Container` sites pass
their token directly. The `Option` is what makes them read as delegating rather than
unwrapping, so the tidy version wants `EngineIdentity` to be what those sites match on --
which is the same refactor `plan/next/a-container-handle-should-hold-the-id-podman-gave-it.md`
is circling, and is why this is filed rather than folded into the `#147` fix.

Deliberately not done there: it would have churned two tests that have nothing to do with
that defect, in a diff whose whole claim was that it changed no argv any existing test
observes.
