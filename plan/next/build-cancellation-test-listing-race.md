# The build-cancellation e2e test survives buildah's listing race

## Context

`crates/outrig/tests/build_cancellation_e2e.rs` polls `buildah containers --json` every 100 ms
while the engine's detached `buildah rm` removes the working container a cancelled build left.
buildah lists its store and then reads each container, so one removed in between fails the whole
listing with `Error: reading build containers: container not known`, and `working_containers()`
asserts that the listing succeeded. `twenty_cancellations_leave_no_growth` failed that way once
in a full e2e run on 2026-10-20, during the review of `0003-29`, and passed on the rerun; the
engine's cleanup was not at fault.

## Shape

- `working_containers()` lists again when buildah reports `container not known`, a bounded number
  of times, and fails on any other error as it does now.

## Acceptance

- The helper retries that error alone, and the tests' assertions are unchanged.
