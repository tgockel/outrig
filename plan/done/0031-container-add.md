# 0031 -- `outrig container add`

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

## Decisions

- **MCP catalogue reconciled to real packages.** The doc listed `mcp-server-shell` and
  `mcp-server-git` as `npm install`-able, but neither package exists on npm under those
  names. v0 ships `fs` (npm `@modelcontextprotocol/server-filesystem`) and `git`
  (PyPI `mcp-server-git`); `shell` is dropped until a real package exists. The docs
  table, transcript, and Dockerfile excerpt in `doc/usage/container.md` were updated
  to match. The default selection became `[fs]` (down from `[fs, shell]`).
- **Per-(family, toolchain) fragments** instead of toolchain-only fragments.
  Two families -- `debian` (apt-based: debian, ubuntu, node:20-bookworm-slim,
  python:3.12-slim) and `alpine` (apk). Even though `rust` and `go` are
  cross-platform via `curl + script`, splitting per family keeps every fragment
  in the same shape and avoids the lone "this one is cross-platform" exception.
  Net 11 template files (2 headers + 4 toolchains x 2 families + 1 footer).
- **Runtime ensure-install in the MCP block.** If the user picks an MCP server
  whose runtime isn't already provided by the base image or selected toolchains
  (e.g. `fs` on `debian:bookworm-slim` with no `node` toolchain), `mcp_block`
  prepends a small `apt-get install nodejs npm` (or `apk add ...`) so the
  resulting Dockerfile builds without the user having to also pick the toolchain.
- **`pip install --break-system-packages`** for `git`. PEP 668 protects system
  Python on Debian 3.11+; opting in is correct here because the entire image
  exists to run that one binary, and a venv adds `PATH`/launcher complexity for
  no upside.
- **`write_atomic` extracted to `repo`**. Was private to `src/config/init.rs`;
  hoisted to `pub(crate) fn repo::write_atomic` so `container::add` can call it
  too. `src/config/init.rs` now delegates.
- **`Cmd::InitContainer` removed**, replaced by `Cmd::Container(ContainerArgs)`
  with `ContainerCmd::Add { name, force }` (mirroring `Cmd::Config`). The old
  variant was a `NotImplemented` stub.
- **`toml_edit` Document mutation** preserves comments and surrounding tables on
  `--force` replacement. `tests/container_add_scripted.rs::force_preserves_unrelated_blocks_and_comments`
  is the explicit gate.
- **Shared `tests/common::scripted_prompt`**. The `tokio::io::duplex` helper used
  by `container_add_scripted.rs` was identical to one in `config_init_scripted.rs`;
  hoisted to `tests/common/mod.rs` and both call sites updated.
- **`DEFAULT_MCP_INDEX = 0` const + `const _: ()` assertion.** The default
  selection for the MCP multi-select (`fs`) is the first variant in
  `McpServer::ALL`. The compile-time assertion guards against a future reorder
  silently changing the default.
