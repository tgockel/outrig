# 0083 -- Fix stale `malformed_mcp_label_is_hard_error` e2e test

## Goal

Repair the stale e2e test. `cargo test -p outrig --features e2e --test embedded_image
malformed_mcp_label` fails on trunk (verified against a clean tree during task 0079): the
test builds an image whose Dockerfile stamps a malformed `org.outrig.mcp` label and
expects `ensure_image` to succeed so the *runtime* read (`embedded::merged_mcp`) can fail.
Since the build path started re-merging inherited labels into the cache tag
(`merged_mcp_config_to_labels` at `image.rs:658`), the malformed label hard-errors during
`ensure_image` itself and the test's `.expect("ensure embedded fixture image")` panics
before the assertion runs.

## Deliverables

The behavior is arguably better (fail at build), so the fix is probably to update the
test to assert `ensure_image` fails with `EmbeddedImageConfigParse` -- and add a separate
runtime-read case using a raw (non-built) image ref if that path still needs coverage.

## Acceptance

- `cargo test -p outrig --features e2e --test embedded_image malformed_mcp_label` passes
  on a clean tree.
- The runtime-read failure path (`embedded::merged_mcp`) either keeps coverage via a
  raw-image-ref case or is shown to be unreachable.

## Dependencies

None (follows up on completed task 0079).

## Decisions

- Split the stale test into two, both keeping the `malformed_mcp_label` filter substring:
  `malformed_mcp_label_fails_ensure_image` (build path hard-errors during the label
  re-merge) and `malformed_mcp_label_on_raw_image_fails_runtime_read` (raw image ref
  passes `ensure_image` untouched, fails at `embedded::merged_mcp`). The old name was
  dropped -- it collided with the unit test `embedded_mcp_malformed_label_is_hard_error`.
- The runtime-read path is reachable only via the `ImageSourceRef::Image` branch, which
  never re-validates labels; the raw fixture is built with `image::build_standalone`
  (stamps nothing beyond the Dockerfile's own labels) rather than a hand-rolled buildah
  invocation, under a fixed tag rebuilt each run so local storage doesn't accumulate.
- `merged_mcp` only reads labels off the handle's image tag, so the runtime-read test
  uses `Container::attach` instead of a full start/bootstrap/stop cycle, and both
  malformed-label fixtures are bare `FROM alpine` + `LABEL` (no node toolchain, no
  shadow) -- the parse failure never touches layer contents.
- Runtime coverage stays library-level in `crates/outrig/tests/embedded_image.rs` (it
  exercises `embedded::merged_mcp` directly); the CLI test file keeps its own raw-image
  CLI cases.
- Bonus non-e2e regression test: `merged_mcp_config_labels_reject_malformed_inherited_label`
  pins the new hard-error site (`merged_mcp_config_to_labels`) in plain `cargo test`.
