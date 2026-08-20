# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`mcp_proxy::SUPPORTED_PROTOCOL_VERSIONS`**, the ordered list of MCP protocol revisions
  outrig's servers are known to serve correctly, and
  `ProxyServer::supported_protocol_versions` returning it. Both exist so the ceiling on what
  `initialize` may agree to is outrig's own rather than whichever revisions the SDK happens to
  know; see **Fixed** for what the inherited one cost.

- **`OutrigError::Canceled`**, carrying the program and argv of a command that a caller's
  stop signal ended before it finished. Distinct from `Process` (the command ran and exited
  badly) and from `Spawn` (it never started). Receiving it means the child is already dead
  *and* already reaped -- the cooperative path waits for that before it returns.

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

### Changed

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
  entry in 0.2.0-rc.2 took, and for the same reason.

- **`merge` cannot apply a repo config's network policy, whatever built that config.** It
  reads the repo's declared mode and nothing else, so the operator-owned policy is safe by
  construction rather than by a check that has to run. The signature stays infallible.

### Fixed

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
  defaults to every revision the SDK can name, so upgrading rmcp 2.2 -> 3.1 moved the ceiling
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

## [0.2.0-rc.2](https://github.com/tgockel/outrig/releases/tag/outrig-v0.2.0-rc.2) - 2026-08-12

A second release candidate, cut because the cycle kept breaking the public surface after rc.1
went out. Everything here is measured against **0.2.0-rc.1**; a consumer still on 0.1.0 should
read that section first.

The breaks an rc.1 consumer hits, all detailed under **Changed**: `LlmProvider::openai` and
`::anthropic` take an options struct instead of a positional timeout (and
`with_retry_budget_secs` moves onto it), `Model::provider` is `Option<String>`, the four exec
methods take an `ExecOptions` in place of a bare environment map, `Workspace`'s `host-path` and
`container-path` are accessors rather than public fields, and the five `MountRuleViolation`
variants are struct variants carrying the file that declared the mount.

### Added

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
  `pasta`, podman 5's default rootless network backend, which the previous entry on this
  subject missed.

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

  **This retracts the rationale published with the `no-new-privileges` key below.** That entry
  said a nested rootless podman needs `newuidmap`, a setuid binary, so `no_new_privs` must be
  cleared for it. Under `--userns=keep-id` that never happens: the primary's user namespace is
  owned by the host user, so its owner is inside-UID 1000 rather than 0, and the kernel grants
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

### Changed

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
  emits no flag, so an exec that does not ask for one is byte-identical to what 0.2.0-rc.1 ran.

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
  `set_container_path` write them. `impl Default for Workspace` is gone with the fields, while
  `Workspace::new(host, container)` is unchanged and remains the way to build one.

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

### Fixed

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
