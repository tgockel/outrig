# 0002-45 -- `McpServerSpec` can build the named-sidecar entrypoint shape

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

This is the same class of gap
`plan/done/phase/0002-sidecars/tasks/0002-19-library-sidecar-parity.md` closed for
`SidecarServerSpec`: a placement the TOML supports and an embedder cannot reach. The 0.2.0 audit
found it independently and put it on the pre-freeze list, since config/library parity is the kind of
promise a release is read as making.

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

Additive on a `#[non_exhaustive]` enum's impl, in the spirit of the 0002-17 constructor sweep
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

- `plan/done/phase/0002-sidecars/tasks/0002-19-library-sidecar-parity.md` -- the precedent, and the
  reason this counts as a hole rather than a missing convenience.
- `crates/outrig/tests/config_merge.rs`, `config_provider_enum.rs` -- ungated out-of-crate config
  tests, the right home; `library_surface.rs` is `e2e`-gated and is not.
- `crates/outrig-cli/src/builtin_image/default.toml` -- the shape in question, in TOML.

## Decisions

- **`entrypoint_in_sidecar` routes through `full(None, None).with_sidecar(..)`** rather than
  taking a third parameter on `full` or writing a second `Full` literal. `full` stays "the one
  `Full` literal in this impl", so a field added to the sealed variant is still filled in exactly
  one place, and the new constructor adds no field-setting code of its own.
- **Cross-references were added in both directions.** `entrypoint`'s doc now points at the named-
  block form, and `with_sidecar`'s doc says the entrypoint-stdio shape is `entrypoint_in_sidecar`
  rather than a commandless spec built through it. `with_sidecar` is where a caller lands after
  trying `entrypoint(x).with_sidecar(y)` and getting `McpPlacementConflict`, so that is the
  highest-value place to name the alternative.
- **The parity test owns a byte copy of `default.toml` under `tests/fixtures/` rather than
  `include_str!`-ing across crates.** `crates/outrig` is published and carries no `include` key,
  so `tests/` ships inside the `.crate` while a path reaching up into `crates/outrig-cli/` does
  not. `cargo package`'s verification step would not catch that -- it builds lib and bin targets
  only (`CompileFilter::Default` under `UserIntent::Build`, cargo `ops/cargo_package/verify.rs`;
  see rust-lang/cargo#4805) -- so the break would land on whoever unpacks or vendors the crate and
  runs `cargo test`. A fixture file also matches how `config_merge.rs` and `config_schema.rs`
  already carry `fixtures/config-full.toml`.
- **Drift is guarded by byte equality from the CLI side, not by duplicating the construction.**
  `the_parity_fixture_is_a_copy_of_this_config` in `builtin_image`'s existing test module asserts
  `DEFAULT_TOML` equals the fixture verbatim and says which file to copy over which. That is
  strictly stronger than asserting the built-in still *looks* like the shape -- it catches any
  divergence, not just a placement change -- and it costs one assertion instead of a second copy
  of the ~25-line hand-built `Config`. The cross-crate `include_str!` is safe in that direction
  because it sits in a `#[cfg(test)]` module, which the packaging build never expands; the same
  module already reaches out of the crate that way in
  `the_embedded_dockerfile_is_pinned_and_matches_the_dogfooded_image`.
- **The tests live in a new `crates/outrig/tests/mcp_placement_parity.rs`, not in `config_merge.rs`
  or `config_schema.rs`.** The claim spans constructor shape, whole-`Config` parity, and three
  validation negatives; splitting it across the schema file and the validation file would leave no
  single place that states it. The file's module doc records why it is not `e2e`-gated, following
  `config_accessors.rs` and `launch_spec_from_config.rs`.
- **`public-api.txt` was hand-edited to the one intended line.** Regenerating with the pinned
  `cargo-public-api` 0.52.0 produced the intended addition *plus* seven lines of `core::io` ->
  `std::io` rendering churn from the local nightly rustdoc, which is the toolchain drift
  `0002-48` exists to deal with. The generator's placement was used to confirm the insertion
  point: it emitted the new line at the same index the hand edit used.
- **No `doc/` change.** `doc/reference/config.md` already documents the `sidecar`-without-`command`
  form correctly, and `doc/` does not name `McpServerSpec` anywhere; the gap was in the Rust
  surface only.

## Decisions from the `/simplify` pass

- **The fixture-file mechanism came from the alternative and replaced an inline `const`.** Both
  implementations produced a byte-identical constructor, so the comparison was entirely about how
  the parity test gets at the built-in's TOML. The alternative's `tests/fixtures/builtin-default.toml`
  plus a byte-equality guard dominates an inline copy plus a shape assertion on both axes that
  matter: the copy is provably the original rather than a paraphrase of it, and the guard names the
  remedy. Adopted wholesale.
- **The extra coverage was kept.** The alternative shipped four tests to this branch's seven,
  dropping the spec-level "equals what the TOML parses to" check and both `args` cases. `with_args`
  in combination with a named block is reachable only now, so `McpArgsDeclaredTwice` is newly
  library-facing and worth pinning; those tests stayed.
- **The alternative's doc comment was not taken.** It described the constructor as "the one
  placement the other constructors cannot compose their way to", which is a claim about the size of
  the placement set rather than about this function, and would read wrong if a fifth placement ever
  appeared. The shipped wording names the counterpart relationship instead, with the "you cannot
  compose it" explanation living on `with_sidecar`, where a caller who tried lands.
