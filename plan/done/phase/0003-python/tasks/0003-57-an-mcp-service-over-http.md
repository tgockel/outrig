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

## Decisions

- **The user's calls (2026-11-21).**
  - Fork 3: plain `http` to a host that is not loopback starts, and `mcp.service.started`
    gains `detail` -- `null` otherwise -- saying the headers went in the clear. No MCP event had a
    `detail`, so this is the one field the task's "nothing new is added" admits.
  - Fork 4: the config `url` form lands here, with its approval and `unapproved`.
  - `outrig run` and `outrig mcp` skip a `url` entry, printing one warning naming each, after
    every label has merged, so it still shadows a label entry of its name.
  - Forks 1, 2 and 5 as recommended: the anchor beside the system roots, `trust = "only"` in their
    place; `allow_stateless` true; the system proxy honored, `#[cfg(test)] .no_proxy()` as the model
    loop's client does.
- **"Unverified", confirmed against rmcp 3.4.1.**
  - `StreamableHttpClientTransport::with_client` takes a `reqwest::Client` built with a custom
    root (`a_trust_anchor_decides_what_is_trusted`). reqwest 0.13's `tls_certs_merge` gives the
    platform verifier extra roots; `tls_certs_only` a webpki store of the anchor alone.
  - `custom_headers` ride every request: `initialize`, its notice, `tools/list`, `tools/call`, the
    standing GET, the cancel notice (`headers_reach_every_request_and_show_nowhere`) and the
    `DELETE` at the close (`a_url_service_is_no_writer`). rmcp reserves `accept`,
    `mcp-session-id` and `last-event-id`, not `authorization`; its `auth_header` prefixes
    `Bearer ` and would duplicate a custom `Authorization`, so it stays unset and every header
    travels as declared.
  - Which errors carry a status: on a POST, 401 or 403 with `WWW-Authenticate` is `AuthRequired`
    / `InsufficientScope`, 404 with a session `SessionExpired`, any other non-2xx
    `UnexpectedServerResponse("HTTP <status>: <body>")` -- the status in text only, which
    `status_said` reads -- and a JSON-RPC error body `JsonRpcError`; GET and `DELETE` give
    `Client(reqwest::Error)` with `.status()`. Connect, TLS and timeout failures are `Client(e)`.
    It reaches `serve_client`'s caller as `ClientInitializeError::TransportError`, recovered by
    `downcast_ref::<StreamableHttpError<reqwest::Error>>()`. `Client` names reqwest's error as no
    source, so the message renders reqwest's chain itself.
  - That JSON-RPC branch drops the status: a gateway's 503 or 429 at `initialize` whose body is a
    JSON-RPC error reached the classifier as the server's answer, `refused`, never retried
    (review). No reqwest hook sees a response's status before rmcp reads it -- its retry
    classifier sees no request headers to tell requests apart -- so the client rmcp drives is a
    `Dialer` of OutRig's: rmcp's own reqwest client for every request but `initialize`, whose POST
    it sends itself, as rmcp does, reading any non-2xx answer as its status and a 2xx one as rmcp
    reads it (`sse-stream`, already rmcp's, parses the stream). Only `initialize`: elsewhere a
    JSON-RPC error with a non-2xx status is the server's answer to that request, and a call's
    dispatch state rests on it.
  - Reading that answer's body first let a 401 or 403 whose body never ended become the attempt's
    timeout, retried, then `unavailable` (review). The `Dialer` now reads a challenge as rmcp does,
    before the body -- `AuthRequired` or `InsufficientScope` -- and waits for any other non-2xx
    body at most a second, or half the attempt's time left, keeping what arrived: the status is
    what the answer means, and the body only says more. rmcp's own reading of a non-2xx POST
    waits for the body unbounded; that stays, the request no longer being `initialize`.
- **Protocol revision.** `serve_client` sends a plain `initialize` offering 2025-11-25
  (`ProtocolVersion::LATEST`) and takes whatever revision the server answers, unchecked, as the
  stdio client does; the transport then sends `MCP-Protocol-Version` with it. Against rmcp's own
  server this is its legacy session mode: a session id is issued, and `notifications/cancelled`
  is POSTed (discover mode would keep a cancel local). `mcp_proxy.rs`'s review note and
  `REVIEWED_RMCP`'s message name both.
- **Session recovery is off** (`reinit_on_expired_session = false`). rmcp's would re-send only a
  POST answered 404, which a compliant server never processed, but it re-initializes behind the
  session: no relist, no `mcp.service.reconnected`, no recheck of the call against the new
  listing. Off, a dead session is a transport failure and `0003-56`'s reconnect makes the new HTTP
  session. A call that meets the 404 reads as may have dispatched, the conservative reading:
  `plan/next/a-404-call-was-not-processed.md`.
- **Bounds.**
  - SSE resumes: three -- at once, then after half a second, then after a second, or after the
    server's own `retry:` hint where that is longer (rmcp's server says three seconds). rmcp
    resumes forever by default, which holds a call whose server died until its deadline; bounded,
    the call ends `TransportClosed` -- transport, may have dispatched, the service down. The same
    policy bounds the standing GET stream, which rmcp, once it gives up, only logs: the session
    stayed up and deaf, its listing changes unheard and the service never stale (review). rmcp
    takes one policy for both kinds of stream and tells it nothing of which, so the policy,
    `Resumes`, cancels a token when it gives up on either, and the table's listener marks the
    service disconnected -- `mcp.service.disconnected`, no call failing -- so the next call
    reconnects: a new session and standing stream, its tools listed again. A call's own stream
    that gives up disconnects it anyway, so the two say the same; the standing stream is not
    resumed apart from calls, which would need the unbounded resumes the bound exists to end.
  - The policy never saw a standing stream that failed to open: rmcp asks for it once and wraps
    it for resuming only once it opens, logging a failed opening as it logs a given-up resume
    (review). So the `Dialer` answers a failed opening -- anything but 405, rmcp's "no stream to
    give", which still leaves the session without one -- with a stream that ends at once, and
    rmcp resumes that as a stream that ended, by `Resumes`: three tries, then the disconnect. The
    opening is the connection's first `GET` with no event id; a response stream resumes only
    after an event id. A retry loop of OutRig's own would have repeated the schedule and its
    give-up; an immediate disconnect would have made one 503 a reconnect.
  - Connect timeout 10 s, for the resumes nothing else waits on; no request timeout, which would
    cut the event streams -- an attempt's bound and a call's deadline bound the rest.
  - Largest server-sent event: 4 GiB, `service-result-max-bytes`'s ceiling. rmcp's 16 MiB would
    end a call the table's 64 MiB bound takes as a transport failure.
  - Cancel notice POST: rmcp's 5 s; the call waits on it no longer than `SHUTDOWN_GRACE`. The
    `DELETE` gets what `McpClient::shutdown`'s grace leaves, as best it can.
  - No redirect followed, so headers reach no other address; no idle connection kept, as rmcp's
    own client keeps none.
