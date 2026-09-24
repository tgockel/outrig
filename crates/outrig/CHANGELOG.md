# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Intercepted DNS no longer falls back to a hard-coded public resolver.** When the host's
  `/etc/resolv.conf` was missing, unreadable, or named no nameserver, the `audit`/`filter`
  DNS listener forwarded every lookup to Cloudflare at `1.1.1.1:53`, without saying so anywhere.
  It did this even when systemd-resolved's `/run/systemd/resolve/resolv.conf` named a usable
  upstream, which had already been read and was then thrown away. That upstream is now preferred
  whenever `/etc/resolv.conf` names nothing or only loopback addresses. If neither file names a
  resolver, attaching fails with a `Configuration` error that says what each file held, so a
  host that had been resolving through Cloudflare now fails `audit`/`filter` setup instead.

- **One slow DNS lookup no longer stalls the container's others.** The interceptor's DNS
  listener forwarded one query at a time and did not read the next until the current one was
  answered or had waited out its 5s timeout at every host resolver. So one name a resolver was
  slow to answer held up every lookup from every process in that container, a large enough burst
  behind it overflowed the socket and was silently dropped, and in `filter` mode a stalled
  lookup recorded no binding, so a connection the policy allows could be denied. Each lookup is
  now forwarded in its own task, up to 64 at once per attachment. Past that the listener stops
  reading until one finishes. A detach abandons whatever is still in flight, as before.

- **The runtime-user bootstrap refuses a home that is not a directory.** If the image already
  had `/home/<user>` as a regular file, a FIFO, or a symlink, `Container::bootstrap_user` took
  the existing path as done, `chown`ed it -- through the symlink, onto its target, which could
  be a file in the bind-mounted workspace -- and reported the user ready, so every later exec
  ran with a `HOME` it could not use. The home directory is now opened as a directory without
  following a final symlink and `chown`ed through that descriptor, and anything else fails the
  bootstrap with `BootstrapNamespace`, whose `step` now names the home path. This is stricter
  than `mkdir -p` in one place: a symlink to a directory at `/home/<user>` is refused too. A
  symlinked `/home` still works.

- **A refused image cleanup no longer disarms its retry.** A build removed its temporary
  `outrig-tmp-*` tag, and a failed label-stamping pass its `outrig-label-*` working container,
  then released the guard that owed the removal whether or not buildah had done it. A removal
  refused for a transient reason -- a lock, a busy image -- left the resource behind with nothing
  coming for it, and a temporary tag stays tagged, so pruning never collects it. The guard now
  stays armed unless the removal worked or buildah reports the target as not there, and a
  refusal is logged as a warning and reissued in the background under the guard's usual bounded
  retries. The build's own result is returned either way.

## [0.2.0](https://github.com/tgockel/outrig/releases/tag/outrig-v0.2.0) - 2026-09-23

The first release since 0.1.0. It breaks the public Rust surface in most of the ways a 0.1
consumer will notice, so everything below is measured against **0.1.0** and **Migrating from
0.1** is the ordered list of work. Read it before the entries: three of the breaks it names
are compiler-silent, and a consumer who updates, compiles clean, and stops there will be
wrong about all three.

### Migrating from 0.1

Every break a 0.1 consumer hits, by name, so that the list can be worked through rather than
reconstructed from the entries below.

Most are source breaks, and the compiler finds those for you. Three are not, and they are the
ones to read first, because a consumer can update, compile clean, and still be wrong:
**referenced-sidecars-only** changes which sidecars a config starts, **advertised tool names**
changes the strings a cached tool list holds, and **the implicit preamble** changes what an
agent that omits `preamble` sends. Four more are compile breaks with a silent half, where
fixing the signature does not settle the behavior: `LaunchSpec::from_config` (what the session
enforces), `ExecOptions` (where an exec with no workdir runs), `Model::source` (it panics
rather than failing to compile), and `Workspace::set_container_path` (which provenance
survives).

**There is no stable binary ABI, and this release does not introduce one.** The crate produces
ordinary `rlib` and metadata artifacts. It exposes no `cdylib`, no stable `extern "C"` entry
points, no `#[repr(C)]` types, and no fixed symbol layer. Public type layouts moved here and
downstream crates rebuild against the new ones, as Cargo does for any dependency. Nothing in
outrig claims ABI compatibility across versions, and nothing should be built on the assumption
that it does.

- **Toolchain and platform.** The minimum supported Rust version (MSRV) is 1.88, up from
  1.87. outrig builds for Linux on x86-64 and AArch64 only -- its container plumbing calls
  `setns(2)` and `CLONE_NEW*` unconditionally, so any other target stops at a `compile_error!`
  in `lib.rs` rather than failing later. podman 4.3 or newer is required at run time, and the
  matching `<arch>-unknown-linux-musl` target is needed to build `view = "primary"` sidecars.

- **rmcp 1.x -> 3.1**, two major versions. rmcp types are public only where an item exists to
  participate in rmcp's own machinery -- implementing one of its traits, or handing a value
  straight back to it. What survives is eleven items, all under `outrig::mcp_proxy`:
  `ProxyServer`'s dispatch and listing methods, its `ServerHandler` impl, and
  `SUPPORTED_PROTOCOL_VERSIONS`. For those, an rmcp major is an outrig major. Everywhere a
  value carried something outrig reports in its own right, the type is now outrig's:
  `OutrigError::McpService` and `McpToolsListFailed::source` carry `McpSessionError` with an
  `McpFailureKind` (`Transport`, `Protocol`, `Timeout`, `Canceled`, `Other`) and a rendered
  `message`, so match on the kind rather than on rmcp's `ServiceError`. Delete any arm for
  `OutrigError::McpServerInitialize` and any `?` relying on `From<ServerInitializeError>`;
  both are gone, and a caller that drives `serve_server` itself holds rmcp's error directly.
  One coupling is knowingly left: `McpStartupFailure::source` is a boxed `dyn Error` that in
  practice holds rmcp's `ClientInitializeError`, so no signature breaks across an rmcp major
  but a downcast onto that type starts returning `None`.

- **`#[non_exhaustive]` on 92 public types and 45 variants** -- two separate consequences.
  *Construction*: a `#[non_exhaustive]` struct cannot be built with a struct expression from
  outside this crate, and that **includes functional update**, so `..Default::default()` is
  not a workaround. Use the type's `new()` plus its `with_*` methods, or `Default::default()`
  followed by assignment to the fields that are still public. *Matching*: every `match` on an
  outrig public enum needs a `_ =>` arm, and a sealed struct variant needs `..` in its pattern
  even when you bind every field it has today. A sealed variant is unconstructible from
  outside forever, which is why the ones that were previously built downstream -- notably
  `McpServerSpec::Full` and `LlmProvider::OpenAi` -- gained constructors in the same change.

- **`BackingClient` is sealed.** A downstream `impl BackingClient for MyType` no longer
  compiles and has no replacement. Drive `ProxyServer` with `Arc<McpClient>`; a blanket impl
  covers `Arc<T>`, so a `Vec<Arc<McpClient>>` needs no upcast. `error::IoPathExt` is sealed
  the same way -- calling `path_ctx` is unaffected, implementing the trait is not.

- **`ImageTag` is opaque.** It was `pub struct ImageTag(pub String)`. Replace `ImageTag(s)`
  with `ImageTag::new(s)` or `s.into()`, and `tag.0` with `tag.as_str()` when borrowing or
  `tag.into_string()` when you need the `String` itself.

- **Remote providers are constructed through an options struct.**
  `LlmProvider::openai(base_url, api_key, options)` and the new
  `LlmProvider::anthropic(base_url, api_key, options)` take `OpenAiOptions` and
  `AnthropicOptions`; each carries `request_timeout_secs` and the retry configuration that
  used to hang off the enum. `LlmProvider::with_retry_budget_secs` is gone -- it was a silent
  no-op on the in-process variant, which had nowhere to record it. The no-override spelling is
  `OpenAiOptions::new()`. They are deliberately two types rather than one shared one, so
  either provider can grow a setting the other has no meaning for. `request-timeout-secs` is
  now range-checked, and `retry-budget-secs` defaults to 600 with `0` meaning no retries.

- **`LlmProvider::Mistralrs` is a braced variant**, `Mistralrs {}`. Patterns become
  `LlmProvider::Mistralrs { .. }`. It is deliberately *not* `#[non_exhaustive]`, so it stays
  constructible from outside.

- **`Model::provider` is `Option<String>`**, because a row names a provider or other models
  and never both. `Model::new(provider)` and `Model::alias([..])` construct one, and
  `Model::source()` returns a `ModelSourceRef` saying which it is. `source()` **panics on a
  hand-built `Config` that has not been validated** -- it is total only after
  `Config::validate`. Read `provider` and `alias` directly if you must stay total.
  `Model::resolved_model_path(repo_root)` is the one place a relative `model-path` gets its
  base, and that base is the repo root rather than the declaring file.

