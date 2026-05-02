# Dedup `init_tracing` across e2e tests

## Goal

The four e2e test files each carry a hand-rolled `init_tracing()` helper:

- `tests/runtime_user.rs:68-74`
- `tests/container_lifecycle.rs:48-54`
- `tests/image_build_smoke.rs:32-37` (inline, slightly different spelling)
- `tests/mcp_handshake.rs:33-39`

All four bodies are the same six lines (`tracing_subscriber::fmt()` with
`Level::INFO`, stderr writer, ANSI off, `try_init`). One copy was added per
e2e task as we landed them. Time to consolidate before a fifth lands.

## Deliverables

- `tests/common/mod.rs` (new) with `pub fn init_tracing()`. The integration
  test convention is to put shared helpers in `tests/common/mod.rs` (the
  `mod.rs` filename keeps cargo from treating it as its own integration
  binary).
- Each of the four e2e test files: `mod common;` + replace the inline
  `init_tracing` body with a call to `common::init_tracing()`.
- The `image_build_smoke` test currently inlines the body inside its
  `#[tokio::test]`; adopt the helper there too.

## Acceptance

- All four e2e tests compile and pass under
  `cargo test --features e2e -- --nocapture`.
- No `tracing_subscriber::fmt()` lines remain inside individual test files.

## Dependencies

None.
