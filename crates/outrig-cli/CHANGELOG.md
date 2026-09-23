# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **`--config .agents/outrig/config.toml` takes `.` as the repo root**, as its `./`-prefixed
  spelling always did. A relative path of exactly three components derived the empty path
  instead, so a bare `model-path = "local.gguf"` validated clean and then reached the mistralrs
  loader with an empty directory, which it looks up on Hugging Face rather than opening.

## [0.2.0](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.2.0) - 2026-09-23

The first release since 0.1.0. Everything below is measured against **0.1.0**, and
**Migrating from 0.1** is the ordered list of what a 0.1 config or command line has to change
-- including the two breaks that announce themselves nowhere.

### Migrating from 0.1

Everything a 0.1 config or command line has to change, by name. Most announce themselves --
the config fails to parse, fails to validate, or the session says so on stderr. Two do not and
want checking by hand: a `[sidecars.<sc>]` block that no `[mcp]` entry names now simply does
not start, with nothing said about it, and an agent that omits `preamble` stops sending the
sentence 0.1 supplied on its behalf.

- **Toolchain and platform.** The minimum supported Rust version (MSRV) is 1.88, up from
  1.87. outrig builds for Linux on x86-64 and AArch64 only; any other target stops at a
  `compile_error!` rather than failing somewhere in the container plumbing, and on Windows the
  supported arrangement is WSL2. podman 4.3 or newer is required at run time. Building
  `view = "primary"` sidecars additionally needs the matching `<arch>-unknown-linux-musl`
  target installed -- without it the built-in default degrades rather than failing, with `fs`
  falling back to a read-write bind mount of the workspace and `shell` unavailable.

- **`outrig design` is `outrig design prompt`.** The bare form was never a command and exits
  with a usage error.

- **Sidecars are declared at the top level** as `[sidecars.<sc>]` rather than inside an
  image-config, and one starts only when some `[images.<name>.mcp]` entry names it. Declaring
  a block instantiates nothing on its own, which retires two shapes: a sidecar hosting no MCP
  servers at all, and one whose servers came only from its own image's `org.outrig.mcp` label.

- **An agent that omits `preamble` sends no system prompt.** 0.1 substituted a fixed sentence
  that appeared in no config and could not be turned off -- `You are a careful assistant whose
  tools run inside a sandboxed container.` Paste it into `preamble` to keep it. Agents that
  set `preamble` are unaffected, and so are subagents.

- **An alias fails over mid-turn.** A `[models.<name>]` entry naming other models is no longer
  fixed to one endpoint at startup: a failing request moves to the next candidate under a
  budget shared across the chain. If you relied on a name resolving to exactly one endpoint,
  declare it with a single target, which takes the pre-failover path unchanged.

- **A repo config may declare only `mode` under `[network]`.** `default`, `allow`, and `deny`
  in a repo file are a validation error. The rule predates this release, but it was applied by
  scanning the config text and a differently formatted table could slip past it; it now reads
  the parsed value, so a repo policy that previously went through is refused. A `[network]`
  table with no `mode` inherits the global mode instead of resetting it to `default`.

- **`style = "mistralrs"` is deprecated, not removed**, along with the `local-llm`, `cuda`, and
  `metal` features, the six `[models.<name>]` weight keys, `model-cache-root`, and
  `outrig run --device`. Everything still parses, validates, and runs. Two things do change:
  a provider block carrying a key this style never read -- `base-url`, `request-timeout-secs`,
  `retry-budget-secs`, or an outright typo -- now fails to load instead of being discarded,
  and a relative `[models.<name>].model-path` is opened against the repo root rather than the
  process's working directory, so one written in a *global* config follows whichever repo is
  current. Give that one an absolute path. The replacement is an OpenAI-compatible server on
  `localhost` behind a `style = "openai"` provider.

Config file names and locations are unchanged, and no key not listed here changed spelling.

### Added

- **`outrig clean --session <id>`**, which narrows both sweeps to one session. Only that
  session's record is considered for removal, and only containers labeled
  `org.outrig.session=<id>` are stray candidates. The sweep is machine-wide without it, and a
  stray is defined by the *absence* of a record -- so a container whose record lives under a
  different `--session-root` (another checkout, a parallel CI job) has no record the sweep can
  see and is removed as a stray. The default is unchanged.

