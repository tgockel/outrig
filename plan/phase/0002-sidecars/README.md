# 0002 -- Sidecars

Open. Its finished tasks are already in `plan/done/phase/0002-sidecars/tasks/`; the queue that
remains is in `plan/todo/`. This page was written once the phase was most of the way through,
when `plan/` moved to phase-scoped numbering, so its first two sections describe work that has
largely landed and the exit criteria are what is still being held to.

## Goal

By the end of this phase, an MCP server no longer has to live inside the agent's primary
image. A repository declares tools as sidecar containers, and OutRig places each one:
executing into the primary, running an image's entrypoint as the server, or sharing the
primary's filesystem view. A library consumer reaches every one of those placements without
dropping to the CLI. The public surface of both crates is then narrowed, sealed, and frozen,
and 0.2.0 ships with a migration guide for anyone on 0.1.

## User-visible deliverables

- Sidecar containers as the unit of tool delivery: an exec-stdio core, an entrypoint-stdio
  transport with arguments, dynamic addition to a live session, and translation from
  `from_image_config` so an image's own declaration produces sidecars.
- `outrig-enter`, a static launcher that lets a sidecar share the primary container's
  filesystem view while still dropping privilege to the session user.
- A network interceptor generalized from one container to N, with attach and detach as a true
  inverse pair, hostname rules that grant only against a bound destination, and a declared
  `[network]` mode treated as structural rather than as a hidden bit.
- Nested container runtimes: device passthrough and a `no_new_privs` opt-out, so an agent
  image can run podman or buildah of its own.
- Host-side user bootstrap, replacing the in-container `useradd` path.
- Library parity: every sidecar placement and primary exec reachable from `outrig::*`, plus
  config path provenance so a relative path resolves against the file that declared it and
  errors name that file.
- A native Anthropic Messages API provider, model aliases naming an ordered set of
  equivalents, mid-turn failover between them, and a shorter leash on endpoints that never
  answered.
- Subagent controls: a width cap, per-subagent model selection, all-or-nothing release, and a
  shutdown grace measured against a full tree.
- A public API narrowed, swept with `#[non_exhaustive]`, moved onto options structs and sealed
  traits, and then gated by a snapshot check rather than by the honor system.

## Exit criteria

- `crates/outrig/public-api.txt` and its `outrig-cli` counterpart are regenerated and enforced
  in CI, so a surface change cannot land unnoticed.
- The e2e suite runs against a live podman on both x86-64 and AArch64, and both rows are
  green.
- `doc/` carries no contract that is false at 0.2.0, and a 0.1 -> 0.2 migration guide exists.
- 0.2.0-rc.3 ships with its soak parameters recorded in advance, and 0.2.0 final is checked
  against that record rather than against one composed after the fact.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/concepts/llm-providers.md`, `doc/reference/config.md`, and
`doc/reference/cli.md`.

## Tasks

Numbered `0002-01` onward, with no gaps. Finished tasks are in
`plan/done/phase/0002-sidecars/tasks/`; `0002-41` through `0002-54` are queued in
`plan/todo/`, and `plan/todo/README.md` carries the per-step index and the sequencing
rationale.

## Out of scope

- TLS-terminating inspection (`plan/next/network-interceptor-mitm.md`). The interceptor gains
  multi-container reach and correct policy semantics, not visibility into payloads.
- macOS and Windows hosts. Both stay declared and unreachable; see
  `plan/next/macos-host-support.md` and `plan/next/windows-host-support.md`.
- Removing the deprecated in-process local-LLM backend. 0.2.0 decides what the deprecated
  surface does; the removal itself is `plan/next/remove-deprecated-local-llm.md`.
- The post-0.2.0 buffer that the release audit surfaced. It is catalogued under
  "Not queued, deliberately" in `plan/todo/README.md`, and is deferred by decision rather than
  by omission.
