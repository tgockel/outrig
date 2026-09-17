# 0073 -- Repair pre-existing e2e test compile rot

## Context

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
  None, .. }`, but the field was renamed to `image`. Update the five call sites.

## Goal

Repair the pre-existing e2e compile drift so later image tasks can rely on the full
`--features e2e` suite as an acceptance check.

## Deliverables

- Move the CLI-driven `embedded_image.rs` cases that need `CARGO_BIN_EXE_outrig` into
  `crates/outrig-cli/tests/`, leaving the in-process `merged_mcp` coverage in
  `crates/outrig`.
- Rename the stale `BuildArgs { container: ... }` field to `image` in all five
  `crates/outrig-cli/tests/build_cli.rs` literals.
- Audit for any additional e2e compile drift surfaced after the first failing crate.

## Acceptance

- `cargo test --features e2e --no-run` compiles all e2e targets.
- The e2e suites run green when podman/buildah are present.

## Dependencies

- **Hard: 0072**. The drift was found while migrating the embedded-image e2e suites in
  the OCI-label task.

## Decisions

- The `BuildArgs { container: ... }` compile drift noted in the original task had
  already been repaired on trunk; the remaining stale e2e failure was the
  library-crate integration test using `CARGO_BIN_EXE_outrig` for CLI-driven
  cases.
- The CLI-driven embedded-image cases were moved into `outrig-cli` instead of
  switching to runtime `std::env::var("CARGO_BIN_EXE_outrig")`, because Cargo only
  guarantees that binary path for the package that owns the binary.
- Running the full e2e suite exposed harness rot beyond compilation: the network
  interceptor tests depended on external DNS/HTTPS reachability and blocked the
  current-thread Tokio runtime with synchronous podman calls. Those tests now use
  local host-container traffic, resolve the target IP explicitly, and run on a
  small multi-thread runtime.
- Intercepted DNS forwarding now prefers the systemd-resolved upstream file when
  `/etc/resolv.conf` only points at a loopback stub, so containers can still
  resolve through the interceptor on systemd-resolved hosts.
- The image-build e2e test now parses the `org.outrig.mcp` label JSON instead of
  matching the old short-form substring shape.
- The CLI `rig_tool_dispatch` and `run_smoke` e2e tests now resolve the shared
  `mcp-fs` fixture from the library crate path, matching the other CLI e2e tests.
