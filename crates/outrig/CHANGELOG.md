# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Arguments for entrypoint-stdio MCP servers** -- an `args` key on `[images.<name>.mcp]`
  entries and on `[sidecars.<sc>]` blocks supplies the container's trailing argv,
  so images that take their configuration positionally (`docker.io/mcp/filesystem` and most of
  the MCP catalog) can be named and run without spelling out their internal layout as an
  exec-stdio `command`.
- **Named sidecars can be entrypoint hosts** -- a `[sidecars.<sc>]` block whose
  one MCP entry omits `command` runs that image's `ENTRYPOINT` as the server, which is how an
  entrypoint-stdio server gets a workspace view, mounts, or its own security policy. Such a
  block hosts exactly one server and must be `start = "auto"`.

### Changed

- **Breaking:** every public struct and enum that stays public is now `#[non_exhaustive]`, so
  adding a field or a variant stops being a breaking change. Downstream crates can no longer
  build these types with a struct literal -- including with `..Default::default()`, which the
  attribute blocks along with every other struct expression -- and `match` on a public enum now
  needs a catch-all arm.

  Two construction paths replace the literal, and this release ships both. Types whose fields
  are all optional gained or kept `Default`, and their `pub` fields stay assignable:
  `let mut cfg = Config::default(); cfg.default_image = Some(name);`. Types with a required
  field gained a constructor naming exactly that field -- `ImageConfig::from_dockerfile` /
  `from_image_name`, `SidecarConfig::new`, `Model::new`, `Workspace::new`, `MountConfig::new`,
  `MountSpec::new`, `WorkspaceSpec::new`, `CapabilitySpec::new`, `SidecarServerSpec::new`,
  `ContainerWorkspace::new`, `ContainerMount::new`, `ContainerCapabilities::new`,
  `PrimaryView::new`, `McpTool::new`, and `McpToolResult::ok` / `error`. Types that only ever
  come back out of the library -- `ToolHandle`, `ContainerInspect`, `ImageBuildOutcome`,
  `McpStartupFailure`, the `sidecar` planning types, the `embedded` parse results, and every
  error enum -- get the attribute alone, since nothing outside builds them.

  Two enum variants are sealed the same way and so grew constructors of their own, since a
  sealed variant is otherwise unconstructible from outside: `McpServerSpec::Full` is now reached
  through `McpServerSpec::exec` / `entrypoint` plus `with_env` / `with_sidecar` / `with_args` /
  `with_view`, and `LlmProvider::OpenAi` through `LlmProvider::openai`. `McpServerSpec::Short`
  is unaffected. Variant-level sealing is otherwise limited to what is known to churn: every
  field-bearing `OutrigError` variant, `ImageSourceRef`'s two, and
  `ConfigValidationError::{DockerfileMissing, ContextMissing}`. Patterns that bind a sealed
  variant's fields need a trailing `..`.

  Nothing was removed and no signature changed; the surface diff is the attribute plus the new
  constructors. Three additions exist so a caller need not match a sealed enum at all:
  `McpServerSpec::command` / `env` borrow what only `normalize` used to clone,
  `Placement::sidecar_name` answers the one question callers asked `Placement` for, and
  `SidecarView::as_str` gives the wire name. `From<&ContainerSecurity> for ContainerCapabilities`
  is also new: that mapping now has to live here, because a future security knob can only be
  wired through inside this crate.
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
  field; struct-literal constructions of either need it. The entrypoint argv reaches
  `Container::create_initialized` through the new options struct described below.
- **Breaking:** `Container::create_initialized` takes a single `ContainerCreateOptions` instead
  of seven positional parameters. That list had already grown once this release (the entrypoint
  argv), and a `#[non_exhaustive]` options struct means the next knob is an addition rather than
  another break. Build it with `ContainerCreateOptions::new(image, launch, name)` plus
  `with_transcript` / `with_env` / `with_intercept_dns` / `with_args`; the four omitted default
  to none, empty, and off. `Container::start_named` is unchanged -- `podman run` takes none of
  the create-only knobs, so one shared struct would have meant silently ignored fields.
- **Breaking:** `mcp_proxy::BackingClient` is sealed and can no longer be implemented outside
  this crate. Nothing about *using* `ProxyServer` changes; only an external `impl BackingClient`
  is affected, and the only known one was this repo's own test fake, now a crate-internal
  module. Sealing is what lets the trait gain a method later without a break.
  `ProxyServer::list_tools_inner` and `dispatch_call` stay public: they are the
  `RequestContext`-free half of the dispatch path, useful to a caller driving the proxy without
  an rmcp server.
- **Breaking:** `ImageTag`'s tuple field is private. `ImageTag::new` (taking anything
  `Into<String>`) and `From<String>` construct one; `as_str` borrows the reference and
  `into_string` takes it by value, replacing `.0` reads and moves respectively. `Display` is
  unchanged and still covers the common read path. The field was the last thing freezing the
  tag's representation into the contract -- `ApiKeyRef` has always been opaque this way.
- `ConfigValidationError`'s `SidecarNameInvalid`, `SidecarImageEmpty`, `SidecarMount`, and
  `SidecarArgsWithoutEntrypoint` lost their `image` field -- a sidecar block no longer belongs
  to one image-config -- and their messages are scoped `sidecars.<sc>` rather than
  `<image>.sidecars.<sc>`.
- `sidecar = "<sc>"` without a `command` is no longer a config error -- it is the named
  entrypoint-host form. `ConfigValidationError::McpSidecarRequiresCommand` is removed.
- `args` is rejected in an `org.outrig.mcp` label and in standalone `image.toml`, alongside the
  placement keys.

### Removed

- **Breaking:** four label-plumbing helpers left `container::embedded` --
  `mcp_config_to_labels`, `merged_mcp_config_to_labels`, `primary_scoped_mcp`, and `merge_mcp` --
  along with `container::sidecar::bootstrap_needed`. They served the crate's own build and launch
  paths, never a caller: each takes the internal shape of a half-resolved MCP table, and none had
  a consumer outside this crate. `standalone_config_to_labels` and `parse_standalone_image_labels`
  remain for building and reading a standalone image's labels.

  This is the only reachability `0.2.0` removes. The rest of the surface is now settled
  deliberately: `config`, `container`, `error`, `image`, `mcp_proxy`, and `network` are all
  supported API, so a caller can drive containers, images, and egress policy directly rather than
  only through the `Outrig` facade.

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
