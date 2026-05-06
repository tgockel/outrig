# 0037 -- Refactor: factor `tool_name::sanitize` out of `rig_tool`

## Goal

Move the `<server>__<tool>` namespacing logic out of `src/rig_tool.rs` into a
small standalone module so both `rig_tool` (used by `outrig run`) and the
forthcoming `mcp_proxy` (0039) share a single source of truth. No behavioral
change.

## Deliverables

- New `src/tool_name.rs` containing:
  - `pub fn sanitize(server: &str, tool: &str) -> String` -- exactly the logic
    currently at `src/rig_tool.rs:123-146`, including the blake3-suffix collision
    avoidance.
  - The supporting constants `MAX_NAME_LEN`, `HASH_HEX_LEN`, `SUFFIX_LEN`.
- `src/lib.rs` -- add `pub mod tool_name;` (or `mod tool_name;` if 0042's library
  surface revisits this; for now make it pub so internal callers can reach it).
- `src/rig_tool.rs` -- delete the inline `sanitize` and the constants, replace with
  `use crate::tool_name::sanitize;` (or fully-qualified call sites).
- No re-export shim, no deprecated alias -- both call sites update in the same
  commit.
- Existing unit tests for the namespacing (whatever lives in `rig_tool.rs` today)
  move alongside the function into `tool_name.rs`.

## Acceptance

- `cargo build` clean.
- `cargo test` clean -- existing namespacing tests pass under their new home.
- `cargo test --features e2e` passes; tool-call routing in `outrig run` is
  unchanged.
- `grep -rn "fn sanitize" src/` returns exactly one definition (in
  `tool_name.rs`).

## Dependencies

None.

## Notes

- This unblocks 0039 (`ProxyServer`) which needs to use the same sanitizer to
  produce public tool names that match what `outrig run` already produces.
- Keep the function signature stable -- 0042 (library surface) may decide whether
  `tool_name` is part of the public crate API or stays `pub(crate)`. Don't pre-empt
  that decision here.
