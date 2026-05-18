# Network MITM

> Opt-in HTTPS interception. Off unless you explicitly enable it. The session CA is generated
> fresh per session, lives only under the session directory, and is never installed as a host
> trust anchor.

## What MITM does

The 0059/0060 network interceptor sees HTTPS as opaque bytes. It can record the destination IP
and the TLS SNI, but URL, method, status, and body all stay inside the encrypted tunnel.
Policy stops at the connection layer: a single allow for `github.com:443` lets the container
hit every URL on the host.

MITM mode flips that. With MITM on, the interceptor:

1. Generates a per-session ECDSA P-256 CA at startup, writes the public cert to
   `<session_dir>/tls/ca.crt` and the private key to `<session_dir>/tls/ca.key` (mode `0600`).
2. Installs the CA into the session container's trust stores (system bundle + standalone PEM
   + language env vars). The host trust anchors are untouched.
3. Terminates incoming TLS on each HTTPS-bearing port, minting a leaf certificate keyed on the
   client's SNI and signed by the session CA.
4. Opens its own TLS connection upstream, validated against the host's native trust anchors.
5. Inspects each HTTP request, applies URL-aware policy, optionally captures bodies, then
   forwards or rejects.
6. Removes the private key during teardown. The public cert stays so audit-log readers can
   verify recorded handshakes after the session ends.

ALPN advertises both `h2` and `http/1.1` in each direction, so HTTP/1.1 and HTTP/2 clients
both work transparently.

## Enabling MITM

MITM composes with `mode = "audit"` or `mode = "filter"`. It is a separate toggle, not a
fourth value for `mode`. Enable it under `[network.mitm]`:

```toml
[network]
mode    = "filter"
default = "deny"
allow   = ["github.com:443"]

[network.mitm]
enable = true
```

See [Reference -> Config](../reference/config.md) for the full schema, including
`capture-bodies`, `max-body-bytes`, and `ports`.

## URL-aware policy

Entries in `[network].allow` and `[network].deny` may carry an optional `path` glob using the
inline-table form. Entries without `path` keep their 0060 host/port semantics. Entries with
`path` are skipped at TCP-open time and only fire after MITM has parsed the HTTP request
line:

```toml
[network]
mode    = "filter"
default = "deny"
allow   = [
    "api.github.com:443",
    { host = "api.github.com", port = 443, path = "/repos/*" },
]
deny   = [
    { host = "api.github.com", port = 443, path = "/admin/*" },
]
```

`deny` wins over `allow`, and unmatched requests fall through to `default`. Path-bearing
entries require `enable = true`; the validator rejects them otherwise.

## Audit log shape

Every HTTPS request handled by MITM produces one record in `<session_dir>/logs/network.jsonl`
(non-MITM connections produce the 0060 shape unchanged):

```json
{"ts": 1700000000.0, "uid": "C...", "id.orig_h": "10.0.2.100", "id.orig_p": 50123,
 "id.resp_h": "140.82.121.5", "id.resp_p": 443, "proto": "tcp", "service": "ssl-mitm",
 "duration": 0.082, "orig_bytes": 0, "resp_bytes": 0, "conn_state": "SF",
 "local_orig": true, "local_resp": false, "missed_bytes": 0,
 "server_name": "api.github.com",
 "method": "GET", "url": "https://api.github.com/repos/foo/bar", "status": 200,
 "outrig.session_id": "...", "outrig.container": "...",
 "outrig.host": "api.github.com", "outrig.action": "allow", "outrig.rule": "allow[1]"}
```

The Zeek `uid` matches the TCP-level audit record's `uid` for the same flow, so consumers can
group request records with their underlying TCP connection. `service` is `ssl-mitm` (vs.
`ssl` for opaque-pass-through TLS) to make the two record types easy to filter.

When `capture-bodies = true`, the record also carries `outrig.request_body_b64` and/or
`outrig.response_body_b64` (base64-encoded, capped at `max-body-bytes` each) plus an
`outrig.body_truncated` flag (`"request"`, `"response"`, or `"both"`) when the cap fired.
Bodies past the cap still pass on the wire; only the captured copy is truncated.

## Limitations

- **HTTPS to IP literals (no SNI)** passes through opaquely, exactly as it would with MITM
  off. The interceptor cannot honestly mint a name-bearing leaf for an IP destination, so it
  declines to terminate TLS in that case. URL/method/status are not captured for those
  connections.
- **HTTP/2 streams are proxied, not tunneled.** Hyper handles header decoding, flow control,
  and stream multiplexing under the hood, so end-to-end HTTP/2 features (server push, custom
  trailers beyond what hyper exposes) may not round-trip.
- **Client certificate authentication is not supported.** The interceptor accepts client TLS
  with `with_no_client_auth`, so any upstream that requires a client cert from the agent
  side will fail with MITM enabled.
- **CA validity is 30 days from session start.** Sessions running past that window will see
  TLS handshake failures inside the container.

## Cleanup

When the session ends, `NetworkInterceptor::shutdown` removes
`<session_dir>/tls/ca.key`. The public certificate stays at
`<session_dir>/tls/ca.crt` for post-mortem analysis. The container is destroyed at the same
time, so the trust-store installation goes with it.

If you need to inspect the CA after the session, `openssl x509 -in <session_dir>/tls/ca.crt
-text -noout` shows the subject, validity, and basic constraints.

## See also

- [Concepts -> Workspace](workspace.md) -- audit/filter modes and the broader network model.
- [Reference -> Config](../reference/config.md) -- the `[network]` and `[network.mitm]` schema.
- [Usage -> Sessions](../usage/sessions.md) -- session directory layout and `network.jsonl`
  fields.
