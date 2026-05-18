//! HTTPS MITM proxy: terminate TLS in the interceptor, apply URL-aware
//! policy, re-encrypt upstream, and emit one audit record per HTTP request.
//!
//! Wire shape (per TCP connection):
//!
//! ```text
//!  client TCP --[client TLS, ALPN h2 or http/1.1]--> outrig
//!     outrig opens TLS to dst (real SNI, native roots) for upstream
//!     for each request on the client conn:
//!         parse method + path + Host
//!         decide_post_mitm(...) -> allow / deny
//!         deny -> synthesize 403, audit deny, continue
//!         allow -> forward; tee bodies if capture is on; audit on completion
//! ```
//!
//! Both server and client speak HTTP/1.1 and HTTP/2; the negotiated ALPN
//! decides which hyper builder is used.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HOST, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::warn;

use crate::config::{MitmConfig, NetworkAction};
use crate::error::{OutrigError, Result};
use crate::network::tls::{self, SessionCa};

/// ALPN protocols advertised by both the server-side (toward the
/// container) and client-side (toward the real upstream) of the MITM
/// proxy. Listed in preference order, h2 first.
const ALPN_PROTOCOLS: &[&[u8]] = &[b"h2", b"http/1.1"];

fn alpn_protocols() -> Vec<Vec<u8>> {
    ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect()
}

/// Lifetime-extended view of the MITM configuration plus its derived state
/// (CA, native upstream root store, compiled HTTPS port set). Lives inside
/// the interceptor task and is cloned (cheaply, Arc internally) into each
/// per-connection handler.
#[derive(Clone)]
pub struct MitmContext {
    inner: Arc<MitmContextInner>,
}

impl std::fmt::Debug for MitmContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MitmContext")
            .field("enabled", &self.inner.enabled)
            .field("ports", &self.inner.ports)
            .field("capture_bodies", &self.inner.capture_bodies)
            .field("max_body_bytes", &self.inner.max_body_bytes)
            .finish()
    }
}

struct MitmContextInner {
    enabled: bool,
    ports: Vec<u16>,
    capture_bodies: bool,
    max_body_bytes: usize,
    ca: Option<SessionCa>,
    /// Server-side TLS acceptor wired to the session CA. Built once at
    /// startup so per-connection cost is one `Arc::clone`, not a fresh
    /// `rustls::ServerConfig` build.
    acceptor: Option<TlsAcceptor>,
    /// Client-side TLS connector wired to the host's native trust anchors.
    /// Built once at startup for the same reason as `acceptor`.
    connector: Option<TlsConnector>,
}