- **`[<...>.security]` accepts `unmask`**, so a session container can host a container runtime
  of its own. `outrig run` carries the key from the selected image-config onto the primary and
  from each `[sidecars.<sc>.security]` block onto that sidecar. Combined with
  `cap-add = ["SYS_ADMIN"]` and `devices = ["/dev/fuse", "/dev/net/tun"]`, `unmask = ["/proc/*"]`
  is what lets an agent build an image or run a throwaway container inside its own session --
  see [Nested container runtimes](../../doc/concepts/containers.md#nested-container-runtimes),
  which also retracts the `newuidmap` rationale that page previously gave for
  `no-new-privileges`.

- **`[models.<name>]` entries can be aliases**, so `--model`, `default-model`,
  `[agents.<name>].model`, and a subagent's `model` argument all accept a name that stands
  for one other model or for an ordered set of provider-equivalent ones. The session picks
  the first candidate this build can reach -- provider defined, style compiled in, `api-key`
  variable set and non-empty -- which is what lets one committed config serve a laptop with
  `ANTHROPIC_API_KEY` and a CI runner holding a Bedrock role.

  Selection happens at startup and answers "am I configured for this" rather than "is this
  endpoint up": building a remote client does no network I/O, so nothing here can tell a
  rate-limited endpoint from a healthy one. Whether an endpoint answers is settled per
  request instead, by the chain failover below. An alias none of whose candidates this build
  is configured for ends the session listing every candidate and the distinct reason each was
  skipped.

  Attribution follows the name: the banner, the subagent launch trace, the subagent tool
  result, and the transcript header all render `alias -> concrete` when a hop was taken. A
  direct model name prints byte-for-byte what it printed before.

  `--device` is refused for an alias spanning more than one candidate -- it selects hardware
  for one in-process model, and an alias may span styles. A single-target alias takes it.

- **An alias fails over mid-turn.** A request that fails against one candidate moves to the
  next from inside the same model call, so the turn continues rather than ending. The move is
  announced on stderr (`[outrig] model <a> failed (<reason>); trying <b>`), and a chain with
  nothing left reports every candidate and the distinct reason each failed -- ending the turn
  if any of those reasons was recoverable, and the session only if all of them were terminal.

  Three properties govern what it costs. The retry budget is **shared across the chain**: one
  deadline is armed per model call and every candidate's retry loop is bounded by what remains
  of it, so three candidates cannot each spend a full `retry-budget-secs` against one outage.
  A fresh request **restarts at the head** of the chain, because the list is a preference order
  and a single rate-limit window should not quietly demote it for the rest of the session. And
  **completed tool calls are never replayed**: tools run between model calls and never inside
  one, so moving candidates within a call re-runs nothing that already happened in a container.

  A chain of one takes the pre-failover path byte for byte -- same retry stack, same errors,
  same output. The banner lists the fallbacks up front, so the ordering is visible before a
  turn needs it.

  The chain's `retry-budget-secs` comes from the first *remote* candidate's provider, which is
  not always the first candidate: the lookup skips `style = "mistralrs"` rows, because an
  in-process provider has no such key and rejects one. So a local-first alias takes its budget
  from the first OpenAI- or Anthropic-style row after the local ones, and writing
  `retry-budget-secs` onto the local provider to influence the chain does not configure it --
  it fails to load.

- **Subagents.** An agent can launch another agent in the same session through the built-in
  `outrig__subagent` tool, which occupies a reserved `outrig__` namespace alongside the
  documentation tools rather than coming from any MCP server. A subagent gets its own turn
  loop and reports back through a `set_result` protocol fragment composed into its preamble.
  `subagent-depth-max` bounds how deep the nesting goes and `subagent-width-max` how many one
  agent may hold at once; the launching agent may name the `model` its subagent runs under,
  and a release list naming an unknown subagent is rejected whole rather than half-applied.

- **A built-in default image-config**, so a session that names no image no longer fails.
  `--image`, `agents.<n>.image`, and `default-image` gain a fourth rung below them, which
  makes `outrig run` work in a repo with no `.agents/outrig/config.toml` at all -- a global
  config that resolves a model is all it needs. The two errors it replaces
  (`no --image or default-image configured` and its three-rung sibling) are gone. What gets
  injected is ordinary config, at the bottom of the precedence order:

      [images.outrig-default]
      image-name = "docker.io/library/buildpack-deps:bookworm-scm"

        [images.outrig-default.mcp]
        fs    = { sidecar = "outrig-default-fs" }
        shell = { sidecar = "outrig-default-shell" }

  `buildpack-deps:bookworm-scm` is the smallest `docker.io/library` image with `git`, `curl`,
  and `ca-certificates` that sets no `ENTRYPOINT` and bakes in no user. Both servers run as
  `view = "primary"` sidecars, so the commands `shell` spawns resolve in the primary's
  filesystem -- which is why the primary is the container that needs `git`. `fs` is pulled
  from `docker.io/mcp/filesystem:latest`; `shell` is built from a Dockerfile outrig writes
  into the user cache directory, never into your repo. All three are reachable by name from
  both `--image` and `outrig build`, so the first-run cost can be paid deliberately:
  `outrig build --image outrig-default`, then `--image outrig-default-fs`, then
  `--image outrig-default-shell`.

  `outrig-default`, `outrig-default-fs`, and `outrig-default-shell` are reserved names.
  Declaring any one of them shadows the built-in entirely -- injection is all-or-nothing,
  since a half-injected set is a broken config -- and outrig says which file took the name.
  `outrig build --all` skips the built-in: it means the image-configs you declared.
  `default-image = "outrig-default"` remains an error, because that key is validated before
  the built-in is injected; you never need to write it.

  Without the `outrig-enter` launcher (a build with no `<arch>-unknown-linux-musl` target)
  the built-in degrades instead of failing: `fs` switches to `workspace = "rw"`, giving the
  same file tools over a bind mount, and `shell` is dropped. A bind-mounted shell would see
  none of the primary's toolchain, so reporting a different environment than the one you have
  is worse than reporting none.

- **outrig's own documentation tools in a built-in-default session.** The eight tools
  `outrig mcp self` serves -- `outrig__list_docs`, `outrig__get_doc`,
  `outrig__get_config_schema`, `outrig__list_base_images`,
  `outrig__list_mcp_server_suggestions`, `outrig__validate_dockerfile`,
  `outrig__validate_config`, `outrig__validate_image_toml` -- are offered to the agent
  directly, beside `outrig__subagent`. They are pure host-side functions over embedded data,
  so there is nothing to install in an image. They appear **only** when the session fell
  through to the built-in default, which is exactly the user who has not written a config
  yet; a configured repo's tool list is unchanged. `outrig mcp self` is untouched and remains
  the surface for external authoring tools.

- **`args` for entrypoint-stdio MCP servers**, on an `[images.<name>.mcp]` entry or on the
  sidecar block it names, so an off-the-shelf image that takes its configuration positionally
  (`docker.io/mcp/filesystem` and most of the MCP catalog) runs from config alone:

      [images.coding.mcp]
      fs = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }

