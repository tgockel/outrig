# 0065 -- Tighten library module visibility after split

## Context

Several modules in `crates/outrig/src/lib.rs` are `pub` only because the
old in-tree binary used to reach into them directly. After 0064 settles
the per-item surface, this task picks the right module-level visibility
so the library exposes a curated set of modules rather than its full
internal layout.

## Goal

Audit each `pub mod` in `crates/outrig/src/lib.rs` and demote the ones
that nothing outside the library needs.

## Deliverables

- Survey of likely demotion candidates: `process` (subprocess wrappers),
  `tool_name` (internal sanitizer), much of `image` (low-level buildah
  plumbing). Demote to `pub(crate)` (or private) when no outside consumer
  is left.
- Curated re-exports preserved for modules that genuinely expose types
  to consumers (`container`, `mcp`, `network`, `session`); internal
  helpers inside those modules can still shrink.
- `crates/outrig/src/lib.rs` updated; `use outrig::...` paths in
  `outrig-cli` adjusted to go through curated re-exports rather than
  newly-private modules.

## Acceptance

- `cargo doc -p outrig --no-deps` shows only the curated module
  surface.
- `cargo build -p outrig-cli`, `cargo test --workspace`, `cargo clippy
  --workspace --all-targets -- -D warnings` all pass.
- `library_surface` integration test passes (updated if necessary).

## Dependencies

- Soft on 0064 (per-item surface should be settled before drawing the
  module boundary).