- **The placement.** A crate-private `Http` in `mcp_placement.rs`, one host per URL service keyed
  `<url:NAME>` -- no sidecar name can be -- which `McpServices::start` builds itself, so the
  session's `place` closures and test placers stay as they were. It holds the identity pairs (an
  owner's service only) and the client, built at the first connect and reused by reconnects; a
  trust anchor that cannot be read or parsed is `misconfigured`. `freeze`, `thaw` and `stop` do
  nothing, so a pause holds the service's calls and waits for those running, and the close ends
  the HTTP session. Header values are marked sensitive. `McpStopHow::Disconnected` is new --
  neither `stopped` nor `with_primary` is true of a server the session does not run -- and its
  row is proven, since nothing of the session's ran there. `writes` is `Nothing`.
- **A classified failure crosses `Placement::connect`** as a crate-private
  `Unconnected { Outrig, Http }`, rather than a process faked into the public
  `McpStartupFailure`; its HTTP side has no `exit` or `stderr_tail`, and a listing failure with no
  stderr file has no tail either.
- **Classification** (`mcp/http.rs`, `http_class`): `AuthRequired`, `InsufficientScope`, 401 and
  403 refused; rustls's `InvalidCertificate` among the error's causes -- an `io::Error` looked
  inside, since its `source` skips what it wraps -- a 404 at `initialize`, a reserved header and a
  missing session where one is required misconfigured; a connect, request or body failure, a
  timeout, 408, 429 and 5xx, a broken stream and an expired session transient; the rest
  unclassified. `rustls` is a direct dependency for that downcast alone, at the version reqwest
  already locks. A listing that fails after `initialize` under an HTTP failure is classified the
  same way (review): `McpSessionError` carries the class, crate-private as
  `McpStartupFailure`'s, and `Failed::of` reads it before the kind, so a 401 or 403 at
  `tools/list` is `refused` after one attempt and an expired session's 404 stays transient.