- **A named sidecar can be an entrypoint host** -- omit `command` on the entry naming it and
  that container's ENTRYPOINT is the server, which is how such a server gets a workspace view,
  mounts, or its own security policy.

- **Agents can run against Anthropic's native Messages API** by selecting a
  `style = "anthropic"` provider. Turns take the same non-streaming path as `openai`, with
  the MCP tool loop, tool-call cap, tool-result truncation, conversation history,
  sidecar-added tools, and subagents all unchanged; only the wire format differs. The banner
  reports `provider: anthropic`.

  Anthropic requires an output-token ceiling on every request, and outrig always sends one:
  `max-tokens` from the agent or the model if set, otherwise the published ceiling for a
  Claude identifier it recognizes, otherwise a fallback of `32768` announced once on stderr.
  That last tier covers an older model, a proxy's own naming, and any Claude newer than the
  pinned rig release; it errs high on purpose, since a model whose real limit is lower rejects
  the request and names that limit, whereas too low a ceiling truncates replies with nothing
  logged. `outrig config init` offers `anthropic` as a provider style -- defaulting to
  `https://api.anthropic.com` and `ANTHROPIC_API_KEY` -- and prompts for `max-tokens`
  alongside the model identifier, so a generated config carries an explicit ceiling anyway.

  Where a missing ceiling does still surface as an error, outrig words it itself rather than
  passing through the provider's `` `max_tokens` must be set for Anthropic ``: the key in
  config is `max-tokens`, under `[models.<name>]` or `[agents.<name>]`, and the message says
  so.

