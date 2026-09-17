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

## Decisions

User-confirmed during planning (2026-07-11):

- `--attach` + declared sidecars/placements is a hard `Configuration` error (matches the
  existing audit/filter-with-attach rejection); an attached session cannot own containers.
- The `outrig clean` label sweep never removes running containers, labeled or not; running
  record-less containers are reported under a "skipped running labeled containers" section.
  Only stopped, record-less containers whose podman `Created` is older than `--older-than`
  are `podman rm -f`'d. Record-backed containers stay owned by the record walk, except that
  a record removed in the same run counts as gone (so a failed `--rm` and its record clean
  up together).
- The `podman wait` primary watcher spawns only for sessions that actually started
  sidecars, preserving byte-for-byte behavior of single-container sessions. Never armed
  for `--attach`.
- `show-merged` output stays a TOML document; placement appears natively via the new
  `sidecar`/`image` keys plus a `# <name>: <placement> (<provenance>)` comment per server.

Drafting-level calls made during execution:

- The entrypoint-stdio form (`{ image = "...", env }`, no `command`) parses -- `command`
  became `Option` on `McpServerSpec::Full` -- but validation rejects it with an
  "arrives in a later release" error (`McpEntrypointStdioUnsupported`); 0080 deletes that
  rule. Parse-level rejection inside the untagged enum would have produced useless errors.
- Placement keys are repo-config-only: `parse_mcp_table` (runtime label read) and
  `image.toml` parsing reject `sidecar`/`image` in `org.outrig.mcp`. Consequently, label
  stamping and the build cache key exclude placement-bearing config entries
  (`primary_scoped_mcp`) -- a sidecar entry neither belongs in the primary image's label
  nor invalidates its cache.
- Library facade: `Outrig::launch` errors on placement-bearing specs and library
  containers carry no session label until 0081 adds the sidecar API, keeping `clean` from
  reasoning about containers that have no records by design.
- `start = "manual"` sidecars are image-ensured and label-merged at session start (their
  servers reserve names in the flat namespace and appear in `show-merged` as
  `manual, not started`), then skipped with a stderr notice; failures follow `on-failure`
  like any other sidecar.
- Label-merge collisions and malformed sidecar labels are startup errors regardless of
  `on-failure`; only ensure/start/bootstrap/attach/connect failures are `warn`-able.
- A `warn`-sidecar connect failure drops the whole sidecar: its already-connected clients
  shut down, the container stops, remaining servers are skipped. Its interceptor
  attachment is left for teardown's `shutdown` walk (detach against a stopped container
  is tolerated per 0078).
- `ContainerWorkspace` gained an `access` field (sidecar `workspace = "ro"`) and
  `ContainerLaunchSpec` a `labels` map; `SessionContainers` declares `sidecars` before
  `primary` so field-order `Drop` mirrors teardown order.
- Sidecar names allow a leading digit (`^[A-Za-z0-9][A-Za-z0-9_-]*$`) since they embed in
  container names, unlike server names.
- The mid-session death notice for sidecars comes from per-sidecar `podman wait` tasks
  that log once; an orderly warn-skip stop can also trigger that log line (accepted
  noise, the warning already explains it).

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- full design, decisions 1-15.
- `doc/concepts/mcp-servers.md` -- current MCP declaration and lifecycle narrative.
- `doc/concepts/mcp-trust-model.md` -- trust boundary rationale sidecars must preserve.
