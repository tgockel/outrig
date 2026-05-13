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
default = "allow"   # or "deny"; default-of-default is "allow"

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

| Key               | Type   | Required | Default   | Description                               |
|-------------------|--------|----------|-----------|-------------------------------------------|
| `default`         | string | no       | `"allow"` | Action when no rule matches.              |
| `rules`           | array  | no       | `[]`      | Rules evaluated top-to-bottom.            |
| `rules[*].host`   | string | yes      | --        | Host glob, IP, or CIDR pattern.           |
| `rules[*].port`   | int    | no       | any       | TCP port; omitted means any.              |
| `rules[*].action` | string | yes      | --        | `"allow"` or `"deny"`.                    |

When `[network]` is absent, behavior matches `default = "allow"` with no rules:
every connection is allowed and logged.

## Runtime Behavior

Rules are evaluated top-to-bottom; the first match wins. If no rule matches,
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
