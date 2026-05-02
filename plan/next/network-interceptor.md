# Network Interceptor

> **Status:** preliminary spec. Carved into numbered tasks in `plan/todo/` when ready.

## Context

Today's `doc/concepts/workspace.md` calls out one deferred line:

> egress interception (CONNECT proxy + per-host allowlist + per-session log) is
> deferred.

`plan/todo/0022-e2e-acceptance.md` re-affirms the deferral. v0's only mitigation against
an agent calling arbitrary network endpoints is "don't include a `shell` MCP server."
That works as a stopgap but stops scaling the moment a user wants `git push`, `cargo
publish`, `pip install`, or any tool that needs the network for legitimate reasons --
they have to grant blanket access or grant nothing.

The network interceptor closes that gap. It places a host-side proxy in the path of
*every* outbound connection from the container, logs each connection to a per-session
audit file, and -- once policy is configured -- enforces an allow/deny list against
each one. "Done" includes MITM TLS so policy can apply at the URL level and the audit
log can include request/response bodies. Earlier phases stop at the host:port level.

## Goals and non-goals

**In scope:**

- Auditing every outbound network connection from a session's container.
- Enforcing a global allow/deny policy on those connections.
- Eventually (final phase): MITM TLS so policy and audit can see request URLs and
  bodies.

**Out of scope:**

- The host outrig process's own outbound calls (LLM provider HTTPS, registry pulls
  during `outrig build`, etc.). LLM calls are audited at the text layer in a separate
  feature; build-time pulls happen before the container's network namespace exists.
- Tool-call auditing (which tool was called, with what arguments). That's a separate
  task at the MCP layer.
- Inbound connections to the container. There are none in v0 -- nothing exposes ports.
- Telling the LLM why a request was denied. The agent's socket sees a generic
  connection failure; surfacing a structured "denied because rule X" reason to the LLM
  is a higher-level feature handled elsewhere.

## User surface

### Configuration

Policy lives in **global** config only (`~/.outrig/config.toml`). Per-repo overrides
are deliberately not supported in this feature -- network policy is a property of the
machine the agent runs on, not the repo it's working in. A user who needs different
policies for different repos manages them at the host-config level.

```toml
[network]
default = "allow"   # or "deny". Default-of-default: "allow".

# Rules evaluated top-to-bottom; first match wins. If nothing matches, `default`
# applies. Concrete syntax (glob vs. CIDR vs. URL) is one of the open sub-decisions
# below; the example below is illustrative.
[[network.rules]]
host   = "*.npmjs.org"
action = "allow"

[[network.rules]]
host   = "github.com"
port   = 443
action = "allow"

[[network.rules]]
host   = "*"
port   = 22
action = "deny"
```

| Key                | Type   | Required | Default   | Description                                |
|--------------------|--------|----------|-----------|--------------------------------------------|
| `default`          | string | no       | `"allow"` | Action if no rule matches: `allow`/`deny`.  |
| `rules`            | array  | no       | `[]`      | Rule entries; first match wins.            |
| `rules[*].host`    | string | yes      | --        | Host pattern (glob, IP, or CIDR).          |
| `rules[*].port`    | int    | no       | any       | TCP port; omitted means any.               |
| `rules[*].action`  | string | yes      | --        | `"allow"` or `"deny"`.                     |

When `[network]` is absent entirely, behavior is identical to `default = "allow"` with
no rules: every connection allowed, every connection logged.

### What the agent sees on denial

A denied connection sees a socket-level rejection (TCP RST or immediate close on
CONNECT). The MCP server or shell tool that issued the call surfaces that as whatever
its own error path produces -- usually "connection refused" -- and the agent's response
either reports it back to the user or retries somewhere else.

outrig itself prints nothing into the REPL when a denial happens. The user finds out by
reading the audit log.

### Per-session audit log

Each `outrig run` writes a JSON-Lines file at:

```
<session_dir>/logs/network.jsonl
```

This mirrors the existing per-MCP-server `<session_dir>/logs/<server>.stderr` pattern
documented in `doc/concepts/mcp-servers.md`. One record per connection, with these
fields:

