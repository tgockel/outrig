# Tighten library module visibility after workspace split

## Goal

After the `outrig` library / `outrig-cli` binary split, audit which modules
in `crates/outrig/src/lib.rs` really need to remain `pub` versus
`pub(crate)`. Many were public only because the (in-tree) binary needed
them; the binary now lives in a separate crate and goes through whatever
the library chooses to publish.

## Notes

- Likely candidates for `pub(crate)`: `process` (generic subprocess
  wrappers), `tool_name` (internal sanitizer), much of `image` (low-level
  buildah plumbing).
- Modules the curated surface explicitly exposes types from (`container`,
  `mcp`, `network`, `session`) probably stay `pub` but their internal
  helpers can shrink.

## Acceptance

- `cargo doc -p outrig --no-deps` shows only the curated surface.
- Bin crate still builds.
- No regressions in `library_surface` test.