- **`ExecOptions` replaces the `env` parameter.** `exec_stdio(&argv, &env_map)` becomes
  `exec_stdio(&argv, &ExecOptions::new().with_env(env_map))`, and `exec_capture` joins it with
  the same shape. `with_workdir` sets `--workdir`. Note what an *unset* workdir means: a
  workspace-backed launch sets the working directory on the run, so an exec with no workdir
  lands in the workspace -- on the host-mounted checkout -- not in the image's `WORKDIR`. A
  relative or destructive command needs `with_workdir` unless that is what you meant.
  `Container::create_initialized` took the same treatment, trading seven positional parameters
  for `ContainerCreateOptions::new(image, launch, name)` plus `with_*`.

- **Config paths that carry provenance are private, behind accessor pairs.** `Workspace` has
  `host_path()` / `container_path()` for the effective value -- what was declared, else the
  built-in default -- and `declared_host_path()` / `declared_container_path()` for whether the
  config said anything at all; `set_host_path` and `set_container_path` write them. Only
  `set_host_path` clears the recorded `ConfigSource`, because only the host path is resolved
  against the file that declared it: a hand-set one belongs to no file and resolves against
  the `repo_root` argument instead. `set_container_path` leaves that provenance intact, so
  changing the container path of a workspace loaded from the global config keeps its relative
  `host-path` resolving against the global config's directory. The same rule reaches
  `MountConfig` (all three fields, plus `config_source()` and `resolved_host_path()`),
  `ImageConfig` (`dockerfile()` / `context()`, written together by one `set_build_paths`,
  because a config with one of the pair set is not a shape that exists), and `NetworkConfig`
  (`mode()` / `declared_mode()` / `set_mode`, `policy()` / `set_policy`). The TOML keys are
  unchanged; this is a Rust-source break only.

- **The mount errors reshaped.** `MountRuleViolation` and the five
  `ConfigValidationError::WorkspaceMount*` variants are sealed struct variants, each carrying
  `declared_in: Option<PathBuf>` so an error can name the file to go and edit. Add `..` to
  those patterns. The largest single break is that `WorkspaceMountContainerRoot` stops being a
  unit variant -- it is the case with no path in its message at all, which is exactly why it
  needed the clause. `declared_in` is `None` for a hand-built `MountConfig`, and no repo-config
  fallback is substituted: naming a file that never mentioned the mount would be a fabrication.

- **The bootstrap helper is gone.** `container::direct_bootstrap_supported` was public and has
  no replacement -- it answered whether a host would need `useradd`/`groupadd` inside the
  image, and there is no longer a yes case, because the runtime user is written into the
  container from the host through its own namespaces. The `OUTRIG_BOOTSTRAP` environment
  variable went with it and is inert. `Container::bootstrap_user` is unchanged.

- **`LaunchSpec::from_image_config` is gone; `LaunchSpec::from_config` replaces it**, and the
  shapes differ in three ways at once. 0.1's
  `from_image_config(&image_config, &workspace, repo_root, log_dir) -> Self` was synchronous
  and infallible and took the two config fragments it needed. The replacement is
  `from_config(&config, image_name, repo_root, log_dir).await?` -- `async`, returning
  `Result`, and taking the whole `Config` plus the name of the image-config to lower, since it
  now resolves sidecars and the network block as well. Naming an image-config the config does
  not declare is the error case that makes it fallible.

  **Check the network behavior while you are rewriting the call.** A previous build wrote
  `NetworkSpec::default()` and never read `config.network`, so an embedder whose config
  declared `mode = "audit"` or `mode = "filter"` with an allow list got a session with no
  interceptor attached, and nothing said so. Assume an earlier build enforced nothing here,
  whatever the config said -- including if you arrived through a 0.2 release candidate, where
  `from_config` already existed and the compiler will flag nothing.
  `Config::validate_as_repo` is the new check that a repo-side config declares only `mode`;
  `merge` is infallible and will drop a repo policy rather than report it, so an embedder
  assembling one by hand should call it.

- **Top-level sidecars instantiate by reference.** `[sidecars.<sc>]` is declared once at the
  top level and started only when some `[mcp]` entry names it. Declaring a block instantiates
  nothing, which retires a sidecar that hosts no MCP servers and one whose servers came only
  from its image's `org.outrig.mcp` label. `McpServerSpec::entrypoint_in_sidecar` completes the
  set, so the four placements the TOML can describe are the four an embedder can construct.

- **`McpToolResult::content_text` is `render_text()`**, a method rather than a field, and
  deliberately under a different name: a method spelled like the old field would let a call
  site keep compiling while its meaning changed from "the result" to "one view of the result".
  The rendering is byte-identical for text-only results. A result is now an ordered
  `Vec<McpContent>` beside `structured_content` and `_meta`, and `McpTool` carries the whole
  upstream descriptor.

- **Advertised tool names carry a hash suffix whenever outrig had to change them**, not only
  when they were too long. Anything holding a cached tool list must refresh it. The common case
  is untouched -- `fs__read_file` and `outrig__subagent` are byte-for-byte what they were --
  but a server name ending in `_` or containing `__`, an upstream name outside the permitted
  character set, and every already-suffixed name all move.

- **The implicit preamble is gone**, for embedders reading agent config: an `Agent` that omits
  `preamble` now means no system prompt, where 0.1 substituted a fixed sentence.

- **`style = "mistralrs"` is deprecated but operational.** `LlmProvider::Mistralrs`,
  `MistralrsDeviceSpec`, the six `Model` weight fields, `Config::model_cache_root`, and the
  nine `ConfigValidationError` variants policing them will be removed in a future release --
  not this one. Nothing is removed here and no key changed spelling. Point a `style = "openai"`
  provider at an OpenAI-compatible server on `localhost` instead.

### Added

- **`image::read_image_env(tag, transcript)`** returns a local image's declared `Config.Env`
  as a `KEY` -> `value` map. It is the same `podman image inspect` read `read_image_labels`
  and `read_image_entrypoint_cmd` already make, one field over, and it starts no container.
  An embedder layering its own environment over a per-repository image's `PATH` -- what a
  `view = "primary"` sidecar needs, since it keeps its own image's environment -- no longer
  has to shell out to podman for that one value. The map rather than the raw `KEY=value`
  list is the point: precedence is what the caller is computing. An entry without `=` is
  skipped, and a repeated key takes its last entry.

- **`Model::resolved_model_path(repo_root)`** returns `model_path` made absolute, and is the
  single place the base for a relative one is chosen. `Config::validate` and the CLI's model
  resolution both call it; they used to join the same value independently, against the repo
  root and against the process's working directory respectively, and agreed only when `outrig`
  happened to be invoked from the repo root.

  The base is the repo root rather than the declaring file's directory, which makes
  `[models.<name>].model-path` the one exception to the rule `ConfigSource` states for every
  other config-declared path. `Model` therefore gains no `ConfigSource` and no accessor break.
  The consequence to know: a global `[models.<name>]` with a relative `model-path` resolves it
  under whichever repo is current, so name an absolute path there.

- **MCP tool results carry the protocol's data.** `McpToolResult` is an ordered
  `Vec<McpContent>` -- text, images, audio, embedded resources, resource links -- beside
  `structured_content` and result-level `_meta`, and `McpTool` carries the whole upstream
  descriptor: `title`, `output_schema`, `annotations`, `icons`, `_meta`. `ToolHandle` grows the
  same fields. A 0.2.x consumer may depend on a tool result reaching it, and reaching a client
  connected to `outrig mcp`, with every block in order and nothing but `resultType` normalized.

  **Breaking: `McpToolResult::content_text` the field is gone**, replaced by
  `McpToolResult::render_text()`. It had been the only result model there was, so a consumer
  who shipped against it would have built around a rendering; keeping both a stored string and
  the blocks it was rendered from is two sources of truth that can disagree. The rendering
  itself is unchanged -- same placeholders, same newline joins -- so a transcript recorded
  against 0.1 reads the same. `McpToolResult::ok` and `::error` still build a single text
  block; `McpToolResult::from_content` takes the list.

  The types are outrig's, not the MCP SDK's. That is the boundary this crate holds to: SDK
  types appear in the public API only where an item exists to participate in the SDK's own
  machinery -- `ProxyServer`'s `ServerHandler` impl and the two `RequestContext`-free halves of
  it, plus `SUPPORTED_PROTOCOL_VERSIONS`, which is returned from one. For those, an SDK major
  is an outrig major. Anything outrig reports in its own right is outrig-typed, so an SDK
  upgrade does not reach a caller who only ever handled a tool result.

  A content kind the SDK knows and this build does not is kept as the JSON it arrived as and
  forwarded unchanged, rather than being flattened to a placeholder. A kind the SDK itself does
  not know never arrives: it fails to decode one layer below outrig.

  `resultType` is deliberately not relayed: `ProxyServer` answers `complete`, because `task`
  and `input_required` promise follow-up methods it does not implement.

- **`mcp_proxy::SUPPORTED_PROTOCOL_VERSIONS`**, the ordered list of MCP protocol revisions
  outrig's servers are known to serve correctly, and
  `ProxyServer::supported_protocol_versions` returning it. Both exist so the ceiling on what
  `initialize` may agree to is outrig's own rather than whichever revisions the SDK happens to
  know; see **Fixed** for what the inherited one cost.

- **`OutrigError::Canceled`**, carrying the program and argv of a command that a caller's
  stop signal ended before it finished. Distinct from `Process` (the command ran and exited
  badly) and from `Spawn` (it never started). Receiving it means the child is already dead
  *and* already reaped -- the cooperative path waits for that before it returns.