- **Checked before any request**, at plan or resolve time and never quoting the URL or a value:
  a URL that does not parse, userinfo, a scheme other than http or https, `trust = only` with no
  anchor, a header name that is not one, one named twice in two cases, one HTTP or the transport
  sets itself (`accept`, `connection`, `content-length`, `content-type`, `host`,
  `last-event-id`, `mcp-method`, `mcp-name`, `mcp-protocol-version`, `mcp-session-id`,
  `transfer-encoding`, and any `mcp-param-*`), one naming an identity header -- for any
  declaration, an owner's or a configuration's -- and a value, literal or resolved, that is not a
  header value. Under protocol 2026-07-28 rmcp sets `Mcp-Method` and `Mcp-Name` on each request
  and `Mcp-Param-*` from a tool's annotated arguments, replacing a custom header so named, so an
  identity header so named would change between requests (review): the owner's identity names
  are checked as a declaration's headers are, and an owner's server is `misconfigured` while one
  is such a name; a configured server, which never carries the identity, is not. A `${VAR}` in a URL's headers is allowed: the host
  sends it, and no container sees it.
- **Config.** `McpServerSpec::Full` gains `url`, `headers`, `trust-anchor`, `trust` and
  `allow-stateless`, each elided unset, so no image's label or cache key changes. `trust` is
  `McpTrust`, `"with-system-roots"` or `"only"`, a string enum with a named default, in `config`
  and re-exported at the root as `SidecarView` is. A url entry takes no container key, and an
  entry without `url` none of its keys (`McpUrlConflict`). A url entry is
  `container::sidecar::Placement::Url`, which puts it in `placed_elsewhere` and out of the launch
  spec's map with no change there, and lets it shadow a label entry. A repo config's header
  reference is held to `env-allow` (`RepoMcpHeaderRef`, whose `declared_in` is boxed: a second
  variant as large as `RepoMcpEnvRef` cost `ConfigValidationError` its niche and every `Result`
  carrying it 8 bytes, which clippy's `result_large_err` refuses). A label refuses `url` and its
  keys as `PlacementInLabel`. `trust-anchor` resolves against the declaring file's directory.
- **Approval.** `ApprovalRequest` is a struct, so it gains a field, `subject: ApprovalSubject`
  (`Binding` or `McpUrl { url, headers }`), not a variant. The shown `declaration` is the entry's
  table with its headers by name. The remembered digest -- prefix `outrig mcp url approval 1`,
  never a binding's -- is over the repository root, the table and the entry as declared, each
  header's value or reference included, so an edit asks again while no value is shown or written
  to the store. Approvals share the bindings' store. A repo entry is unapproved until asked, so a
  path that forgot to ask starts nothing; `start` and `start_on_host` ask before the launch,
  through `spawn_blocking`, as bindings are asked. `TerminalApprover` words its prompt by subject.
- **Python.** `outrig.McpUrl`; a path anchor is made absolute in the program, and bytes are sent as
  `{"pem": ...}`.
- **Test crates, after a survey.** A server's TLS is `axum::serve` over a small
  `axum::serve::Listener` doing the rustls accept, in place of `axum-server`, which nothing locks,
  or hyper-util by hand. `tokio-rustls` 0.26.4, the rustls project's, is already compiled in
  through reqwest's hyper-rustls; its crate ships its suite (`tests/test.rs`, `badssl.rs`,
  `early-data.rs`). `rcgen` 0.14.10, the rustls project's certificate minter, ships 37 unit tests
  and checks what it mints against openssl in its dev-dependencies; it runs on `ring`, already in
  the graph for the build script, because its `aws_lc_rs` feature raised the lock's aws-lc-rs
  1.17 to 1.19 under every TLS user.
- **A configured entry is its spec.** `ConfiguredUrl` holds the entry as declared -- what is
  dialed, and what its approval shows and digests, so the two cannot differ -- with its trust
  anchor resolved against its file, its table, and why it may not start yet: a repository's
  entry starts out not asked. A binding's approval digest is the one recorded before, byte for
  byte (`a_bindings_digest_is_the_one_recorded_before`).
- **The loopback server** runs on a runtime of its own, so stopping it cuts every connection
  unanswered, as a server that dies does; `fake::Served` is the toolbox as a typed rmcp handler,
  which rmcp's HTTP server takes and the raw form is not. A front records each request and can
  answer a status in the server's place, or cut a call's connection once the server has it. It
  says a 50 ms `retry:` hint, so the reconnect test waits on the client's resumes, not on rmcp's
  three-second default.
