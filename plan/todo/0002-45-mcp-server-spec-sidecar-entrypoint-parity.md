# 0122 -- `McpServerSpec` can build the named-sidecar entrypoint shape

## Context

The config path can express an entrypoint-stdio server hosted by a *named* `[sidecars.<sc>]`
block -- omit `command`, name `sidecar` -- and the library API cannot build one.

`McpServerSpec::full` is private (`crates/outrig/src/config/mod.rs:1584`), and the two public
constructors both set a field that rules the shape out:

- `exec(command)` -> `command: Some(..)`, which makes it exec-stdio.
- `entrypoint(image)` -> `image: Some(..)`, the *anonymous* sidecar form.

`entrypoint(x).with_sidecar(y)` sets both `image` and `sidecar`, which `validate_mcp_placement`
rejects as `McpPlacementConflict` (`crates/outrig/src/config/validate.rs:702-707`). There is no
path to `{ command: None, image: None, sidecar: Some(..) }`.

This is the same class of gap `plan/done/0096-library-sidecar-parity.md` closed for
`SidecarServerSpec`: a placement the TOML supports and an embedder cannot reach. The 0.2.0 audit
found it independently and put it on the pre-freeze list, since config/library parity is the kind
of promise a release is read as making.

## Why it did not block the built-in default image

The built-in default work needed exactly this shape and got it by embedding the config as TOML
(`crates/outrig-cli/src/builtin_image/default.toml`) and parsing it with the public
`Config::load_from_str`. That was the better choice there for independent reasons -- the built-in
reads as ordinary config, is quotable verbatim in `doc/reference/config.md`, and needs no library
change at all -- so the gap was recorded rather than worked around.

It does mean nothing in-tree currently exercises the missing constructor, which is worth knowing
when sizing this: the fix is for external embedders, not for a blocked caller here.

## Sketch

Add one function beside `entrypoint` (`config/mod.rs:1536-1541`), routing through the private
`full(None, None)` and setting `sidecar`:

```rust
/// Entrypoint-stdio server hosted by the sidecar declared under
/// `[sidecars.<sc>]`: that image's own ENTRYPOINT is the server, so there
/// is no command to exec. The counterpart of `entrypoint`, which creates a
/// dedicated anonymous sidecar instead.
pub fn entrypoint_in_sidecar(sidecar: impl Into<String>) -> Self
```

Additive on a `#[non_exhaustive]` enum's impl, in the spirit of the 0094 constructor sweep
(`crates/outrig/CHANGELOG.md:219-235`).

## Acceptance

- `McpServerSpec::entrypoint_in_sidecar("sc")` yields `command() == None`, `sidecar() ==
  Some("sc")`, `image() == None`, and `is_entrypoint_stdio()`.
- A `Config` built entirely through library constructors can express what
  `builtin_image/default.toml` expresses, and validates. This is the parity claim; a test that
  only checks the four accessors does not make it.
- **Negative cases**, since parity means the constructor is subject to the same validation as the
  TOML: naming a sidecar that no `[sidecars.<sc>]` block declares is rejected, and the
  image-plus-sidecar combination still fails as `McpPlacementConflict`.
- **The tests live in a non-`e2e` external integration test.** `crates/outrig/tests/
  library_surface.rs` is `#![cfg(feature = "e2e")]`, so putting constructor and validation parity
  there means it runs in almost no configuration -- and this task's whole subject is a path with
  no in-tree caller. `config_merge.rs` and `config_provider_enum.rs` are the shape to follow:
  out-of-crate, no podman.
- `crates/outrig/CHANGELOG.md` records it under `### Added`, framed as config/library parity.
- `crates/outrig/public-api.txt` regenerated.

## Dependencies

None. Small and additive; it can land at any point in this phase.

## See also

- `plan/done/0096-library-sidecar-parity.md` -- the precedent, and the reason this counts as a
  hole rather than a missing convenience.
- `crates/outrig/tests/config_merge.rs`, `config_provider_enum.rs` -- ungated out-of-crate config
  tests, the right home; `library_surface.rs` is `e2e`-gated and is not.
- `crates/outrig-cli/src/builtin_image/default.toml` -- the shape in question, in TOML.
