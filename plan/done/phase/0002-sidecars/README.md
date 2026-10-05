# 0002 -- Sidecars

Closed. Every task is in `tasks/`, and each file's `## Decisions` section remains authoritative
for anything this page summarizes. This page was written once the phase was most of the way
through, when `plan/` moved to phase-scoped numbering, so its first two sections describe work
that had largely landed and the exit criteria are what the rest of the phase was held to.

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
- 0.2.0-rc.3 ships, and what would force a further candidate is recorded in advance rather
  than argued after the fact.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/concepts/llm-providers.md`, `doc/reference/config.md`, and
`doc/reference/cli.md`.

## Tasks

Numbered `0002-01` through `0002-55`, with no gaps, all in `tasks/`. `plan/todo/README.md`
keeps the sequencing rationale for the release-gate tail (`0002-37` onward) and the note on why
`0002-55` sits above `0002-54` although it landed first.

## Out of scope

- TLS-terminating inspection (#294). The interceptor gains multi-container reach and
  correct policy semantics, not visibility into payloads.
- macOS and Windows hosts. Both stay declared and unreachable; see #295 and #296.
- Removing the deprecated in-process local-LLM backend. 0.2.0 decides what the deprecated
  surface does; the removal itself is `plan/next/remove-deprecated-local-llm.md`.
- The post-0.2.0 buffer that the release audit surfaced. It is catalogued under
  "Not queued, deliberately" in `plan/todo/README.md`, and is deferred by decision rather than
  by omission.

## Decisions

**Closed 2026-10-04**, at tags `outrig-v0.2.0` and `outrig-cli-v0.2.0`. The phase's work
shipped as 0.2.0 on 2026-09-23: the release commit (`8881ba2`) landed `0002-54` with four
acceptance criteria recorded as outstanding -- `cargo publish`, the two tags, the GitHub
release, and the install smoke test -- because publication is the maintainer's, not the
commit's. The maintainer did all four the same day; crates.io carries both crates at 0.2.0,
both tags point at `8881ba2`, and release `outrig-cli-v0.2.0` exists. This page waited on that
record and was moved here with the 0.2.1 release, which is why the close is dated eleven days
after the version it closes on.

The 43 fixes between 0.2.0 and 0.2.1 are not phase work. They are tracked by the 0.2.1 GitHub
milestone and the two changelogs, and what they set aside for later went into the `plan/next/`
buffer, which `plan/todo/README.md` describes under "Not queued, deliberately".
