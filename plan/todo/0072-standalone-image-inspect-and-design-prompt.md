# 0072 -- `outrig image inspect` and standalone design prompts

## Context

After a standalone image is built, users need a quick way to see what it
declares and, optionally, verify the live MCP tool inventory. AI-assisted design
also needs a standalone-specific prompt and validator so generated projects use
`image.toml` instead of repo-local `[images.<name>]` fragments.

## Goal

Add local standalone-image inspection plus standalone prompt support for
`outrig design prompt`.

## Deliverables

- Add `outrig image inspect <ref> [--live]`.
- Match `podman inspect` behavior for missing images: inspect local images only
  and do not pull remote refs.
- Static default behavior:
  - Read `/etc/outrig/image.toml` from the local image.
  - Print image ref, optional image metadata, and declared MCP server commands.
  - Do not initialize MCP servers.
- `--live` behavior:
  - Start a temporary container from the image.
  - Initialize each declared MCP server.
  - Call `tools/list`.
  - Print the live tool names or counts per server.
- Clean up temporary containers on success and failure.
- Add `outrig design prompt --standalone`.
- Keep the existing default `outrig design prompt` output for repo-local
  image-config design.
- The standalone prompt must ask for a complete project containing
  `Dockerfile`, `image.toml`, and `README.md`, and must describe
  `/etc/outrig/image.toml` as the embedded path.
- Document `image inspect`, `--live`, and standalone AI-assisted design in the
  same task.

## Acceptance

- `outrig image inspect rust-dev` reports metadata and declared MCP servers
  from a local image without starting MCP servers.
- Inspecting a missing local image returns a clear error and does not pull.
- `outrig image inspect rust-dev --live` reports live MCP tool inventory and
  cleans up its temporary container.
- `outrig design prompt` keeps the existing repo-local prompt behavior.
- `outrig design prompt --standalone` includes standalone image conventions,
  `image.toml` schema guidance, and a worked project example.
- Tests cover static inspect, missing-image behavior, live inspect, and both
  design prompt modes.

## Dependencies

- **Hard: 0069**. Inspect and prompt content depend on the canonical
  `/etc/outrig/image.toml` path and schema.
- **Hard: 0071**. Live inspect reuses the same temporary-container MCP test
  behavior as standalone image build.
