# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
