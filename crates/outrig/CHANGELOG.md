# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Anthropic's native Messages API is a provider style.** `style = "anthropic"` on a
  `[providers.<name>]` block reaches Claude directly -- `POST {base-url}/v1/messages` with
  `x-api-key` auth, tools advertised as `input_schema`, and `tool_use` / `tool_result` content
  blocks -- rather than through an OpenAI-compatible bridge. It takes the same `base-url`,
  `api-key`, and `request-timeout-secs` fields as `style = "openai"`, and the same timeout,
  transient-retry, tool-call, history, and subagent behavior applies. `base-url` is the API
  root (`https://api.anthropic.com`); a trailing `/v1`, `/messages`, or `/v1/messages` is
  trimmed if present. Reaching Claude through a bridge is still an `openai` provider pointed
  at that bridge, and both remain supported.

  `LlmProvider::Anthropic` and its `LlmProvider::anthropic(..)` constructor are additive:
  the enum and its variants have been `#[non_exhaustive]` since 0.2.0-rc.1, so a match with
  a catch-all arm keeps compiling.

- **`LlmProvider::style()`,** the `style` tag a provider serializes as. It lives next to the
  serde attributes that define those tags, so a diagnostic or a label can name a style
  without retyping the string somewhere it can drift out of agreement with what the config
  file actually accepts.

- **`[models.<name>].max-tokens`,** the output-token ceiling for turns run against that
  model. `[agents.<name>].max-tokens` still wins where it is set; the model value covers
  every agent that uses it and is `None` by default, so nothing changes for a config that
  does not set it.

  It exists because Anthropic requires `max_tokens` on every request and the ceiling is a
  property of the model, not of the role using it. outrig sends the published ceiling for
  the Claude identifiers it recognizes; any other identifier now has somewhere to declare
  one other than every agent that names it. outrig does not fall back to a ceiling of its
  own -- a wrong one truncates replies with nothing logged, which is much harder to
  diagnose than the error naming the missing setting.

- **Relative config paths resolve against the file that declared them.** A path in
  `~/.outrig/config.toml` (or a `--global-config` file) is now relative to that file's own
  directory rather than to the repo root. The practical effect is that a global
  `[images.<name>]` can finally use the build shape: its `dockerfile` and `context` live beside
  the global config and resolve identically from any repo on the machine. Before this, such a
  block parsed and validated as legal but was joined onto the current repo root, so it either
  failed with `DockerfileMissing` or -- worse -- silently picked up a same-named path inside the
  repo. The same fix covers `host-path` in `[[workspace.mounts]]` and `[sidecars.<sc>.mounts]`,
  which matters most for workspace mounts: global and repo mount lists are *concatenated*, so a
  single base directory could never have been right for every element of the result.

  Repo-declared paths are unchanged in every case, and absolute paths were never affected. An
  entry that never went through `Config::load` -- anything hand-built from the library API --
  records no source and keeps resolving against the `repo_root` it is passed.

  The provenance is carried, not discarded after use: `ConfigSource` is public, with
  `base_dir()` for the directory paths resolve against and `config_path()` for the file to name
  in a diagnostic. `ImageConfig::config_source()`, `ImageConfig::base_dir()`,
  `ImageConfig::resolved_build_paths()`, `MountConfig::config_source()`,
  `MountConfig::resolved_host_path()`, and `Workspace::resolved_host_path()` expose it.

  `ConfigValidationError::DockerfileMissing` and `ContextMissing` gained a
  `declared_in: Option<PathBuf>` field, so a global entry's failure no longer reads as a repo
  problem. Both variants were already `#[non_exhaustive]`, so this is additive. It is an
  `Option` rather than a defaulted path because a filename in an error message is a claim: an
  entry built by hand and validated directly has no declaring file, and the message omits the
  `(declared in ...)` clause entirely rather than naming a config that never mentioned it.

