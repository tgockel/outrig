# 0069 -- Embedded `image.toml` for standalone images

## Context

OutRig already supports two pieces needed by shared toolset images:

- A repo image-config can reference a prebuilt image via `image-name`.
- An image can carry its own MCP declarations, which OutRig merges with
  `[images.<name>.mcp]` at session startup.

That embedded image config was originally named `/etc/outrig/container.toml`.
The public model has since moved from "container config" to "image config", and
the standalone-image workflow should not add new surfaces on the old path.

This task establishes `/etc/outrig/image.toml` as the only embedded image config
path and defines the schema that later standalone-image commands will consume.

## Goal

Rename and extend the embedded image config surface so both runtime image-configs
and standalone image projects use the same `image.toml` file shape.

## Deliverables

- Replace every runtime read of `/etc/outrig/container.toml` with
  `/etc/outrig/image.toml`.
- Remove fallback handling for the old path; an image either has
  `/etc/outrig/image.toml` or no embedded config.
- Rename code, errors, tests, docs, and self-description metadata away from
  "embedded container" / `container.toml` where the new image terminology is
  correct.
- Add a typed parser/validator for complete standalone `image.toml` files:

  ```toml
  [image]
  ref = "rust-dev"
  description = "Optional human-readable summary"
  version = "0.1.0"
  tags = ["rust"]

  [build]
  dockerfile = "Dockerfile"
  context = "."

  [mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
  ```

- Require `[image].ref` and a valid nonempty `[mcp]` table for standalone
  validation.
- Treat `[build]` as optional. If omitted, later standalone commands use
  defaults relative to `image.toml`: `dockerfile = "Dockerfile"` and
  `context = "."`. If `[build]` is present, both fields are required.
- Treat `[image].description`, `[image].version`, and `[image].tags` as optional
  metadata.
- Add `outrig mcp self` tool `validate_image_toml` for complete standalone
  `image.toml` content.
- Update user docs as part of this task, including embedded MCP config docs,
  config reference cross-links, and MCP self tool descriptions.

## Acceptance

- `rg "container\\.toml" crates doc` returns no hits outside historical
  `plan/done` files.
- Existing embedded-MCP behavior still works when an image ships
  `/etc/outrig/image.toml`.
- Images without `/etc/outrig/image.toml` still work with repo-only
  `[images.<name>.mcp]` entries.
- Malformed embedded `image.toml` is a hard runtime error.
- `validate_image_toml` accepts standalone files with either explicit
  `[build]` fields or the default sibling `Dockerfile` layout, and rejects:
  missing `image.ref`, partial build fields, empty `[mcp]`, invalid MCP server
  names, and empty MCP commands.
- `cargo test embedded` and the MCP self validator tests pass.

## Dependencies

- **Hard: 0051**. The later standalone workflow depends on repo configs being
  able to reference published images by `image-name`.
- **Hard: 0053**. This task renames and extends the embedded image-config
  support introduced there.
- **Hard: 0055**. The new validator is exposed through `outrig mcp self`.

## Decisions

- Standalone `[build]` is optional in `image.toml`. When omitted, disk-aware
  standalone commands default to a sibling `Dockerfile` and `context = "."`.
  The validator only checks TOML shape; it does not check for sibling files.
