# `buildah build` can lose a reused layer the same way the label commit did

## Context

Found while fixing #728. The label commit outrig issues (`buildah commit --rm --quiet
<builder> <tag>`) is now reissued when containers/storage refuses the image record with
`layer not known`, which is what a concurrent removal of the layer it reused looks like.
`buildah build` ends in a commit of its own, through the same storage path and the same
layer-reuse lookup, so a build racing another build's temporary-tag removal can be refused
the same way -- the failure would arrive as `buildah build` rather than as the commit, and
nothing retries it.

Not seen in a log so far, which is why it is here rather than fixed alongside #728: #728's
one occurrence named the label commit, and a retry of a whole `buildah build` is a different
cost (minutes of `RUN` steps, not one metadata commit) that wants its own thinking --
probably a retry only when buildah failed before any step ran, or a `--no-cache`-free
re-run leaning on the layer cache.

## Goal

A build refused because another build removed a layer under it is not the user's problem.

## Deliverables

- Decide whether `buildah build` gets the same bounded retry as `commit_builder`, and on
  what evidence (a step counter, the sentinel alone, elapsed time).
- If it does: the retry, plus a `tests/cancellation.rs` case using the fake's
  `lostlayer.build.<token>` marker, which already exists and is unused by any build case.

## Dependencies

- After the #728 fix (`commit_builder` and `lost_a_layer` in `crates/outrig/src/image.rs`).
