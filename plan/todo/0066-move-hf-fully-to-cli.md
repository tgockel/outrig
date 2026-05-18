# 0066 -- Move `hf` module fully into `outrig-cli`

## Context

The library still carries `hf::{HfFile, HfTreeFetcher}` (trait + struct)
because the old in-tree `init` swapped a fake fetcher into tests. After
the split, `init` lives in `outrig-cli`, so the trait can move with it.
Library users have no business depending on a HuggingFace abstraction.

## Goal

Delete `crates/outrig/src/hf.rs`; the trait, the real impl, and any test
doubles live entirely under `crates/outrig-cli/`.

## Deliverables

- Verify no library-facing API (`config`, the curated re-exports in
  `lib.rs`, anything reachable via `pub use`) references `HfFile` or
  `HfTreeFetcher`. This verification is part of the task -- if a
  reference is found, either remove it or push the relocation back to
  `plan/next/` with a note.
- `crates/outrig/src/hf.rs` deleted; `pub mod hf;` removed from
  `crates/outrig/src/lib.rs`.
- `crates/outrig-cli/src/hf.rs` carries the trait, the real impl, and
  the test fakes.
- Every `use outrig::hf::...` rewritten to `use outrig_cli::hf::...`
  (or `crate::hf::...` within `outrig-cli`).

## Acceptance

- `crates/outrig/` has no `hf` module and no references to it.
- `cargo doc -p outrig --no-deps` contains no `hf` items.
- `cargo test --workspace` passes.

## Dependencies

- Soft on 0065 (run after the module-level surface settles so this
  relocation doesn't churn module visibility).
