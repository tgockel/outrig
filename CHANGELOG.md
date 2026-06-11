# Changelog

All notable changes to this project are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-06-11

First public release. OutRig runs an LLM agent on the host and connects it to MCP servers
running inside a podman-managed container, so filesystem and shell tools stay inside a sandbox
you define with a `Dockerfile`.

### Added

- **Agent session (`outrig run`)** -- interactive stdin/stdout REPL that drives a
  [Rig](https://github.com/0xPlaygrounds/rig) agent loop against the MCP tools exposed by the
  session container.
- **MCP serving (`outrig mcp`)** -- expose the configured MCP servers as a single MCP server over
  stdio, with a self-describing mode when no repository config is present.
- **Image lifecycle** -- `outrig build`, plus `outrig image add`, `image init`, `image build`, and
  `image inspect` (local and remote, reading OCI labels without starting a container).
- **Setup commands** -- `outrig init` and `outrig config init` for interactive global/repository
  configuration scaffolding.
- **Design helper (`outrig design`)** -- generate AI-assisted design prompts.
- **Session management** -- `outrig ls`, `logs`, `discard`, and `clean` for inspecting and pruning
  on-disk session records and MCP-server stderr.
- **LLM providers** -- OpenAI-compatible endpoint bridge (works with any compatible API), routed
  through Rig.
- **Optional in-process model (`local-llm` feature)** -- mistral.rs backend with `cuda` and
  `metal` acceleration features.
- **Container isolation** -- podman-managed session containers with configurable bind mounts,
  capability profiles, and network interception (host:port filtering, DNS, and audit logging).
- **Configuration** -- layered global + repository TOML config with schema validation.
- **Documentation** -- full mdbook documentation tree published at
  <https://tgockel.github.io/outrig/>.

[0.1.0]: https://github.com/tgockel/outrig/releases/tag/v0.1.0
