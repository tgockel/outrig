# `default-image = "outrig-default"` is an error

## Problem

The built-in default image-config is reachable as a fallback rung and by explicit
`--image outrig-default`, but not by name from config:

```toml
default-image = "outrig-default"    # error: does not match any [images.<name>]
```

`UnknownDefaultImage` is raised inside `Config::load*`
(`crates/outrig/src/config/validate.rs:425-428`), and the built-in is injected by
`outrig-cli` *after* load (`crates/outrig-cli/src/builtin_image::inject`). Validation
therefore runs against a map that does not yet contain the name.

Injecting earlier is not the fix. `Config::load*` is supported public API of the `outrig`
crate, and an embedder calling it must get their file's contents, not the CLI's product
opinion -- that is the reasoning `plan/done/0093-shrink-reachable-surface.md` established and
the built-in default work followed.

## Why it is only a wart today

You never *need* to write the key: the built-in is precisely the rung that fires when it is
absent. Pinning it explicitly is possible by declaring `[images.outrig-default]` yourself,
which shadows the built-in. So this is a discoverability edge -- someone reads the banner's
`outrig-default`, tries to make it sticky, and hits an error naming a block they were never
meant to write. The behavior is documented
(`crates/outrig-cli/src/mcp_self/docs/reference/config.md`, "The built-in default
image-config"), which is a substitute for fixing it, not a fix.

## Sketch

`ValidationOptions` is `pub(super)` (`validate.rs:397`), so the CLI cannot pass extra known
names without widening something. The additive shape:

- Add an `extra_image_names: &[&str]` field to `ValidationOptions`, consulted by the
  `UnknownDefaultImage` and `UnknownAgentImage` checks.
- Add a `Config::load_for_run_with(..)` (or an options argument on the existing entry points)
  that the CLI calls with `builtin_image::RESERVED_IMAGES`.

Both are additive; `#[non_exhaustive]` on the error type means no break. Worth checking
whether `agents.<n>.image = "outrig-default"` should be accepted by the same change -- it has
the same shape and the same surprise.

## Acceptance

- `default-image = "outrig-default"` loads, validates, and resolves to the built-in.
- The same name in `agents.<n>.image` behaves consistently with whatever is decided.
- A genuinely unknown `default-image` still errors, naming the unknown value.
- The `default-image` asymmetry paragraph is removed from `reference/config.md`.

## See also

- `crates/outrig-cli/src/builtin_image/mod.rs` -- `RESERVED_IMAGES`, and `inject`'s placement
  in the call order.
- `plan/done/0093-shrink-reachable-surface.md` -- why the injection is CLI-side.
