# 0001 -- Bootstrap

Written retroactively, after the phase closed. It records what 0.1.0 actually shipped rather
than what was imagined at the start; the 74 files under `tasks/` are the contemporaneous
record, and their `## Decisions` sections remain authoritative for anything this page
summarizes.

## Goal

By the end of this phase, a user can point OutRig at a repository, have it build a container
image from that repository's Dockerfile, and run an LLM agent whose every tool call executes
inside the resulting container. `outrig run` drives the agent loop from a REPL; sessions
persist and can be listed, read back, and discarded. `outrig mcp` offers the same
containerized toolset to any other MCP client, and `outrig image build` produces a reusable
toolset image that carries its own MCP declaration. Both crates publish to crates.io.

## User-visible deliverables

- A layered configuration system: a global file and a repo file, merged and validated, with
  `${VAR}` substitution for API keys and build arguments, and `outrig config init` to write a
  first one.
- Image build through buildah with a content-addressed cache, and container lifecycle through
  podman with three-layer cleanup. `outrig build` pre-warms the cache; `outrig container add`
  and `outrig init` scaffold the Dockerfile and config block.
- A runtime user-mapping bootstrap, so files the agent writes are owned by the invoking user
  rather than by root.
- An MCP client over `podman exec` stdio, adapted into Rig's dynamic-tool interface, so an
  agent's tools are processes inside the container.
- LLM resolution from agent to model to provider, with a serde-tagged `LlmProvider` enum, and
  an optional in-process `mistralrs` backend behind the `local-llm` feature, including
  Hugging Face download, model sharing through `LlmRegistry`, streaming output, and GPU device
  selection.
- `outrig run`: the agent loop, a REPL with slash commands and SIGINT handling, a configurable
  and resumable tool-call cap, and per-tool-result truncation.
- Session persistence plus `outrig ls`, `outrig logs`, and `outrig discard`.
- `outrig mcp`: a `ProxyServer` fronting a pool of MCP clients, over stdio and over HTTP/SSE,
  able to attach to an already-running container, plus `outrig mcp self` for self-description.
- Workspace controls: runtime bind mounts, capability profiles, and a network interceptor that
  audits traffic and then enforces host:port policy.
- Standalone toolset images: `outrig image init`, `outrig image build`, MCP config stamped
  into OCI labels, and `outrig image inspect` reading those labels locally or from a registry
  without pulling layers.
- A curated `outrig::*` library surface, split from the `outrig-cli` binary, with internal
  modules hidden and each crate shipping its own crates.io README.
- `outrig design prompt`, which prints a one-shot prompt for designing a repo's OutRig setup.

## Exit criteria

- `cargo test --workspace`, `cargo clippy --all-targets`, and `cargo fmt --check` exit 0, and
  CI gates every pull request on them plus the mdbook build and the doc-style audit.
- The end-to-end acceptance test drives a real build, a real container, and a real agent turn.
- `outrig` and `outrig-cli` both publish to crates.io at 0.1.0.
- mdbook output publishes to GitHub Pages from `trunk`.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/workspace.md`,
`doc/concepts/llm-providers.md`, `doc/concepts/in-process-llm.md`, `doc/reference/config.md`,
`doc/reference/cli.md`, and the `doc/usage/` pages for `init`, `build`, `run`, `mcp`,
`image`, `config`, and `sessions`.

## Tasks

All 74 are in `tasks/`, numbered `0001-01` through `0001-77`. The gaps at `0001-23`,
`0001-25`, and `0001-27` are permanent: two early renumbers moved those tasks to other
numbers, and the vacated ones were never reused.

## Out of scope

- Sidecar containers. 0.1.0 runs MCP servers inside the primary container only; moving them
  into containers of their own is phase 0002.
- TLS-terminating network inspection. The interceptor reaches host:port policy and stops
  there; URL- and body-aware policy stays in `plan/next/network-interceptor-mitm.md`.
- Any stability promise about the library surface. The API was curated but not frozen, which
  is what made the phase-0002 breaking changes possible.

## Decisions

**Closed 2026-06-26**, at tags `outrig-v0.1.0` and `outrig-cli-v0.1.0`. The queue drained to
empty before the release, so the phase boundary is exact: every task numbered `0001-NN`
shipped in 0.1.0, and nothing else did.

This phase was defined retroactively when `plan/` moved to phase-scoped numbering. The
sequence numbers are the original flat ones, carried over unchanged, so a task's number here
matches the number used in commit messages and in earlier task files from the period.