| Field        | Type    | Description                                                  |
|--------------|---------|--------------------------------------------------------------|
| `ts`         | string  | RFC 3339 timestamp at connection open.                       |
| `host`       | string  | Hostname if known (from DNS interception or SNI), else `""`. |
| `ip`         | string  | Resolved destination IP.                                     |
| `port`       | int     | Destination TCP port.                                        |
| `proto`      | string  | Best-guess: `http`, `https`, `ssh`, or `tcp`.                |
| `sni`        | string  | TLS SNI hostname if observed; omitted otherwise.             |
| `action`     | string  | `"allow"` or `"deny"`.                                       |
| `rule`       | string  | Rule that matched (`"default"` if none).                     |
| `bytes_tx`   | int     | Bytes from container to remote (0 on deny).                  |
| `bytes_rx`   | int     | Bytes from remote to container (0 on deny).                  |
| `duration_ms`| int     | Connection lifetime; 0 on deny.                              |

Sample line (formatted across multiple lines for readability; the actual log line is
single-line JSON):

```json
{ "ts": "2026-05-02T14:33:21Z", "host": "github.com", "ip": "140.82.112.3",
  "port": 443, "proto": "https", "sni": "github.com", "action": "allow",
  "rule": "rules[1]", "bytes_tx": 412, "bytes_rx": 8421, "duration_ms": 318 }
```

Once MITM lands, the record gains `url`, `method`, `status`, and optional
`request_body` / `response_body` fields (subject to a size cap and an opt-in flag).

## Architecture

### Container networking

Each session's container runs in its own network namespace, isolated from the host's
default networking. outrig configures the namespace so that:

- All outbound TCP from the container is redirected (via iptables/nftables `REDIRECT`
  or netavark equivalent) to a local listener on the host -- the *interceptor*.
- All outbound UDP/53 (DNS) is redirected the same way, so the interceptor can record
  hostnames rather than only IPs.
- Loopback inside the container is unaffected.

Whether this is built as a custom netavark/CNI plugin or as a managed
`slirp4netns`/`pasta` configuration with manual iptables rules is one of the open
sub-decisions; both are viable. The user-visible behavior is the same.

### The interceptor process

A host-side process accepts the redirected connections. For each:

1. Recover the original destination via `SO_ORIGINAL_DST` (after iptables
   `REDIRECT`).
2. Sniff the first bytes to determine protocol:
   - HTTP: request-line carries the `Host` header.
   - HTTPS: TLS ClientHello carries SNI; without MITM, the rest is opaque.
   - SSH: protocol banner.
   - Other: treated as raw TCP; only host:port is known.
3. Apply policy: rules first, `default` last. Action determines whether to bridge to
   upstream or close immediately.
4. If allowed, open the upstream connection and bridge bytes. Track byte counts and
   duration.
5. Write the audit record on close.

DNS handling is symmetric: a DNS request hits the interceptor, which resolves via the
host's resolver, caches the (name -> IP) mapping for that session, and uses the
mapping to fill the `host` field on subsequent TCP records that hit a recently-resolved
IP.

### What's not routed through it

The host outrig process itself -- the part that talks to LLM providers, pulls registry
images during `outrig build`, etc. -- is not in the container's network namespace and
its traffic is never redirected. LLM provider audit lives at a different layer (the
chat/tool-call transcript). Build-time registry pulls happen before any session starts.

### MITM mode (final phase)

When MITM is enabled (an opt-in flag, off by default):

- outrig generates a CA per session and writes its public cert into the container's
  trust store (`/etc/ssl/certs/`, plus language-specific stores like `NODE_EXTRA_CA_CERTS`
  for Node, the Python `certifi` bundle, etc.) at startup.
- The interceptor terminates incoming TLS using a per-host leaf cert signed by that CA,
  inspects the request, applies URL-aware policy, then opens its own TLS connection
  upstream.
- Audit records gain `url`, `method`, and `status` fields. Body capture is a separate
  opt-in with a size cap.

