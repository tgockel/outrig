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
