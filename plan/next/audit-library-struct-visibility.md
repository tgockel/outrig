# Audit struct field and method visibility in the library

## Goal

After the workspace split, sweep `crates/outrig/src/` for `pub` fields and
methods that were public only for in-tree use. Demote anything not needed
by external callers to `pub(crate)` or private.

## Notes

- Focus areas: `container::{ContainerHandle, ...}`, `session::SessionStore`,
  `mcp::McpClient`, `image::*`. These all had `pub` items for the binary's
  benefit.
- Distinguish between "external library consumers might need this" and
  "the in-tree binary used to need this." Anything in the second category
  is a candidate for tightening.

## Acceptance

- `cargo doc -p outrig --no-deps` shows a smaller public surface.
- `cargo build -p outrig-cli` still works with no `pub(crate)` complaints.
