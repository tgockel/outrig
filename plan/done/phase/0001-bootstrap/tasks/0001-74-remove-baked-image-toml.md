# 0074 -- Remove the baked `/etc/outrig/image.toml`

## Context

After 0072, standalone images carry their config as OCI labels, and OutRig stamps, reads, and
validates via labels. The baked `/etc/outrig/image.toml` is still copied into the image by the
`image init` scaffold but is no longer read by anything. This task deletes that dead path so
labels are the sole image-side config surface.

The on-disk `image.toml` in a standalone *project* stays -- it is the human authoring source
that `outrig image build` validates and serializes into labels. Only the *baked-into-the-image*
copy and its readers go away.

## Goal

Make OCI labels the only image-side config surface by removing the baked file, its `COPY`, its
readers, and the documentation that describes the file model.

## Deliverables

- Drop the `COPY image.toml /etc/outrig/image.toml` splice from the `image init` Dockerfile
  scaffold (`crates/outrig-cli/src/image_setup/init.rs`); the generated `image.toml` and
  `README.md` remain, with the README explaining that `outrig image build` stamps labels.
- Remove the now-dead lib surface: `EMBEDDED_IMAGE_CONFIG_PATH`, the `podman exec cat` file
  readers (`read_embedded_image_config` and the file form of `read_standalone_image_toml`), and
  `is_missing_embedded_image_config`. Keep the label codec/readers from 0072.
- Remove `COPY image.toml` lines from test fixtures (`embedded_image.rs`, `image_build.rs`, and
  any fixture Dockerfiles).
- Update `outrig mcp self`: `ConfigPaths` no longer advertises an `image_config` file path --
  describe the label surface instead; update `get_config_schema` accordingly.
- Rewrite the docs that describe the baked file to describe labels:
  `doc/concepts/mcp-servers.md`, `doc/reference/config.md`, `doc/usage/image.md`,
  `doc/usage/mcp.md`, `doc/reference/cli.md`.

## Acceptance

- `rg "/etc/outrig/image.toml"` and `rg "EMBEDDED_IMAGE_CONFIG_PATH"` return no hits outside
  `plan/done`.
- `outrig image init` output contains no `COPY image.toml` line.
- A built `image init` scaffold still boots its MCP servers via labels end-to-end (e2e).
- `cargo test`, the e2e suites, `clippy`, and `fmt` pass.
- `python3 scripts/audit-doc-style.py` passes; the rewritten docs drop any stale
  `> TODO: Incomplete` markers that are now real.

## Dependencies

- **Hard: 0072**. Labels must be authoritative before the file path is removed.

## Decisions

- `outrig mcp self` now keeps `paths` path-only and reports the image-side label surface in a
  top-level `image_labels` object (`mcp`, `schema`, `description`, `version`, `tags`).
- The runtime merge path reads `org.outrig.mcp` labels directly inside `merged_mcp`; the public
  `read_embedded_image_config` wrapper and `EmbeddedImageConfig` return type were removed with the
  baked-file path.
- `outrig image init` renders the standard Dockerfile directly. The generated project
  `image.toml` remains the authoring source, and `outrig image build` is responsible for stamping
  it into OCI labels.
