# `outrig clean` deletes a session record whose container was started during the prompt

## Context

Found in review of the #482 fix. The record walk in `crates/outrig-cli/src/cli/clean.rs` selects
an aged session whose container is stopped in the single `podman ps -a` taken at startup, then
deletes `session.json`, the saved logs, and the enter helper after the confirmation prompt. If
the container is started with `podman start` while the prompt is open, the record of a live
container is deleted. The stray sweep no longer has the equivalent problem (it removes without
`-f`), but the record walk still trusts the old snapshot.

## Goal

A session record is not deleted when its container is live at the moment of deletion.

## Deliverables

- Re-check liveness (a fresh listing, or `podman inspect` per target) after the prompt and before
  `store.remove_by_id`; keep and report a record whose container is now running or paused.
- Unit test of the stopped-to-started transition through the `execute_with` seam.

## Dependencies

- After the #482 fix (strays removed without `-f`).
