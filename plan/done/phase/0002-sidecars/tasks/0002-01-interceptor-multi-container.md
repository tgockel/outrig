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

## Decisions

- `start` / `start_with_policy` keep their exact signatures as convenience wrappers
  (`new` + `attach` of the primary), so both call sites (`outrig_.rs`,
  `session_setup.rs`) and the existing e2e tests are untouched; sidecar tasks will call
  `new`/`attach`/`detach` directly.
- Attaching an already-attached container name and detaching an unknown name are both
  `Configuration` errors rather than silent no-ops, so 0081's bookkeeping bugs surface
  immediately.
- `detach` takes the container *name*, not `&Container`, so a container that already died
  can still be detached; the nft delete against its defunct pid fails harmlessly
  (`try_capture_logged` tolerates non-zero exit).
- The DNS name-to-IP cache is shared across attachments (session-level): IP-to-hostname is
  container-independent, every attachment forwards through the same host resolvers, and
  sharing lets filter rules and audit `outrig.host` attribution work when one container
  connects to an IP another container resolved.
- The nft table name stays session-derived (identical in every netns); per-container
  uniqueness comes from netns isolation, and per-attachment `Cleanup { pid, table }` scopes
  deletion to one container.
- Audit stamping moved from the sink to per-attachment `AuditSink::for_container` clones
  sharing one file lock, leaving `write_audit`, `AuditRecord`, and the JSONL schema
  untouched.
- The `disposed` flag is gone: `shutdown`/`detach` drain the attachment map, so `Drop`
  (detached per-pid nft deletes) is naturally a no-op after orderly teardown.
- Detach leaves the container's `/etc/resolv.conf` pointing at the loopback listener --
  same as today's post-shutdown state; detach is documented as running just before the
  container stops.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Network parity" section.
- `plan/done/0059-network-interceptor-plumbing.md` -- traffic capture and audit logging.
- `plan/done/0060-network-interceptor-enforcement.md` -- host:port policy enforcement.