impl MitmContext {
    /// Build a disabled context. Calls to `is_enabled` return false and no
    /// CA is generated. Used when MITM is off so callers can pass the same
    /// type through unconditionally.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(MitmContextInner {
                enabled: false,
                ports: Vec::new(),
                capture_bodies: false,
                max_body_bytes: 0,
                ca: None,
                acceptor: None,
                connector: None,
            }),
        }
    }

    /// Materialize an enabled MITM context. Generates the per-session CA
    /// under `session_dir/tls/`, builds the upstream `RootCertStore` from
    /// the host's native trust anchors, and locks in the configured ports
    /// and body cap.
    pub fn enabled(
        config: &MitmConfig,
        session_dir: &std::path::Path,
        session_id: &str,
    ) -> Result<Self> {
        tls::ensure_rustls_provider_installed();
        let ca = SessionCa::generate(session_dir, session_id)?;

        let mut server_cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(ca.resolver());
        server_cfg.alpn_protocols = alpn_protocols();
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));

        let client_cfg = build_upstream_client_config()?;
        let connector = TlsConnector::from(Arc::new(client_cfg));

        Ok(Self {
            inner: Arc::new(MitmContextInner {
                enabled: true,
                ports: config.effective_ports(),
                capture_bodies: config.capture_bodies,
                max_body_bytes: config.effective_max_body_bytes(),
                ca: Some(ca),
                acceptor: Some(acceptor),
                connector: Some(connector),
            }),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    pub fn ca_pem(&self) -> Option<String> {
        self.inner.ca.as_ref().map(|ca| ca.ca_pem())
    }

    pub fn cleanup(&self) {
        if let Some(ca) = &self.inner.ca {
            ca.cleanup();
        }
    }

    pub fn is_https_port(&self, port: u16) -> bool {
        self.inner.ports.contains(&port)
    }

    /// Effective per-body capture cap. `0` means capture is off; callers
    /// pass this directly to `collect_capped` and `encode_capture`.
    fn capture_cap(&self) -> usize {
        if self.inner.capture_bodies {
            self.inner.max_body_bytes
        } else {
            0
        }
    }
}

/// Audit hook the per-request loop calls to record one HTTP request. The
/// callback owns serialization details; this module stays free of the
/// `AuditRecord` shape.
pub type MitmAuditHook = Arc<
    dyn Fn(MitmAuditEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Policy hook the per-request loop calls. Returns the action + a rule
/// label for audit. Letting the caller own decode lets us reuse the
/// existing `CompiledNetworkPolicy::decide_post_mitm` without making it
/// public.
pub type MitmPolicyHook = Arc<dyn Fn(&str, &str, &str) -> MitmPolicyDecision + Send + Sync>;

/// What `MitmPolicyHook` returns. Mirrors `PolicyDecision` in `network.rs`
/// but without re-exporting that internal type.
#[derive(Debug, Clone)]
pub struct MitmPolicyDecision {
    pub action: NetworkAction,
    pub rule: String,
}

/// Per-request data handed to the audit hook. Includes both connection-
/// level identifiers (so records group cleanly with the TCP-level audit)
/// and per-request HTTP metadata.
#[derive(Debug, Clone)]
pub struct MitmAuditEvent {
    pub opened: SystemTime,
    pub duration: Duration,
    pub orig: std::net::SocketAddr,
    pub dst: std::net::SocketAddr,
    pub server_name: Option<String>,
    pub host: String,
    pub action: NetworkAction,
    pub rule: String,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub request_body_b64: Option<String>,
    pub response_body_b64: Option<String>,
    pub body_truncated: Option<&'static str>,
    /// Same Zeek `uid` as the TCP-level audit record for this connection,
    /// so consumers can group request records back to their underlying
    /// TCP flow.
    pub uid: String,
}

/// Per-connection inputs handed in by `network::handle_tcp`. Everything the
/// proxy needs to run, without depending on internal types from
/// `network.rs`.
pub struct MitmConnection {
    pub mitm: MitmContext,
    pub orig: std::net::SocketAddr,
    pub dst: std::net::SocketAddr,
    pub sni: String,
    pub uid: String,
    pub policy: MitmPolicyHook,
    pub audit: MitmAuditHook,
}

/// Run the MITM proxy for one accepted TCP connection.
///
/// `client_io` carries the original sniffed bytes prepended ahead of the
/// live TCP stream (because we already peeked the ClientHello to identify
/// it as TLS). The handshake completes against those bytes seamlessly.
pub async fn handle_tls_mitm<S>(client_io: S, conn: MitmConnection) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let acceptor = conn
        .mitm
        .inner
        .acceptor
        .clone()
        .expect("handle_tls_mitm called with disabled MitmContext");
    let connector = conn
        .mitm
        .inner
        .connector
        .clone()
        .expect("handle_tls_mitm called with disabled MitmContext");

    let tls_stream = match acceptor.accept(client_io).await {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "outrig::mitm", "client tls handshake failed: {e}");
            return Ok(());
        }
    };
    let is_h2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");

    let upstream_tcp = match TcpStream::connect(conn.dst).await {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "outrig::mitm", "upstream connect {}: {e}", conn.dst);
            return Ok(());
        }
    };
    let sni_owned: ServerName<'_> = match ServerName::try_from(conn.sni.clone()) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "outrig::mitm", "invalid SNI {:?}: {e}", conn.sni);
            return Ok(());
        }
    };
    let upstream_tls = match connector.connect(sni_owned, upstream_tcp).await {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "outrig::mitm", "upstream tls handshake {}: {e}", conn.dst);
            return Ok(());
        }
    };
    let upstream_h2 = upstream_tls.get_ref().1.alpn_protocol() == Some(b"h2");

    let svc_state = Arc::new(ServiceState {
        mitm: conn.mitm.clone(),
        orig: conn.orig,
        dst: conn.dst,
        sni: conn.sni.clone(),
        uid: conn.uid.clone(),
        policy: conn.policy.clone(),
        audit: conn.audit.clone(),
        upstream: Arc::new(UpstreamSender::new(upstream_tls, upstream_h2).await?),
    });

    let svc = service_fn(move |req| {
        let state = svc_state.clone();
        async move { handle_request(state, req).await }
    });

    if is_h2 {
        let exec = TokioExecutor::new();
        if let Err(e) = hyper::server::conn::http2::Builder::new(exec)
            .serve_connection(TokioIo::new(tls_stream), svc)
            .await
        {
            warn!(target: "outrig::mitm", "h2 server conn: {e}");
        }
    } else if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tls_stream), svc)
        .with_upgrades()
        .await
    {
        warn!(target: "outrig::mitm", "http1 server conn: {e}");
    }
    Ok(())
}

