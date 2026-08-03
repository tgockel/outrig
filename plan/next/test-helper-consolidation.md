# Consolidate duplicated integration-test helpers

## Context

Both crates have a `tests/common/mod.rs` for shared test helpers, but a large number of test
binaries carry private byte-identical copies of helpers that already live there. Surfaced while
landing 0092, which removed exactly one such duplicate (`rig_tool_dispatch.rs`'s private
`init_tracing`) because it was touching that file anyway. The rest was left alone to keep a
bit-rot repair from turning into a test-suite refactor.

## Inventory

Helpers that already exist in `common` and are shadowed by private copies:

- **`init_tracing`** -- the widest. ~19 inline copies of the same 5-line
  `tracing_subscriber::fmt()...try_init()` block: 5 in `outrig-cli/tests/mcp_subcommand_smoke.rs`,
  6 in `outrig-cli/tests/mcp_sidecar_smoke.rs`, 3 in `outrig-cli/tests/run_smoke.rs`, one each in
  `outrig-cli/tests/{build_cli,e2e_quickstart,primary_view_e2e,image_add_buildable}.rs`, and
  `outrig/tests/library_surface.rs`. About 95 lines.
- **`run_capture`** -- `outrig/tests/network_interceptor.rs` and
  `outrig/tests/container_lifecycle.rs`. Both files already declare `mod common;`, so each is a
  pure deletion.
- **`ALPINE` / `pull_alpine`** -- private copies in `outrig/tests/container_lifecycle.rs` and
  `outrig/tests/container_security.rs`; both files already declare `mod common;`.
  (`container_security`'s `start_alpine` legitimately differs -- it takes a
  `ContainerLaunchSpec` where `common`'s takes a host workspace. Leave it.)

Helpers duplicated across files but **not** yet in `common`, i.e. candidates to add:

- **`try_capture`** -- triplicated across `outrig/tests/network_interceptor.rs`,
  `outrig/tests/container_lifecycle.rs`, `outrig-cli/tests/build_cli.rs`. Natural companion to
  the existing `run_capture`.
- **`start_and_bootstrap`** -- identical in `outrig/tests/mcp_handshake.rs`,
  `outrig/tests/embedded_image.rs`, `outrig-cli/tests/rig_tool_dispatch.rs`.
- **`ensure_*_image` wrappers** -- five near-identical wrappers around
  `ensure_image(&common::fixture_build_config(), dir, false)`, differing only in fixture
  directory, `expect` message, and whether they return `Result` or panic:
  `outrig/tests/{mcp_handshake,network_interceptor,embedded_image}.rs` and
  `outrig-cli/tests/{rig_tool_dispatch,mcp_subcommand_smoke}.rs`. An
  `ensure_built_image(dir) -> Result<ImageTag>` in each `common` absorbs all five and lets
  `fixture_build_config` drop back to private.
- **`dockerfile_escape` / `label_line`** -- triplicated across
  `outrig/tests/embedded_image.rs`, `outrig/tests/library_surface.rs`,
  `outrig-cli/tests/embedded_image.rs`.
- **`fs_spec`** -- `outrig/tests/embedded_image.rs` and `outrig-cli/tests/mcp_subcommand_smoke.rs`,
  with further inline copies in `mcp_handshake.rs` and `rig_tool_dispatch.rs`.
- **`stream_lines`** -- `outrig-cli/tests/e2e_quickstart.rs:365` is a ~20-line near-duplicate of
  `common::stream_lines`, differing only by an extra `kind: &'static str` label parameter.
  Unifiable by formatting the label at the call site.
- **Spawning the `outrig` binary** -- roughly ten `outrig-cli` test binaries each roll their own
  `env!("CARGO_BIN_EXE_outrig")` + `Command::new(bin).args(..).current_dir(..).stdin(null)` +
  `timeout(..)` + `String::from_utf8_lossy(&output.stderr)` block, with only the argv and the
  timeout constant differing: `embedded_image.rs`, `builtin_default.rs`, `run_smoke.rs`,
  `session_cli.rs`, `build_cli.rs`, `image_build.rs`, and others. A
  `run_outrig(cwd, args) -> (bool, String)` in `outrig-cli/tests/common/mod.rs` absorbs all of
  them. Note this one is *not* a shadowed helper -- `common` has no binary-spawn helper today,
  so every copy is following the entrenched convention rather than ignoring an available one.

## Also worth doing

`crates/outrig-cli/tests/common/mod.rs` carries ten per-item `#[allow(dead_code)]` attributes.
Its sibling `crates/outrig/tests/common/mod.rs` solves the same problem once with a module-level
`#![allow(dead_code)] // each test binary uses a different subset`. Adopt the sibling's form and
delete the ten attributes.

`crates/outrig/tests/library_surface.rs` has no `mod common;` at all despite duplicating
`init_tracing`, `dockerfile_escape`, and `label_line`. It is `required-features = ["e2e"]`, so
it now sits inside the CI compile gate 0092 added.

## Constraint

`tests/common/` is per-crate; neither crate exports test utilities to the other, so genuinely
cross-crate helpers stay duplicated once unless a `test-util` feature on `outrig` is introduced.
That is a public-surface decision and should not be made here -- see 0093-0095.

## Sequencing

Cheap and mechanical, but touches most test files, so it will conflict with anything else in
flight. Land it in a quiet window, ideally after the 0093-0095 surface work settles.
