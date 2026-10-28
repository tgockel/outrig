# 0003-57 -- An MCP service over HTTP

## Context

`crates/outrig/Cargo.toml:60` enables rmcp's `transport-child-process`, `transport-io` and
`transport-streamable-http-server` features: OutRig serves streamable HTTP (`outrig mcp --listen`)
and speaks it to no one. The locked rmcp, 3.4.1, has the client side behind
`transport-streamable-http-client` and its reqwest backing behind
`transport-streamable-http-client-reqwest` (the registry copy's `Cargo.toml`); its
`StreamableHttpClientTransportConfig` (`src/transport/streamable_http_client.rs:2035`) carries the
`uri`, `custom_headers` and `auth_header` (`:2055-2057`), `allow_stateless`, a retry policy for the
SSE stream, `max_sse_event_size` and timeouts for control requests and session recovery, and
`StreamableHttpClientTransport::with_client(client, config)` (`:2028`) takes the HTTP client it
sends through. The `outrig` crate already depends on reqwest with `rustls`, privately, for the
model loop. `McpClient` is built over `serve_client` from any transport
(`crates/outrig/src/mcp.rs`), so an HTTP service is the same client over another transport, and
the service mechanism's deadline and cancel (`services.md`) and `0003-56`'s dispatch states apply
to it unchanged.

After `0003-54` a declaration's placement is `Command`, `Sidecar` or `PrimaryView`, the enum is
`#[non_exhaustive]`, identity headers are held with nowhere to go, and a `${VAR}` in a declaration
resolves through the session's `Secrets` (`crates/outrig/src/config/api_key.rs:46`). The config's
entry shapes have no URL form (`McpServerSpec`, `crates/outrig/src/config/mod.rs:2870`). A-08
freezes the 0.2 facade; `McpServerSpec` is not among its items, and this task adds a variant to a
`#[non_exhaustive]` enum and keeps `url` entries out of a `LaunchSpec`'s `mcp` map, so
`Outrig::launch`'s behavior for every input it accepts today is unchanged.

The requirement is OR-MCP1's URL placement -- "a `url` toolset needs an HTTP client transport in
OutRig, with a custom trust anchor, static resolved headers and identity headers", preferred over
a client the application holds so there is one MCP implementation -- and OR-MCP4's carrying of the
owner's opaque identity to services that can enforce a fence. The design is
`plan/phase/0003-python/mcp-presenter.md` ("Declarations", "Startup", "Secrets, placement and
what a service grants"); its classification table gives HTTP 401 and 403 to `refused` and 429 and
5xx to `transient`, and its "Unverified" names what this task confirms first: that rmcp's client
takes a reqwest client built with a custom root certificate, that it carries the configured
headers on every request, and which of its errors carry an HTTP status.
`plan/next/rmcp-list-result-spec-gaps.md`'s review obligation extends to the client's negotiated
protocol
revision. A URL service reaches no file the session mounts, so it is not a writer for the cut.

## Goal

A declaration naming a streamable-HTTP URL starts as a service like any other -- with its resolved
headers on every request, the owner's identity headers too when the owner declared it, a trust
anchor of the owner's choosing, HTTP failures classified for retry or refusal -- and reconnects as a
new HTTP session.

## Deliverables

- **The placement.** `McpPlacement::Url { url, headers, trust_anchor, trust, allow_stateless }`:
  `headers` as `EnvValue`s, resolved by the session's `Secrets` once at start; `trust_anchor`, an
  optional PEM certificate by path or bytes; `trust`, whether the anchor is added to the system
  roots or replaces them (fork 1); `allow_stateless` (fork 2). A URL carrying userinfo, a scheme
  other than `http` or `https`, or a header naming an identity header is `misconfigured`. `http`
  to a host that is not loopback is allowed and is an event's `detail`, since the headers travel
  in clear (fork 3).
- **The transport.** `transport-streamable-http-client-reqwest` enabled; a `reqwest::Client` per
  service built with the trust anchor and the workspace's rustls; rmcp's config filled with the
  URL, `custom_headers` holding the resolved headers and the identity headers (and `auth_header`
  where rmcp reserves the name), `allow_stateless`, and bounds of the task's choosing for the SSE
  event size and the control timeouts, recorded in `## Decisions`. `McpClient` gains a constructor
  over this transport, with no child process to own; `shutdown` ends the HTTP session as best it
  can (the protocol's `DELETE`) and says nothing about the server.
- **Classification.** The transport's errors mapped into the startup classes and `McpFailureKind`:
  401 and 403 `refused`; a TLS verification failure, a 404 at `initialize` and an unparseable URL
  `misconfigured`; connection refused or reset, a timeout, 408, 429 and 5xx `transient`; the rest
  `unclassified`. A test names every variant of rmcp's HTTP error type the mapping handles, as
  `every_rmcp_service_error_is_classified` does, and the `REVIEWED_RMCP` constant covers it.
- **Identity.** The owner's identity headers from `0003-54` are stamped on every request of every
  URL service the owner declared on the builder or `outrig.Session` -- `initialize`, `tools/list`,
  `tools/call`, the GET stream, the cancel notice -- carried unchanged and never read, logged or
  previewed; a URL service a configuration declared receives its resolved `headers` and no identity
  header, whatever its approval, since its URL is a destination the repository's author chose
  (`mcp-presenter.md`, "Identity and the fence token"). A fence-aware server enforces with the token
  it is handed; the task promises nothing of an unfenced one.
- **Reconnect.** A disconnected URL service is re-established as a new HTTP session with the same
  client and the values resolved at start, under `0003-56`'s rules and bound. rmcp's own session
  recovery on an expired session is configured so that it never re-sends a request the server may
  have processed (its `session_recovery_timeout` semantics), or is turned off; `## Decisions` says
  which and why.
- **Configuration** (fork 4): `[images.<name>.mcp]` gains a `url` entry form -- `url`, `headers`,
  `trust-anchor`, `allow-stateless` -- mutually exclusive with `command`, `sidecar` and `image`; a
  `${VAR}` in a repo config's `headers` is held to `[repo] env-allow` as `env` is, and a
  repository's `url` entry starts only once the user approves that exact declaration in that
  repository, through the `Approver` a repository's binding already passes
  (`crates/outrig/src/harness/approval.rs`; `doc/reference/config.md`, "A repository's binding needs
  your approval"), with `ApprovalRequest` gaining a variant that shows the URL and the header names
  and never a value, the approval remembered as a binding's is, and the global config needing none
  -- the host dials a URL service itself, so a repository's entry would otherwise reach loopback and
  host-only services the container's aliases keep from the agent, which is authority the entry's
  author chose and the user did not; an unapproved entry connects to nothing and is the outcome
  `unapproved`, naming the entry, for which this task adds `McpServiceOutcome::Unapproved` and the
  `mcp.service.failed` outcome value; an `org.outrig.mcp` label rejects `url` as it rejects
  placement keys; `LaunchSpec::from_config` leaves a `url` entry out of the spec's `mcp` map, as it
  leaves sidecar-placed entries out (`placed_elsewhere`), so the facade never sees one (A-08).
  `doc/reference/config.md` documents it.
- **Not a writer.** `writers()` reports a URL service as no writer, and `quiesce` on it holds
  dispatch and waits for in-flight calls only, with nothing to freeze.
- **Protocol revision.** `## Decisions` records which revision the client negotiates and how a
  server answering in another is handled, and the review note in `mcp_proxy.rs` names the client
  too.
- **Events.** `mcp.service.started` carries `placement: url`; nothing new is added.
- **Docs.** `doc/concepts/mcp-servers.md` gains the URL placement; `doc/concepts/mcp-trust-model.md`
  says what it grants -- the agent's calls leave the host under the resolved headers and, for an
  owner-declared service, the identity OutRig stamps, so every tool the server offers that identity
  is the session's, and a configured URL never carries the owner's identity; `SECURITY.md`'s entry
  from `0003-54` gains the sentence; `crates/outrig/CHANGELOG.md`.
- **`## Decisions` records** the forks, the confirmed or refuted items of the page's "Unverified"
  that concern HTTP, the bounds chosen, and the test server and certificate crates chosen after a
  survey (below).

## Acceptance

Mock-driven: rmcp's streamable-HTTP server (its server feature is already enabled) in-process on
loopback over axum, fronting the fake handler of `0003-54`'s tests; a TLS variant for the trust
anchor, served through a crate the task chooses after surveying candidates -- `axum-server` with
rustls, `tokio-rustls` over hyper -- with a certificate minted at test time by a crate chosen the
same way (`rcgen` is the obvious candidate), each with its test-suite evidence in `## Decisions`.

- **It starts and serves.** A `Url` declaration against the loopback server is `Ready`, lists the
  fake's tools, and a call through `0003-55`'s surface returns the result; `mcp.service.started`
  says `placement: url`.
- **Headers on every request.** With `X-Token = "${TOKEN}"` in `headers` and two identity headers on
  the builder, the server asserts all three on `initialize`, `tools/list`, `tools/call`, the GET
  stream and a cancel notice; the token's value appears in no event, outcome, report, `repr`,
  `help()` or exception, checked by searching their serialized forms. A declaration naming an
  identity header is `Misconfigured` before any request. A `url` entry from a repository config
  against the same server, approved, receives `X-Token` and neither identity header on any request.
- **A repository's entry needs approval.** A `url` entry in a repository config against the loopback
  server with an `Approver` that refuses: no connection is made (the server's request count stays at
  zero), the outcome is `unapproved` naming the entry, and the session starts without it; with an
  `Approver` that approves, it is `Ready`, and the request shown to the approver carries the URL and
  the header names and no header value. The same entry in the global config asks nothing and starts.
- **Trust anchor.** Against the TLS server with a minted certificate, the declaration without an
  anchor is `Misconfigured` with a TLS cause and one attempt; with the anchor it is `Ready`. With
  `trust` replacing the system roots, the loopback server's certificate is the only one accepted
  (a second minted authority is refused).
- **Classification.** A server answering 401 to `initialize` gives `Refused` after one attempt;
  one answering 503 then 200 gives `Ready { attempts: 2 }` with one `mcp.service.retry`; a closed
  port gives retries until the budget then `Unavailable`; 404 gives `Misconfigured`. The variant
  test names every HTTP error variant the locked rmcp declares.
- **Reconnect as a new session.** The server is stopped and started again on the same port after
  a call; the next call reconnects with a new MCP session id, relists, and dispatches once; the
  earlier call is in `uncertain()` and is never re-sent; the counting `Secrets` was not called
  again.
- **Dispatch states over HTTP.** A server that closes the connection after reading a `tools/call`
  gives `MayHaveDispatched`; a slow tool under `timeout=1` gives kind timeout and the server sees
  the cancel notice; a tool answering `isError` gives `ToolError`.
- **Configuration.** A `url` entry parses and lowers to the declaration; one beside `command` is a
  validation error; a `${VAR}` in a repo config's `headers` not in `env-allow` is refused at load;
  a label with `url` is refused.
- **Not a writer.** `writers()` lists the URL service as no writer; `quiesce(Pause)` on it with no
  call in flight returns `Quiesced` without freezing anything.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; the docs say what the code does, checked
  by reading.

## Design forks

1. **The trust anchor beside or in place of the system roots -- Recommended: beside, with a
   `trust = "only"` option.** A private authority usually fronts one service while the system roots
   serve the rest; an owner that wants pinning says so. The alternative makes an anchor exclusive
   always, which breaks a mixed deployment silently.
2. **`allow_stateless` -- Recommended: true by default.** A stateless server answers each POST
   without a session id, and a session-based one still issues one; the alternative refuses a
   server that never issues a session, which many do not.
3. **Plain `http` to a non-loopback host -- Open; recommended: allowed, recorded in the started
   event's `detail`.** Resolved headers travel in clear; an owner on a private network may want
   it. The alternative refuses it as `misconfigured`, which a mixed deployment would route around.
4. **A `url` form in the config -- Recommended: yes**, so `run-new` reaches URL services. The
   alternative keeps URL placements builder-only.
5. **The system proxy -- Open; recommended: honored, as the model loop's client honors it**, with
   `loopback-mocks-follow-system-proxy.md`'s finding noted for the tests, which build their client
   with no proxy. The alternative ignores proxy variables for MCP alone.

## Dependencies

- **Hard: `0003-54`.** The placement enum, the service table, the startup classification, the
  identity headers and the `Secrets` path.
- **Hard: `0003-56`.** The dispatch states, the cancel notice and the reconnect this transport
  goes through; through it `0003-44`'s deadline and token.
- **Soft: `0003-55`.** The Python surface the acceptance calls through; the Rust-side tests do
  not need it.

## See also

- `plan/phase/0003-python/mcp-presenter.md` -- "Declarations", "Startup", "Secrets,
  placement and what a service grants", "Unverified".
- `plan/next/rmcp-list-result-spec-gaps.md`, and the upgrade-review entry `0003-54` folded in
  -- the review the client's revision and errors join.
- `plan/next/loopback-mocks-follow-system-proxy.md` -- the proxy finding fork 5 cites.
- `crates/outrig/Cargo.toml`; `crates/outrig/src/mcp.rs`; rmcp 3.4.1's
  `src/transport/streamable_http_client.rs` in the cargo registry.
