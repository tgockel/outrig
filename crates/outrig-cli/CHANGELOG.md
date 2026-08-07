# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`[models.<name>]` entries can be aliases**, so `--model`, `default-model`,
  `[agents.<name>].model`, and a subagent's `model` argument all accept a name that stands
  for one other model or for an ordered set of provider-equivalent ones. The session picks
  the first candidate this build can reach -- provider defined, style compiled in, `api-key`
  variable set and non-empty -- which is what lets one committed config serve a laptop with
  `ANTHROPIC_API_KEY` and a CI runner holding a Bedrock role.

  Selection happens once, at startup, and answers "am I configured for this" rather than "is
  this endpoint up": building a remote client does no network I/O, so an alias does **not**
  fail over when a vendor rate-limits mid-session. An alias with no reachable candidate ends
  the session listing every candidate and the distinct reason each was skipped.

  Attribution follows the name: the banner, the subagent launch trace, the subagent tool
  result, and the transcript header all render `alias -> concrete` when a hop was taken. A
  direct model name prints byte-for-byte what it printed before.

  `--device` is refused for an alias spanning more than one candidate -- it selects hardware
  for one in-process model, and an alias may span styles. A single-target alias takes it.

### Changed

- **The `outrig__subagent` tool no longer advertises a model whose `api-key` variable is
  unset or empty.** The schema's `enum` and the alias selector are now one predicate, so a
  name is offered exactly when a launch could reach it. Previously an unset key was
  advertised and failed at launch. An alias is offered when *any* of its candidates is
  reachable, which is what lets `alias = ["opus-local", "opus-anthropic"]` stay launchable
  in a build without `--features local-llm` where naming `opus-local` directly would not be.

### Removed

- **The `OUTRIG_BOOTSTRAP` environment variable**, along with the `podman exec` user-bootstrap
  fallback it selected. The runtime user is written into the container from the host, and that
  is now the only path.
- **The `user_bootstrap_package_missing` warning** from `validate_dockerfile` (`mcp self` and
  the `rig` self tool). It advised installing `passwd`/`shadow` on hosts that would fall back
  to `useradd`/`groupadd`; with no fallback there is no such host. The tool no longer runs a
  `podman info` probe to decide, so it answers without touching podman at all.

### Added

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

### Changed

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

### Fixed

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

- **Breaking (config):** sidecars are declared at the top level as `[sidecars.<sc>]`, not
  `[images.<name>.sidecars.<sc>]`. Move the blocks up a level; the keys are unchanged. The old
  form is now an unknown-field parse error. One block can be shared by several image-configs,
  and the global config can declare sidecars a repo references.
- **Breaking (config):** a sidecar starts only when an `[images.<name>.mcp]` entry names it.
  Declaring a block no longer starts it -- with blocks shared and global, it cannot.

### Removed

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

## [0.2.0-rc.1](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.2.0-rc.1) - 2026-07-24

A release candidate, cut so the library's breaking changes get integration testing ahead of
0.2.0 final. A pre-release is opt-in, so install it explicitly:
`cargo install outrig-cli --version 0.2.0-rc.1`.

### Added

- **MCP sidecar containers** -- an `[mcp.<name>]` entry can declare `sidecar = "<sc>"` to run
  the server in a sidecar declared under `[images.<name>.sidecars.<sc>]`, or an inline `image`
  to give it a dedicated anonymous one. A declared sidecar carries its own image, workspace
  access, mounts, and security; `start = "auto"` brings it up with the session, and
  `start = "manual"` waits for `/sidecar add`.
- **Entrypoint-stdio servers** -- an `image` with no `command` runs that image's `ENTRYPOINT`
  as the MCP server, so off-the-shelf MCP images work without repo-side command knowledge.
  The `--env` overlay applies to them at container-create time.
- **`/sidecar` in the REPL** -- `/sidecar list` shows the declared sidecars and their status;
  `/sidecar add <name>` starts a `start = "manual"` sidecar mid-session, and its tools join
  the running agent.

### Changed

- Upgraded the LLM/agent stack: `rig-core` 0.39 -> 0.40 and the `rmcp` MCP SDK 1.x -> 2.x,
  plus routine dependency bumps (anyhow, ignore, jiff, rand, toml). rig 0.40's `max_turns` now
  counts total model calls rather than tool-call rounds; outrig compensates so the per-turn
  tool-call limit behaves as before.
- Egress policy and the audit log cover every sidecar container, not just the primary.
- `outrig clean` reads one `podman ps -a` instead of a `podman inspect` per aged session, and
  coalesces stray-container removal into a single `podman rm -f`.
- Slash commands run through one dispatcher. `/help` output is unchanged, but two edge cases
  of the old exact-match arms are gone: a trailing-space `/quit ` now executes, and
  tab-separated `/sidecar` arguments parse.

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