- **`OutrigError::SidecarNotUnwound`**, carrying a `SidecarUnwindFailure` with the sidecar, why
  it was being torn down, and what tearing it down could not finish.
  `SidecarUnwindFailure::new` builds one from outside the crate, which the CLI needs for the
  sidecars it starts itself. A sidecar whose servers
  fail to start is detached and stopped; when that also fails, a live container is left
  carrying interception no attachment owns, and the caller used to see only the startup error.

- **`Container::stop_or_keep`**, `stop` that hands the container back when it did not stop:
  `None` means it is gone and the handle with it, `Some` carries both the failure and the
  handle. For a caller compensating for an earlier failure, where a container that would not
  stop is still running and dropping the handle removes the last chance to try again in an
  orderly way. The disposition is in the signature so that keeping it is not something a call
  site can forget -- it was forgotten in three of them.

  Both forms now report a stop or a removal that did not work, where every outcome but a
  timeout used to read as success and a timeout read as a completed stop: podman refusing the
  removal left the handle disposed and untracked with nothing retrying it, and a removal that
  never answered was reported as "gone" about a container whose state was unknown. Anything but
  a confirmed removal hands the container back. A filter matching nothing -- what `--rm` having
  already done the work looks like -- is still a success.

  The stop now names the container podman made rather than the name it was asked for. A handle
  kept for a retry can outlive its name -- the container goes away, the name is free, something
  else takes it -- and a retry aimed at the name would stop that one. Removals have been scoped
  to the creating attempt since names were first guarded, on the grounds that a name is a
  request and not a claim; stops had not been.

  That id is the full hex podman prints from the `create` that made the container, and nothing
  else is accepted as one: a wrapper script or an engine with another output format would
  otherwise hand back a *container selector* that every later stop would name. A creation whose
  output carries no id fails while the attempt-label guard is still armed, so what was made is
  removed rather than kept under a handle that cannot name it.

  The stop itself is bounded now. `-t` is how long podman waits for the *container's* processes
  before killing them and says nothing about the client asking for it, so a wedged client or an
  engine that never answered used to hold `stop` for the rest of the session -- on the path of
  every sidecar compensation and every shutdown. It gets the grace the container is owed plus a
  floor for the client, and a timeout hands the container back. That sum saturates: `Duration`
  addition panics on overflow, so a caller passing `Duration::MAX` -- or anything within the
  floor of it -- used to bring the process down before any cleanup ran.

- **`error::superseded_by_a_confirmed_stop`**, which takes a failure and the thing it was
  attached for and returns what is still worth telling a caller once the container has been
  confirmed stopped. A `NetworkAttachNotUndone` says a container may still be carrying
  interception nothing owns; stopping it ends that claim, and handing the error on anyway
  reports obligations against something that no longer exists. Public because the CLI unwinds
  its own sidecars and needs the same rule.

- **`OutrigError::NetworkAuditUnwritten`**, carrying a container, how many of its audit
  records could not be written, the first failure, and -- when the log may hold a partial
  record -- what stopped the writer proving otherwise. A writer that had to be stopped
  reports one of these per attachment whose records it was still holding, with that
  attachment's own count. Both, not one in place of the other:
  they say which record was lost, and whether the file can still be trusted. A count rather
  than an entry per record: a container that can open connections can make the sink fail as
  often as it likes,
  so an outage such as `ENOSPC` would otherwise grow host memory, and the teardown error, for
  as long as it lasted.

- **`OutrigError::NetworkAttachNotUndone`**, carrying a `NetworkAttachFailure` with the
  container, the failure that stopped the attach, and everything undoing it could not put back.
  An attach that fails and is fully undone still returns the plain cause -- the container is as
  it was and the call can be retried. This is the other case, and it is a different thing to be
  told, because the container may still be carrying interception that no attachment owns. The
  residue stays armed for the destructor to reissue, so it is a report rather than the last word.

- **`OutrigError::NetworkConnectionsUnfinished`**, carrying the grace that expired. Distinct
  from `NetworkTasksAborted`, which says an attachment's accept and DNS loops had to be
  stopped: this says a *connection* was still running after that, so a bridge may still be
  moving bytes for a container the caller has been told is detached. Both windows can expire in
  one teardown.

- **`OutrigError::NetworkTeardown`**, carrying a `NetworkTeardownFailure` of
  `NetworkTeardownCause`s -- one per obligation a detach could not discharge, each naming its
  container and boxing the error that stopped it. Detaching runs several independent
  obligations, and one failing is no reason to skip the others, so teardown collects rather
  than short-circuits. `NetworkTasksAborted` and `NetworkTaskPanicked` carry what used to be
  prose: a task that had to be aborted after its grace, and one that panicked.

- **`Config::validate_as_repo`**, the rules that apply to a repo config file rather than to a
  merged one. Today there is one: `[network]`'s `default`, `allow`, and `deny` describe the
  machine's egress and belong to the operator, so a repo config may declare `mode` and
  nothing else. `Config::load` applies it to the repo file it reads; an embedder assembling a
  repo-side `Config` by hand should call it too, because `merge` is infallible and drops a
  repo policy rather than reporting it. The new `ConfigValidationError::RepoNetworkPolicy`
  carries the offending key.

  The rule previously lived in a raw-TOML text scan, so it applied only to configs that came
  from files and could be defeated by formatting; it now reads the parsed value. Keeping its
  fidelity is why every policy key is an `Option` and not a bare value -- a bare
  `NetworkAction` or `Vec` cannot tell an absent key from an explicit `default = "deny"` or
  `allow = []`, so moving off the text without them would have quietly started accepting the
  spellings the scan rejected. `outrig` no longer depends on `toml_edit`; the two text scans
  were its only users.

- **`McpServerSpec::entrypoint_in_sidecar`**, the constructor for an entrypoint-stdio server
  hosted by a sidecar declared under `[sidecars.<sc>]` -- no `command`, because that container's
  own ENTRYPOINT is the server. The config path has always expressed it as
  `fs = { sidecar = "tools" }`, and the library had no way to build it: `exec` always sets a
  command, `entrypoint` always sets the *anonymous* sidecar's `image`, and stacking
  `with_sidecar` on the latter is the `McpPlacementConflict` validation exists to reject. The
  four placements the TOML can describe are now the four an embedder can construct.

  It is the counterpart of `entrypoint`, which gives the server a dedicated anonymous sidecar
  instead, and it is subject to the same rules the parsed form is -- `sidecar` must name a
  declared block, `args` may be declared on the block or the entry but not both. Outrig's own
  built-in default config is exactly this shape, so parity here is not hypothetical: it was
  reached by parsing TOML because the library could not be asked.

- **`[<...>.security]` gains an `unmask` key**, an ordered list of paths excluded from podman's
  default masking, lowered one `--security-opt=unmask=<path>` per entry beside the existing
  `--device` loop. It rides `ContainerSecurity`, `SecuritySpec`, and `ContainerLaunchSpec`, so
  images and sidecars declare it the same way and both `podman run` and `podman create` carry
  it. Default empty: no existing launch changes.

  This is the key that makes a **nested container runtime** possible at all. The kernel's
  "fully visible" rule for procfs (`mount_too_revealing` in `fs/namespace.c`) lets a process in
  a non-initial user namespace mount a fresh `procfs` only while the `/proc` already in its
  mount namespace is unobstructed. Podman's default hardening mounts read-only tmpfs over
  `/proc/acpi`, `/proc/scsi`, and friends, and those mounts are created by a more privileged
  namespace and locked -- so no capability the container can hold will remove them, and an
  inner `podman run` dies at creation with ``crun: mount `proc` to `proc`: Operation not
  permitted``. Measured against podman 5.7 on Linux 7.0, the working recipe is
  `unmask = ["/proc/*"]`, `cap-add = ["SYS_ADMIN"]` (inner-namespace capabilities are bounded
  by the outer set), and `devices = ["/dev/fuse", "/dev/net/tun"]` -- the second device for
  `pasta`, podman 5's default rootless network backend, which an earlier published recipe
  for this omitted.

  Entries reach podman verbatim, so `unmask = ["ALL"]` stays expressible without an image that
  asked for `/proc/*` being silently widened into it. Seven `ConfigValidationError` variants
  come with it, and every one of them exists because the alternative is silence rather than a
  failure: `UnmaskPathEmpty`; `UnmaskPathRelative` (absolute paths and `ALL` only);
  `UnmaskPathListSeparator`, since podman splits an unmask value on `:` and a colon-joined
  entry would expand back into several at launch; `UnmaskPathDuplicate`; `UnmaskPathBadGlob`,
  because podman answers a malformed pattern with a log line and a container whose path is
  still masked; and `UnmaskAllNotCanonical` / `UnmaskAllNotAlone`, because podman lifts the
  *read-only* paths -- the ones that make `/sys/fs/cgroup` writable -- only when `ALL` is
  spelled in exact uppercase and comes first. Lowercase `all` and `["/proc/*", "ALL"]` both
  still clear the masked paths, so they look like a full unmask and are not one. outrig
  rejects them rather than reordering a caller's list behind their back.

  **This retracts the `newuidmap` rationale previously given for `no-new-privileges`.** That
  rationale said a nested rootless podman needs `newuidmap`, a setuid binary, so `no_new_privs`
  must be cleared for it. Under `--userns=keep-id` that never happens: the primary's user
  namespace is owned by the host user, so its owner is inside-UID 1000 rather than 0, and the
  kernel grants
  capabilities in a namespace only to a process whose effective UID *is* the owner -- so
  reaching euid 0 through `newuidmap` gains nothing and fails. With no `/etc/subuid` entry
  podman takes its rootless single-mapping path instead, creating the namespace with a plain
  `unshare` and never calling `newuidmap` at all. That is the path the recipe above runs on,
  with `--security-opt=no-new-privileges` still applied. Adding `/etc/subuid` and
  `/etc/subgid` entries actively breaks nesting by pushing podman back onto the `newuidmap`
  path. The key itself is unchanged and still useful for images that do carry setuid tooling;
  only its stated motivation was wrong.

