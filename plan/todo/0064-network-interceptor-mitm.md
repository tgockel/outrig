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
