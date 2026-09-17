# 0080 -- Sidecar entrypoint-stdio transport

## Context

With sidecar core landed (0079), the inline `{ image = "..." }` form with no `command` gains
meaning: the image's ENTRYPOINT *is* the MCP server, enabling off-the-shelf MCP images (the
Docker MCP catalog pattern) with zero repo-side command knowledge. See
[`mcp-sidecars-spec.md`](mcp-sidecars-spec.md), "Transports" (entrypoint-stdio). The hard part
is attaching network interception before the entrypoint can emit a packet.

## Goal

Support entrypoint-stdio MCP servers: `{ image = "<ref>", env = {...} }` with no command runs
the image's ENTRYPOINT as the server over piped stdio, with container lifetime equal to server
lifetime and network policy applied from the first packet.

## Deliverables

- The no-command inline form: `podman run -i` with piped stdio; `env` becomes `--env` flags.
- Interception attached before the entrypoint runs. Candidate mechanism to validate:
  `podman create` + `podman init` to materialize the container process and netns without
  executing the entrypoint, attach the interceptor to the created PID, then
  `podman start --attach --interactive` for the stdio pipes.
- Lifetime-equals-server semantics: server exit surfaces exactly like a mid-session sidecar
  death (logged, tools error, session survives).
- Validation already specified in 0079 holds: an entry with `image` and no `command` may carry
  `env` but nothing else.
- Docs updated alongside the code (`concepts/mcp-servers.md`, `usage/mcp.md`,
  `reference/config.md`).

## Acceptance

- An off-the-shelf MCP image runs as a server with session network policy applied from its
  first packet.
- Server exit surfaces as tool errors without killing the session.

## Dependencies

- **Hard: 0079**. Extends the sidecar container machinery with a second transport.

## Decisions

Drafting-level calls made during execution (2026-07-12):

- **The candidate mechanism validated** (podman 4.9.3 rootless, runc): after `podman create -i
  --rm` + `podman init`, `.State.Pid` is a live `runc init` process (entrypoint not executed),
  `/proc/<pid>/ns/{user,net}` are openable by the invoking user, and `nsenter -t <pid> -U -n nft`
  works. `podman wait` on an initialized-never-started container blocks (no watcher change
  needed), and `podman start --attach --interactive` demuxes stdout/stderr for non-tty
  containers, preserving JSON-RPC framing.
- **resolv.conf via `--dns` at create time**, not the exec-based install: `podman exec` cannot
  reach a created-but-not-started container, and `nsenter -m` into the mount namespace has
  uid-mapping hazards. `podman create --dns 127.0.0.1 --dns-option ndots:0` yields the
  interceptor resolv.conf (plus a harmless `search` line). Rather than a second attach entry
  point, `Container` records `dns_preconfigured` at create and the one
  `NetworkInterceptor::attach` skips the exec-based install for such containers -- callers
  cannot pick the wrong flavor. The resolver values live once, as constants in `network.rs`,
  consumed by both the exec install and the `--dns` flags.
- **Env resolves at create, not connect**: `podman start` carries no `--env`, so the entry's
  env (config + CLI `--env` overlay, shared logic extracted as `outrig::resolve_mcp_env`) is
  baked into `podman create --env`. Consequences: a bad `${VAR}` now fails at sidecar-create
  rather than connect (same session-startup failure class), the values are visible to host
  `podman inspect` (same trust level as exec argv today, noted in the config reference), and
  `SessionSetupArgs` threads `cli_env` into setup.
- **`Container::stop` uses `podman stop --ignore`**: an entrypoint sidecar is normally already
  `--rm`-reaped at orderly-stop time (MCP shutdown closes stdin first), and "make it not exist"
  already succeeded. `--ignore` is the documented contract for exactly this, applied to every
  container rather than a per-container flag; the defensive `rm -f` tail still runs.
- **e2e fixture retries its network touch** (3x `wget -T 5`): parallel first-run image builds
  can saturate the network long enough for a single short attempt to give up before opening a
  connection, leaving no audit record for the first-packet assertion.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Transports" section.
- `doc/concepts/mcp-servers.md` -- MCP transport narrative to extend.
