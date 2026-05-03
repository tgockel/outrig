# 0023 -- `outrig container add`

## Goal

Interactive scaffolding of a new container-config: prompts the user for name, base image,
toolchains, MCP servers; writes `.agents/outrig/containers/<name>/Dockerfile` from compiled-in
templates; appends `[containers.<name>]` and `[containers.<name>.mcp]` blocks to the existing
repo `config.toml` while preserving the surrounding TOML.

`outrig container` is a new top-level command group; v0 ships only the `add` subcommand
(`ls`, `rm`, etc., are reserved for later).

## Deliverables

- `src/container/add.rs::run(name: Option<String>, force: bool) -> Result<()>` matching the
  flow in `doc/usage/container.md`.
- Prompts (using 0022's `ask_*`):
  - Container-config name (default from arg or `coding`).
  - Base image (select from preset list: `debian:bookworm-slim`, `ubuntu:24.04`,
    `alpine:latest`, `node:20-bookworm-slim`, `python:3.12-slim`).
  - Language toolchains (multi-select: `rust`, `node`, `python`, `go`, `none`; default empty).
  - MCP servers (multi-select: `fs`, `shell`, `git`; default `fs, shell`).
- Dockerfile templates compiled in via `include_str!` under `src/container/templates/`:
  - One header file per base-image option (selects apt-get / apk / etc. for the install lines).
  - One fragment per toolchain (`rust.frag`, `node.frag`, `python.frag`, `go.frag`).
  - One MCP-install block computed at runtime by stitching per-server install lines.
  - Universal footer: `WORKDIR /workspace\nCMD ["sleep", "infinity"]`. **No** `useradd`/`USER`.
- Template assembly: `fn render(base, toolchains, mcps) -> String` concatenates header +
  fragments + MCP block + footer in that order.
- File output:
  - `.agents/outrig/containers/<name>/Dockerfile` -- atomic via `tempfile::persist`.
  - Append `[containers.<name>]` and `[containers.<name>.mcp]` to the repo `config.toml`
    using `toml_edit` so surrounding content (comments, formatting) is preserved.
- Idempotency: refuse without `--force` if either output exists (or the config block already
  has a `[containers.<name>]` entry); with `--force`, replace both atomically.
- `tests/container_add_render.rs` for each (base x toolchain) combo (deterministic output).
- `tests/container_add_buildable.rs` (`#[cfg(feature = "e2e")]`): generate, then run
  `outrig build` against the result; assert the image builds.

## Acceptance

- `cargo test container_add_render` passes.
- `outrig container add coding` from a configured repo writes a buildable Dockerfile and a
  config block; `outrig build` then succeeds.
- Drop the `> TODO: Incomplete` marker on `doc/usage/container.md`.

## Dependencies

- 0007-image-build
- 0022-prompt-ux

## Notes

- Templates are plain text; don't pull in a templating engine. String concat + format!() is
  enough.
- For the base-image x toolchain matrix, store fragments per toolchain; the header just sets
  up the package manager, then each toolchain fragment uses the right manager. e.g. the rust
  fragment uses `curl + sh -c rustup-init` regardless of base image.
- The TOML append should be careful: if the user has hand-edited their `config.toml`, we don't
  want to disturb their formatting. `toml_edit` preserves whitespace and comments.
