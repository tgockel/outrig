# 0059 -- Network interceptor plumbing

## Context

Today's `doc/concepts/workspace.md` calls out one deferred line:

> egress interception (CONNECT proxy + per-host allowlist + per-session log) is
> deferred.

v0's only mitigation against an agent calling arbitrary network endpoints is
"don't include a `shell` MCP server." That stopgap does not scale once a user
wants legitimate network access for `git push`, `cargo publish`, `pip install`,
or similar tools.

The network interceptor closes that gap in phases. This first phase puts a
host-side process in the path of outbound container traffic and writes a
per-session audit log. It does not enforce policy yet; all connections are
allowed.

## Goal

When network audit mode is enabled, outbound connections from the session
container appear in `<session_dir>/logs/network.jsonl` with enough metadata to
support later allow/deny policy. The default mode leaves Podman's configured
default networking alone so existing sessions do not depend on nftables or
namespace support.

## Deliverables

- Per-session network namespace setup plus TCP and UDP/53 redirection from the
  container to a host-side interceptor.
- A config `[network].mode = "default" | "audit"` setting, `--network` CLI
  override for fresh sessions, and matching public Rust launch option.
- Host-side listener that recovers the original destination, opens the upstream
  connection, and bridges bytes in both directions.
- DNS interception with a per-session name-to-IP cache so later TCP records can
  include hostnames instead of only IPs.
- Protocol sniffing for HTTP host headers, HTTPS SNI, SSH banners, and raw TCP.
- JSON-lines audit writer at `<session_dir>/logs/network.jsonl`.
- Smoke coverage that runs `curl https://example.com` from inside a container
  and verifies an audit-log line with the expected host and byte counts.

## Architecture

Each fresh audit-enabled session's container runs in its own network namespace.
Outbound TCP and UDP/53 are redirected via iptables/nftables `REDIRECT`,
netavark, CNI, or the equivalent mechanism chosen during implementation.
Loopback inside the container is unaffected. Sessions with network mode
`default` skip this setup entirely.

For each accepted TCP connection the interceptor:

1. Recovers the original destination, for example via `SO_ORIGINAL_DST`.
2. Sniffs the first bytes to identify HTTP, HTTPS, SSH, or raw TCP.
3. Allows the connection unconditionally in this phase.
4. Bridges bytes to the upstream destination.
5. Writes an audit record when the connection closes.

The host outrig process itself is not routed through the interceptor. LLM
provider HTTPS calls, image pulls during `outrig build`, and other host-side
traffic remain out of scope.

## Audit Log

Each record is single-line Zeek `conn.log`-style JSON with these fields:

| Field               | Type   | Description                                             |
|---------------------|--------|---------------------------------------------------------|
| `ts`                | number | Unix epoch seconds at connection open.                  |
| `uid`               | string | Per-connection id.                                      |
| `id.orig_h`         | string | Container-side socket IP.                               |
| `id.orig_p`         | int    | Container-side socket port.                             |
| `id.resp_h`         | string | Resolved destination IP.                                |
| `id.resp_p`         | int    | Destination TCP port.                                   |
| `proto`             | string | Transport protocol, always `"tcp"` in this phase.       |
| `service`           | string | Best guess: `http`, `ssl`, `ssh`, or `-`.               |
| `duration`          | number | Connection lifetime in seconds.                         |
| `orig_bytes`        | int    | Bytes from container to remote.                         |
| `resp_bytes`        | int    | Bytes from remote to container.                         |
| `conn_state`        | string | Coarse Zeek connection state.                           |
| `local_orig`        | bool   | Always `true` for container-originated traffic.         |
| `local_resp`        | bool   | Always `false` for remote destinations.                 |
| `missed_bytes`      | int    | Always `0`; OutRig is not a packet sensor.              |
| `server_name`       | string | TLS SNI hostname if observed; omitted otherwise.        |
| `outrig.host`       | string | Hostname from DNS, HTTP Host, or SNI if known.          |
| `outrig.action`     | string | Always `"allow"` in this phase.                         |
| `outrig.rule`       | string | Always `"default"` in this phase.                       |
| `outrig.session_id` | string | OutRig session id.                                      |
| `outrig.container`  | string | Podman container name.                                  |

MITM fields such as `url`, `method`, `status`, request body, and response body
are deliberately deferred to `0064`.

## Open Sub-Decisions

- **Interceptor process model.** One per session is simplest; a shared daemon
  could reuse DNS caches but adds lifecycle complexity.
- **Network backend.** Choose between netavark/CNI integration,
  `slirp4netns`/`pasta` configuration, or managed iptables/nftables rules.
- **DNS strategy.** Prefer intercepting and resolving through the host resolver
  so the audit log can reliably fill the `outrig.host` field.
- **Source attribution.** Recording which MCP server opened a connection is
  useful but requires process/socket correlation inside the namespace and is not
  part of this phase.

## Acceptance

- Existing sessions keep working when no network policy is configured.
- A network-enabled session writes `<session_dir>/logs/network.jsonl`.
- `curl https://example.com` inside the container produces an `"allow"` audit
  record with Zeek-style host, IP, port, service, byte count, and duration
  fields.
- DNS names are reflected in later TCP audit records when the container resolved
  the name through the intercepted DNS path.
- Host-side outrig traffic is not captured or affected.

## Dependencies

None.

## Decisions

- Network monitoring is explicit. The default mode leaves existing sessions
  using Podman's configured default networking and does not require `nft`,
  `nsenter`, or namespace permissions.
- Persistent network mode can live in global or repo config. When both declare
  `[network]`, the repo value wins for that repo; an omitted repo table keeps
  the global value.
- `outrig run --network` and fresh `outrig mcp --network` override config mode
  for one selected container session; `--network audit` is rejected with
  `outrig mcp --attach`.
- The public Rust API exposes `LaunchSpec::with_network_mode` with the same
  `default` / `audit` modes.
- Audit mode uses per-session nftables rules and listener sockets in the
  container network namespace. A forked helper enters the rootless podman
  user/network namespaces and passes the bound listener file descriptors back
  to the host process.
- Audit mode rewrites the session container's `/etc/resolv.conf` to point at
  the in-namespace DNS listener. Non-loopback UDP/53 redirection remains in
  place for later policy work, but ordinary resolver traffic uses loopback so
  replies come from the nameserver address the client expects.

## See also

- `0060-network-interceptor-enforcement.md` -- policy enforcement on top of this
  traffic plumbing.
- `0064-network-interceptor-mitm.md` -- URL/body-aware HTTPS inspection after
  host:port policy exists.
- `doc/concepts/workspace.md` -- the v0 deferred-network stance this replaces.
