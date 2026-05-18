# 0064 -- Post-split library tidy-up: visibility, names, placement

## Context

The workspace split (`361c6dbb`) moved the binary into `outrig-cli` and
left a number of `pub` items, names, and module homes that were sized for
the old single-crate layout. Sweep them in one pass so the public surface
of `outrig` reflects what external consumers actually need, and so items
inside `outrig-cli` sit in the module that best fits the new shape.

Module-level `pub` vs `pub(crate)`, the `hf` module relocation, and
README/quickstart signposting are tracked separately as 0065, 0066, and
0067 -- this task is intentionally scoped to per-item visibility, names,
and intra-crate placement.

## Goal

Demote in-tree-only `pub` items, rename items whose names made sense only
inside the old layout, and relocate items inside their crate to better
homes now that the binary/library boundary is real.

## Deliverables

- Field/method-level `pub` sweep on `crates/outrig/src/`. Focus areas:
  `container::ContainerHandle`, `session::SessionStore`, `mcp::McpClient`,
  `image::*`. Demote to `pub(crate)` or private when nothing outside the
  library needs the item.
- Name review across the new crate boundary. The `Cli` arg struct in
  `crates/outrig-cli/src/main.rs:24` is the leading example: under the
  old layout the name was unambiguous, but inside an `outrig_cli` crate
  it now reads as `outrig_cli::Cli`, which is fine. Audit similar items
  in both crates and rename anything whose meaning is ambiguous or
  under-specified once the crate name is taken into account.
- Within `outrig-cli`, move items that logically belong in a submodule
  rather than `main.rs`. `Cli` plus the top-level `Cmd` enum (and any
  sibling arg structs in `main.rs`) are the leading relocation
  candidates -- a `cli/mod.rs` (or similar) home makes them reachable
  from tests and keeps `main.rs` focused on argv -> dispatch.

## Acceptance

- `cargo doc -p outrig --no-deps` shows a smaller public surface than
  before this task.
- `cargo build -p outrig-cli`, `cargo test --workspace`, `cargo clippy
  --workspace --all-targets -- -D warnings`, and `cargo fmt --check`
  all pass.
- The `library_surface` integration test still passes (updated if a
  curated item was renamed).
- `crates/outrig-cli/src/main.rs` shrinks: `Cli`/`Cmd` and any sibling
  arg structs have moved to a more appropriate home, or the task notes
  why they were kept in place.

## Dependencies

- None in `plan/todo/`. Implicitly requires the workspace split
  (`361c6dbb`, already merged).
