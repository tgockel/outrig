# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Arguments for entrypoint-stdio MCP servers** -- an `args` key on `[images.<name>.mcp]`
  entries and on `[images.<name>.sidecars.<sc>]` blocks supplies the container's trailing argv,
  so images that take their configuration positionally (`docker.io/mcp/filesystem` and most of
  the MCP catalog) can be named and run without spelling out their internal layout as an
  exec-stdio `command`.
- **Named sidecars can be entrypoint hosts** -- a `[images.<name>.sidecars.<sc>]` block whose
  one MCP entry omits `command` runs that image's `ENTRYPOINT` as the server, which is how an
  entrypoint-stdio server gets a workspace view, mounts, or its own security policy. Such a
  block hosts exactly one server and must be `start = "auto"`.

### Changed

- **Breaking:** sidecars moved from `[images.<name>.sidecars.<sc>]` to a top-level
  `[sidecars.<sc>]` map, matching every other cross-referenced entity in the config
  (`[models.<n>]`, `[providers.<n>]`, `[images.<n>]`). One block can now be shared by any
  number of image-configs, and the global config can declare sidecars a repo references,
  because top-level maps merge global-then-repo by name. `ImageConfig::sidecars` is gone;
  `Config::sidecars` replaces it, and `sidecar::plan_from_config` takes the `Config` too.
  A config using the old nested form fails to parse with an unknown-field error.
- **Breaking:** a sidecar starts only when some `[images.<name>.mcp]` entry names it.
  Previously every declared block started. With blocks now shared and global, declaring one
  can no longer mean "run it" -- a personal toolbox in the user config would otherwise start
  in every repo. A block hosting no MCP servers, and one whose servers came only from its
  image's `org.outrig.mcp` label, are no longer reachable.
- **Breaking:** `McpServerSpec::Full` gained an `args` field and `SidecarConfig` an `args`
  field; struct-literal constructions of either need it. `Container::create_initialized` takes
  the entrypoint argv as a new trailing parameter.
- `ConfigValidationError`'s `SidecarNameInvalid`, `SidecarImageEmpty`, `SidecarMount`, and
  `SidecarArgsWithoutEntrypoint` lost their `image` field -- a sidecar block no longer belongs
  to one image-config -- and their messages are scoped `sidecars.<sc>` rather than
  `<image>.sidecars.<sc>`.
- `sidecar = "<sc>"` without a `command` is no longer a config error -- it is the named
  entrypoint-host form. `ConfigValidationError::McpSidecarRequiresCommand` is removed.
- `args` is rejected in an `org.outrig.mcp` label and in standalone `image.toml`, alongside the
  placement keys.

## [0.2.0-rc.1](https://github.com/tgockel/outrig/releases/tag/outrig-v0.2.0-rc.1) - 2026-07-24

A release candidate. This is the first cycle to break the public surface, so it goes out for
integration testing ahead of 0.2.0 final. The two items consumers hit first are the rmcp 2.x
content model under **Changed** and the removed constructor under **Removed**.

### Added

- **MCP sidecar containers** -- an MCP server can run in its own container alongside the
  primary. `LaunchSpec::with_sidecar` declares sidecars at launch (abort-only: any failure
  tears down everything already started), and `Outrig::add_sidecar` starts one mid-session,
  returning the new tool handles so callers need not diff `tools()`. `SidecarSpec` and
  `SidecarServerSpec` are the builder types -- a raw podman image ref used verbatim, plus
  workspace access, mounts, security, and exec-stdio servers.
- **Entrypoint-stdio MCP servers** -- an image whose `ENTRYPOINT` is itself the server (an
  inline image with no command) is now supported, so off-the-shelf MCP images work with no
  repo-side command knowledge. Launch splits into `podman create` + `podman init` +
  `podman start`, which lets the network interceptor attach to the held process before the
  server's first packet.
- **Config-driven sidecar placement** -- `LaunchSpec::from_config(&Config, image_name,
  repo_root, log_dir)` translates an image config's `[mcp]` map into the primary MCP map plus
  resolved `SidecarSpec`s, resolving a sidecar's image against sibling `[images.<name>]`
  blocks. `start = "manual"` sidecars are skipped, and entrypoint-stdio placements are
  rejected with an error naming the server.
- `SidecarSpec`, `SidecarServerSpec`, `SidecarWorkspaceAccess`, and `resolve_mcp_env` are
  exported from the crate root.

### Changed

- **Breaking:** upgraded the `rmcp` MCP SDK from 1.x to 2.x. The proxy and client now build on
  rmcp 2.2's flat `ContentBlock` content model (replacing `RawContent`), so consumers of this
  library link against rmcp 2.x.
- The network interceptor spans N containers rather than one: a single policy and audit log
  covers the primary and every sidecar, with traffic attributed to the container that
  produced it.
- The session watcher is a single `podman events` stream per session, replacing one
  `podman wait` child per container.
- Sidecar bring-up fans out -- distinct images are ensured and label-inspected concurrently
  and only once each, then containers start concurrently. Label-collision errors stay
  deterministic.

### Removed

- **Breaking:** `LaunchSpec::from_image_config`, which copied an image config's `[mcp]` map
  verbatim and left placement-bearing entries to fail at launch. Use
  `LaunchSpec::from_config`, which performs the translation faithfully.

### Fixed

- `Container::stop` passes `--ignore`, so an entrypoint sidecar that has already self-reaped
  no longer fails session teardown.
- A malformed `org.outrig.mcp` label on a repo-built image fails during the build rather than
  at session start.

## [0.1.0](https://github.com/tgockel/outrig/releases/tag/outrig-v0.1.0) - 2026-06-26

### Added

- **Standalone toolset images** -- build a reusable OutRig image from a project's
  `image.toml`: `outrig image build` builds the declared Dockerfile and verifies the
  result, embedded `image.toml` is validated, and the MCP config is stamped into OCI
  labels (replacing the baked `/etc/outrig/image.toml` file). Built images are tagged
  after their `[images.<name>]` config, and repo-local build images carry the same
  `org.outrig.mcp` declaration that startup reads.
- **Image introspection** -- `outrig image inspect <ref>` reads OCI labels from the local
  store, and `outrig image inspect --remote <ref>` reads them from a registry via
  `skopeo` -- both without pulling layers or starting a container.
- **Embedded-MCP policy** -- `LaunchSpec` carries an `EmbeddedMcpPolicy`: `Merge` (the
  backward-compatible default) overlays the launch spec onto the image's embedded MCP
  map, while `Ignore` treats the provided map as authoritative and skips
  `org.outrig.mcp`.
- **Flexible launch inputs** -- `run`/`mcp` work without a repository config (falling back
  to the global config plus an explicit `--image`), an `--image` that matches no config
  block is used as a raw local Podman ref, and `outrig run` accepts a `--model` override.

### Fixed

- Skip agent/model/provider validation during image builds -- building an image never
  instantiates a model, so a dangling `default-model` no longer blocks it.
- Repair e2e suite rot.

### Changed

- Split the workspace into the `outrig` library and the `outrig-cli` binary, narrowed the
  runtime crate surface, and hid internal runtime modules.
- Centralized dependency versions in the workspace and declared features per crate; each
  crate now ships its own crates.io README.
- Renamed the `[container]` config table to `[image]` and the tool-call/result `cap`
  limits to `max`.
