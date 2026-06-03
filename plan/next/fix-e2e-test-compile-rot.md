# Repair pre-existing e2e test compile rot

The `--features e2e` test suite does not compile (it is not run in CI, so it has
drifted). This is independent of any single feature task; discovered while
migrating the embedded-image e2e suites in 0072.

Known breakages (audit for more -- `cargo test --features e2e --no-run` stops at
the first crate):

- `crates/outrig/tests/embedded_image.rs` -- the three CLI-driven tests
  (`mcp_show_merged_*`, `run_mode_*`, `mcp_server_mode_*`) use
  `env!("CARGO_BIN_EXE_outrig")`, but the `outrig` binary is defined in the
  `outrig-cli` crate, so that variable is only set for `outrig-cli`'s integration
  tests -- never for `outrig`'s. These tests cannot compile (or be run) where they
  live. Fix: move the CLI-driven cases to `crates/outrig-cli/tests/` (where the
  bin var is set and cargo builds the bin before the test), leaving the
  in-process `merged_mcp` cases in `crates/outrig`.

- `crates/outrig-cli/tests/build_cli.rs` -- constructs `BuildArgs { container:
  None, .. }`, but the field was renamed to `image`. Update the four call sites.

Acceptance: `cargo test --features e2e --no-run` compiles all e2e targets; the
suites run (podman/buildah present) green.
