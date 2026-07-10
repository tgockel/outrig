# 0078 -- Interceptor multi-container generalization

## Context

The network interceptor (0059/0060) assumes exactly one container per session: sockets are
bound in one namespace pair, the nft NAT table is applied once, and audit records are stamped
with a single container identity. MCP sidecar containers (see
[`mcp-sidecars-spec.md`](mcp-sidecars-spec.md), "Network parity") require `[network]` policy
to cover every session container, and that generalization must land *before* sidecars become
usable so there is never a state where sidecar egress silently bypasses audit or filter.

## Goal

Generalize `NetworkInterceptor` from one container to N: one interceptor per session owning
the compiled policy and the shared `AuditSink`, with a per-container attach/detach operation.

## Deliverables

- Per-container attach: repeat today's single-container mechanics in the target container's
  namespaces -- fork + `setns` into its user and net namespaces, bind the TCP and DNS sockets
  there, pass fds back via `SCM_RIGHTS`, apply the per-session nft NAT table via `nsenter`
  (netns isolation keeps table names from colliding across containers), and install the audit
  `resolv.conf`.
- Per-container detach: tear down one container's rules and sockets without disturbing other
  attachments; support attach mid-session (for dynamic sidecar addition later).
- Audit records stamp their container field per attachment instead of once per session.
- Full-interceptor teardown walks and detaches every attachment.
- No behavior change for single-container sessions.

## Acceptance

- Two containers attached under one session policy produce correctly attributed audit records
  in both audit and filter modes.
- Teardown leaves no nft tables in either container's netns.
- Existing single-container sessions behave as before (existing interceptor tests pass
  unchanged).

## Dependencies

None queued. Builds on the landed interceptor work (0059/0060 lineage).

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Network parity" section.
- `plan/done/0059-network-interceptor-plumbing.md` -- traffic capture and audit logging.
- `plan/done/0060-network-interceptor-enforcement.md` -- host:port policy enforcement.