The CA and its private key live only in `<session_dir>/`, never on disk anywhere a
non-session process would expect to find a trust anchor, and are deleted with the
session. The CA is *not* trusted by anything on the host.

## Phased delivery

Three phases. Each becomes its own chain of `plan/todo/NNNN-*` tasks.

### Phase &alpha; -- plumbing

**Goal:** every outbound connection from a session container appears in
`<session_dir>/logs/network.jsonl`. No enforcement; effective policy is always-allow.

Deliverables:

- Per-session network namespace + iptables/netavark redirect of TCP and UDP/53.
- Host-side listener that bridges accepted connections to their original destination.
- DNS interception with name-IP cache.
- JSONL audit writer with the host/ip/port/proto/bytes fields.
- Smoke test: `curl https://example.com` from inside the container produces one
  audit-log line with the right host and bytes.

### Phase &beta; -- enforcement (no MITM)

**Goal:** policy from `[network]` is enforced at the host:port level.

Deliverables:

- TOML schema and validator for the `[network]` block; integrate with the existing
  `deny_unknown_fields` config loader.
- Rule matcher (host glob / CIDR / port).
- HTTPS SNI parsing, HTTP `Host`-header parsing, SSH banner sniffing.
- Connection close on deny, with the audit record marked `"action": "deny"` and
  `bytes_tx`/`bytes_rx` zero.
- Doc updates: drop the TODO line in `doc/concepts/workspace.md`; add the `[network]`
  table to `doc/reference/config.md`.

### Phase &gamma; -- MITM

**Goal:** policy and audit can apply at the URL/body level for HTTPS.

Deliverables:

- Per-session CA generation; install into the container's trust store as part of the
  existing post-`podman-run` bootstrap.
- TLS termination + per-host leaf cert minting in the interceptor.
- URL-aware rules (extension to the rule schema; concrete syntax decided then).
- `inspect-bodies` opt-in flag with a size cap.
- Doc page added to `doc/SUMMARY.md` if the feature has grown beyond what fits in
  `doc/concepts/workspace.md`.

## Open sub-decisions

These are deliberately left for the task author who picks each phase up:

- **Interceptor process model.** One per session, one per user shared across sessions,
  or a daemonized service? Per-session is simpler; shared lets DNS caches warm up.
- **Rule syntax.** Host globs, CIDR for IPs, full URLs once MITM lands -- pick a
  concrete grammar and stick to it. The example above leans toward
  `{ host, port, action }` tables; an alternative is a flat string DSL like
  `"allow github.com:443"`.
- **DNS strategy.** Intercept and resolve via the host resolver (recorded above), or
  proxy DNS to a host-side resolver via TCP and trust the container's resolv.conf? The
  former is more reliable for the audit log's `host` field.
- **Source attribution.** The audit log doesn't currently record *which* MCP server's
  process opened the connection. Adding that requires correlating socket source-PID
  inside the namespace with the spawning MCP server. Useful, not free.
- **Build-time network policy.** `outrig build` runs `buildah` on the host, outside any
  session. If users want to firewall builds too, that's a separate feature on a
  different surface.

## Doc updates required when this ships

The future tasks that implement each phase should include the corresponding doc churn:

- `doc/concepts/workspace.md` -- drop the "egress interception ... is deferred" TODO
  block once Phase &beta; lands.
- `doc/reference/config.md` -- add the `[network]` schema, the `default` key, the
  `rules[*]` table, and the validation rules.
- `plan/todo/0022-e2e-acceptance.md` -- remove "Network egress interception (CONNECT
  proxy + allowlist)" from the deferred-list.
- New page `doc/concepts/network-interceptor.md` (linked from `doc/SUMMARY.md`) when
  the surface area exceeds what fits in `workspace.md`. Likely needed by Phase &gamma;.

## See also

- `doc/concepts/workspace.md` -- the v0 stance this feature replaces.
- `doc/concepts/mcp-servers.md` -- where the per-session log convention comes from.
- `doc/reference/config.md` -- formatting model for the eventual `[network]` schema.
- `plan/todo/0022-e2e-acceptance.md` -- the deferral note unwound when this lands.