- **A `[models.<name>]` entry can name other models instead of a provider.** The new `alias`
  key takes one model name (`alias = "opus-5"`) or an ordered list of them
  (`alias = ["opus-5-bedrock", "opus-5-anthropic"]`), and both spellings deserialize through
  the same helper `model-file` already uses. `Model::alias` joins `Model::new` as a
  constructor, `Model::source` returns the new `ModelSourceRef` discriminating the two shapes,
  and `Config::model_candidates` flattens an alias graph to the ordered list of concrete model
  names it stands for.

  An alias *is* a model: it lives in the same table, under one namespace and one lookup, so
  everything that already accepts a model name accepts it unchanged. A separate
  `[model-aliases]` table would have needed a documented precedence rule for a name declared
  in both; inside one table that collision cannot be expressed, so the rule does not need to
  exist.

  Six `ConfigValidationError` variants come with it -- `ModelSourceMissing`,
  `ModelSourceConflict`, `ModelAliasEmpty`, `UnknownModelAliasTarget`, `ModelAliasCycle`, and
  `ModelAliasTooDeep`, the last bounding traversal depth at `MODEL_ALIAS_DEPTH_MAX` (32) so a
  long chain reports a bad config instead of exhausting the stack. The enum is
  `#[non_exhaustive]`, so all six are additive. Unlike the other model rules,
  these are checked on every validation path including `outrig build`'s: they establish an
  entry's *shape* rather than resolve a cross-reference, and until `provider` became optional
  serde's own "missing field" enforced half of it everywhere.

  `Config::model_candidates` is the first method on `Config` that is neither `load*` nor
  `validate*`. It is public because both crates walk this graph -- validation checks it, and
  the binary's resolver selects from it -- and two traversals that had to agree on ordering
  and on cycle handling would be two chances to disagree.

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

  Set it with `OpenAiOptions::with_retry_budget_secs(..)` or
  `AnthropicOptions::with_retry_budget_secs(..)`. The options struct is also what replaces the
  positional timeout argument to `LlmProvider::openai(..)` / `::anthropic(..)`, detailed under
  **Changed**. `LlmProvider`, its `OpenAi` and `Anthropic` variants, and `Config` are
  `#[non_exhaustive]`; `Mistralrs {}` deliberately is not, so it stays constructible.

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
  primary container's filesystem view with `with_view(SidecarView::Primary)`. Both
  `Outrig::add_sidecar` and `LaunchSpec::with_sidecar` accept them, so an embedding program
  reaches the same tool
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

- **`LlmProvider::Mistralrs` is a braced variant**, `Mistralrs {}`, rather than a unit one.
  That is a source break with a one-token migration: a pattern becomes
  `LlmProvider::Mistralrs { .. }` and a construction becomes `LlmProvider::Mistralrs {}`. The
  variant is deliberately **not** `#[non_exhaustive]`, unlike its two siblings, so it stays
  constructible outside this crate and needs no constructor function of its own.

  It buys the one thing a unit variant could not have: `deny_unknown_fields` on the
  internally-tagged enum now has a field set -- an empty one -- to check a `[providers.<name>]`
  block against, so a key written on a `style = "mistralrs"` provider is refused instead of
  silently discarded. It is a pre-0.2.0 break on purpose; after the freeze the same change
  costs a major version, and the surface it repairs is one every other style already had.

- **Breaking: an advertised tool name carries a hash suffix whenever outrig had to change
  it**, not only when it was too long. `sanitize_tool_name` used to apply its blake3 suffix on
  the length path alone, so `("fs", "read/file")` and `("fs", "read file")` both became
  `fs__read_file` and the second one took down `ProxyServer::build` -- and with it the
  session's entire tool list. Both now get a suffix: `fs__read_file_5d2270` and
  `fs__read_file_4e31b7`.

  The rule is one rule: a composition is returned byte-identical when it satisfies
  `^[a-zA-Z0-9_-]{1,64}$` *and* splits back into its pair, meaning its first `__` is the
  separator. Everything else -- a replaced character, an over-long name, a server name that
  itself ends in `_` or contains `__` -- is truncated to fit and suffixed.

  The common case is untouched, so `fs__read_file` and `outrig__subagent` are byte-for-byte
  what they were. Three classes do move, and anything holding a cached tool list should
  refresh it: a tool whose upstream name was never in the character set; *every* tool on a
  server whose own name ends in `_` or contains `__`, which `^[a-zA-Z][a-zA-Z0-9_-]*$` allows
  and which now fails the separator test, so `fs___read_file` becomes `fs___read_file_76c554`
  with no character replaced; and every name that was already truncated and suffixed, because
  the new preimage re-keys it -- `("fs", "x" * 120)` moves from `..._2e60f8` to `..._780fcd`.

  The suffix's preimage is now the length-delimited *pair* rather than `<server>__<tool>`.
  That concatenation is not self-delimiting when either side may contain `_`, so `("a", "_b")`
  and `("a_", "b")` shared a preimage and the suffix could not tell them apart.

  What this can promise is collision *resistance*, not collision freedom: an unbounded pair of
  Unicode strings does not inject into 64 characters. Two unsuffixed names never collide -- that
  form is injective -- but two suffixed ones can, and a suffixed name can land on an unsuffixed
  one with no collision involved at all, when a tool's own name already ends the way a suffix
  does. `ProxyServer::build` no longer fails on either. It re-derives one side's name at a wider
  suffix until it is free and logs both tools' identities, so a clash costs a longer name rather
  than every tool in the session.

  Which side moves is chosen from the two `(server, tool)` identities, not from the order
  `tools/list` returned them in. `tools/list` promises no order, so awarding the contested name
  by arrival would let a restarted backing server rebind it to the other tool -- and a client
  replaying a cached name would then reach a different backend and get a plausible answer
  instead of an error. Listing order is unchanged and still the caller's.

- **Breaking: every config path that carries provenance is now private, behind an accessor
  pair.** `MountConfig::host_path` becomes `host_path()` / `set_host_path`, and
  `ImageConfig::dockerfile` and `context` become `dockerfile()` / `context()` and a single
  `set_build_paths(dockerfile, context)`. `MountConfig::container_path` and `access` move to
  `container_path()` / `set_container_path` and `access()` / `set_access` along with them, so
  the struct reads one way rather than two. `MountConfig::new`, `ImageConfig::from_dockerfile`
  and `ImageConfig::from_image_name` are unchanged.

  These paths are each paired with a `ConfigSource` recorded at load -- the directory a
  relative value resolves against. A public field let a caller replace the value and leave the
  pairing behind, after which the new value resolved against the directory of a file that never
  contained it: a bind mount of the wrong host tree, read-write if the mount says so. The
  setters clear the source, because a hand-set value belongs to no config file and resolves
  against the `repo_root` argument like any other. `Workspace::set_host_path` takes the same
  treatment in the entry below; this is the rest of the rule.

  `set_build_paths` replaces the pair in one call rather than offering a setter per path,
  because one source backs both: clearing it for `dockerfile` alone would rebase `context` from
  the declaring file's directory to the repo root, which is the same bug one field over.
  `ImageConfig::source` already requires the two to be set together.

  The TOML and JSON Schema keys are unchanged -- `host-path`, `container-path`, `access`,
  `dockerfile`, and `context` all still parse, serialize, and appear in the schema published
  through `get_config_schema`. Privatization is a source break for Rust callers only.

- **`NetworkInterceptor::shutdown` returns `Result<()>`.** It previously returned `()` and
  reached `tracing::warn!` with everything that went wrong, so a session could report a clean
  shutdown having failed to remove a container's redirect rules. `detach` kept its signature
  and gained the same honesty: it used to return `Ok(())` unconditionally. The CLI and
  `Outrig::shutdown` log the failure and carry on stopping containers, which is what they
  already did for a sidecar that would not stop.

