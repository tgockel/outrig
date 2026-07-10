# 0079 -- Sidecar core + exec-stdio

## Context

The core of the MCP sidecar program (see [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md)):
additional podman containers owned by a session, hosting MCP servers over the existing
exec-stdio transport, with lifecycle coupled to the primary container. The spec's "Config
surface", "Runtime behavior", "Transports" (exec-stdio), and "MCP merge and naming" sections
define this task; its "Decisions" section records the design calls.

## Goal

Run MCP servers in sidecar containers declared under `[images.<name>.sidecars.<sc>]` and
placed via `sidecar = "<sc>"` or inline `image` + `command` on MCP entries, with full
lifecycle coupling, network-policy parity, and label-merge semantics.

## Deliverables

- Config surface and validation per the spec: sidecar blocks (`image`, `workspace`, `start`,
  `on-failure`, `mounts`, optional `security`), `McpServerSpec` gaining `sidecar` and `image`
  keys on its `Full` form, anonymous-sidecar shorthand, name rules, and the mutual-exclusion /
  named-sidecar-requires-command validation errors.
- Container labels `org.outrig.session` and `org.outrig.sidecar` via `build_podman_run_cmd`;
  sidecar naming `outrig-<sid>-<sc>`; session records listing sidecar container names.
- Lifecycle: startup order (primary, then `start = "auto"` sidecars, then interceptor attach,
  then `connect_mcp_clients` for all servers); `--userns=keep-id` everywhere; conditional
  `bootstrap_user`; N-container `TRACKED` cleanup, `stop()`/`Drop`/panic-hook coverage; the
  `podman wait` primary watcher that reaps sidecars; teardown ordering per the spec.
- `outrig clean` sweeps `podman ps -a --filter label=org.outrig.session` alongside the
  session-record walk.
- Scoped `org.outrig.mcp` label merge on named sidecars, flat per-session server namespace
  with duplicate-name startup errors, and a placement column in `outrig mcp show-merged`.
- exec-stdio connection through `McpClient::connect_via_podman_exec` against sidecar names,
  with env resolution, stderr files, and `for_server` scoping unchanged.
- `on-failure = "abort" | "warn"` semantics at session start; uniform mid-session death
  handling (log, tools error, no restart).
- Doc updates alongside the code: `concepts/containers.md`, `concepts/mcp-servers.md`,
  `concepts/workspace.md`, `concepts/mcp-trust-model.md`, `usage/mcp.md`,
  `reference/config.md` (under `crates/outrig-cli/src/mcp_self/docs/`).

## Acceptance

- A declared sidecar hosts servers visible to both `outrig run` and `outrig mcp`.
- `podman kill` of the primary reaps sidecars; `outrig clean` sweeps labeled strays.
- `abort` and `warn` behave as specified; sidecar egress obeys session network policy.
- Existing single-container sessions behave byte-for-byte as before when no sidecar is
  declared.

## Dependencies

- **Hard: 0078**. Sidecars must not become usable before network parity exists.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- full design, decisions 1-15.
- `doc/concepts/mcp-servers.md` -- current MCP declaration and lifecycle narrative.
- `doc/concepts/mcp-trust-model.md` -- trust boundary rationale sidecars must preserve.
