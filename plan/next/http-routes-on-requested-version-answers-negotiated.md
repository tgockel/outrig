# Streamable HTTP routes on the requested version but answers the negotiated one

rmcp picks the HTTP session lifecycle from the version in the *request body* and then lets
negotiation rewrite the version in the *response*. When those disagree the client is stranded:
it is told which revision it is speaking, and the transport has already committed to the other
one's lifecycle.

`is_legacy_request` (`streamable_http_server/tower.rs:359`) reads `params.protocolVersion` off
the initialize body and hands it to `uses_legacy_lifecycle`, which is `version <
V_2026_07_28`. Below the boundary the request takes the session path and the response carries
an `Mcp-Session-Id`; at or above it the request is stateless and no session id is issued.
`negotiate_protocol_version` (`service/server.rs:469`) runs separately and answers with the
server's own default whenever the request is unsupported. Nothing reconciles the two.

Measured against `outrig mcp --listen 127.0.0.1:7331` on this branch:

| requested    | `Mcp-Session-Id` | negotiated   |
| ------------ | ---------------- | ------------ |
| `2025-06-18` | issued           | `2025-06-18` |
| `2026-07-28` | none             | `2026-07-28` |
| `2027-01-01` | none             | `2025-11-25` |

The third row is the broken one. The client asked for something newer, was routed statelessly
and given no session id, and was then told it is speaking `2025-11-25` -- a session-lifecycle
revision. Its next request, sent the way that revision requires, is rejected:

```
POST /mcp  (Mcp-Protocol-Version: 2025-11-25, no session id)
HTTP/1.1 422 Unprocessable Entity
Unexpected message, expect initialize request
```

The session is dead on arrival, one request after a `200 OK` handshake.

This interacts with `SUPPORTED_PROTOCOL_VERSIONS` and is worth understanding before that list is
ever allowed to lag rmcp. Today the pinned list is set-identical to `ProtocolVersion::
KNOWN_VERSIONS`, so every revision rmcp can name is echoed and only an invented string reaches
the fallback -- nothing regressed. But the entire point of pinning is that some future rmcp bump
adds a revision outrig deliberately does not list, and on that day a client requesting it over
HTTP stops getting a merely malformed tool list and starts getting a dead session. That is
arguably the better failure -- loud beats silent -- but it is a different one, and it should be a
choice rather than a surprise.

The real defect is upstream: routing ought to key off the negotiated version, not the requested
one, and this is worth reporting alongside the SEP-2549 gap in
[rmcp-list-result-spec-gaps](rmcp-list-result-spec-gaps.md). Locally, the levers are
`StreamableHttpServerConfig::with_legacy_session_mode(false)` on the `--listen` service, so every
request takes one path regardless of version, or refusing an unsupported version outright instead
of falling back. Pinning `ServerInfo::protocol_version` looks like a fix and is not: it moves the
fallback to the stateless side for newer requests while creating the mirror-image mismatch for
older ones, which are routed onto the session path and would then be answered in a stateless-era
revision.