- **`retry-budget-secs`**, at the top level and on any remote provider, bounds how long a
  transiently-failing LLM call keeps retrying. Defaults to `600`; `0` disables retries; the
  ceiling is `3600`. A provider's own value wins over the top-level one, which wins over the
  default -- rate limits belong to the endpoint, so per-provider is usually the right place.

- **MCP sidecar containers** -- an `[images.<name>.mcp]` entry can declare `sidecar = "<sc>"`
  to run the server in a sidecar declared at the top level as `[sidecars.<sc>]`, or an inline
  `image` to give it a dedicated anonymous one. A declared sidecar carries its own image,
  workspace access, mounts, and security; a referenced `start = "auto"` sidecar comes up with
  the session, and `start = "manual"` waits for `/sidecar add`. Declaring a block starts
  nothing on its own -- see **Changed**.
- **Entrypoint-stdio servers** -- an `image` with no `command` runs that image's `ENTRYPOINT`
  as the MCP server, so off-the-shelf MCP images work without repo-side command knowledge.
  The `--env` overlay applies to them at container-create time.
- **`/sidecar` in the REPL** -- `/sidecar list` shows the declared sidecars and their status;
  `/sidecar add <name>` starts a `start = "manual"` sidecar mid-session, and its tools join
  the running agent.

### Changed

- **The `outrig__subagent` tool no longer advertises a model whose `api-key` variable is
  unset or empty.** The schema's `enum` and the alias selector are now one predicate, so a
  name is offered exactly when a launch could reach it. Previously an unset key was
  advertised and failed at launch. An alias is offered when *any* of its candidates is
  reachable, which is what lets `alias = ["opus-local", "opus-anthropic"]` stay launchable
  in a build without `--features local-llm` where naming `opus-local` directly would not be.

- **An endpoint that never answered gets a much shorter leash.** While every attempt against a
  candidate has failed to connect -- nothing has come back from it at all -- the retry loop is
  bounded at 30 seconds rather than by the whole `retry-budget-secs`, and each attempt's
  connect phase is capped at 10. A misconfigured `base-url` or an unreachable host now reports
  in well under a minute instead of spending the full budget on a socket that was never going
  to open. The short bound lifts the moment the endpoint answers anything, an error included,
  because at that point the failure is the provider's and the configured budget is the right
  one. It is also what makes an alias chain cheap to walk past a dead candidate.

- **A dropped future no longer leaves its subprocess running.** Interrupting a session used to
  leave podman and buildah children behind to finish on their own, so a cancelled build could
  still be writing layers after the command that asked for it returned. Cancellation now
  reaches the subprocess, and a command cut short reports `OutrigError::Canceled` naming the
  program and argv rather than an exit status it never collected.

- **A repo config may declare `mode` under `[network]`, and nothing else.** `default`, `allow`,
  and `deny` in a repo file are a validation error naming the key. The rule is not new, but it
  used to be applied by scanning the config text, which a differently formatted table could
  slip past; it now reads the parsed value, so a policy that a repo file previously smuggled
  through is refused. A `[network]` table that declares no `mode` also inherits the global one
  instead of resetting it to `default`.

- **The MCP SDK moved to rmcp 3.1**, two majors on from the 1.x this project shipped in 0.1.0;
  the intermediate 2.x step is folded into it. The revisions `outrig mcp` and `outrig mcp self`
  advertise are an explicit list of outrig's own rather than whatever the SDK knows, so an SDK
  bump cannot silently move the ceiling out from under a client; see **Fixed**.

- **`validate_dockerfile` now flags `ENTRYPOINT` rather than a missing `CMD`** (`outrig mcp
  self` and the `rig` self tool). OutRig appends `sleep infinity` after the image reference, so
  the container's command comes from OutRig and the image's `CMD` is overridden -- a Dockerfile
  that sets a different one, or none at all, was never the problem the old warnings described.
  The `cmd_missing` warning is gone, `cmd_may_exit` becomes `cmd_ignored` and says the `CMD` has
  no effect rather than that the container may exit, and a new `entrypoint_takes_args` warning
  covers the instruction that does break a primary image: podman appends trailing arguments to
  an exec-form `ENTRYPOINT` instead of replacing it, so such an image runs
  `<entrypoint> sleep infinity`. The documentation and `outrig design prompt` carried the same
  mistake and are corrected to match.

