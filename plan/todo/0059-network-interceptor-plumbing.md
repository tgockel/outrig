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

Every outbound connection from a session container appears in
`<session_dir>/logs/network.jsonl` with enough metadata to support later
allow/deny policy.

## Deliverables

- Per-session network namespace setup plus TCP and UDP/53 redirection from the
  container to a host-side interceptor.
- Host-side listener that recovers the original destination, opens the upstream
  connection, and bridges bytes in both directions.
- DNS interception with a per-session name-to-IP cache so later TCP records can
  include hostnames instead of only IPs.
- Protocol sniffing for HTTP host headers, HTTPS SNI, SSH banners, and raw TCP.
- JSON-lines audit writer at `<session_dir>/logs/network.jsonl`.
- Smoke coverage that runs `curl https://example.com` from inside a container
  and verifies an audit-log line with the expected host and byte counts.

## Architecture

Each session's container runs in its own network namespace. Outbound TCP and
UDP/53 are redirected via iptables/nftables `REDIRECT`, netavark, CNI, or the
equivalent mechanism chosen during implementation. Loopback inside the
container is unaffected.

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

Each record is single-line JSON with these fields:

| Field         | Type   | Description                                                  |
|---------------|--------|--------------------------------------------------------------|
| `ts`          | string | RFC 3339 timestamp at connection open.                       |
| `host`        | string | Hostname if known from DNS or SNI, else `""`.                |
| `ip`          | string | Resolved destination IP.                                     |
| `port`        | int    | Destination TCP port.                                        |
| `proto`       | string | Best guess: `http`, `https`, `ssh`, or `tcp`.                |
| `sni`         | string | TLS SNI hostname if observed; omitted otherwise.             |
| `action`      | string | Always `"allow"` in this phase.                              |
| `rule`        | string | Always `"default"` in this phase.                            |
| `bytes_tx`    | int    | Bytes from container to remote.                              |
| `bytes_rx`    | int    | Bytes from remote to container.                              |
| `duration_ms` | int    | Connection lifetime.                                         |

MITM fields such as `url`, `method`, `status`, request body, and response body
are deliberately deferred to `0064`.

## Open Sub-Decisions

- **Interceptor process model.** One per session is simplest; a shared daemon
  could reuse DNS caches but adds lifecycle complexity.
- **Network backend.** Choose between netavark/CNI integration,
  `slirp4netns`/`pasta` configuration, or managed iptables/nftables rules.
- **DNS strategy.** Prefer intercepting and resolving through the host resolver
  so the audit log can reliably fill the `host` field.
- **Source attribution.** Recording which MCP server opened a connection is
  useful but requires process/socket correlation inside the namespace and is not
  part of this phase.

## Acceptance

- Existing sessions keep working when no network policy is configured.
- A network-enabled session writes `<session_dir>/logs/network.jsonl`.
- `curl https://example.com` inside the container produces an `"allow"` audit
  record with host, IP, port, protocol, byte counts, and duration.
- DNS names are reflected in later TCP audit records when the container resolved
  the name through the intercepted DNS path.
- Host-side outrig traffic is not captured or affected.

## Dependencies

None.

## See also

- `0060-network-interceptor-enforcement.md` -- policy enforcement on top of this
  traffic plumbing.
- `0064-network-interceptor-mitm.md` -- URL/body-aware HTTPS inspection after
  host:port policy exists.
- `doc/concepts/workspace.md` -- the v0 deferred-network stance this replaces.
