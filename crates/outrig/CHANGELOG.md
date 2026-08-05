# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Breaking: `request-timeout-secs` is now range-checked**, closing an asymmetry with its
  sibling `retry-budget-secs`, which has validated against `RETRY_BUDGET_SECS_CEILING` since it
  landed. A remote provider's `request-timeout-secs` must be between `1` and the new
  `REQUEST_TIMEOUT_SECS_CEILING` (`3600`, the same hour as the budget's ceiling); both bounds
  are inclusive. Breaking because a config 0.2.0-rc.1 accepted -- any `u64` at all -- can now be
  rejected at load; the two new error variants are themselves additive.

  `0` is rejected rather than treated as "no timeout". Checked against the pinned reqwest
  0.13.4, `Duration::ZERO` is an *immediate* timeout: the builder stores it, it becomes a
  `tokio::time::sleep` that is ready on first poll, and the request fails before it can be
  answered. So `request-timeout-secs = 0` was a config that parsed, validated, and then could
  not work. Note this differs from `retry-budget-secs = 0`, which means "do not retry" and
  remains legal -- one key counts attempts, the other bounds a single one.

  Two `ConfigValidationError` variants carry it: `RequestTimeoutSecsTooLarge { path, value,
  max }` and `RequestTimeoutSecsZero { path, max }`. The enum is `#[non_exhaustive]`, so both
  are additive.

  The bound applies per provider, which is the only place the key exists -- unlike
  `retry-budget-secs` there is no top-level default to check. Adding one is additive and is
  filed as follow-up work rather than folded in here. It is also checked only on the paths that
  can reach an HTTP client: `outrig build` skips the `[providers]` block wholesale, as it does
  for every LLM-side rule, so a build still succeeds against a config `outrig run` would reject.

- **Breaking: a mount validation error names the config file that declared the mount**, the way
  an image path error has since 0.2.0-rc.1. Global and repo `[[workspace.mounts]]` lists are
  *concatenated*, so a bare relative path in a diagnostic is ambiguous between two files:
  `workspace mount host-path "shared" does not exist` reads as a repo problem even when
  `shared` was only ever meant to be found beside `~/.outrig/config.toml`. The message now ends
  with `(declared in "/home/you/.outrig/config.toml")`, and an entry with no recorded source --
  every hand-built `MountConfig` -- renders no clause at all rather than an empty one.

  Every variant of `MountRuleViolation` changed shape to carry it. The five were tuple variants
  and are now struct variants: `HostMissing { path, declared_in }`,
  `HostNotDirectory { path, declared_in }`, `ContainerNotAbsolute { path, declared_in }`,
  `ContainerDuplicate { path, declared_in }`, and `ContainerRoot { declared_in }`, which was a
  unit variant. The five `ConfigValidationError::WorkspaceMount*` variants gained the same
  field, `WorkspaceMountContainerRoot` likewise ceasing to be a unit variant.
  `ConfigValidationError::SidecarMount` is unchanged and needed no change -- it wraps the
  violation whole, so the clause arrives through the violation's own rendering.

  The container-path rules carry it too, though they judge the value rather than look for a
  directory on disk: the clause answers which file to go edit, which is the same question for
  every rule, and `ContainerRoot`'s message carries no path at all, so the declaring file is
  the only handle it offers. On a duplicate, the named file is the one that declared the
  *rejected* entry -- the later of the two, and the one to edit -- rather than both sides of
  the collision.

  All ten reshaped variants are now `#[non_exhaustive]`, so the next field they take is
  additive. That makes the pattern in a `match` need a trailing `..`; these are return-only
  error variants, so sealing them removes no construction path.

### Removed

- **The `podman exec` user-bootstrap fallback, and the `OUTRIG_BOOTSTRAP` environment
  variable.** Since 0.2.0-rc.1 the runtime user has been written into the container from the
  host, through the container's own namespaces; the older `getent` / `groupadd` / `useradd`
  chain remained only for hosts that cannot enter those namespaces, which means a podman
  service on another machine. OutRig does not support that topology -- the built-in default
  image-config alone declares two `view = "primary"` sidecars, which cannot work against a
  remote engine -- so the fallback kept one subsystem limping where nothing else would run.

  **Breaking:** `container::direct_bootstrap_supported` was public and is gone. It answered
  "will this host need `useradd`/`groupadd` in the image", a question that no longer has a
  yes case. `Container::bootstrap_user` is unchanged, and its failures are unchanged in
  kind -- only in that a namespace-entry failure is now reported rather than absorbed.

## [0.2.0-rc.1](https://github.com/tgockel/outrig/releases/tag/outrig-v0.2.0-rc.1) - 2026-08-02

A release candidate. This is the first cycle to break the public surface, so it goes out for
integration testing ahead of 0.2.0 final. Everything here is measured against **0.1.0**, the
last published release.

The three things a 0.1.0 consumer hits first: the crate now links **rmcp 3.x** (0.1.0 was on
1.x), the **minimum supported Rust version is 1.88** (was 1.87), and every public struct and
enum is `#[non_exhaustive]`, so struct literals and exhaustive `match`es on them no longer
compile. Each is detailed under **Changed**.

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

  `LlmProvider::Anthropic` and its `LlmProvider::anthropic(..)` constructor arrive together
  with the `#[non_exhaustive]` sweep below, so a match with a catch-all arm keeps compiling.

- **`retry-budget-secs`,** as `Config::retry_budget_secs` and a field on the `OpenAi` and
  `Anthropic` variants of `LlmProvider`, bounding how long a transiently-failing LLM call
  keeps retrying. `DEFAULT_RETRY_BUDGET_SECS` is `600` and `RETRY_BUDGET_SECS_CEILING` is
  `3600`; `0` disables retries. A provider's own value wins over the top-level one, which
  wins over the default. Over-ceiling values are rejected with the new
  `ConfigValidationError::RetryBudgetSecsTooLarge`.

  Set it with `LlmProvider::with_retry_budget_secs(..)` rather than a fourth argument to
  `LlmProvider::openai(..)` / `::anthropic(..)`, which would have been a breaking change to
  a surface this release otherwise settles. The enum and its variants are
  `#[non_exhaustive]`, and so is `Config`.

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
  one other than every agent that names it. Setting neither is no longer an error: an
  Anthropic model outrig has no published ceiling for falls back to 32768 and says so once
  on stderr, naming the tables the ceiling belongs in. The fallback errs high on purpose --
  a model whose real limit is lower rejects the request and names that limit, whereas a
  ceiling set too low truncates replies with nothing logged.

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
- **MCP sidecar containers** -- an MCP server can run in its own container alongside the
  primary. `LaunchSpec::with_sidecar` declares sidecars at launch (abort-only: any failure
  tears down everything already started), and `Outrig::add_sidecar` starts one mid-session,
  returning the new tool handles so callers need not diff `tools()`. `SidecarSpec` and
  `SidecarServerSpec` are the builder types -- a raw podman image ref used verbatim, plus
  workspace access, mounts, security, and servers.
- **Config-driven sidecar placement** -- `LaunchSpec::from_config(&Config, image_name,
  repo_root, log_dir)` translates an image config's `[mcp]` map into the primary MCP map plus
  resolved `SidecarSpec`s, resolving a sidecar's image against sibling `[images.<name>]`
  blocks. It replaces `LaunchSpec::from_image_config`; see **Removed**.
- **Entrypoint-stdio MCP servers** -- an image whose `ENTRYPOINT` is itself the server (an
  inline image with no command) is supported, so off-the-shelf MCP images work with no
  repo-side command knowledge. Launch splits into `podman create` + `podman init` +
  `podman start`, which lets the network interceptor attach to the held process before the
  server's first packet.
- **Sidecars that share the primary's filesystem view** -- `view = "primary"` runs a sidecar's
  server against the primary container's own tree, via the embedded `outrig-enter` launcher,
  so a server needs no bind mount and cannot disagree with the primary about paths. It is
  entrypoint-stdio only, requires the `<arch>-unknown-linux-musl` helper at build time, and
  costs `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` in the primary's user namespace.
  `OutrigError::FilesystemHelperUnavailable` names the missing artifact, and the build-time
  reason it was not produced, when the helper is absent.

  The launcher holds those capabilities only until the graft is in place: it clears its
  supplementary groups and becomes the session's uid/gid immediately before exec'ing the
  payload, which drops the permitted, effective and ambient sets with the uid transition. So a
  server placed this way is indistinguishable from an exec-stdio one in what it may do and what
  it may own -- in particular, what it writes into the workspace comes back owned by the
  invoking user. A server needing root over the primary's filesystem is not supported.
- **Device passthrough and a no-new-privileges opt-out** -- `[image.security]` carries
  `devices` and `no-new-privileges`, surfaced as `ContainerSecurity`. `no_new_privileges`
  defaults to `true`, so the default posture is unchanged from 0.1.0.
- **The container's runtime user is written from the host** -- the session user is grafted
  into the container's `/etc/passwd` and `/etc/group` without executing anything inside it,
  so a bootstrap no longer depends on the image shipping `useradd`.
- **`subagent-depth-max` and `subagent-width-max`** bound how deeply subagents nest and how
  many one agent may hold at once. Both are top-level `Config` keys with an
  `[agents.<name>]` override; the defaults are 3 and 8.
- **Every podman/buildah command line is logged at debug level**, so a session can be
  reconstructed from a trace without reproducing it.
- **Arguments for entrypoint-stdio MCP servers** -- an `args` key on `[images.<name>.mcp]`
  entries and on `[sidecars.<sc>]` blocks supplies the container's trailing argv,
  so images that take their configuration positionally (`docker.io/mcp/filesystem` and most of
  the MCP catalog) can be named and run without spelling out their internal layout as an
  exec-stdio `command`.
- **Named sidecars can be entrypoint hosts** -- a `[sidecars.<sc>]` block whose
  one MCP entry omits `command` runs that image's `ENTRYPOINT` as the server, which is how an
  entrypoint-stdio server gets a workspace view, mounts, or its own security policy. Such a
  block hosts exactly one server and must be `start = "auto"`.
- `SidecarSpec`, `SidecarServerSpec`, `SidecarWorkspaceAccess`, `SidecarView`, and
  `resolve_mcp_env` are exported from the crate root.

### Changed

- **Breaking:** the `rmcp` MCP SDK moved from 1.x to **3.1**, so consumers of this library
  link against rmcp 3.x. Two migrations are folded into this one step. rmcp 2.x replaced the
  `Annotated<RawContent>` content model with a flat `ContentBlock`, which the proxy and client
  now build on; rmcp 3.x then changed `ServerHandler::call_tool` to return `CallToolResponse`
  (the `Complete` / `InputRequired` / `Task` enum) rather than `CallToolResult`, and gave
  `ListToolsResult` three further fields that block struct-literal construction.

  `ProxyServer::dispatch_call` still hands back a `CallToolResult`, so a caller driving the
  dispatch path directly is unaffected by the second change. Peers negotiating a protocol
  version older than `2026-07-28` see the same bytes as before.
- **Breaking:** the minimum supported Rust version is **1.88**, up from 1.87, matching rmcp
  3.1.0's own declared MSRV.
- **Breaking:** the crate builds on Linux only, and a non-Linux target now fails with an
  explicit `compile_error!` naming the reason. `network`, `nsfork`, and
  `container::namespace` call `setns` and `CLONE_NEW*` with nothing between them and the
  crate root, so an Apple or Windows target never resolved; it previously surfaced as an
  avalanche of unresolved-import errors instead of one message. Supported architectures are
  x86-64 and AArch64.
- The network interceptor spans N containers rather than one: a single policy and audit log
  covers the primary and every sidecar, with traffic attributed to the container that produced
  it.
- The session watcher is a single `podman events` stream per session, replacing one
  `podman wait` child per container.
- Sidecar bring-up fans out -- distinct images are ensured and label-inspected concurrently and
  only once each, then containers start concurrently. Label-collision errors stay deterministic.
- **A `view = "primary"` sidecar's server runs as the session user**, not as the sidecar
  image's `USER`; see the placement's entry under **Added**. Servers that expect to write to
  root-owned paths in the primary -- or to the image's `HOME`, typically `/root` -- are
  refused.
- **Breaking:** the three provider-specific `ConfigValidationError` variants are now named
  for what they check rather than for one style, and the two remote ones carry the style
  they are reporting on: `OpenAiModelMissingIdentifier { model }` becomes
  `RemoteModelMissingIdentifier { model, style }`, `OpenAiModelHasMistralrsField` becomes
  `RemoteModelHasMistralrsField { model, style, field }`, and `MistralrsModelHasOpenAiField`
  becomes `MistralrsModelHasRemoteField`. Both remote variants are now `#[non_exhaustive]`,
  so a third remote style will not break them again.

  The rendered messages for `openai` models are unchanged; an `anthropic` model now reports
  `(provider style=anthropic)` instead of claiming to be an openai one.
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
  `MountSpec::new`, `WorkspaceSpec::new`, `CapabilitySpec::new`,
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
- `McpServerSpec::Full` gained `sidecar`, `image`, `args`, and `view` fields alongside its
  existing `command` and `env`, carrying the placement of a server that runs in a sidecar.
  The variant is sealed by the sweep above, so it is built through `McpServerSpec::exec` /
  `entrypoint` plus the `with_*` methods rather than as a literal.

### Removed

- **Breaking:** three label-plumbing helpers left `container::embedded` --
  `mcp_config_to_labels`, `merged_mcp_config_to_labels`, and `merge_mcp`. They served the
  crate's own build and launch paths, never a caller: each takes the internal shape of a
  half-resolved MCP table, and none had a consumer outside this crate.
  `standalone_config_to_labels` and `parse_standalone_image_labels` remain for building and
  reading a standalone image's labels.
- **Breaking:** `LaunchSpec::from_image_config`, which copied an image config's `[mcp]` map
  verbatim and left placement-bearing entries to fail at launch. Use `LaunchSpec::from_config`,
  which performs the translation faithfully.

  These are the only reachability `0.2.0` removes. The rest of the surface is now settled
  deliberately: `config`, `container`, `error`, `image`, `mcp_proxy`, and `network` are all
  supported API, so a caller can drive containers, images, and egress policy directly rather than
  only through the `Outrig` facade.

### Fixed

- A `view = "primary"` sidecar whose image declares an `ENTRYPOINT` no longer fails to start,
  in either of the two ways it used to. An **absolute** program was looked for under the graft
  twice: `build_primary_view_argv` prefixed it, and `outrig-enter` applies the graft itself when
  handing the path to the loader, having opened the program before joining the primary's
  namespace while the sidecar's own rootfs is still at `/`. The program is now passed bare and
  every other image-declared element keeps its prefix. A **relative** program
  (`["node", "/app/dist/index.js"]`, which is `docker.io/mcp/filesystem:latest`) was never
  resolved at all, because the launcher did a literal `open` rather than an `execvp`-style
  search; it is now searched along the launcher's own `PATH`, and a failed search reports where
  it looked.
- A `view = "primary"` payload gets a `HOME` it can write (`/home/<name>`, the same path every
  exec-stdio server gets) rather than inheriting the image's, typically a root-owned `/root`.
  The visible symptom was tooling that reads per-user config through `HOME` failing oddly --
  libgit2 treats an unstattable `core.excludesFile` as a hard error, so `cargo` subcommands
  failed while others succeeded.
- A `view = "primary"` payload sees its own `/proc`. `outrig-enter` joins the primary's mount
  namespace only, so the inherited procfs was an instance of the primary's PID namespace with
  no entry for the payload: `/proc/self` resolved to nothing and every rustup shim failed with
  "no /proc/self/exe available". The launcher now unshares its mount namespace unconditionally
  and mounts a fresh `proc` over the inherited one, which also stops the payload from seeing
  the primary's process list.
- The library builds for `*-unknown-linux-musl`. `nsfork` assigned `usize` into
  `msghdr.msg_controllen` and `cmsghdr.cmsg_len`, which musl types as `socklen_t` and glibc as
  `size_t`; the fields are now written and read through inference so both libcs work.
- `Container::stop` passes `--ignore`, so an entrypoint sidecar that has already self-reaped no
  longer fails session teardown.
- A malformed `org.outrig.mcp` label on a repo-built image fails during the build rather than at
  session start.

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
