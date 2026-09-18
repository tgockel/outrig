# `validate_image_source` re-asserts a shape it just established

## Context

`validate_image_source` (`crates/outrig/src/config/validate.rs`) discriminates the build shape
with two booleans:

```rust
let has_image_name = image.image_name.is_some();
let has_dockerfile = image.dockerfile().is_some();
let has_context = image.context().is_some();
```

and then, several arms later, inside the branch where both build paths are known to be present,
reaches back through the accessors and re-asserts it:

```rust
path: image.dockerfile().expect("build path").to_path_buf(),
```

`ImageConfig::source` exists to do exactly this discrimination once, returning
`ImageSourceRef::Build { dockerfile, context, .. }` with the two paths bound as `&Path`. 0002-42
carried the `.unwrap()`s forward as `.expect("build path")` during a mechanical field-to-accessor
migration rather than noticing the redundancy; `crates/outrig-cli/src/cli/build.rs` had the same
shape and was fixed there, because its caller already had the `ImageSourceRef::Build` arm.

## Goal

The build shape is established once per function, and the paths flow from that binding.

## Deliverables

- `validate_image_source` binds the pair from the same match that decides the shape, so the two
  `expect("build path")` calls in the `DockerfileMissing` / `ContextMissing` arms go away rather
  than moving.
- The booleans survive only where they are actually reporting a *conflict* (the arm that lists
  which of `image-name`, `dockerfile`, `context` were set together) -- that arm genuinely needs
  the raw presence flags and cannot use `source`, which panics on exactly that input.
- No behavior change: the same errors, with the same `path` and `declared_in` values.

## Notes

The catch is ordering: `source` panics on the invalid shapes, and this function is what *proves*
the shape is valid. So the binding has to come after the conflict checks, not replace them. That
constraint is why a one-line swap does not work and why this is a separate entry rather than a
line in 0002-42.