- **A dropped future no longer leaves its subprocess running.** Every process outrig spawns
  is now owned: dropping the future that holds it -- which is what `tokio::time::timeout`
  and any cancelled task do -- delivers `SIGKILL` **synchronously**, before the drop
  returns, and the reap happens as soon as the runtime is next driven, with no further
  caller involvement. That is the bound; it is measured in single-digit milliseconds in the
  suite, and it is deliberately not an *instant* reap, because `Drop` cannot await and so
  nothing can promise one. A caller that drops a future and then blocks its runtime thread
  will see the process dead but not yet reaped.

  Previously nothing was killed at all: tokio does not kill on drop by default, so a
  cancelled `Container::start`, `exec_capture`, or image build orphaned its podman or
  buildah client. `Container::start` was the worst of them -- no `Container` value exists
  until the run returns, so `Drop for Container` could not compensate.

  Engine-side resources are covered too, which killing a client does not do on its own: a
  cancelled create removes the container it made, a cancelled build removes its temporary
  tag, and a cancelled label-stamping pass removes its buildah working container. All three
  are owned by a scope guard armed *before* the command that creates them, so no instant
  exists at which the resource can be in the engine with nothing responsible for it.

  The container case removes by a **per-attempt label**, not by name: each attempt stamps a
  fresh `org.outrig.attempt` value on the container it asks podman to create, and the guard
  removes by that label. A name is a request, not a claim -- `--name N` fails when N is
  already in use, and that is an ordinary outcome -- so a name-based cleanup on that path
  would destroy a container the call never created. A create that collided made nothing
  carrying the label, so the distinction falls out of the mechanism, and a cleanup still in
  flight cannot reach a same-name container the caller has since started. Labels are applied
  at creation, so there is no interval in which the container exists unidentified.

  A caller cannot set `org.outrig.attempt` itself: it is refused before anything is spawned,
  since podman takes the last `--label` for a key and a duplicate would quietly disable the
  cleanup. outrig also emits its own after the caller's, so neither half has to hold alone.

  Removing by label needs `podman rm --filter`, which arrived in **podman 4.3** -- now the
  documented floor in the quickstart's prerequisites.

- **`Container::exec_stdio`'s child is kill-on-drop.** The signature is unchanged; the
  behavior is not. Dropping the returned `tokio::process::Child` now SIGKILLs the `podman
  exec` client instead of orphaning it. **The reap is the holder's** -- outrig does not
  supervise a child it has handed away -- and, as before, killing the client does not stop
  the process running *inside* the container, which conmon supervises in its own
  namespaces. This is the one deliberate exception to the ownership guarantee above, and it
  is now written down in the rustdoc rather than left implicit.

- **`Container::stop` returns with its `podman rm -f` client confirmed gone.** The removal
  gets `max(grace, 30 seconds)` -- the floor keeps a zero `grace`, which legitimately means
  "do not wait for the container's processes", from reducing the removal to no attempt at
  all, and is set far enough out to separate a client that will never return from one that is
  merely slow on a loaded engine. The bound is applied cooperatively, so the client is killed
  *and reaped* before `stop` returns rather than merely abandoned, and if the budget is spent
  the removal is handed to the supervisor rather than dropped. `stop` is the last thing to touch the
  container name, and a podman client still holding it is how the next run under that name
  fails.

- **A `McpClient` that cannot start its transport reports `Spawn`, not `Io`.** Routing that
  spawn through the shared chokepoint gave it the same labelling as every other: the program
  name, the full argv, and -- for a missing binary -- the pointer at the prerequisites. It
  used to surface as a bare `io::Error`.

- **Detached cleanup commands no longer leave zombies.** `container::force_remove_detached`
  and the interceptor's `Drop`-path nft delete both spawn a command and cannot await it.
  They now hand the reap to a single supervisor thread, so a long-lived embedder accumulates
  one fewer defunct process per cleanup. That thread polls rather than waiting on one child
  at a time, and there is one of it per process rather than one per cleanup, so neither a
  burst of cancellations nor a wedged `podman rm` costs threads or delays anything else.
  Behavior is otherwise unchanged: both are still synchronous, still need no tokio runtime,
  and are still safe to call from a destructor or a panic hook.

- **Breaking: `NetworkConfig`'s four fields are accessors, not public fields.** All four
  are private `Option`s now. Read the
  effective mode -- what was declared, else `default` -- with `mode()`, ask what a config
  actually wrote with `declared_mode()`, and write it with `set_mode()`. The policy keys are
  reached as a unit: `policy()` for the effective `NetworkPolicy`, unchanged and already the
  type every consumer takes, and `set_policy()` to write all three. `impl Default` and the
  hand-written `PartialEq` are gone with the fields, both replaced by derives.

  The `Option` is what a per-key merge and a per-file trust rule both need: `NetworkMode`
  cannot tell an absent key from one written out to the value the default happens to have,
  `NetworkAction` cannot tell an absent `default` from an explicit `default = "deny"`, and
  `Vec` cannot tell an absent `allow` from an explicit `allow = []`. The **Fixed** entries
  below all turn on exactly that distinction. `None` now *is* "the config did not declare
  this", carried by the type through serde like every other key, rather than by a
  `#[serde(skip)]` bit only the file loader could set.

  Making a public field private is a break `#[non_exhaustive]` does not cover, so it is free
  before the 0.2.0 freeze and costs a major version after -- the same trade the `Workspace`
  accessors took, and for the same reason.

- **`merge` cannot apply a repo config's network policy, whatever built that config.** It
  reads the repo's declared mode and nothing else, so the operator-owned policy is safe by
  construction rather than by a check that has to run. The signature stays infallible.

- **Breaking: the MCP SDK's error types are off the public surface.** `OutrigError::McpService`
  and `OutrigError::McpToolsListFailed::source` carried `rmcp::service::ServiceError`, so an SDK
  major was a break on every fallible call in the crate -- `tools/list` and `tools/call` reach a
  caller through `error::Result` like everything else does. Both now carry
  `error::McpSessionError`: an `error::McpFailureKind` -- `Transport`, `Protocol`, `Timeout`,
  `Canceled`, `Other` -- beside the SDK's own rendering, kept verbatim as `message`.

  A caller that printed the error sees no change; the wording and the prefixes are what they
  were. A caller that matched the SDK's enum branches on `kind` instead and gets a
  classification that survives an SDK upgrade. The mapping needs a wildcard, because the SDK's
  enum is `#[non_exhaustive]`, so a variant a future SDK adds reads as `Other` until the mapping
  is revisited -- an obligation that now rides along with the one
  `mcp_proxy::SUPPORTED_PROTOCOL_VERSIONS` already carries on every SDK bump.

  This finishes the boundary the tool-result entry above states. The SDK remains on the surface
  in exactly eleven places, every one of them under `mcp_proxy`: `ProxyServer`'s `ServerHandler`
  impl, the two `RequestContext`-free halves of it, and `SUPPORTED_PROTOCOL_VERSIONS`. Each
  exists to participate in the SDK's own machinery, and **an SDK major is an outrig major for
  those -- and, now, for nothing else that this crate's signatures name.**

  One opt-in coupling survives and is worth knowing about rather than discovering:
  `McpStartupFailure::source` is a `Box<dyn Error + Send + Sync>` that in practice holds the
  SDK's client-handshake error. Nothing in a signature says so, so an SDK major does not break a
  build -- but a consumer who downcasts it is depending on the SDK's major, and gets a silent
  `None` rather than an error when that moves. Reading it, or its rendering, costs nothing.

- **Breaking: `error::IoPathExt` is sealed.** It is the internal helper that turns a
  context-free `io::Error` into `OutrigError::Path`, and it has exactly one receiver,
  `Result<T, io::Error>`. Nothing outside this crate plausibly implemented an IO-error context
  helper, and sealing it means a second required method is an addition rather than a break.
  Calling `path_ctx` is unaffected.

- **Breaking: `LlmProvider::openai` and `LlmProvider::anthropic` now take an options
  struct**, `OpenAiOptions` and `AnthropicOptions` respectively, instead of a positional
  `request_timeout_secs`. Both structs also carry `retry_budget_secs`, so
  `LlmProvider::with_retry_budget_secs` is removed -- a retry budget can no longer be handed to
  the in-process `Mistralrs` variant, which had nowhere to record it and discarded it in
  silence. Migrate a constructor with no overrides by replacing its third argument with
  `OpenAiOptions::new()` or `AnthropicOptions::new()`, and one that set a timeout with
  `OpenAiOptions::new().with_request_timeout_secs(secs)`; `with_retry_budget_secs(secs)` on
  either options type replaces the removed `LlmProvider` method. The structs are
  `#[non_exhaustive]`, so later connection settings can be added without changing the
  constructor signatures again -- which also means the `with_*` setters, not the public fields,
  are how a downstream crate builds one.

- **Breaking: `Model::provider` is now `Option<String>`.** A model entry has two mutually
  exclusive shapes -- a provider that serves it, or an `alias` naming other models -- so the
  field that identifies the first cannot be required. This follows `ImageConfig` exactly,
  which has carried `image-name` XOR `dockerfile`+`context` in one table since 0.1: every
  field `Option`, exactly-one-shape enforced by validation, and a discriminated accessor
  (`ImageConfig::source`, now joined by `Model::source`) as the way readers ask which shape
  they got.

  `Model::new(provider)` is unchanged and still the way to build a provider-shape model, so
  the common construction path does not move. Readers of the field take a one-line migration:
  `model.provider` becomes `model.provider.as_deref()` compared against `Some("...")`, or a
  `match model.source()` where the shape matters.

  Taken now rather than deferred because it is a field *type* change, which the
  `#[non_exhaustive]` sweep does not make additive the way it does field and variant
  additions. It rides the breaking changes already in this section rather than forcing a new
  one; after 0.2.0 it would have had to wait for the next major.

  One consequence worth stating: `Model` carries `deny_unknown_fields`, so a config using
  `alias` is rejected outright by an older outrig rather than degrading. That is the correct
  behavior, and it makes a shared repo config with an alias in it a breaking change for
  collaborators who have not upgraded.