- **The library API reaches every sidecar placement.** A hand-built `SidecarSpec` can now host
  an entrypoint-stdio server -- one with no `command`, whose container's `ENTRYPOINT` is the
  server -- with `SidecarSpec::with_entrypoint_server(name, args)`, and can run it against the
  primary container's filesystem view with `with_view(SidecarView::Primary)`. `SidecarView` is
  re-exported from the crate root beside `SidecarWorkspaceAccess`. Both `Outrig::add_sidecar`
  and `LaunchSpec::with_sidecar` accept them, so an embedding program reaches the same tool
  topology as `outrig run` -- including serving an off-the-shelf MCP image over the primary's
  own tree. `SidecarSpec::with_server_spec(name, server)` is the general form the other
  `with_*server` methods are shorthands for, for a server that needs both a transport and an
  environment.

  `view = "primary"` from the library is the same posture change it is from the CLI
  (`CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` in the primary's user namespace), and a build without
  the `<arch>-unknown-linux-musl` helper fails before any container is created, naming the
  missing artifact. The helper is materialized into the `LaunchSpec`'s log directory.

  The placement rules are no longer duplicated: a hand-built spec that sets `view = "primary"`
  alongside workspace access, or hosts an entrypoint server next to another server, is rejected
  by the same code and with the same message as the equivalent `[sidecars.<sc>]` block.
- **`Outrig::exec_stdio` and `Outrig::exec_capture`** run a command in the *primary* container
  as the session's runtime user -- the first streaming, the second returning a
  `std::process::Output` with a non-zero exit reported as data rather than an error. The
  primary `Container` stays private, so an embedder cannot stop a container the session owns.
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

- **A `view = "primary"` sidecar's server now runs as the session user**, not as the sidecar
  image's `USER`. That placement is entrypoint-stdio, so the container process *is* the server
  and there is no `podman exec` window for the user bootstrap -- and because `outrig-enter` needs
  `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` to join the primary's namespace, the image's `USER` was
  effectively forced to root. Under `--userns=container:<primary>` that root is a host subuid, so
  everything such a server wrote into the workspace came back owned by an id the invoking user
  does not have, and every process it spawned inherited `CAP_SYS_ADMIN`.

  The launcher now gives the privileges up as soon as the graft is in place: it clears its
  supplementary groups and becomes the session's uid/gid immediately before exec'ing the payload,
  which clears the permitted, effective and ambient capability sets with the uid transition. A
  server placed this way is therefore indistinguishable from an exec-stdio one in what it may do
  and what it may own. Servers that relied on writing to root-owned paths in the primary -- or to
  the image's `HOME`, typically `/root` -- will now be refused.
- **Breaking:** `container::sidecar::build_primary_view_argv` and
  `container::sidecar::entrypoint_create_args` take a trailing `ids: Option<(u32, u32)>`, the
  `(uid, gid)` the payload drops to, emitted as the launcher's `--uid`/`--gid`. `None` reproduces
  the previous argv exactly and leaves the payload as whatever the image's `USER` says; every
  OutRig-launched sidecar passes the session's ids. The flags are optional on the launcher too,
  which is a standalone binary with a documented argv contract.
- **Breaking:** the three provider-specific `ConfigValidationError` variants are now named
  for what they check rather than for one style, and the two remote ones carry the style
  they are reporting on: `OpenAiModelMissingIdentifier { model }` becomes
  `RemoteModelMissingIdentifier { model, style }`, `OpenAiModelHasMistralrsField` becomes
  `RemoteModelHasMistralrsField { model, style, field }`, and `MistralrsModelHasOpenAiField`
  becomes `MistralrsModelHasRemoteField`. Both remote variants are now `#[non_exhaustive]`,
  so a third remote style will not break them again.

  The rendered messages for `openai` models are unchanged; an `anthropic` model now reports
  `(provider style=anthropic)` instead of claiming to be an openai one.
- **Breaking:** `SidecarServerSpec` is now a two-variant enum rather than a struct, since a
  sidecar server is either exec-stdio (a `command`) or entrypoint-stdio (`args`, no command).
  `SidecarServerSpec::new(command)` becomes `SidecarServerSpec::exec(command)`, mirroring
  `McpServerSpec::exec`, and its sibling is `SidecarServerSpec::entrypoint(args)`. Both
  variants are sealed, so `env` is attached with `with_env` instead of by field assignment, and
  read back through the `command()` / `args()` / `env()` / `is_entrypoint()` accessors.
  `SidecarSpec::with_server` and `with_server_env` are unchanged.

  `SidecarSpec` also gained a `view` field. Because the type is `#[non_exhaustive]`, callers
  that build it through `from_image` plus `with_*` need no change.
- **Breaking:** `LaunchSpec::from_config` no longer errors on an entrypoint-stdio placement.
  It lowers one into a `SidecarSpec` like any other placement, carrying `view` and resolving
  `args` from whichever of the entry or the sidecar block declared them. Code matching on that
  error to fall back to the CLI can drop the fallback.

  Two config keys still have no library counterpart, and a repo config that uses them behaves
  differently under `Outrig::launch` than under `outrig run`: `start = "manual"` sidecars are
  skipped rather than carried, and `on-failure = "warn"` is not honored -- launch-time sidecars
  are abort-only, so a config that degrades gracefully in the CLI fails the whole launch here.
  Both were already true; they are called out now that entrypoint hosts (which cannot be
  `start = "manual"` at all) make the surface worth stating.
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

### Fixed

- A `view = "primary"` sidecar whose image declares an **absolute** `ENTRYPOINT` no longer
  fails to start. `build_primary_view_argv` graft-prefixed the payload's program, but
  `outrig-enter` opens that program before joining the primary's namespace -- while the
  sidecar's own rootfs is still at `/` -- and applies the graft itself when handing the path to
  the loader, so the program was being looked for under the graft twice. The program is now
  passed bare and every other image-declared element keeps its prefix.

  An image whose `ENTRYPOINT` is a *relative* program name (`["node", "/app/dist/index.js"]`,
  which is `docker.io/mcp/filesystem:latest`) still fails: the launcher does no `PATH` search.
  That is tracked separately.

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
