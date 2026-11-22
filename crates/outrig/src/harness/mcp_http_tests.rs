//! MCP services dialed at a URL: rmcp's own streamable-HTTP server on loopback, fronting the fake
//! handler the table's other tests use, reached through the URL placement -- its headers on every
//! request, the owner's identity only where the owner declared it, a repository's entry put to the
//! approver, a trust anchor of the owner's choosing, HTTP failures classified, the three dispatch
//! states, and a reconnect that is a new HTTP session.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use http::{HeaderMap, Method, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};

use super::approval::{Approval, ApprovalRequest, ApprovalSubject};
use super::bindings_tests::{output, start, stop, taken};
use super::event::Subscription;
use super::mcp_dispatch_tests::{MCP, starts};
use super::mcp_placement::fake::{Act, Served, Toolbox};
use super::mcp_presenter_tests::{printed, text, tool};
use super::mcp_tests::{of_service, outcome, policy};
use super::services_tests::{self, VAR, of_kind, quick};
use super::{
    McpPlacementKind, McpPolicy, McpServiceOutcome, McpServiceSpec, McpStopHow, McpWrites, Quiesce,
    QuiesceOutcome, ServicePolicy, Session, SessionBuilder, TrustAnchor,
};
use crate::config::{EnvValue, McpServerSpec, McpTrust};
use crate::error::McpFailureKind;
use crate::outrig_::{ConfiguredMcp, ConfiguredUrl, NOT_ASKED};
use crate::python::testing::{until, within};

/// What `${TOKEN}` resolves to: a value no event, outcome, report or exception may hold.
const SECRET: &str = "tok-4f1e9b2c-never-shown";

