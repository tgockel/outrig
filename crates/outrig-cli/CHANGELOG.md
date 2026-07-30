# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

  Anthropic requires an output-token ceiling on every request. outrig sends the published one
  for the Claude identifiers it recognizes; for any other identifier the turn fails with
  `` `max_tokens` must be set for Anthropic `` unless `max-tokens` is set on the model or the
  agent. `outrig config init` offers `anthropic` as a provider style -- defaulting to
  `https://api.anthropic.com` and `ANTHROPIC_API_KEY` -- and prompts for `max-tokens`
  alongside the model identifier, so a generated config carries an explicit ceiling.

- **`retry-budget-secs`**, at the top level and on any remote provider, bounds how long a
  transiently-failing LLM call keeps retrying. Defaults to `600`; `0` disables retries; the
  ceiling is `3600`. A provider's own value wins over the top-level one, which wins over the
  default -- rate limits belong to the endpoint, so per-provider is usually the right place.

### Changed

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