- **`outrig run` no longer needs an agent.** With neither `--agent` nor `default-agent`, the
  session starts against `--model` (or `default-model`) and `--image` (or `default-image`) and
  sends no preamble -- the same shape `outrig mcp` has always had, now with an LLM attached.
  Every agent-level knob falls through to its top-level default, subagents stay enabled, and
  the banner leads with `model:` in place of the usual `agent:` line. Declaring
  `[agents.<name>]` is still how you attach a preamble or per-agent limits; it is just no
  longer the price of admission. A `default-agent` that names nothing remains an error.

- **Breaking (config):** sidecars are declared at the top level as `[sidecars.<sc>]`, not
  `[images.<name>.sidecars.<sc>]`. Move the blocks up a level; the keys are unchanged. The old
  form is now an unknown-field parse error. One block can be shared by several image-configs,
  and the global config can declare sidecars a repo references.
- **Breaking (config):** a sidecar starts only when an `[images.<name>.mcp]` entry names it.
  Declaring a block no longer starts it -- with blocks shared and global, it cannot.

- **Breaking (config): an agent that omits `preamble` now sends no system prompt.** outrig used
  to fill the gap with a fixed sentence ("You are a careful assistant whose tools run inside a
  sandboxed container."), which appeared nowhere in config and could not be turned off. Unset
  now means unset, so an agent that was relying on that text needs to spell it out. Agents that set
  `preamble` are unaffected. Subagents are unaffected too -- their preamble is composed from the
  `set_result` protocol fragment plus whatever the parent passes.

- **Transient LLM failures are retried against a time budget, and honor `Retry-After`.**
  Retrying used to mean two attempts roughly a second apart with the server's own guidance
  ignored, which is not enough for a rate limit measured in minutes. outrig now retries until
  `retry-budget-secs` runs out, waiting exactly as long as a `Retry-After` header asks (both
  delta-seconds and HTTP-date forms, clamped at 300s) and falling back to jittered exponential
  backoff otherwise. Each retry prints the wait and the budget spent so far. The retried set is
  unchanged: `408`, `425`, `429`, `5xx`, timeouts, and connection errors.

  Reading the header meant moving the retry into outrig's own `http_client` implementation:
  rig's error type keeps a status and a body and drops every header, so nothing above that
  layer can see it. Retries still replay exactly one HTTP request, so no already-executed
  container tool call is repeated.

  A side effect worth having: the old wrapper cloned the whole `CompletionRequest` -- chat
  history, every tool definition and its JSON schema -- on *every* model call, including the
  overwhelmingly common one that succeeds first try. Replaying at the HTTP layer clones a
  header map and bumps a refcount on the serialized body instead.

- Upgraded the LLM/agent stack: `rig-core` 0.39 -> 0.40, plus routine dependency bumps
  (anyhow, ignore, jiff, rand, toml). rig 0.40's `max_turns` now
  counts total model calls rather than tool-call rounds; outrig compensates so the per-turn
  tool-call limit behaves as before.
- Egress policy and the audit log cover every sidecar container, not just the primary.
- `outrig clean` reads one `podman ps -a` instead of a `podman inspect` per aged session, and
  coalesces stray-container removal into a single `podman rm -f`.
- Slash commands run through one dispatcher. `/help` output is unchanged, but two edge cases
  of the old exact-match arms are gone: a trailing-space `/quit ` now executes, and
  tab-separated `/sidecar` arguments parse.

### Deprecated

- **The `local-llm` Cargo feature and the `style = "mistralrs"` provider**, along with the
  `cuda` and `metal` features that select a backend for them, the six `[models.<name>]` weight
  keys (`model-id`, `model-path`, `model-file`, `revision`, `context-length`, `device`), the
  top-level `model-cache-root`, and `outrig run --device`. They will be removed in a future
  release -- not this one. This release *carries* the deprecation: the style parses, validates
  and runs as it always has, and the two defects it had been shipping are fixed rather than
  left standing for a surface on its way out. The earliest a removal can land is the release
  after the one that first puts this warning in users' hands.

  **Nothing changes today.** A build with `--features local-llm` still runs in-process models,
  every config still parses and validates, and no key changed spelling. What changed is that
  outrig now says the feature is going away, at three moments: `cargo build --features
  local-llm` emits a build warning, loading an in-process model prints a one-line warning per
  model, and the error a default build already raised for `style = "mistralrs"` now names the
  deprecation and the replacement alongside the `--features local-llm` flag that still works.

  Run local models under an OpenAI-compatible server -- [Ollama](https://ollama.com), vLLM, or
  `llama.cpp`'s server -- and point a `style = "openai"` provider at its `localhost`
  `base-url`. Migration, before and after, is in
  [In-process LLMs](../../doc/concepts/in-process-llm.md).

  Two consequences worth stating plainly. Running a local model well is a problem with good
  dedicated tools, and outrig was a worse place to solve it: the backend roughly triples the
  dependency count, and outrig's config duplicated weight, quantization, and device knobs those
  servers expose better. And outrig is **giving up a property it claimed** -- that a question
  never crosses a process boundary. A localhost server does serialize payloads over a socket.
  That trade is deliberate, and the documentation retracts the argument it made against
  localhost servers rather than quietly dropping it; the unimplemented egress filter, tool-use
  filter, and prompt-injection scanner that were the stated consumers of the property will have
  to answer the question on their own terms.

### Removed

- **The `OUTRIG_BOOTSTRAP` environment variable**, along with the `podman exec` user-bootstrap
  fallback it selected. The runtime user is written into the container from the host, and that
  is now the only path.
- **The `user_bootstrap_package_missing` warning** from `validate_dockerfile` (`mcp self` and
  the `rig` self tool). It advised installing `passwd`/`shadow` on hosts that would fall back
  to `useradd`/`groupadd`; with no fallback there is no such host. The tool no longer runs a
  `podman info` probe to decide, so it answers without touching podman at all.

- **Breaking (library):** this crate's internals are no longer a public path. `builtin_tool`,
  `cli`, `config_init`, `error`, `hf`, `image_setup`, `init`, `llm`, `mcp_self`, `repl`,
  `rig_tool`, `session`, `session_tool`, and `subagent` were `pub` only so the integration tests
  in `tests/` could reach them, which the crate's own module doc has always said. They are now
  crate-private unless the `internal-test-api` feature is on, which only those tests enable.
  `CliError`, `LlmResolveError`, `ResolvedProvider`, `ResolvedAgent`, `MistralrsWeights`,
  `RigAgent`, and `resolve_agent_with_overrides` were the growth points this closes -- all of
  them gained fields, variants, or parameters during `0.2`.

  The published surface is now `outrig_cli::run() -> ExitCode`, which runs the CLI and returns
  its exit code. Nothing else is covered by SemVer. Depend on the `outrig` crate for a supported
  Rust API; **the `outrig` command-line interface itself is unaffected.**

### Fixed

- **The session watcher reaped sidecars by container name.** When the primary container dies
  out from under outrig, the watcher removes that session's sidecars -- and it asked podman for
  them by name. Sidecars die at the same moment the primary does and `--rm` frees a name on the
  spot, so a removal resolving a name a moment later could reach whatever had taken it. Each
  sidecar now carries an `org.outrig.instance` label, unique per container and per session, and
  the reap selects on that. A failed reap is now reported rather than discarded, the way
  `outrig clean` reports one. (`outrig clean`'s own stray sweep still removes by name, and
  still has to: a stray's session is over, so nothing is left holding the label it was
  started with.)

- **`outrig clean` reported removing stray containers it had not removed.** The sweep coalesces
  its removals into one `podman rm -f <name>...`, and printed `removed container <name>` for
  every name in the batch on the strength of that one exit status. podman exits zero having
  skipped a container another process is tearing down at the same moment, so the line was a
  claim about the batch rather than about the container -- and it was self-perpetuating: the
  container clean said it had removed joined the next sweep's batch and was skipped again.

  Each name's outcome now comes from re-reading the container list. A name the batch skipped is
  retried on its own, which removes it; one that is still there afterwards is reported as
  `could not remove container <name>` and makes `outrig clean` exit **1** instead of 0, so a
  script checking the status no longer reads a partial sweep as a complete one. The batch
  remains the common path and the per-name retry runs only when something survived it.

- **`--network audit` recorded nothing and `--network filter` refused nothing** on hosts with
  nft 1.0.9, which is what Ubuntu 24.04 ships. The interceptor's redirect table was created
  empty, so no container traffic ever reached it. Fixed in `outrig`; see that crate's
  changelog for what the script does now and why no unit test could have caught it.

- **`style = "mistralrs"` rejects unknown keys**, closing the one hole in this schema's
  "unknown keys are an error" rule. The provider was a serde *unit* variant, so
  `deny_unknown_fields` had no field set to check against, and every key written on one of
  these blocks -- `retry-budget-secs`, `request-timeout-secs`, `base-url`, an outright typo --
  parsed clean and was discarded. Aiming a remote-only setting at an in-process provider
  through TOML was the last surviving way to have one vanish rather than be refused; the
  Rust-side spelling had already gone.

  The provider table is unchanged -- `style` and nothing else -- so the only configs this stops
  loading are ones that were carrying a key outrig never read.

- **A relative `[models.<name>].model-path` is opened against the repo root**, which is the base
  it has always been *validated* against. The resolver copied the row's text into the runtime
  weights unjoined and the mistralrs loader opened it relative to the process's working
  directory, so a config that validated clean named a different file -- usually no file --
  whenever `outrig` was started from anywhere but the repo root. Both halves now go through one
  library call, `Model::resolved_model_path`.

  This stays the one path in the schema that is repo-relative rather than relative to the file
  that declared it. A *global* `[models.<name>]` with a relative `model-path` therefore follows
  whichever repo is current; give that one an absolute path.

- **A hostname allow-rule grants only against a bound destination.** Under `mode = "filter"`
  with `default = "deny"`, a rule like `allow = ["allowed.example:443"]` was matched against
  the name the *client itself* announced in `Host:` or SNI as well as against the address the
  connection was going to. The client half is attacker-controlled, so a container could open a
  connection to an unrelated address, claim to be `allowed.example`, and be bridged to it --
  the enforcement half of the interceptor did not hold the property it advertised, and
  `SECURITY.md` names failure to enforce a host:port policy as in scope.

  A name now authorizes a destination only when this attachment's own DNS listener validated
  it for that address. What the client claims is kept apart from what was resolved and is
  consulted on the deny list only: a claim may cost a client its own connection and may never
  buy it one. The `ip` and `cidr` allow forms lost the same client-asserted disjunct.

  The bindings behind it are per attachment rather than session-global, so one container's
  lookup no longer grants another authority over an address; they are keyed address to name to
  expiry, so shared hosting keeps every name rather than the latest lookup erasing the rest;
  their TTLs come from the answering record, clamped to between 30 seconds and an hour; and
  the table is capped. DNS answers are validated before they bind -- right resolver,
  transaction id, question, and QR bit -- addresses are attributed through the CNAME chain to
  the name that was queried, and decoded names are checked, since a wire label may legally
  contain a `.` and an unchecked one could forge a parent domain.

- **A provider response outrig cannot use ends the turn, not the session.** A reply that
  decodes into nothing outrig can turn into a turn is retried a couple of times and then
  reported as `the model returned a response outrig could not use (<detail>)`, leaving the
  history untouched so the prompt can simply be sent again. This is a different class from the
  rate-limited or unreachable provider the retry stack already handled -- that one is a
  transport or status failure, this one is a well-formed response with an unusable body
  -- and it used to take the whole session down.

- **A turn that produced only reasoning is reported rather than swallowed.** A response with
  no text and no tool calls, but with reasoning content, reached the user as pure silence: the
  agent layer concatenates the final turn's text parts, which is the empty string here, and
  the REPL printed nothing at all. A minute of billed generation was indistinguishable from
  outrig ignoring the prompt. The structured turn is now salvaged and shown, the streaming
  path is covered too, and a turn that genuinely finished with nothing to display says so and
  names the finish reason and any ceiling in force.

- **A session record outlives the schema that wrote it.** `outrig ls` failed outright with
  `missing field image_config_name` against any session started before that field was renamed,
  and `clean`, `logs`, and `discard` broke identically because all four read the same listing.
  The field is now optional and accepts its old name, so an old record keeps its real value
  rather than degrading to a blank. A `session.json` that still cannot be read costs its own
  row and a report instead of the whole command: `ls` distinguishes an empty session root from
  one where nothing parsed and exits 0 either way, `clean` counts an unparsable entry as
  surviving rather than treating its container as record-less and force-removing it, and
  naming one broken session by id still fails, because that is a request for that session.

- **A subagent launched under another model takes that model's `max-tokens`.** The launching
  agent's ceiling used to be copied onto the subagent whatever model it named, so an expensive
  parent delegating to a cheap one sent a ceiling the cheap model refuses -- and every turn of
  that subagent failed, not merely long ones. That is precisely the case the `model` argument
  exists for: cheap models generally serve fewer output tokens than expensive ones. The ceiling
  now follows the model, resolved the way any other turn resolves it
  (`[agents.<name>].max-tokens`, else the named model's `[models.<name>].max-tokens`).
  `temperature`, `tool-call-max`, and `tool-result-max` still come from the launching agent --
  those are agent knobs, where an output-token ceiling is a model one. A launch that names no
  model is unchanged.

  Relatedly, the stderr line that names the ceiling when `set_result` is cut off used to print
  the *parent's* number for such a subagent, which was never the one in effect.

- **A configured `max-tokens` above an Anthropic model's published ceiling is lowered to it**
  rather than sent and refused. The Messages API rejects an over-ceiling request outright, so the
  whole turn failed where a capped one runs. Only applies to identifiers outrig recognizes a
  ceiling for; for the rest the configured value still travels whole, since there is nothing to
  cap against and guessing would be wrong for exactly the newest models.

- **A rate-limited or unreachable provider ends the turn, not the session.** A transient
  failure that outlived the retries used to escape the REPL loop, tear down the containers,
  and exit `1` with the conversation lost -- so a two-second rate limit cost the whole session.
  It now prints what happened and returns you to the `>` prompt with history unchanged, to
  resend when the window clears. Genuine faults -- a bad API key, a malformed config -- still
  exit `1`. A subagent round that hits this still reaches its parent as a failed round.

- **`outrig mcp` and `outrig mcp self` list their tools again to a client speaking protocol
  revision `2026-07-28`.** That revision requires `ttlMs` and `cacheScope` on every list
  result, both servers omitted them, and the client's answer to a malformed `tools/list` is to
  retry a few times and then give up -- so the session came up clean, reported the servers
  ready, and offered the agent nothing to call. A client on an older revision was unaffected,
  which is what made this look like a client bug rather than outrig's.

  The two servers scope their answers differently. `mcp self` is `public`: its tool set is
  compiled into the binary, identical for every user of a given outrig, and any intermediary
  may cache it. `mcp` is `private`: the list is the union of one session's backing servers.
  Both declare a five-minute window. Both also advertise an explicit list of revisions they
  serve, so a client asking for something newer is negotiated down instead of being answered in
  a revision outrig has never spoken.

  Both also stop answering `resources/list`, `prompts/list`, and `resources/templates/list`,
  which they never had a resource or a prompt to put in. rmcp replied to all three from default
  handler bodies with an empty success -- a surface neither server advertises, malformed on
  `2026-07-28` in the same way `tools/list` was. They now return method-not-found.

## [0.1.0](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.1.0) - 2026-06-26

### Added

- **Standalone image projects** -- `outrig image init` scaffolds a project whose build
  output is a reusable container image, `outrig image build` builds and verifies it, and
  `outrig image inspect` (local and `--remote`) reads its OCI labels without pulling
  layers or starting a container. Image config is stamped into labels rather than a baked
  file, and repo-local build images are named after their `[images.<name>]` config.
- **Design helper** -- `outrig design prompt --standalone` generates an AI-assisted prompt
  for standalone image projects.
- **Session management** -- `outrig clean` bulk-removes stopped session records with a
  30-day default retention window, preview/confirm, and `--older-than` / `--yes` controls.
- **Run ergonomics** -- a `run --model <NAME>` override, config-less `run`/`mcp` backed by
  the global config plus an explicit `--image`, and raw local image refs accepted for
  `--image`.
- **Resilient LLM calls** -- transient endpoint failures (request timeouts, dropped
  connections, HTTP 408/429/5xx) are retried with bounded exponential backoff, and the
  OpenAI provider's `request-timeout-secs` is now actually applied.
- Renamed the in-process model build feature from `mistralrs` to `local-llm`.

### Fixed

- Show the final image tag after a build.
- Render prompt help links as the published mdBook URLs while keeping local `doc/...`
  metadata for sync checks.
- Skip agent/model/provider validation during image builds.
- Repair e2e suite rot.

### Changed

- Split the workspace into the `outrig` library and the `outrig-cli` binary; each crate
  ships its own crates.io README.
- Renamed the `[container]` config table to `[image]` and the tool-call/result `cap`
  limits to `max`.