struct ServiceState {
    mitm: MitmContext,
    orig: std::net::SocketAddr,
    dst: std::net::SocketAddr,
    sni: String,
    uid: String,
    policy: MitmPolicyHook,
    audit: MitmAuditHook,
    upstream: Arc<UpstreamSender>,
}

/// Upstream-protocol-aware request sender. HTTP/1.1 connections handle one
/// request at a time, so the sender lives behind a `Mutex` and requests
/// serialize naturally. HTTP/2 multiplexes many requests on one connection;
/// its `SendRequest` is `Clone`, so we hand each request a fresh clone and
/// avoid the per-request lock.
enum UpstreamSender {
    H1(tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<Full<Bytes>>>),
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
}

impl UpstreamSender {
    async fn new<IO>(io: IO, is_h2: bool) -> Result<Self>
    where
        IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        if is_h2 {
            let (sender, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(io))
                    .await
                    .map_err(|e| {
                        OutrigError::Configuration(format!("upstream h2 handshake: {e}"))
                    })?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    warn!(target: "outrig::mitm", "upstream h2 conn closed: {e}");
                }
            });
            Ok(Self::H2(sender))
        } else {
            let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .map_err(|e| {
                    OutrigError::Configuration(format!("upstream http1 handshake: {e}"))
                })?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    warn!(target: "outrig::mitm", "upstream http1 conn closed: {e}");
                }
            });
            Ok(Self::H1(tokio::sync::Mutex::new(sender)))
        }
    }

    async fn send(
        &self,
        req: Request<Full<Bytes>>,
    ) -> std::result::Result<Response<Incoming>, hyper::Error> {
        match self {
            Self::H1(mu) => mu.lock().await.send_request(req).await,
            Self::H2(sender) => sender.clone().send_request(req).await,
        }
    }
}

async fn handle_request(
    state: Arc<ServiceState>,
    req: Request<Incoming>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let opened = SystemTime::now();
    let started = Instant::now();
    let method = req.method().clone();
    let req_uri = req.uri().clone();

    // Reconstruct an absolute URL: https://<authority>/<path?query>. The
    // authority comes from the request URI when present (HTTP/2 always),
    // else from the Host header (HTTP/1.1).
    let authority = req_uri
        .authority()
        .map(|a| a.to_string())
        .or_else(|| {
            req.headers()
                .get(HOST)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| state.sni.clone());
    let path_and_query = req_uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req_uri.path().to_string());
    let url = format!("https://{authority}{path_and_query}");

    let url_path = req_uri.path().to_string();
    let decision = (state.policy)(&method.as_str().to_lowercase(), &url, &url_path);

    if matches!(decision.action, NetworkAction::Deny) {
        let resp = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                b"outrig: blocked by network policy\n",
            )))
            .expect("static deny response builds");
        (state.audit)(MitmAuditEvent {
            opened,
            duration: started.elapsed(),
            orig: state.orig,
            dst: state.dst,
            server_name: Some(state.sni.clone()),
            host: authority,
            action: NetworkAction::Deny,
            rule: decision.rule,
            method: method.to_string(),
            url,
            status: Some(StatusCode::FORBIDDEN.as_u16()),
            request_body_b64: None,
            response_body_b64: None,
            body_truncated: None,
            uid: state.uid.clone(),
        })
        .await;
        return Ok(resp);
    }

    let cap = state.mitm.capture_cap();

    let (parts, body) = req.into_parts();
    let (req_bytes, req_truncated) = match collect_capped(body, cap).await {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "outrig::mitm", "reading request body: {e}");
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    let mut upstream_req_builder = Request::builder()
        .method(parts.method.clone())
        .uri(build_upstream_uri(&parts.uri));
    copy_proxy_headers(
        &parts.headers,
        upstream_req_builder.headers_mut().expect("fresh"),
    );
    let upstream_req = match upstream_req_builder.body(Full::new(req_bytes.clone())) {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "outrig::mitm", "building upstream request: {e}");
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    let resp = match state.upstream.send(upstream_req).await {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "outrig::mitm", "upstream send: {e}");
            (state.audit)(MitmAuditEvent {
                opened,
                duration: started.elapsed(),
                orig: state.orig,
                dst: state.dst,
                server_name: Some(state.sni.clone()),
                host: authority,
                action: NetworkAction::Allow,
                rule: decision.rule,
                method: method.to_string(),
                url,
                status: None,
                request_body_b64: encode_capture(&req_bytes, cap),
                response_body_b64: None,
                body_truncated: truncated_label(req_truncated, false),
                uid: state.uid.clone(),
            })
            .await;
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    let status = resp.status();
    let (resp_parts, resp_body) = resp.into_parts();
    let (resp_bytes, resp_truncated) = match collect_capped(resp_body, cap).await {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "outrig::mitm", "reading response body: {e}");
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    let mut out_builder = Response::builder().status(status).version(Version::HTTP_11);
    copy_proxy_headers(
        &resp_parts.headers,
        out_builder.headers_mut().expect("fresh"),
    );
    let out_resp = match out_builder.body(Full::new(resp_bytes.clone())) {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "outrig::mitm", "building downstream response: {e}");
            return Ok(error_response(StatusCode::BAD_GATEWAY));
        }
    };

    (state.audit)(MitmAuditEvent {
        opened,
        duration: started.elapsed(),
        orig: state.orig,
        dst: state.dst,
        server_name: Some(state.sni.clone()),
        host: authority,
        action: NetworkAction::Allow,
        rule: decision.rule,
        method: method.to_string(),
        url,
        status: Some(status.as_u16()),
        request_body_b64: encode_capture(&req_bytes, cap),
        response_body_b64: encode_capture(&resp_bytes, cap),
        body_truncated: truncated_label(req_truncated, resp_truncated),
        uid: state.uid.clone(),
    })
    .await;

    Ok(out_resp)
}

