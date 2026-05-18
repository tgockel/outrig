# 0064 -- Network interceptor MITM

## Context

Tasks 0059 and 0060 give outrig audited and enforceable host:port egress policy.
They cannot see HTTPS URLs, methods, statuses, or bodies because TLS remains
opaque. This final phase adds opt-in MITM TLS so policy and audit can apply at
the URL/body level.

This is deliberately queued after the other post-v0 work because it changes TLS
trust inside the session container and has a larger failure surface than
host:port interception.

## Goal

Add opt-in HTTPS MITM mode for session containers, with per-session trust roots,
URL-aware policy, and expanded audit records.

## Deliverables

- Per-session CA generation with private material stored only under the session
  directory.
- Container trust-store installation during startup, including common language
  stores where practical.
- TLS termination in the interceptor plus per-host leaf certificate minting.
- Upstream TLS connection from the interceptor to the real destination.
- URL-aware rule extensions for policy matching.
- Optional request/response body capture with an explicit opt-in flag and size
  cap.
- Documentation for the security model, limitations, cleanup behavior, and how
  to inspect the expanded audit records.

## Runtime Behavior

MITM mode is off by default. When enabled, outrig generates a CA for the session,
writes the public cert into the container trust store, and uses the private key
only in the session's interceptor process. The CA is not trusted by the host and
is removed with the session.

The interceptor terminates incoming TLS using per-host leaf certificates signed
by the session CA, inspects the request, applies URL-aware policy, and opens its
own TLS connection upstream.

Audit records gain `url`, `method`, and `status`. If body inspection is enabled,
records may also include capped `request_body` and `response_body` fields.

## Acceptance

- MITM mode is opt-in and leaves 0060 behavior unchanged when disabled.
- A session with MITM enabled can make HTTPS requests from inside the container
  without certificate validation failures for common system tools.
- URL-aware allow/deny rules apply to HTTPS requests after TLS termination.
- Audit records include URL, method, and status for inspected HTTPS requests.
- Body capture is disabled by default and capped when enabled.
- The generated CA private key is scoped to the session directory and is not
  installed as a host trust anchor.

## Dependencies

- **Hard: 0060**. MITM builds on the host:port enforcement path and extends its
  rule matcher and audit record shape.

## See also

- `0059-network-interceptor-plumbing.md` -- traffic capture and audit logging.
- `0060-network-interceptor-enforcement.md` -- host:port policy enforcement.
- `doc/concepts/workspace.md` -- existing workspace/network concept page.

## Decisions

- **MITM is an orthogonal toggle, not a fourth `mode` value.** Filter and MITM compose -- filter
  handles host-level denies pre-TLS, MITM handles URL-level denies post-TLS. Adding a fourth
  mode would have forced an unhelpful choice between observability and enforcement.

- **HTTP/1.1 and HTTP/2 both supported.** Hyper 1.x serves both protocols cleanly; ALPN
  advertises `[h2, http/1.1]` in each direction. The audit record shape is identical between
  the two.

- **URL rules share the existing `allow`/`deny` lists via an optional `path` glob.** Inline-
  table form only; the string sugar (`"host:port"`) is *not* extended. Entries with `path` are
  skipped by `decide_pre_mitm` so they only fire post-TLS termination. A `path` entry without
  `[network.mitm].enable = true` is a schema error so the config stays coherent.

- **Bodies stored inline as base64 in `network.jsonl`.** Self-contained log, easy to grep/jq,
  capped at 64 KiB default. Side files would scale further but complicate cleanup. The
  `_b64` suffix on field names tells consumers what to do.

- **CA validity is 30 days.** Long enough for any plausible session length, short enough to
  limit blast radius if `ca.key` leaks. The key is 0600 inside the session directory and
  removed by `NetworkInterceptor::shutdown`; `ca.crt` stays so audit-log readers can verify
  recorded handshakes after the session ends.

- **IP-only HTTPS (no SNI) passes through opaquely.** The interceptor cannot honestly mint a
  name-bearing leaf for an IP destination, so it declines to terminate TLS in that case.
  URL/method/status are not captured for those connections; service stays `ssl` (not
  `ssl-mitm`) so audit consumers can filter cleanly.

- **`reject_repo_network_policy` extended to allow `[network.mitm].enable` but reject `ports`,
  `capture-bodies`, `max-body-bytes`.** A repo can opt *out* of MITM (or opt in if the global
  hasn't), but cannot widen the capture footprint. Mirrors the 0060 decision that the
  expensive `allow`/`deny` lists are global-only.

- **Trust-store install writes three PEMs and sets four env vars** via a single
  `podman exec --user=0:0` heredoc script. Both `update-ca-certificates` and
  `update-ca-trust extract` run with `|| true` so the wrong-distro updater fails silently.
  `NODE_EXTRA_CA_CERTS` points at the standalone PEM (Node ignores the system bundle); the
  other bundle vars point at the system bundle path that `update-ca-certificates` /
  `update-ca-trust` rebuild after our anchor is added.

- **MCP servers spawned via `podman exec` get the trust-store env explicitly,** not via
  `/etc/profile.d` (which `podman exec` doesn't source). `session_setup::mitm_session_env`
  produces the map; `connect_mcp_clients` merges it under per-server CLI overlay (so explicit
  user intent wins).

- **One audit record per HTTP request, sharing the Zeek `uid` of the TCP flow.** Keep-alive
  and HTTP/2 multiplexing both carry many requests over one connection; grouping back to the
  TCP flow via `jq 'group_by(.uid)'` is the documented consumer pattern. Service is
  `ssl-mitm` (vs. `ssl`) on MITM records to distinguish them.

- **CA generation is lazy, at interceptor start time -- not eager at session create.** Sessions
  with MITM off pay no rcgen / rustls overhead and never touch `<session_dir>/tls/`.
