# 0070 -- `outrig image init` for standalone image projects

## Context

`outrig image add` scaffolds a repo-local image-config under
`.agents/outrig/images/<name>/` and appends `[images.<name>]` to the repo config.
Standalone toolset images are different: they are independent projects whose
output is a reusable container image. They need their own project scaffold, not
a repo config mutation.

## Goal

Add a noninteractive `outrig image init` command that creates a standalone image
project with a Dockerfile, embedded `image.toml`, and a README.

## Deliverables

- Add `outrig image init [DIR]` under the existing `outrig image` command group.
- Generate these files in the target directory:
  - `Dockerfile`
  - `image.toml`
  - `README.md`
- Default the project name from `DIR` when provided, otherwise from the current
  directory name.
- Write bare `image.ref = "<name>"` by default, e.g. `rust-dev`.
- Use the existing Debian slim base-image conventions by default.
- Install and declare the filesystem MCP server by default:
  - Dockerfile installs `@modelcontextprotocol/server-filesystem`.
  - `[mcp].fs` command is `["mcp-server-filesystem", "/workspace"]`.
- Ensure the generated Dockerfile follows OutRig image conventions:
  - Includes packages needed for runtime UID/GID bootstrap.
  - Does not set `USER`.
  - Ends with `CMD ["sleep", "infinity"]`.
  - Copies `image.toml` into `/etc/outrig/image.toml`.
- Refuse to overwrite existing files unless an explicit `--force` flag is
  supplied.
- Document `outrig image init`, the generated files, and a minimal consuming
  repo config that uses `image-name` with no `[images.<name>.mcp]` block.

## Acceptance

- `outrig image init rust-dev` creates `rust-dev/Dockerfile`,
  `rust-dev/image.toml`, and `rust-dev/README.md`.
- The generated `image.toml` passes `validate_image_toml`.
- The generated Dockerfile passes the existing Dockerfile advisory validator.
- Running the command twice without `--force` reports the existing-file conflict.
- `--force` replaces only the generated files for that project.
- Unit or integration tests cover default naming, generated contents, and
  idempotency.

## Dependencies

- **Hard: 0069**. The scaffold writes the canonical standalone `image.toml`
  shape and embeds it at `/etc/outrig/image.toml`.