fn copy_proxy_headers(src: &hyper::HeaderMap, dst: &mut hyper::HeaderMap) {
    for (k, v) in src.iter() {
        if is_hop_by_hop(k) {
            continue;
        }
        dst.insert(k.clone(), v.clone());
    }
}

fn error_response(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .expect("status-only response builds")
}

fn truncated_label(req_truncated: bool, resp_truncated: bool) -> Option<&'static str> {
    match (req_truncated, resp_truncated) {
        (false, false) => None,
        (true, false) => Some("request"),
        (false, true) => Some("response"),
        (true, true) => Some("both"),
    }
}

fn encode_capture(bytes: &Bytes, cap: usize) -> Option<String> {
    if cap == 0 || bytes.is_empty() {
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn build_upstream_uri(orig_uri: &Uri) -> Uri {
    // Hyper accepts origin-form path-and-query on `send_request` regardless of
    // the negotiated protocol; it attaches the right pseudo-headers for h2.
    orig_uri
        .path_and_query()
        .and_then(|pq| Uri::try_from(pq.as_str()).ok())
        .unwrap_or_else(|| Uri::try_from("/").expect("static / is valid"))
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

/// Read an `Incoming` body to completion, capturing at most `cap` bytes
/// of payload. Frames past the cap are still drained (we are a transparent
/// proxy) but their data is discarded from the captured buffer.
///
/// Returns the captured slice as `Bytes` so downstream forwarding and
/// audit encoding both share the same Arc-backed buffer without a copy.
async fn collect_capped(
    body: Incoming,
    cap: usize,
) -> std::result::Result<(Bytes, bool), hyper::Error> {
    let mut buf = Vec::new();
    let mut truncated = false;
    let mut body = std::pin::Pin::new(Box::new(body));
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Ok(data) = frame.into_data() {
            if cap == 0 {
                continue;
            }
            if buf.len() >= cap {
                truncated = true;
                continue;
            }
            let remaining = cap - buf.len();
            if data.len() <= remaining {
                buf.extend_from_slice(&data);
            } else {
                buf.extend_from_slice(&data[..remaining]);
                truncated = true;
            }
        }
    }
    Ok((Bytes::from(buf), truncated))
}

fn build_upstream_client_config() -> Result<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for cert in certs.certs {
        let _ = roots.add(cert);
    }
    for err in certs.errors {
        warn!(target: "outrig::mitm", "loading native cert: {err}");
    }
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn_protocols();
    Ok(cfg)
}
