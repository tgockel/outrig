# 0060 -- Network interceptor enforcement

## Context

Task 0059 makes every outbound session-container connection visible in
`<session_dir>/logs/network.jsonl`, but it allows all traffic. This task adds
host:port policy so users can grant legitimate network access without granting
blanket egress.

Policy applies only to traffic from the session container. The host outrig
process's own outbound calls, including LLM provider HTTPS and registry pulls
during `outrig build`, stay out of scope.

## Goal

Enforce global network allow/deny policy at the host:port level and record the
matched action for every audited connection.

## Deliverables

- TOML schema and validation for a global `[network]` block in
  `~/.outrig/config.toml`.
- Rule matcher for host globs, IP/CIDR patterns, optional ports, and
  `allow`/`deny` actions.
- Policy integration into the interceptor before upstream connections are
  opened.
- Denial behavior that closes the socket immediately and writes an audit record
  with `"action": "deny"` and zero byte counts.
- Documentation updates for `doc/concepts/workspace.md` and
  `doc/reference/config.md`.

## User Surface

Policy lives in global config only. Per-repo overrides are deliberately not
supported in this feature because network policy is a property of the machine
running the agent.

```toml
[network]
mode    = "filter"
default = "deny"   # or "allow"; filter mode defaults to "deny"
allow   = ["*.npmjs.org", "github.com:443"]
deny    = ["*:22", { host = "169.254.169.254", port = 80 }]
```

| Key       | Type   | Required | Default | Description                         |
|-----------|--------|----------|---------|-------------------------------------|
| `mode`    | string | no       | default | `default`, `audit`, or `filter`.    |
| `default` | string | no       | `deny`  | Action when no entry matches.       |
| `allow`   | array  | no       | `[]`    | Host/CIDR entries to allow.         |
| `deny`    | array  | no       | `[]`    | Host/CIDR entries to deny.          |

`allow` and `deny` accept compact string entries such as `"github.com:443"`,
`"*.npmjs.org"`, `"*:22"`, `"10.0.0.0/8"`, and `"[2001:db8::1]:443"`.
They also accept inline tables shaped as `{ host = "...", port = 443 }`.

When `[network]` is absent, behavior matches 0059: normal sessions use
Podman's default networking, and audit mode allows every connection and logs
it. Filtering is enabled only with `mode = "filter"`.

## Runtime Behavior

Deny entries are evaluated before allow entries. If neither list matches,
`default` applies. The matcher uses DNS cache entries, HTTP `Host` headers,
HTTPS SNI, IP address, and port as available from 0059.

A denied connection sees a socket-level rejection, such as TCP RST or immediate
close on CONNECT. Outrig does not print a REPL message for the denial; the user
finds the reason in the audit log.

## Acceptance

- With no `[network]` block, behavior is identical to 0059's allow-and-log mode.
- `default = "deny"` blocks unmatched connections and records `"action": "deny"`.
- A matching allow rule permits the connection and records the matching rule.
- A matching deny rule closes the connection before upstream bytes are bridged.
- Invalid actions, malformed host patterns, invalid ports, and unknown fields
  fail during config validation.
- Docs describe the global-only policy model and the audit-log denial workflow.

## Dependencies

- **Hard: 0059**. Enforcement depends on the interceptor, DNS cache, protocol
  sniffing, and audit-log plumbing from the first network phase.

## See also

- `0059-network-interceptor-plumbing.md` -- traffic capture and audit logging.
- `0064-network-interceptor-mitm.md` -- later URL/body-aware HTTPS policy.
- `doc/reference/config.md` -- formatting model for the `[network]` schema.

## Decisions

- Preserve the existing `default` and `audit` modes. Add `filter` for sessions
  that install the interceptor and enforce policy.
- Keep network policy global-only, but continue allowing repo config to set
  `network.mode`. Repo-local `default`, `allow`, or `deny` entries are invalid.
- Replace ordered `[[network.rules]]` with compact `allow` and `deny` lists.
  Both string entries and inline `{ host, port }` tables are accepted; string
  entries map to the same typed host/port form.
- Deny wins when a destination matches both lists. If neither list matches,
  the configured `default` action applies.
- `mode = "filter"` requires at least one `allow` or `deny` entry, even when
  `default = "allow"`, so filter mode cannot silently behave like audit mode.
- The public Rust launch API gets a typed `NetworkPolicy` builder and
  `LaunchSpec::with_network_filter(policy)`, which selects `filter` mode.