- **Breaking: an exec can name the directory it runs in, and the four exec methods now take an
  `ExecOptions` instead of a bare environment map.** `Outrig::exec_stdio`, `Outrig::exec_capture`,
  and the two `Container` methods behind them previously took `(&[String], &BTreeMap<String,
  String>)` and had no way to say where the command should run. They now take `(&[String],
  &ExecOptions)`, where `ExecOptions::with_workdir` becomes `--workdir <path>` on the
  `podman exec` and `ExecOptions::with_env` carries what the map used to. Omitting the directory
  emits no flag, so an exec that does not ask for one issues the `podman exec` it always has.

  Note what "no flag" actually means, because the docs got this wrong at first and it is
  load-bearing: the exec inherits the container's configured working directory, which is the
  image's `WORKDIR` only when nothing overrode it. A workspace-backed session sets `-w` to the
  workspace's container path on the run, so an unset exec runs *in the workspace*, on the
  mounted checkout. Set the directory explicitly if a relative or destructive command must not
  land there.

  Without this a caller wanting a build to run in the checkout had three bad options: wrap the
  command in `sh -c 'cd ... && ...'`, which defeats the argv form that exists so a shell-less
  image stays usable and pushes quoting onto the caller; set `PWD`, which changes the variable
  without moving the process, so `getcwd` never notices; or require every path to be absolute,
  which does not help a tool that resolves relative paths itself.

  The environment moved inside the struct rather than staying a third parameter.
  `ContainerCreateOptions` already holds its `env` that way, so keeping it out here would have
  meant env is in the bag on create and beside it on exec; a timeout and a tty flag are the
  foreseeable next knobs and would all land inside. Rust has no default arguments, so leaving
  `env` in place would have broken every call site anyway without buying source compatibility.
  `ExecOptions` is `#[non_exhaustive]`, so those later fields are additive. It lives in
  `outrig::container` next to `ContainerCreateOptions` and is re-exported at the crate root,
  since `Outrig`'s methods name it.

  A directory the container does not have stays podman's error to report. It surfaces the way
  any failing exec does -- a non-zero `Output::status` with podman's message, which names the
  path, on stderr -- not as an `Err`. Validating existence up front would cost an extra exec on
  every call to pre-empt a case podman already handles.

- **Breaking: `Workspace::host_path` and `Workspace::container_path` are accessors, not public
  fields.** Both are private `Option<PathBuf>` now. Read the effective value -- what was declared,
  else the built-in default -- with `host_path()` / `container_path()`, and ask what a config
  actually wrote with `declared_host_path()` / `declared_container_path()`; `set_host_path` and
  `set_container_path` write them. The *hand-written* `impl Default for Workspace` is gone with
  the fields; the trait is not. `Workspace` derives it, and the derived value means something the
  hand-written one did not: it declares neither path, where the old one declared `.` and
  `/workspace`. That distinction is load-bearing rather than cosmetic -- a `Workspace` declaring
  nothing inherits both fields on merge, which is what stops a repo file with no `[workspace]`
  table from shadowing a global one that had it. `Workspace::new(host, container)` is unchanged
  and remains the way to build one that declares both.

  The `Option` is what per-key merge needs: `PathBuf` cannot tell an absent key from one written
  out to the value the default happens to have, and the merge fix below turns on exactly that
  distinction. Leaving the fields public would then have let a caller replace a `host-path` while
  leaving behind the `ConfigSource` it is paired with, resolving the substitute against a
  directory it never came from -- and the primary mount is read-write. The setters clear that
  provenance; only a *declared* path carries any, since the built-in `.` belongs to no file.

  Making a public field private is a break `#[non_exhaustive]` does not cover, so it is free
  before the 0.2.0 freeze and costs a major version after -- the same trade the `Model::provider`
  entry above takes.

