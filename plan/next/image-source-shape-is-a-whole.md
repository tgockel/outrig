# `ImageConfig`'s source shape should be replaced whole, not one field at a time

## Context

0002-42 privatized `ImageConfig::dockerfile` and `context` because one `ConfigSource` is the
base directory for both, and added `set_build_paths(dockerfile, context)` to replace the pair
atomically. `image_name` stayed `pub`, because it carries no provenance and the task's goal was
provenance-bearing paths.

But the three fields are one discriminant, not a path pair plus an unrelated string.
`ImageConfig::source` panics unless exactly one shape is set: `image_name` alone, or `dockerfile`
and `context` together. So:

- a caller can still write `image.image_name = Some(..)` on a build-source config and reach the
  panic, exactly as they could before 0002-42; and
- `set_build_paths` on a *pull*-source config manufactures the both-set state, which is a
  blessed method producing a value its own type rejects. That half needs no break, and is
  filed for 0.2.1 as #270: `set_build_paths` clears `image_name`, and a new
  `set_image_name` clears both paths.

What stays here is the field, which only a breaking release can close.

## Goal

The shape is replaced whole, so no sequence of public calls reaches the state `source` panics on.

## Deliverables

- `image_name` becomes private with an `image_name()` getter, alongside `dockerfile()` and
  `context()`.
- A test that every public mutation sequence leaves `source()` callable, ideally exhaustive over
  the two setters #270 adds rather than a sampled pair.
- `crates/outrig/public-api.txt` regenerated; `crates/outrig/CHANGELOG.md` records the break.

## Notes

This is the same class of break as 0002-42's. It missed the 0.2.0 freeze, so the `image_name`
field stays public for all of 0.2.x and field assignment can still reach the panic; it is 0.3
work.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-42-provenance-bearing-paths-move-with-their-base.md`
  -- where the paths were privatized and `image_name` was left out on scope grounds.
