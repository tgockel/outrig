# 0072 -- OCI labels for standalone image config (stamp + read)

## Context

0069/0070/0071 gave standalone images a baked config file: `outrig image init` writes
`image.toml`, the Dockerfile copies it to `/etc/outrig/image.toml`, `outrig image build`
validates the baked copy, and runtime reads it back with `podman exec cat`.

We are replacing that file with **OCI image labels** as the canonical surface a standalone
image uses to advertise its MCP/config. Labels are read from image metadata with no running
container, no `podman exec`, and -- via `podman image inspect` -- without pulling; they also
leave the door open to reading a *remote* ref's config without pulling layers (a later task).
This is a pre-0.1 foundation fix, so we change the mechanism now rather than carry the file.

This task introduces labels and makes them **authoritative** for stamping, runtime read, and
build validation. To avoid a broken intermediate, the Dockerfile still copies the baked file
(it is simply no longer read); 0073 removes the now-dead file and its `COPY`.

Authoring is unchanged: humans still write `image.toml` (TOML on disk) for a standalone
project. The build validates it and serializes its contents into labels.

## Goal

Stamp a standalone image's config into OCI labels at build time, and read those labels at
runtime and at build-validation time, replacing the `podman exec cat` file read.

## Deliverables

- Define the label keyspace (constants in the `outrig` lib):
  - `org.opencontainers.image.description` <- `[image].description`
  - `org.opencontainers.image.version` <- `[image].version`
  - `org.outrig.tags` <- `[image].tags` as a JSON array
  - `org.outrig.mcp` <- the `[mcp]` table as a JSON object (the load-bearing entry)
  - `org.outrig.schema` <- schema-version string (e.g. `"1"`) for forward-compat
  - (`[image].ref` is the image tag itself, not a label.)
- A pure codec in `crates/outrig/src/container/embedded.rs`:
  - serialize a validated standalone config to `BTreeMap<String, String>` labels
    (`org.outrig.mcp`/`.tags` via `serde_json`; `McpServerSpec` is `#[serde(untagged)]`, so
    Short serializes to a JSON array and Full to an object),
  - parse a label map back into the metadata + `BTreeMap<String, McpServerSpec>` mcp table,
    validating server names and non-empty commands as the TOML path does today.
- Thread `--label key=value` through `buildah_build_cmd` (`crates/outrig/src/image.rs`),
  mirroring the existing `--build-arg` threading.
- `build_standalone` stamps labels from the validated on-disk `image.toml`.
- `read_image_labels(tag)` in the lib: `podman image inspect <tag> --format '{{json
  .Config.Labels}}'`, parsed into the label map. Local-only (no pull).
- Flip the runtime embedded read (`merged_mcp` / `read_embedded_image_config`) to read
  `org.outrig.mcp` from labels instead of the file, preserving lenient semantics: a missing
  `org.outrig.mcp` yields an empty map and falls back to repo `[images.<name>.mcp]`.
- Flip the build-validation read (`read_standalone_image_toml`) to validate the stamped
  labels read back from the built image, instead of `cat`-ing the baked file.
- Update `outrig mcp show-merged` to the label-based read.
- Migrate the e2e suites (`crates/outrig/tests/embedded_image.rs`,
  `crates/outrig-cli/tests/image_build.rs`) from baked-file fixtures to stamped-label
  fixtures/assertions.

## Acceptance

- Codec unit tests round-trip Short and Full `McpServerSpec` entries plus optional
  description/version/tags through the label map (no podman).
- `outrig image build` on the `image init` scaffold stamps the expected labels; an e2e test
  reads them back off the built image and asserts `org.outrig.mcp` content.
- A session (`outrig run` / `outrig mcp`) against a label-carrying image boots its embedded
  MCP servers (e2e).
- An image with no `org.outrig.mcp` label falls back to repo `[images.<name>.mcp]` (e2e or
  unit on the parse path).
- A malformed `org.outrig.mcp` value (bad JSON, invalid server name, empty command) is a hard
  error at runtime and at build validation.
- `cargo test` plus the migrated e2e suites pass.

## Dependencies

- **Hard: 0069**. Serializes the standalone schema/validation introduced there into labels.
- **Hard: 0071**. Extends the standalone build path and its build-validation read.