- **Breaking: `request-timeout-secs` is now range-checked**, closing an asymmetry with its
  sibling `retry-budget-secs`, which has validated against `RETRY_BUDGET_SECS_CEILING` since it
  landed. A remote provider's `request-timeout-secs` must be between `1` and the new
  `REQUEST_TIMEOUT_SECS_CEILING` (`3600`, the same hour as the budget's ceiling); both bounds
  are inclusive. Breaking because a config 0.1 accepted -- any `u64` at all -- can now be
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
  an image path error does. Global and repo `[[workspace.mounts]]` lists are
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

### Deprecated

- **`style = "mistralrs"` and the config surface behind it**: `LlmProvider::Mistralrs`,
  `MistralrsDeviceSpec` and `MistralrsDeviceParseError`, the six `Model` weight fields
  (`model_id`, `model_path`, `model_file`, `revision`, `context_length`, `device`),
  `Config::model_cache_root`, and the nine `ConfigValidationError` variants that police them.
  They will be removed in a future release -- not this one.

  This is the library half of the deprecation `outrig-cli` announces for the config surface;
  every item above lives here, so both crates record it. Nothing is
  removed, no key changed spelling, and a config naming this style still parses and validates.
  Run local models under an OpenAI-compatible server and point a `style = "openai"` provider at
  its `localhost` `base-url`; the migration is in
  [In-process LLMs](../../doc/concepts/in-process-llm.md).

### Removed

- **Breaking: `OutrigError::McpServerInitialize`, and the `From<ServerInitializeError>` that
  built it.** The library never produced either. An initialize failure happens while *serving*
  MCP, and the library does not serve: a consumer driving `mcp_proxy::ProxyServer` calls the
  SDK's `serve_server` itself and already holds the SDK's error. The variant existed so that
  `outrig-cli`'s `?` would compile, and it has moved there, onto a type that is not published
  surface. Nothing in this crate could return it, so no `match` on `OutrigError` loses an arm it
  could reach.

- **The `podman exec` user-bootstrap fallback, and the `OUTRIG_BOOTSTRAP` environment
  variable.** The runtime user is now written into the container from the host,
  through the container's own namespaces; the older `getent` / `groupadd` / `useradd`
  chain remained only for hosts that cannot enter those namespaces, which means a podman
  service on another machine. OutRig does not support that topology -- the built-in default
  image-config alone declares two `view = "primary"` sidecars, which cannot work against a
  remote engine -- so the fallback kept one subsystem limping where nothing else would run.

  **Breaking:** `container::direct_bootstrap_supported` was public and is gone. It answered
  "will this host need `useradd`/`groupadd` in the image", a question that no longer has a
  yes case. `Container::bootstrap_user` is unchanged, and its failures are unchanged in
  kind -- only in that a namespace-entry failure is now reported rather than absorbed.

- **Breaking:** the `McpToolResult::content_text` field, replaced by the
  `McpToolResult::render_text()` method. The rendering is byte-identical for a text-only
  result, so the migration is mechanical -- but it is deliberately a method under a different
  name rather than a renamed field, because a call site that kept compiling would have had its
  meaning move from "the result" to "one view of the result". The blocks it was rendered from
  are the result now; see the entry under **Added**.

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

- **The panic-hook sweep could remove a container outrig never created.** The last-resort
  cleanup layer tracked container *names* and swept them with `podman rm -f <name>`. A name
  is a request, not a claim: `podman run --name N` failing because N is already in use is an
  ordinary outcome, so a panic arriving between the name being reserved and the start guard
  dropping force-removed whatever held N -- another session, a stray, a container made by
  hand. Every other cleanup layer had already moved to the per-attempt `org.outrig.attempt`
  label; the sweep now replays that same removal and holds no name it could remove by.

  The registry it sweeps is also keyed by the attempt token rather than by the name. It was a
  set of names, so two starts asking for one name collapsed into one entry and whichever
  finished first discharged the other's obligation -- leaving a container outrig had made with
  no last-resort cleanup behind it. Two attempts are now two obligations.

  This is reachable without a name collision. `start` passes `--rm`, so after a stop whose
  removal timed out the container is likely gone and its name free, and a panic in that window
  swept whatever had since taken the name.

- **The network interceptor installed no rules at all on nft 1.0.9**, which is what Ubuntu
  24.04 ships. The redirect script was one `create table inet <t> { chain output { ... } }`;
  nft parses that, exits zero, creates the table, and silently drops the nested block. The
  result was a table with no chain in it, so nothing was redirected to the interceptor: in
  `audit` mode no connection was ever recorded, and in `filter` mode every connection was
  allowed, including the ones a `default = deny` policy exists to refuse. `attach` and
  `detach` both reported success throughout.

  The script is now a flat sequence -- `create table`, then `add chain`, then one `add rule`
  per rule -- which installs the same ruleset and keeps both properties `create` was chosen
  for: it still fails rather than merging if a table of that name already exists, and `nft -f`
  is a single transaction either way, so a failure leaves nothing behind.

  Nothing in the unit suite could see this. `nft_rules` was checked by asserting the generated
  text *contained* each rule, which both forms satisfy, and no test ran nft. It surfaced the
  first time the `e2e` suite was executed against a live engine rather than compiled: seven of
  the eight `network_interceptor` tests failed, every one of them waiting for an audit record
  that was never going to arrive. That suite now runs in CI on every pull request, on x86-64
  and AArch64.

- **A hostname rule grants only against a bound destination.** `CompiledNetworkEntry::matches`
  accepted a hostname pattern when *either* the destination address matched or the name the
  client announced in `Host:` or SNI did. The second disjunct is supplied by the party being
  filtered, so under `mode = "filter"` with `default = "deny"` and
  `allow = ["allowed.example:443"]`, a container could open a connection to an unrelated
  address, announce `allowed.example`, and be bridged to it. `SECURITY.md` names failure to
  enforce a host:port policy as in scope, so the enforcement half of the interceptor did not
  hold the property it claimed. The `ip` and `cidr` allow forms carried the same disjunct and
  lost it too.

  What a client claims and what was resolved for it are now separate values that cannot be
  recombined by accident: the deny list is walked with the client's assertion and the allow
  list without it, so the rule -- a claim may cost a client its own connection and may never
  buy it one -- is one line of code rather than a convention to be remembered.

  Name-to-address bindings replaced the session-global cache behind this. They are created per
  attachment, so one container's lookup no longer grants another authority over an address;
  they are keyed address to name to expiry, so shared hosting keeps every name rather than the
  latest lookup erasing the rest; their TTLs come from the answering record, clamped to between
  30 seconds and an hour; and the table is capped. A DNS answer is validated before it binds
  anything -- it has to arrive from the resolver the query went to and echo the transaction id,
  question, and QR bit, and the receive loop waits out its whole timeout rather than taking the
  first packet to land on the ephemeral port. Truncated and error responses are still forwarded
  for the container's stub resolver to retry, but authorize nothing. Addresses are attributed
  by record owner through the CNAME chain and always bind under the queried name, so an
  authority for one name cannot mint a binding for another by aliasing to it. Decoded names are
  validated, which the allow-side property depends on: a wire label is a counted byte string
  and may legally contain a `.`, so an unchecked one could otherwise forge a parent domain.

- **`LaunchSpec::from_config` applies the `[network]` block it is handed.** Neither 0.1's
  `from_image_config` nor `from_config` as it stood during the 0.2 candidates read
  `config.network`: both lowered `[workspace]`, `[security]`, the image source, MCP placement,
  and sidecars, and then wrote `NetworkSpec::default()`. A library caller whose config
  declared `mode = "audit"` or `mode = "filter"` with an allow list got a session with no
  interceptor attached, so the auditing or filtering they configured was not running and
  nothing reported that. Anyone embedding outrig through `from_config` should assume a previous
  build enforced nothing here, whatever their config said.

  The lowering is `impl From<&NetworkConfig> for NetworkSpec`, beside the existing
  `ContainerSecurity` conversions and for the same reason: both types are `#[non_exhaustive]`,
  so a caller's own copy of the mapping would keep compiling while dropping a key added later.
  It reads the effective mode, and carries the merged global policy only in `filter` mode --
  a spec holding rules in `default` or `audit` would arm itself the moment a caller assigned to
  the public `mode` field, and the interceptor supplies audit's own allow-everything policy
  anyway. There is still no equivalent of the CLI's `--network` override; the builders
  `with_network_mode` and `with_network_filter` remain the way to change the mode after
  lowering.

- **A failed or interrupted `NetworkInterceptor::attach` leaves the container as it found it.**
  `attach` rewrote the container's `/etc/resolv.conf` to point at its DNS listener and only
  then applied the nft redirect table, so a failure in between left a *running* container
  resolving to a loopback port with nothing behind it -- DNS dead, silently, and an error
  returned that said nothing about it. Cancellation was worse: dropping the future anywhere
  between the first rewrite and the `attachments` insert left both the resolver and the table
  owned by nothing at all, since nothing had yet recorded that either was owed.

  The resolver is read before it is written, and every change is armed for undo before it is
  made. The undo list is one value that *moves*: built on `attach`'s own stack, moved into the
  `Attachment` on success, moved on into teardown. There is no release step and so no window
  between "the change is made" and "something owns its inverse" -- a move cannot be interrupted
  by a cancellation, which is what makes the property structural rather than a rule about where
  an `.await` may go. On any awaited path the undos run awaited and their failures are
  reported; on any unawaited one, `Drop` hands each to `supervise::detach_cleanup`, which is
  synchronous and runtime-free and therefore still completes when the runtime the caller was
  running on is being torn down underneath it -- the case an embedder, which owns that runtime,
  is most likely to produce.

  `nft -f` commits a file as one kernel transaction, so the table a failed apply would have
  created never exists, and the undo for it is idempotent besides. A container that has already
  exited is asked about through `/proc/<pid>/ns/net`: it took its namespace, its table and its
  `/etc` with it, so nothing is owed and nothing is reported.

  What teardown removes is the table it created. The apply runs with `--echo --handle`, so the
  kernel reports the handle it assigned in the same transaction that created the table, and the
  undo is narrowed to `nft list table inet <name> ; delete table inet handle <n>` -- one
  invocation, so one transaction, and therefore a check rather than a race. A handle is never
  reissued, so a table deleted and recreated under outrig's name by anything else with
  `NET_ADMIN` in the namespace no longer answers to it. That is the only selector the removal
  carries: nftables numbers table handles per network namespace and never reissues one, so
  within a namespace a handle names the table that transaction created or it names nothing.
  Which namespace is a separate question, and one outrig now answers before issuing any undo --
  it records the namespace instance behind `/proc/<pid>/ns/net` before it arms anything, and a
  pid that has since been handed to another container reads the same as one whose process is
  gone. The removal carries that check into the namespace with it, asking `/proc/self/ns/net`
  once it is inside rather than trusting an answer from before `nsenter` resolved the pid.

  This requires nft 0.9.0 or later for `--echo`, which is no newer than the `create table`
  outrig already depends on. Ownership comes from that transaction and from nowhere else: an
  apply whose echo cannot be read -- no echo, output that is not UTF-8, a format that moved --
  fails the attach and removes nothing: a name stops being this attach's the moment the
  transaction that created it commits and makes it visible, so a removal that can only name its
  target is not one to issue, then or later. The table is reported to the caller as left in
  place, which is what makes it recoverable by hand.

- **`detach` ends every connection it started, and says so.** The accept loop spawned each
  bridged connection and dropped the handle, and no cancellation token reached it, so `detach`
  cancelled two loops, deleted the nft table and returned while connections went on moving
  bytes and appending audit records for a container the interceptor had declared detached.
  Connections are now held in a `JoinSet` the accept loop owns and does not return without
  draining, and the whole of each one runs under its attachment's token -- so the sniff read, a
  stalled upstream connect, the replayed opening bytes and a mid-stream copy are all covered by
  the same cancellation. A connection that is cut is still recorded, before `detach` returns
  rather than after. The DNS loop's forwarding await is cancellable for the same reason; it
  could previously hold that loop for `DNS_TIMEOUT` per resolver, well past the grace teardown
  allows, so a detach racing an in-flight lookup reported a failure that had not happened.

  Termination is cancel, a grace, abort, then an unconditional join. It previously wrapped each
  `JoinHandle` in a `tokio::time::timeout` and dropped the expired result, which *detaches* a
  task rather than ending it -- so a wedged loop was reported as joined -- and spent the whole
  grace per task rather than across them. Finished connections are taken back out of the
  carrier as they complete, so an attachment that serves a long session does not accumulate one
  handle per connection it has ever served.

- **`detach` restores the resolver it replaced**, so attach and detach are a genuine inverse
  pair rather than a one-way door. The bytes ride back as the restoring command's own argument
  rather than quoted into a shell script, so a resolver containing anything at all comes back
  exactly. A container with no resolver file gets none back, which is a state to restore and
  not an error, and one whose resolver was baked in at `podman create --dns` is the deliberate
  exception: nothing was rewritten, so nothing is restored.

- **A non-zero `nft delete` is no longer read as success.** The teardown path used
  `try_capture_logged`, which does not check exit status.

- **The resolver is mutated by a process outrig owns, not by `podman exec`.** `podman exec`
  starts the writer under conmon, so killing the client -- which is all a dropped future can do
  -- leaves it running. Measured against podman 4.9.3: a `podman exec` whose client was killed
  went on to complete its write two seconds later, which a rollback racing it loses, leaving the
  container pointing at a listener that was never installed. The read, the install and the
  restore now run through `nsenter -t <pid> -U -m`, which execs the shell directly, so killing
  it kills the writer. The same check under `nsenter` left the write undone.

- **An attach will not adopt an nft table it did not create.** A plain `table` block merges
  into an existing table rather than failing -- measured, a second apply took the chain count
  from one to two and exited zero -- so a stale table from a crashed run of the same session,
  or an operator's own, used to be merged into on the way in and deleted whole on the way out.
  The table name now carries a per-attach random tail, so a table by that name is one this
  attach created; `create table`, which fails rather than merging, backs it up. A preflight
  check was not enough on its own, because another actor in the same namespace can create the
  name between the check and the apply.

- **An undo will not act on a namespace it was not aimed at.** These commands name a namespace
  by pid and the kernel hands pids out again, so an undo delayed past its container's exit --
  by a slow command ahead of it in the chain, or a destructor firing late -- could write one
  container's resolver into whatever holds that pid now. The resolver undos fire only if the
  file still holds what this attach installed, which also means they will not clobber a
  resolver something else legitimately changed. What it looks for is a marker carrying the
  attach's own table name, written into the installed resolver as a comment, so a pid reused by
  *another* outrig container does not satisfy it. The check is plain shell: the obvious
  spelling used `cmp`, which a minimal image need not ship, and a missing one exits 127 --
  which `|| exit 0` reads as "not ours", skipping the undo while `detach` reports success. The
  nft delete needs no such guard: its table name is unique to the attach, so there is nothing
  to find in a stranger's namespace. What the undo compares is the whole installed text, not
  just the marker in it, so a resolver something has legitimately changed since is left
  alone rather than reverted -- including one that differs only in a terminal newline, which
  needs a sentinel inside the comparison because command substitution strips them, and the
  file's own byte count is checked alongside its text, since shells differ on what they do
  with an embedded NUL. A resolver
  that cannot be *read* fails the undo rather than retiring it, since an unreadable file and
  one belonging to someone else are not the same
  thing; an absent one is checked separately, because that is the case that genuinely owes
  nothing.

- **The commands a destructor hands over run in order.** `supervise::detach_cleanup_chain`
  takes an ordered list and starts each command only once the one before it has ended, where
  submitting them one at a time spawned independent children that raced. The redirect has to
  go before the resolver that was pointed at it, or the container is left resolving through a
  rule aimed at a listener that is gone. They are sequenced, not conditional: a command that
  fails does not cancel the rest of its chain.

- **Resolver states that could not be put back are refused before anything is changed.** A
  resolver that is a symbolic link to a file that does not exist reads as absent, so installing
  would follow the link and create its target while the undo would remove the link. A resolver
  containing a NUL byte, or larger than 64 KiB, cannot be carried in the argument the restore
  puts it back with -- `execve` refuses the first outright and caps the second. Each is now an
  error from `attach` before the resolver is touched, rather than an undo discovered to be
  unrunnable after it.

- **An audit record is written whole or not at all.** The log has one writer, a task that owns
  the file; producers queue a record over a bounded channel and wait for it to be written, so
  no caller's cancellation reaches the bytes and a stalled log applies backpressure rather than
  growing. Teardown drains the writer rather than treating a joined connection as proof its
  record landed. A write that fails partway is rolled back to where the file ended before it,
  because `write_all` is a retry loop and not an atomic commit -- a leftover prefix would make
  every record appended after it unparseable, costing the file rather than the record. That
  rollback truncates, so the writer claims the file with an exclusive `flock` for its lifetime
  and a second interceptor pointed at the same log is refused rather than allowed to have its
  records destroyed by the first.

  "Written" means in the file and readable, not synced: the acknowledgement a producer waits
  for does not survive the host losing power, and nothing here promises that it would.

- **An audit record that could not be written is reported.** `detach` treats a connection's
  task returning as proof its record landed, which was only true if a failed write was kept
  rather than logged and dropped. The sink retains them, stamped with their container, and
  teardown collects them once the tasks are joined.

- **A repo `[network].mode` set programmatically or by direct serde is now honored.** Only
  `Config::load_from_str` could mark a `[network]` block as declared -- it re-parsed the
  file's raw text to do it -- so an embedder who built a repo `Config` in memory with
  `mode = "audit"`, or deserialized one with `toml::from_str`, merged to `default`: no
  interception at all, and nothing to indicate the setting had been dropped. The
  declaration-blind `PartialEq` meant the two configs compared equal, so a test could not
  have caught it by comparison either.

- **A bare `[network]` table no longer disables a global filter.** The declaration test was
  "does the file contain a `[network]` table", so a repo config consisting of nothing but the
  table header counted as declaring a mode and overwrote the global one with
  `NetworkMode::Default`, the least restrictive mode. A table that declares no `mode` now
  declares nothing and inherits. A repo that means to opt out still writes `mode = "default"`
  explicitly, which is honored as before.

- **`[network].mode` survives a serialize/reparse round trip.** `NetworkConfig::is_default`
  compared through the declaration-blind `PartialEq`, so a config that wrote `mode = "default"`
  was byte-identical to one that wrote no `[network]` block and got dropped by
  `skip_serializing_if` -- losing an explicit opt-out on the way back out.

- **`tools/list` carries the cache metadata protocol revision `2026-07-28` requires.** That
  revision adopted SEP-2549, which makes `ttlMs` and `cacheScope` mandatory on list results.
  `ListToolsResult::with_all_items` leaves both `None`, and both are
  `skip_serializing_if = "Option::is_none"`, so neither ever reached the wire and a conforming
  client rejected the response outright -- not a short tool list but no tools at all, every
  backing server unreachable through a proxy that had started perfectly. The proxy now answers
  `ttlMs = 300000` and `cacheScope = private`: private because the union is specific to one
  session's config, image-config, and `--env` overrides, and five minutes because although the
  table is frozen at `ProxyServer::build` time and no `listChanged` capability is advertised --
  so a far longer window would still be truthful -- a config edit or a rebuilt image ought to
  be picked up by the next session.

  No outrig source had to change for this to start happening. `supported_protocol_versions`
  defaults to every revision the SDK can name, so upgrading rmcp to 3.1 moved the ceiling
  onto `2026-07-28` underneath a server that did not satisfy it. `ProxyServer` now overrides
  the method with `SUPPORTED_PROTOCOL_VERSIONS`, and a client asking for a revision outside
  that list is answered with the server's own default -- `2025-11-25` today -- rather than in
  one outrig has never served. That is a fallback, not a step down: a request older than
  anything listed is answered in a newer revision, not an older one. Adding an
  entry there is an assertion that the servers meet that revision's requirements, which makes
  it a deliberate step on an rmcp upgrade instead of a silent one.

- **`resources/list`, `prompts/list`, and `resources/templates/list` say method-not-found**
  instead of answering. `ProxyServer` advertises `tools` only, but advertised capabilities do
  not gate dispatch: rmcp answered all three from default handler bodies with an empty,
  successful result. That claimed a surface the proxy does not have, and from `2026-07-28` the
  default result was malformed in exactly the way `tools/list` was -- `resultType` present,
  `ttlMs` and `cacheScope` absent. A capability-respecting client never asked, which is why this
  went unnoticed; the three methods now return `-32601`, matching the capability set.

- **A global `[workspace]` block is no longer thrown away.** `host-path` and `container-path` in
  `~/.outrig/config.toml` were parsed, validated, merged, and then discarded in silence: merge
  took the repo's `Workspace` whole and combined only `mounts`, and `Workspace` is
  `#[serde(default)]`, so a repo config with no `[workspace]` table at all still contributed a
  default that beat the global every time. A machine-wide `container-path = "/src"` reached
  nothing. The reference called this "repo-owned as a block", which reads as *repo overrides
  global when both are set* rather than *global is unreachable*, and the two descriptions diverge
  in precisely the case someone writing that stanza expects to work.

  The two primary fields now merge per key -- a repo declaration wins, then a global one, then
  the built-in default -- which is what every other top-level scalar already does, and it lets a
  repo override one field without forfeiting the other. Extra `workspace.mounts` keep their
  existing asymmetric merge, global entries first and then repo, which was already deliberate.
  `Config::load` resolves the global config path to an absolute one before reading it, so a
  `host-path` a global config declared relatively resolves against the directory that file
  actually lives in. `[workspace]` is now the one block that merges per key rather than by name.

- **A `view = "primary"` sidecar on a Debian/glibc base no longer dies on SIGSEGV with an
  empty stderr.** The visible failure was `mcp server "..." failed to start: connection
  closed: initialize response` with `exit: code 139` and nothing to read, which is as close to
  no diagnosis as a failure gets.

  `setns(CLONE_NEWNS)` moves the launcher's *root directory*, not only its mounts, so an
  absolute symlink met under the graft afterwards resolves in the primary's rootfs rather than
  the sidecar's. Debian's `/lib64/ld-linux-x86-64.so.2` is such a link where Ubuntu's is
  relative, so `outrig-enter` exec'd the *primary's* dynamic loader and handed it the
  *sidecar's* `libc.so.6` off `--library-path`. ld.so and libc.so.6 are one version-locked
  unit; the mismatched pair corrupts itself during early startup, before either can write to
  fd 2. Against a musl primary the same escape merely `ENOENT`s -- the identical defect, with
  a legible message.

  `outrig-enter` now resolves every path the exec will later open *by name* -- the program,
  its `PT_INTERP` interpreter, and each `--library-path` entry -- while the sidecar's own
  rootfs is still `/`, so a path that survives to the exec means the same thing on both sides
  of the namespace join. The interpreter is also confirmed present at that point, since a
  missing one is legible before the setns and a segfault after it. Two smaller consequences:
  loader search directories the image does not have are dropped rather than passed dead, and a
  dynamically linked `PROGRAM` named by a relative path is refused rather than exec'd as the
  nonsense `/mnt./server`. Statically linked payloads are untouched -- they exec from a
  descriptor and resolve nothing after the setns, which is why they were never affected.

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
