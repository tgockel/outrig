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

- Drop the stale `sidecars: ...` field from each of the three `ImageConfig { .. }` literals
  listed above. Top-level sidecars are set on `Config`, not `ImageConfig`. Mechanical.
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