/// One request the server took: its method, the JSON-RPC method its body names, and its headers.
#[derive(Debug, Clone)]
struct Seen {
    http: Method,
    rpc: Option<String>,
    headers: HeaderMap,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// What a server's front records and is told to do in its place.
#[derive(Clone, Default)]
struct Front {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Answers the next requests they fit are given, the server not asked.
    canned: Arc<Mutex<VecDeque<Canned>>>,
    /// Tools whose call the server is handed, and whose connection is then cut, unanswered.
    cut: Arc<Mutex<Vec<String>>>,
}

/// An answer a front gives in its server's place.
#[derive(Clone, Copy)]
struct Canned {
    fits: Fits,
    status: StatusCode,
    said: Said,
}

/// Which request a canned answer answers.
#[derive(Clone, Copy)]
enum Fits {
    /// Any.
    Any,
    /// A POST of this JSON-RPC method.
    Rpc(&'static str),
    /// A `GET`, for a stream.
    Get,
}

/// What a canned answer's body is.
#[derive(Clone, Copy)]
enum Said {
    Nothing,
    /// A JSON-RPC error answering the request.
    Error,
    /// A first chunk and then nothing, the body never finished; with a `WWW-Authenticate`
    /// challenge when `challenge`.
    Stalled {
        challenge: bool,
    },
}

/// rmcp's streamable-HTTP server, on loopback at `/mcp`, answering as a [`Toolbox`] says. It runs
/// on a runtime of its own, so that stopping it cuts every connection at once, unanswered, as a
/// server that dies does.
struct Server {
    addr: SocketAddr,
    scheme: &'static str,
    front: Front,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Server {
    /// A server on a port of its own.
    fn start(toolbox: &Arc<Toolbox>) -> Self {
        Self::at("127.0.0.1:0".parse().expect("an address"), toolbox, None)
    }

    /// A server at `addr`, over TLS when `tls` configures it.
    fn at(
        addr: SocketAddr,
        toolbox: &Arc<Toolbox>,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> Self {
        let front = Front::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the server's runtime");
        let listener = std::net::TcpListener::bind(addr).expect("a loopback port");
        listener.set_nonblocking(true).expect("non-blocking");
        let addr = listener.local_addr().expect("its address");
        let scheme = if tls.is_some() { "https" } else { "http" };
        let (toolbox, state) = (Arc::clone(toolbox), front.clone());
        runtime.spawn(async move {
            // A short `retry:` hint, so a broken stream is resumed -- and given
            // up -- at the client's own pace rather than every three seconds.
            let retry = Some(Duration::from_millis(50));
            let mut sessions = LocalSessionManager::default();
            sessions.session_config.sse_retry = retry;
            let service = StreamableHttpService::new(
                move || Ok(Served(Arc::clone(&toolbox))),
                Arc::new(sessions),
                StreamableHttpServerConfig::default().with_sse_retry(retry),
            );
            let router = axum::Router::new()
                .nest_service("/mcp", service)
                .layer(axum::middleware::from_fn_with_state(state, record));
            let tcp = tokio::net::TcpListener::from_std(listener).expect("a listener");
            match tls {
                None => axum::serve(tcp, router).await,
                Some(config) => {
                    let acceptor = tokio_rustls::TlsAcceptor::from(config);
                    axum::serve(Tls { tcp, acceptor }, router).await
                }
            }
        });
        Self {
            addr,
            scheme,
            front,
            runtime: Some(runtime),
        }
    }

    fn url(&self) -> String {
        format!("{}://127.0.0.1:{}/mcp", self.scheme, self.addr.port())
    }

    /// Every request it took, in order.
    fn seen(&self) -> Vec<Seen> {
        self.front.seen.lock().unwrap().clone()
    }

    /// Answer the next requests with `statuses`, one each, and the server nothing.
    fn answer(&self, statuses: impl IntoIterator<Item = StatusCode>) {
        let canned = statuses.into_iter().map(|status| Canned {
            fits: Fits::Any,
            status,
            said: Said::Nothing,
        });
        self.front.canned.lock().unwrap().extend(canned);
    }

    /// Answer the next request that `fits` with `status` and a body that `said`, and the server
    /// nothing.
    fn answer_to(&self, fits: Fits, status: StatusCode, said: Said) {
        let canned = Canned { fits, status, said };
        self.front.canned.lock().unwrap().push_back(canned);
    }

    /// Cut the connection of each call of `tool`, once the server has it.
    fn cut(&self, tool: &str) {
        self.front.cut.lock().unwrap().push(tool.to_string());
    }

    /// Stop it at once, every connection cut: its address, for a server to start at again.
    async fn stop(mut self) -> SocketAddr {
        let runtime = self.runtime.take().expect("running");
        tokio::task::spawn_blocking(move || runtime.shutdown_timeout(Duration::from_secs(2)))
            .await
            .expect("stopped");
        self.addr
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Record the request, then answer it as the front was told to, or hand it to the server.
async fn record(State(front): State<Front>, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let said: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let rpc = said["method"].as_str().map(str::to_string);
    front.seen.lock().unwrap().push(Seen {
        http: parts.method.clone(),
        rpc: rpc.clone(),
        headers: parts.headers.clone(),
    });
    let canned = {
        let mut canned = front.canned.lock().unwrap();
        let fits = canned.iter().position(|answer| match answer.fits {
            Fits::Any => true,
            Fits::Rpc(method) => rpc.as_deref() == Some(method),
            Fits::Get => parts.method == Method::GET,
        });
        fits.and_then(|at| canned.remove(at))
    };
    if let Some(Canned {
        status,
        said: in_body,
        ..
    }) = canned
    {
        return match in_body {
            Said::Nothing => status.into_response(),
            Said::Error => {
                let error = json!({
                    "jsonrpc": "2.0",
                    "id": said["id"],
                    "error": {"code": -32603, "message": "busy; try again"}
                });
                let json = [(http::header::CONTENT_TYPE, "application/json")];
                (status, json, error.to_string()).into_response()
            }
            Said::Stalled { challenge } => {
                let begun = bytes::Bytes::from_static(b"denied, because");
                let body = futures_util::stream::once(async { Ok::<_, std::io::Error>(begun) })
                    .chain(futures_util::stream::pending());
                let mut response = Response::builder().status(status);
                if challenge {
                    response =
                        response.header(http::header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"");
                }
                response.body(Body::from_stream(body)).expect("a response")
            }
        };
    }
    let request = Request::from_parts(parts, Body::from(bytes));
    let called = said["params"]["name"].as_str().unwrap_or_default();
    let cut = rpc.as_deref() == Some("tools/call")
        && front.cut.lock().unwrap().iter().any(|tool| tool == called);
    if !cut {
        return next.run(request).await;
    }
    // The server has it, and does the work; its answer never leaves.
    tokio::spawn(async move { drop(next.run(request).await) });
    let broken =
        futures_util::stream::once(async { Err::<bytes::Bytes, _>(std::io::Error::other("cut")) });
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(broken))
        .expect("a response")
}

/// A TCP listener whose connections are TLS: one whose handshake fails is let go, and the next
/// taken.
struct Tls {
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for Tls {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else {
                continue;
            };
            if let Ok(tls) = self.acceptor.accept(tcp).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

/// A certificate authority minted for a test.
struct Authority(rcgen::CertifiedIssuer<'static, rcgen::KeyPair>);

impl Authority {
    fn mint(name: &str) -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().expect("a key");
        Self(rcgen::CertifiedIssuer::self_signed(params, key).expect("an authority"))
    }

    fn pem(&self) -> Vec<u8> {
        self.0.pem().into_bytes()
    }

    /// A server's TLS, presenting a certificate for `127.0.0.1` this authority signed.
    fn server(&self) -> Arc<rustls::ServerConfig> {
        let key = rcgen::KeyPair::generate().expect("a key");
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .expect("params")
            .signed_by(&key, &self.0)
            .expect("signed");
        let key = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).expect("a key");
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("a server's TLS");
        Arc::new(config)
    }
}

/// A server listing `echo`, `stall` -- which answers once cancelled -- and `fails`, which answers
/// a result marked as an error.
fn toolbox() -> Arc<Toolbox> {
    let toolbox = Toolbox::new(json!([
        tool("echo", text()),
        tool("stall", text()),
        tool("fails", text())
    ]));
    toolbox.act("stall", Act::Stall);
    toolbox.answer(
        "fails",
        json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
    );
    toolbox
}

/// A builder whose secrets resolve the model's key and `${TOKEN}`, counting each ask of `TOKEN`.
fn builder_counting() -> (SessionBuilder, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let secrets = move |var: &str| match var {
        "TOKEN" => {
            counter.fetch_add(1, Ordering::SeqCst);
            Some(SECRET.to_string())
        }
        _ => (var == VAR).then(|| "sk-test".to_string()),
    };
    (services_tests::builder().secrets(secrets), asked)
}

fn builder() -> SessionBuilder {
    builder_counting().0
}

/// `${TOKEN}`.
fn token() -> EnvValue {
    EnvValue::EnvRef("TOKEN".to_string())
}

/// A session started from `builder`, its calls under `calls` and its starts under `starts`, its
/// events subscribed to and `mcp` defined in its primary.
async fn dialed(
    builder: SessionBuilder,
    calls: ServicePolicy,
    starts: McpPolicy,
) -> (Session, Subscription) {
    let mut builder = builder.service_policy(calls).mcp_policy(starts);
    let events = builder.subscribe();
    let session = start(builder).await;
    output(&session, MCP).await;
    (session, events)
}

/// A URL declaration against the loopback server is ready, lists the server's tools, and a call
/// through the Python surface returns its result; it starts as `placement: url`, with no detail
/// for a loopback host, and describes itself so.
#[tokio::test]
async fn a_url_service_starts_and_serves() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    let builder = builder().mcp("fs", McpServiceSpec::url(server.url()));
    let (session, mut events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    assert!(
        matches!(
            outcome(&session, "fs"),
            McpServiceOutcome::Ready {
                tools: 3,
                attempts: 1,
                ..
            }
        ),
        "{:?}",
        outcome(&session, "fs")
    );
    assert_eq!(
        output(&session, "mcp(fs.echo, text='hi')\nprint(sorted(fs.tools))").await,
        "ok hi\n['echo', 'fails', 'stall']\n"
    );
    assert_eq!(toolbox.calls(), ["echo"]);
    stop(session).await;
    let events = taken(&mut events);
    let started = &of_service(&events, "mcp.service.started", "fs")[0];
    assert_eq!(
        (&started["placement"], &started["detail"]),
        (&json!("url"), &Value::Null)
    );
}

/// The resolved header and both identity headers are on every request -- `initialize`, its
/// notice, `tools/list`, `tools/call`, the standing GET stream and a cancel notice -- and the
/// token's value is in no event, outcome, report, `repr`, `help()` or exception.
#[tokio::test]
async fn headers_reach_every_request_and_show_nowhere() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    let builder = builder()
        .mcp_identity([("X-Fence", "fence-1"), ("X-Owner", "owner-1")])
        .mcp(
            "fs",
            McpServiceSpec::url(server.url()).with_header("X-Token", token()),
        );
    let (session, mut events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    let said = output(
        &session,
        r#"
import contextlib, io
shown = io.StringIO()
with contextlib.redirect_stdout(shown):
    help(fs)
said = [repr(fs), shown.getvalue(), repr(fs.tools)]
for call in (lambda: fs.call('stall', {}, timeout=0.5), lambda: fs.fails(text='x'), lambda: fs.nope()):
    try:
        call()
    except Exception as e:
        said += [repr(e), str(e)]
print(fs.echo(text='hi').text, len(said))
print(' '.join(said))
"#,
    )
    .await;
    assert!(said.starts_with("hi 9\n"), "{said}");
    assert!(!said.contains(SECRET), "{said}");
    until("the cancel", || toolbox.cancels() == ["stall"]).await;
    until("the standing stream", || {
        server.seen().iter().any(|seen| seen.http == Method::GET)
    })
    .await;
    let seen = server.seen();
    for asked in [
        "initialize",
        "notifications/initialized",
        "tools/list",
        "tools/call",
        "notifications/cancelled",
    ] {
        assert!(
            seen.iter().any(|seen| seen.rpc.as_deref() == Some(asked)),
            "no {asked}: {seen:?}"
        );
    }
    for request in &seen {
        assert_eq!(
            (
                request.header("x-token"),
                request.header("x-fence"),
                request.header("x-owner")
            ),
            (Some(SECRET), Some("fence-1"), Some("owner-1")),
            "{request:?}"
        );
    }
    let outcomes = serde_json::to_string(&session.mcp().outcomes()).expect("encodes");
    let report = serde_json::to_string(&stop(session).await).expect("encodes");
    let events: Vec<String> = taken(&mut events)
        .iter()
        .map(|event| serde_json::to_string(&event.payload).expect("encodes"))
        .collect();
    for (what, text) in [
        ("outcomes", outcomes),
        ("report", report),
        ("events", events.concat()),
    ] {
        assert!(!text.contains(SECRET), "the token is in the {what}: {text}");
    }
}

/// A declaration whose headers name an identity header, a URL with a password in it, another
/// scheme, a header HTTP or rmcp sets itself, and `trust = only` with no anchor are each
/// misconfigured before any request, as is every owner's declaration while an identity header is
/// one rmcp sets; none says what its URL or header held.
#[tokio::test]
async fn declarations_are_checked_before_any_request() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    let url = server.url();
    let with_password = format!("http://me:{SECRET}@127.0.0.1:{}/mcp", server.addr.port());
    let builder = builder()
        .mcp_identity([("X-Fence", "fence-1"), ("Mcp-Param-Region", "eu")])
        .mcp(
            "fenced",
            McpServiceSpec::url(&url).with_header("x-fence", EnvValue::Literal("mine".into())),
        )
        .mcp("userinfo", McpServiceSpec::url(with_password))
        .mcp("ftp", McpServiceSpec::url("ftp://127.0.0.1/mcp"))
        .mcp(
            "framed",
            McpServiceSpec::url(&url).with_header("Accept", EnvValue::Literal("*/*".into())),
        )
        .mcp(
            "named",
            McpServiceSpec::url(&url).with_header("mcp-name", EnvValue::Literal("echo".into())),
        )
        .mcp(
            "pinned",
            McpServiceSpec::url(&url).with_trust(McpTrust::Only),
        )
        .mcp("plain", McpServiceSpec::url(&url));
    let (session, _events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    for (name, says) in [
        ("fenced", "identity headers"),
        ("userinfo", "user name or password"),
        ("ftp", "http or https"),
        (
            "framed",
            "Accept is one HTTP or the MCP transport sets itself",
        ),
        (
            "named",
            "mcp-name is one HTTP or the MCP transport sets itself",
        ),
        ("pinned", "names none"),
        (
            "plain",
            "identity header Mcp-Param-Region is one HTTP or the MCP",
        ),
    ] {
        let McpServiceOutcome::Misconfigured(cause) = outcome(&session, name) else {
            panic!("{name}: {:?}", outcome(&session, name));
        };
        assert!(cause.message.contains(says), "{name}: {}", cause.message);
        assert!(!cause.message.contains(SECRET), "{}", cause.message);
        assert_eq!(
            (cause.attempts, cause.placement),
            (0, McpPlacementKind::Url)
        );
    }
    assert!(server.seen().is_empty(), "{:?}", server.seen());
    stop(session).await;
}

/// A configured `url` entry at `url`, carrying `X-Token = "${TOKEN}"`, declared by a repository
/// when `repo`.
fn configured(url: &str, repo: bool) -> ConfiguredMcp {
    let spec =
        McpServerSpec::at_url(url).with_headers(BTreeMap::from([("X-Token".to_string(), token())]));
    let entry = ConfiguredUrl {
        spec,
        trust_anchor: None,
        table: "images.coding.mcp.docs".to_string(),
        declared_in: None,
        unapproved: repo.then(|| NOT_ASKED.to_string()),
    };
    ConfiguredMcp {
        urls: BTreeMap::from([("docs".to_string(), entry)]),
        names: ["docs".to_string()].into(),
        ..ConfiguredMcp::default()
    }
}

/// An approver answering `answer`, which keeps each request it is shown.
fn keeping(
    answer: Approval,
) -> (
    impl Fn(&ApprovalRequest) -> Approval,
    Arc<Mutex<Vec<ApprovalRequest>>>,
) {
    let shown = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&shown);
    let approver = move |request: &ApprovalRequest| {
        kept.lock().unwrap().push(request.clone());
        answer
    };
    (approver, shown)
}

/// A repository's `url` entry starts only once approved. Refused, nothing connects and it is
/// `unapproved`, the session starting without it; approved, it is ready, the request showing its
/// URL and header names and no value, and it gets its header and none of the owner's identity.
/// The same entry in the global config asks nothing.
#[tokio::test]
async fn a_repository_url_entry_is_put_to_the_approver() {
    let approvals = tempfile::tempdir().expect("tempdir");
    let (repo, places) = (
        approvals.path().join("repo"),
        approvals.path().join("places"),
    );
    std::fs::create_dir(&repo).expect("a repository");
    let builder = |configured: ConfiguredMcp| {
        builder()
            .mcp_identity([("X-Fence", "fence-1")])
            .repo_root(&repo)
            .binding_places(&places, Vec::new(), &places.join("approved"))
            .mcp_configured(configured, Default::default())
    };

    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    let (refusing, _) = keeping(Approval::Refuse);
    let (session, mut events) = dialed(
        builder(configured(&server.url(), true)).approver(refusing),
        quick(Duration::from_secs(5)),
        starts(),
    )
    .await;
    let McpServiceOutcome::Unapproved(cause) = outcome(&session, "docs") else {
        panic!("{:?}", outcome(&session, "docs"));
    };
    assert_eq!((cause.service.as_str(), cause.attempts), ("docs", 0));
    assert!(
        cause.message.contains("did not approve"),
        "{}",
        cause.message
    );
    assert_eq!(output(&session, "print('started')").await, "started\n");
    stop(session).await;
    assert!(server.seen().is_empty(), "{:?}", server.seen());
    let failed = &of_service(&taken(&mut events), "mcp.service.failed", "docs")[0];
    assert_eq!(failed["outcome"], "unapproved");

    let (approving, shown) = keeping(Approval::Approve);
    let (session, _events) = dialed(
        builder(configured(&server.url(), true)).approver(approving),
        quick(Duration::from_secs(5)),
        starts(),
    )
    .await;
    assert!(matches!(
        outcome(&session, "docs"),
        McpServiceOutcome::Ready { .. }
    ));
    assert_eq!(output(&session, "mcp(docs.echo, text='x')").await, "ok x\n");
    stop(session).await;
    let shown = shown.lock().unwrap().clone();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(
        shown[0].subject,
        ApprovalSubject::mcp_url(server.url(), ["X-Token".to_string()])
    );
    let declaration = &shown[0].declaration;
    assert!(
        declaration.contains(&server.url()) && declaration.contains("X-Token"),
        "{declaration}"
    );
    assert!(
        !declaration.contains(SECRET) && !declaration.contains("TOKEN}"),
        "{declaration}"
    );
    let seen = server.seen();
    assert!(!seen.is_empty());
    for request in &seen {
        assert_eq!(
            (request.header("x-token"), request.header("x-fence")),
            (Some(SECRET), None),
            "{request:?}"
        );
    }

    // Remembered: asked about once.
    let (again, shown) = keeping(Approval::Refuse);
    let (session, _events) = dialed(
        builder(configured(&server.url(), true)).approver(again),
        quick(Duration::from_secs(5)),
        starts(),
    )
    .await;
    assert!(matches!(
        outcome(&session, "docs"),
        McpServiceOutcome::Ready { .. }
    ));
    assert!(shown.lock().unwrap().is_empty());
    stop(session).await;

    let (global, shown) = keeping(Approval::Refuse);
    let (session, _events) = dialed(
        builder(configured(&server.url(), false)).approver(global),
        quick(Duration::from_secs(5)),
        starts(),
    )
    .await;
    assert!(matches!(
        outcome(&session, "docs"),
        McpServiceOutcome::Ready { .. }
    ));
    assert!(shown.lock().unwrap().is_empty());
    stop(session).await;
}

/// Against a server whose certificate a minted authority signed: with no anchor the start is
/// misconfigured after one attempt, its cause the certificate; with the authority as the anchor --
/// as PEM or as a file -- it is ready. Trusting only an anchor, the authority that signed the
/// server's certificate is accepted and a second authority refused.
#[tokio::test]
async fn a_trust_anchor_decides_what_is_trusted() {
    let (first, second) = (Authority::mint("first"), Authority::mint("second"));
    let toolbox = toolbox();
    let server = Server::at(
        "127.0.0.1:0".parse().unwrap(),
        &toolbox,
        Some(first.server()),
    );
    let anchors = tempfile::tempdir().expect("tempdir");
    let path = anchors.path().join("first.pem");
    std::fs::write(&path, first.pem()).expect("written");
    let url = server.url();
    let anchored = |anchor: TrustAnchor| McpServiceSpec::url(&url).with_trust_anchor(anchor);
    let builder = builder()
        .mcp("bare", McpServiceSpec::url(&url))
        .mcp("pem", anchored(TrustAnchor::Pem(first.pem())))
        .mcp("file", anchored(TrustAnchor::Path(path)))
        .mcp(
            "only",
            anchored(TrustAnchor::Pem(first.pem())).with_trust(McpTrust::Only),
        )
        .mcp(
            "other",
            anchored(TrustAnchor::Pem(second.pem())).with_trust(McpTrust::Only),
        );
    let (session, _events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    for name in ["pem", "file", "only"] {
        assert!(
            matches!(outcome(&session, name), McpServiceOutcome::Ready { .. }),
            "{name}: {:?}",
            outcome(&session, name)
        );
    }
    for name in ["bare", "other"] {
        let McpServiceOutcome::Misconfigured(cause) = outcome(&session, name) else {
            panic!("{name}: {:?}", outcome(&session, name));
        };
        assert_eq!(cause.attempts, 1, "{name}");
        assert_eq!(cause.kind, Some(McpFailureKind::Transport), "{name}");
        assert!(
            cause.message.contains("certificate"),
            "{name}: {}",
            cause.message
        );
    }
    stop(session).await;
}

/// A 401 at `initialize` is refused after one attempt, and so is one at `tools/list`, and so are
/// a challenged 401 and an unchallenged 403 whose bodies never end; a 503 and then an answer is
/// ready after two, with one retry, whether or not the 503's body is a JSON-RPC error; a 404 is
/// misconfigured; a port nothing listens on is tried until the budget, then unavailable.
#[tokio::test]
async fn http_failures_are_classified() {
    let [
        refusing,
        busy,
        missing,
        unlisting,
        throttled,
        challenged,
        stalled,
    ] = std::array::from_fn(|_| Server::start(&toolbox()));
    refusing.answer([StatusCode::UNAUTHORIZED]);
    busy.answer([StatusCode::SERVICE_UNAVAILABLE]);
    missing.answer([StatusCode::NOT_FOUND]);
    let initialize = Fits::Rpc("initialize");
    unlisting.answer_to(
        Fits::Rpc("tools/list"),
        StatusCode::UNAUTHORIZED,
        Said::Nothing,
    );
    throttled.answer_to(initialize, StatusCode::SERVICE_UNAVAILABLE, Said::Error);
    let challenge = Said::Stalled { challenge: true };
    challenged.answer_to(initialize, StatusCode::UNAUTHORIZED, challenge);
    let unchallenged = Said::Stalled { challenge: false };
    stalled.answer_to(initialize, StatusCode::FORBIDDEN, unchallenged);
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.local_addr().expect("its address")
    };
    let builder = builder()
        .mcp("refusing", McpServiceSpec::url(refusing.url()))
        .mcp("busy", McpServiceSpec::url(busy.url()))
        .mcp("missing", McpServiceSpec::url(missing.url()))
        .mcp("unlisting", McpServiceSpec::url(unlisting.url()))
        .mcp("throttled", McpServiceSpec::url(throttled.url()))
        .mcp("challenged", McpServiceSpec::url(challenged.url()))
        .mcp("stalled", McpServiceSpec::url(stalled.url()))
        .mcp(
            "closed",
            McpServiceSpec::url(format!("http://{closed}/mcp")),
        );
    let budget = policy(Duration::from_secs(1), Duration::from_millis(1500));
    let (session, mut events) = dialed(builder, quick(Duration::from_secs(5)), budget).await;
    let refusals = [
        ("refusing", "HTTP 401"),
        ("unlisting", "tools/list"),
        ("unlisting", "HTTP 401"),
        ("challenged", "authorization required: Bearer"),
        ("stalled", "HTTP 403"),
    ];
    for (name, says) in refusals {
        let McpServiceOutcome::Refused(cause) = outcome(&session, name) else {
            panic!("{name}: {:?}", outcome(&session, name));
        };
        assert_eq!(
            (cause.attempts, cause.kind),
            (1, Some(McpFailureKind::Protocol)),
            "{name}"
        );
        assert!(cause.message.contains(says), "{name}: {}", cause.message);
    }
    for name in ["busy", "throttled"] {
        assert!(
            matches!(
                outcome(&session, name),
                McpServiceOutcome::Ready { attempts: 2, .. }
            ),
            "{name}: {:?}",
            outcome(&session, name)
        );
    }
    let McpServiceOutcome::Misconfigured(cause) = outcome(&session, "missing") else {
        panic!("{:?}", outcome(&session, "missing"));
    };
    assert_eq!(cause.attempts, 1);
    let McpServiceOutcome::Unavailable(cause) = outcome(&session, "closed") else {
        panic!("{:?}", outcome(&session, "closed"));
    };
    assert!(cause.attempts >= 2, "{cause:?}");
    assert_eq!(cause.kind, Some(McpFailureKind::Transport));
    assert!(cause.message.contains("connect"), "{}", cause.message);
    stop(session).await;
    let events = taken(&mut events);
    for name in ["busy", "throttled"] {
        assert_eq!(
            of_service(&events, "mcp.service.retry", name).len(),
            1,
            "{name}"
        );
    }
    for name in ["refusing", "unlisting", "challenged", "stalled"] {
        assert!(
            of_service(&events, "mcp.service.retry", name).is_empty(),
            "{name}"
        );
    }
}

/// The server dies under a call and starts again at the same port: the call ends may have
/// dispatched, and is never sent again; the next call connects a new HTTP session -- a new
/// session id -- relists and is dispatched once, its header's value the one resolved at start.
#[tokio::test]
async fn a_reconnect_is_a_new_http_session() {
    let first = toolbox();
    let server = Server::start(&first);
    let (builder, asked) = builder_counting();
    let builder = builder.mcp(
        "fs",
        McpServiceSpec::url(server.url()).with_header("X-Token", token()),
    );
    let (session, mut events) = dialed(builder, quick(Duration::from_secs(30)), starts()).await;
    let at_start = asked.load(Ordering::SeqCst);
    let mut stalled = session
        .interpreter()
        .submit("mcp(fs.call, 'stall', {})")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the call", || first.calls() == ["stall"]).await;
    let before: Vec<String> = server
        .seen()
        .iter()
        .filter_map(|seen| seen.header("mcp-session-id").map(str::to_string))
        .collect();
    let addr = server.stop().await;
    match within(stalled.outcome()).await {
        crate::python::host::Outcome::Ok(report) => assert_eq!(
            report.output,
            "MayHaveDispatched unknown may_have_dispatched transport None\n"
        ),
        other => panic!("{other:?}"),
    }
    let second = toolbox();
    let server = Server::at(addr, &second, None);
    assert_eq!(
        output(&session, "mcp(fs.echo, text='back')").await,
        "ok back\n"
    );
    assert_eq!(first.calls(), ["stall"]);
    assert_eq!(second.calls(), ["echo"]);
    let after = server.seen();
    assert_eq!(after[0].rpc.as_deref(), Some("initialize"));
    let renewed = after
        .iter()
        .find_map(|seen| seen.header("mcp-session-id"))
        .expect("a session");
    assert!(
        !before.iter().any(|id| id == renewed),
        "{before:?} {renewed}"
    );
    assert!(
        after
            .iter()
            .all(|seen| seen.header("x-token") == Some(SECRET))
    );
    assert_eq!(asked.load(Ordering::SeqCst), at_start, "not resolved again");
    let uncertain = session.mcp().uncertain();
    assert_eq!(uncertain.len(), 1, "{uncertain:?}");
    assert_eq!(uncertain[0].tool, "stall");
    stop(session).await;
    let events = taken(&mut events);
    assert_eq!(of_kind(&events, "mcp.service.reconnected").len(), 1);
    let echoes = of_kind(&events, "service.call.dispatched")
        .into_iter()
        .filter(|dispatched| dispatched["method"] == "echo")
        .count();
    assert_eq!(echoes, 1);
}

/// The server goes away between calls, for longer than its standing stream is resumed: the
/// service is disconnected with no call failing, and once the server is back the next call
/// connects a new session, whose server's listing changes are heard.
#[tokio::test]
async fn a_standing_stream_given_up_is_a_disconnect() {
    let first = toolbox();
    let server = Server::start(&first);
    let builder = builder().mcp("fs", McpServiceSpec::url(server.url()));
    let (session, events) = dialed(builder, quick(Duration::from_secs(30)), starts()).await;
    let standing = |server: &Server| server.seen().iter().any(|seen| seen.http == Method::GET);
    until("the standing stream", || standing(&server)).await;
    let addr = server.stop().await;
    let (events, heard) = (RefCell::new(events), RefCell::new(Vec::new()));
    let disconnects = || {
        heard.borrow_mut().extend(taken(&mut events.borrow_mut()));
        of_service(&heard.borrow(), "mcp.service.disconnected", "fs")
    };
    until("the disconnect", || !disconnects().is_empty()).await;
    let disconnected = disconnects();
    assert_eq!(disconnected.len(), 1, "{disconnected:?}");
    assert_eq!(disconnected[0]["cause"]["kind"], "transport");
    assert!(
        disconnected[0]["cause"]["message"]
            .as_str()
            .is_some_and(|said| said.contains("would not be heard")),
        "{disconnected:?}"
    );
    let second = toolbox();
    let server = Server::at(addr, &second, None);
    assert_eq!(
        output(&session, "mcp(fs.echo, text='back')").await,
        "ok back\n"
    );
    assert!(session.mcp().uncertain().is_empty());
    until("the new standing stream", || standing(&server)).await;
    second.list_changed().await;
    assert_eq!(
        printed(&session, "print(fs.stale)", "True\n").await,
        "True\n"
    );
    stop(session).await;
    heard.borrow_mut().extend(taken(&mut events.borrow_mut()));
    assert_eq!(of_kind(&heard.borrow(), "mcp.service.reconnected").len(), 1);
}

/// The server answers the first standing stream with a 503, and nothing else: the stream is
/// opened again as a broken one is resumed, calls go on as before, no disconnect is made, and a
/// change to the server's listing is heard.
#[tokio::test]
async fn a_standing_stream_that_did_not_open_is_opened_again() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    server.answer_to(Fits::Get, StatusCode::SERVICE_UNAVAILABLE, Said::Nothing);
    let builder = builder().mcp("fs", McpServiceSpec::url(server.url()));
    let (session, mut events) = dialed(builder, quick(Duration::from_secs(30)), starts()).await;
    let streams = || {
        let seen = server.seen();
        seen.iter().filter(|seen| seen.http == Method::GET).count()
    };
    until("the standing stream again", || streams() == 2).await;
    // A call, so that the stream the server was just asked for is attached before the change
    // is announced on it: one announced before is not sent.
    assert_eq!(output(&session, "mcp(fs.echo, text='on')").await, "ok on\n");
    toolbox.list_changed().await;
    assert_eq!(
        printed(&session, "print(fs.stale)", "True\n").await,
        "True\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    assert!(of_kind(&events, "mcp.service.disconnected").is_empty());
    assert!(of_kind(&events, "mcp.service.reconnected").is_empty());
}

/// A connection cut once the server has the call is may have dispatched; a call past its
/// deadline is a timeout, and the server is sent the cancel notice; a result marked an error is a
/// `ToolError`.
#[tokio::test]
async fn the_dispatch_states_over_http() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    server.cut("echo");
    let builder = builder().mcp("fs", McpServiceSpec::url(server.url()));
    let (session, _events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    assert_eq!(
        output(
            &session,
            r#"
mcp(fs.echo, text='x')
mcp(fs.call, 'stall', {}, timeout=0.5)
try:
    fs.fails(text='x')
except outrig.ToolError as e:
    print(type(e).__name__, e.outcome, e.state)
"#
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n\
         MayHaveDispatched unknown may_have_dispatched timeout deadline\n\
         ToolError returned response_received\n"
    );
    until("the cancel", || toolbox.cancels() == ["stall"]).await;
    assert_eq!(toolbox.calls(), ["echo", "stall", "fails"]);
    stop(session).await;
}

/// A URL service is no writer, and a pause of it holds its calls with nothing frozen; a call
/// after its resume is answered, and the close ends its HTTP session, disconnected and proven.
#[tokio::test]
async fn a_url_service_is_no_writer() {
    let toolbox = toolbox();
    let server = Server::start(&toolbox);
    let builder = builder().mcp("fs", McpServiceSpec::url(server.url()));
    let (session, _events) = dialed(builder, quick(Duration::from_secs(5)), starts()).await;
    let writers = session.mcp().writers();
    assert_eq!(writers.len(), 1);
    assert_eq!(
        (&writers[0].writes, writers[0].is_writer()),
        (&McpWrites::Nothing, false)
    );
    let paused = session
        .mcp()
        .quiesce("fs", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert!(
        matches!(&paused, QuiesceOutcome::Quiesced { services, .. } if services == &["fs"]),
        "{paused:?}"
    );
    assert_eq!(session.mcp().resume("fs").await, Ok(vec!["fs".to_string()]));
    assert_eq!(output(&session, "mcp(fs.echo, text='x')").await, "ok x\n");
    let report = stop(session).await;
    let row = &report.mcp_services[0];
    assert_eq!((row.how, row.proven), (McpStopHow::Disconnected, true));
    until("the session's end", || {
        server.seen().iter().any(|seen| seen.http == Method::DELETE)
    })
    .await;
}

/// Plain `http` to a host that is not loopback is said in the started event; to loopback, or
/// over `https`, it is not.
#[test]
fn plain_http_off_loopback_is_said() {
    for (url, said) in [
        ("http://10.0.0.7:8080/mcp", true),
        ("http://db.internal/mcp", true),
        ("https://db.internal/mcp", false),
        ("http://127.0.0.1:9/mcp", false),
        ("http://localhost/mcp", false),
        ("http://[::1]:9/mcp", false),
    ] {
        assert_eq!(super::mcp::in_the_clear(url).is_some(), said, "{url}");
    }
}

/// A configuration's `url` entry lowers to what the table plans from: its URL, headers as
/// declared, trust anchor resolved against the declaring file, trust and stateless choice and
/// deadline; its name taken among the configuration's, and no container planned for it.
#[tokio::test]
async fn a_configured_url_entry_is_lowered() {
    let cfg = crate::config::Config::load_from_str(
        r#"
[images.dev]
image-name = "docker.io/library/debian"

[images.dev.mcp]
docs = { url = "https://d.test/mcp", headers = { X-Token = "${TOKEN}" }, trust-anchor = "ca.pem", trust = "only", allow-stateless = false, call-timeout-secs = 30 }
"#,
    )
    .expect("parses");
    cfg.validate(None).expect("validates");
    let root = std::path::Path::new("/home/a/project");
    let configured = ConfiguredMcp::from_config(&cfg, "dev", root).await;
    let docs = &configured.urls["docs"];
    assert_eq!(docs.spec, cfg.images["dev"].mcp()["docs"]);
    assert_eq!(
        docs.trust_anchor.as_deref(),
        Some(root.join("ca.pem").as_path())
    );
    assert_eq!(docs.table, "images.dev.mcp.docs");
    assert!(
        docs.unapproved.is_none(),
        "a global entry needs no approval"
    );
    assert!(configured.names.contains("docs"));
    assert!(configured.primary.is_empty() && configured.sidecars.is_empty());
}
