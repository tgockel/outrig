# 0092 -- Fix e2e-gated test bit-rot and compile the e2e suite in CI

## Context

`cargo test --features e2e` (and `cargo clippy --all-targets --features e2e`) fail to compile:
three test-only `ImageConfig { .. }` literals still set a `sidecars` field, but 0088 moved
sidecars to the top-level `Config.sidecars`. CI never compiles the `e2e` feature (only `default`
and `local-llm`), so the rot went unnoticed.

Sites (all `E0560: struct ImageConfig has no field named sidecars`):

- `crates/outrig/tests/network_interceptor.rs:50`
- `crates/outrig/tests/embedded_image.rs:78`
- `crates/outrig/tests/embedded_image.rs:292`

Discovered while landing 0090, whose own gated e2e (`primary_view_e2e.rs`) compiles fine and is
runnable in isolation with `cargo test -p outrig-cli --features e2e --test primary_view_e2e`.
Left out of 0090's commit to keep it scoped; the whole e2e suite only compiles again once these
three are fixed.

This task gates more than it first appears. `crates/outrig/tests/library_surface.rs` is
`required-features = ["e2e"]` (`crates/outrig/Cargo.toml:20-22`), and it is the definition of
the library facade that 0093-0095 are built to preserve. Until the suite compiles, none of the
0.2.0 hardening work has an acceptance test it can actually run.

## Goal

Make `cargo test --features e2e` and `cargo clippy --all-targets --features e2e` compile again,
and add the CI coverage that keeps them compiling.

## Deliverables

- Drop the stale `sidecars: ...` field from each of the `ImageConfig { .. }` literals listed
  above. Top-level sidecars are set on `Config`, not `ImageConfig`. (The Context list is
  short by four -- see Decisions.)
- A CI job that compiles the e2e suite without running it, so this class of rot is caught
  without needing podman on the runner.

  `.github/workflows/ci.yml:20-39` runs a two-row `{name, cargo_args}` matrix whose steps are
  `cargo clippy --all-targets ${{ matrix.cargo_args }}` then `cargo test ${{ matrix.cargo_args }}`.
  An e2e row cannot reuse that second step -- the tests need podman -- so add a `test_args`
  matrix field (`""` for the two existing rows, `--no-run` for the new one) and the row:

  ```yaml
  - { name: e2e, cargo_args: "--features outrig/e2e,outrig-cli/e2e", test_args: "--no-run" }
  ```

  Note the package-qualified feature spec: `e2e` is declared on **both** crates
  (`crates/outrig/Cargo.toml:18`, `crates/outrig-cli/Cargo.toml:19`), so a bare `--features e2e`
  is ambiguous from the workspace root. A separate standalone job is an acceptable alternative
  if the extra matrix field reads worse than duplicating the checkout/cache steps.

## Acceptance

- `cargo test --features outrig/e2e,outrig-cli/e2e --no-run` builds clean from the workspace
  root.
- `cargo clippy --all-targets --features outrig/e2e,outrig-cli/e2e -- -D warnings` is clean.
- The existing `default` and `local-llm` CI rows are unchanged in behavior.
- CI fails if the e2e suite stops compiling, on a runner without podman.
- No test *runs* in the new job -- it is a compile gate only.

## Dependencies

None.

## See also

- `plan/done/0088-entrypoint-stdio-args.md` -- the move of sidecars to `Config` that these
  literals predate.
- `plan/done/0090-primary-view-sidecars.md` -- where the rot was found.

## Decisions

**Seven stale sites, not three.** The Context list was assembled from a single `cargo` error
batch, which stops once each crate's failing test targets are counted. The full set:

| Site                                                 | Fix                        |
|------------------------------------------------------|----------------------------|
| `crates/outrig/tests/mcp_handshake.rs:43`            | helper                     |
| `crates/outrig/tests/embedded_image.rs:78`           | helper                     |
| `crates/outrig/tests/embedded_image.rs:292`          | field dropped in place     |
| `crates/outrig/tests/network_interceptor.rs:50`      | helper                     |
| `crates/outrig/tests/image_build_smoke.rs:54`        | helper                     |
| `crates/outrig-cli/tests/mcp_subcommand_smoke.rs:67` | helper                     |
| `crates/outrig-cli/tests/rig_tool_dispatch.rs:55`    | helper                     |

**Centralized rather than fixed in place.** Five of the seven were byte-identical build-source
literals. They now call `common::fixture_build_config()`, added to each crate's existing
`tests/common/mod.rs`. The trigger is 0094, which marks `ImageConfig` **P0** for
`#[non_exhaustive]` -- that attribute forbids struct literals from `tests/`, which is external
to the crate, so all seven sites would break again. Centralizing first leaves 0094 two call
sites to touch instead of seven. The one-off image-ref literal at `embedded_image.rs:292`
(`image_name: Some(tag)`, no dockerfile/context) has a single caller and stays inline; forcing
it through a parameterized helper would cost more than it saves.

Two helpers rather than one: `tests/common/` is per-crate, and neither crate exports test
utilities to the other. A shared `test-util` feature would be a larger surface decision than
this task should make.

**`rig_tool_dispatch.rs` gained `mod common;`,** which it lacked. Its private `init_tracing()`
was byte-identical to `common::init_tracing()` and was dropped in favor of the shared one.

**CI: matrix field, not a separate job.** A third row carries
`cargo_args: "--features outrig/e2e,outrig-cli/e2e"` and a new `test_args: "--no-run"`; the two
pre-existing rows take `test_args: ""`, so their behavior is unchanged. They declare the field
explicitly even though GitHub Actions expands an absent matrix property to the empty string --
uniform rows read better than rows that rely on that.

The task's Deliverables claim that the feature spec "**must** be package-qualified" because
`e2e` is declared on both crates is **wrong**, and the first draft of this work copied it into
a CI comment. Verified: a bare `cargo test --features e2e --no-run` from the workspace root
succeeds and builds `library_surface` (outrig's `required-features = ["e2e"]` target) alongside
outrig-cli's gated binaries. Cargo fans an unqualified `--features` out to every selected member
that declares it; being declared on both crates is the case where the bare name works, not the
case where it breaks. The qualified spec is kept anyway -- it matches the `local-llm` row and
survives a third crate declaring `e2e` later -- but the recorded reason is explicitness, not
necessity. `CONTRIBUTING.md` documented the bare form all along, which should have been the tell.

**Clippy is the load-bearing gate, not `--no-run`.** `cargo clippy --all-targets` already
type-checks every e2e target, so reintroduced bit-rot fails there first; `cargo test --no-run`
adds only codegen and link coverage. Measured on a 20-core box against a pruned workspace:
clippy 5.1 s, the `--no-run` build 18.6 s after it (22.3 s standalone). The clippy pass is
nearly free and partly warms the build that follows, so both steps stay.

**Acceptance is compilation.** This task adds no runtime behavior, so there is no assertion to
write; the compile gate is the test, and the CI row is what makes it permanent.
