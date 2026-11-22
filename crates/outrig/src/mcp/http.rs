//! The streamable-HTTP transport of an MCP service a session dials: the HTTP
//! client a declaration's trust anchor builds, rmcp's transport over it with
//! the declaration's resolved headers, and what a failed connect means for
//! trying again.
//!
//! The client is rmcp 3.4.1's, which sends a plain `initialize` -- offering
//! [`ProtocolVersion::LATEST`](rmcp::model::ProtocolVersion::LATEST) -- and
//! takes the revision the server answers, as the stdio client does. Its own
//! session recovery is off: a session the server no longer has is a transport
//! failure, and the session's reconnect -- relisted, evented, checked against
//! the new listing -- is what makes a new one.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderName, HeaderValue, StatusCode};
use rmcp::model::{ClientJsonRpcMessage, ClientRequest};
use rmcp::service::{ClientInitializeError, ServiceError, serve_client};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::{ExponentialBackoff, SseRetryPolicy};
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_SESSION_ID, JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient,
    StreamableHttpClientTransportConfig, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Handshake, McpClient, Notices, StartupClass, init_error_class};
use crate::config::effective_mcp_call_timeout;
use crate::error::{McpFailureKind, McpSessionError};

/// How a response stream, or the standing stream, that broke is resumed:
/// three times -- at once, then after half a second, then after a second, or
/// after the server's own `retry:` hint where that is longer -- and then given
/// up, the call it carried ending as the transport failure it is. rmcp's
/// default resumes forever, which would hold a call whose server is gone until
/// its deadline. rmcp waits `RESUME_BASE` times 2 to the number of resumes
/// that failed.
const RESUMES: usize = 3;
const RESUME_BASE: Duration = Duration::from_millis(250);

/// How long one connect, its TLS handshake included, may take: a request's
/// own bound -- an attempt's, a call's deadline -- is usually shorter, and
/// this one bounds the resumes nothing else waits on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest server-sent event the client reads, in bytes: the ceiling of
/// `service-result-max-bytes`. rmcp's 16 MiB would end a call whose result the
/// session's own bound takes as a transport failure -- may have dispatched,
/// the service disconnected -- so the bound that applies is the session's.
const MAX_EVENT_BYTES: u64 = crate::config::SERVICE_RESULT_MAX_CEILING_BYTES;

/// How long the body of an answer to `initialize` that is not 2xx is waited
/// for, at most half what is left of the attempt: what the answer means is its
/// status, and its body only says more, so a body that never ends is not let
/// turn a refusal into a timeout.
const BODY_WAIT: Duration = Duration::from_secs(1);

/// How long a cancel notice's POST may take once it starts: rmcp's own five
/// seconds. The call it cancels waits for it no longer than its grace.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// The headers no request's own may be, by lower-case name: what rmcp sets
/// itself -- refusing some, overwriting others, among them each request's
/// `Mcp-Method` and `Mcp-Name` once a server agrees protocol 2026-07-28 --
/// and what HTTP frames a request with.
const RESERVED_HEADERS: [&str; 11] = [
    "accept",
    "connection",
    "content-length",
    "content-type",
    "host",
    "last-event-id",
    "mcp-method",
    "mcp-name",
    "mcp-protocol-version",
    "mcp-session-id",
    "transfer-encoding",
];

/// The lower-case prefix of the headers rmcp makes of a tool call's annotated
/// arguments under protocol 2026-07-28, in place of any header so named.
const RESERVED_PREFIX: &str = "mcp-param-";

/// Whether the header `name` is one HTTP or the MCP transport sets itself,
/// which neither a declaration nor the owner's identity may set: whatever it
/// was given could be replaced between one request and the next.
pub(crate) fn reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with(RESERVED_PREFIX)
}

/// A streamable-HTTP connect that failed before its client was up: classified
/// as a stdio start's handshake is, with no process, exit or stderr behind it.
#[derive(Debug)]
pub(crate) struct HttpFailed {
    pub(crate) class: StartupClass,
    pub(crate) source: McpSessionError,
}

/// The HTTP client every request of one URL service goes through: `anchor`, a
/// PEM bundle, trusted beside the system's roots or, when `only`, in their
/// place. No redirect is followed, so a declaration's headers reach no address
/// but the one it names, and no idle connection is kept, as rmcp's own client
/// keeps none -- each request is a new connection, and over `https` a new
/// handshake, which rmcp takes over a reused connection's delayed-ACK stall.
/// The system proxy is honored, as the model loop's client honors it; a
/// test's loopback server is reached directly. Building it reads the system's
/// roots, so it is built off the runtime's threads.
pub(crate) fn client(anchor: Option<&[u8]>, only: bool) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none());
    #[cfg(test)]
    let builder = builder.no_proxy();
    let builder = match anchor {
        None => builder,
        Some(pem) => {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| format!("its trust anchor is not PEM: {}", rendered(&e)))?;
            if certs.is_empty() {
                return Err("its trust anchor holds no certificate".to_string());
            }
            match only {
                true => builder.tls_certs_only(certs),
                false => builder.tls_certs_merge(certs),
            }
        }
    };
    builder
        .build()
        .map_err(|e| format!("its HTTP client could not be built: {}", rendered(&e)))
}

/// The header `name: value` a request carries, its value marked sensitive so
/// that no rendering of a request shows it; or why it cannot be one, naming
/// the header and never its value.
pub(crate) fn header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), String> {
    let header = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("its header {name:?} is not a header name"))?;
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| format!("the value of its header {name} is not a header value"))?;
    value.set_sensitive(true);
    Ok((header, value))
}

impl McpClient {
    /// A client of the streamable-HTTP server at `url`, each request sent
    /// through `client` with `headers`, its `initialize` answered within
    /// `within`: a new HTTP session, whatever an earlier one was. A server
    /// that issues no session id is taken when `allow_stateless`. Why not,
    /// classified.
    pub(crate) async fn connect_http(
        name: &str,
        client: reqwest::Client,
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
        allow_stateless: bool,
        within: Duration,
    ) -> Result<Self, HttpFailed> {
        let notices = Notices::default();
        let resumes = Resumes::giving_up(notices.broken.clone());
        let by = Instant::now() + within;
        let dialer = Dialer {
            client,
            by,
            standing: Arc::default(),
        };
        let transport = StreamableHttpClientTransport::with_client(
            dialer,
            config(url, headers, allow_stateless, resumes),
        );
        let handshake = tokio::time::timeout_at(by, serve_client(notices, transport));
        let (kind, class, message) = match handshake.await {
            Ok(Ok(service)) => {
                return Ok(Self {
                    name: name.to_string(),
                    service,
                    child: None,
                    stderr_path: std::path::PathBuf::new(),
                    call_timeout: effective_mcp_call_timeout(None, None),
                });
            }
            Ok(Err(source)) => {
                let (kind, class) = init_class(&source);
                (kind, class, init_message(&source))
            }
            Err(_) => {
                let Handshake {
                    classified, class, ..
                } = Handshake::timed_out(within);
                (classified.kind, class, classified.message)
            }
        };
        Err(HttpFailed {
            class,
            source: McpSessionError::new(kind, message),
        })
    }
}

/// rmcp's transport configuration for one connect: no session recovery of its
/// own, resumes and bounds of OutRig's, and `headers` on every request -- the
/// `initialize`, every POST, the standing stream and its resumes, a cancel
/// notice and the `DELETE` that ends the session. rmcp's `auth_header` stays
/// unset: it writes `Bearer` before its value, and an `Authorization` header a
/// declaration gives travels as it was given.
fn config(
    url: &str,
    headers: HashMap<HeaderName, HeaderValue>,
    allow_stateless: bool,
    resumes: Resumes,
) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
        .custom_headers(headers)
        .control_request_timeout(CONTROL_TIMEOUT)
        .max_sse_event_size(max_event_bytes())
        .reinit_on_expired_session(false);
    config.retry_config = Arc::new(resumes);
    config.allow_stateless = allow_stateless;
    config
}

/// [`MAX_EVENT_BYTES`], as rmcp takes it.
fn max_event_bytes() -> usize {
    usize::try_from(MAX_EVENT_BYTES).unwrap_or(usize::MAX)
}

/// How a broken stream is resumed -- [`RESUMES`] times, from [`RESUME_BASE`]
/// -- and the end of the connection's listening once it is not. rmcp resumes
/// a call's response stream and the standing stream by one schedule, and a
/// standing stream it gives up on it only logs, leaving the session up and
/// deaf: a listing change announced after is never heard. So giving up on
/// either cancels `gave_up`, which marks the service disconnected, and the
/// next call connects it again -- a new session, a new standing stream, its
/// tools listed again. A call whose stream it was ends as the transport
/// failure it is either way. A standing stream that would not open is resumed
/// by it too: [`Dialer`] says how.
#[derive(Debug)]
struct Resumes {
    backoff: ExponentialBackoff,
    gave_up: CancellationToken,
}

impl Resumes {
    fn giving_up(gave_up: CancellationToken) -> Self {
        let mut backoff = ExponentialBackoff::default();
        backoff.max_times = Some(RESUMES);
        backoff.base_duration = RESUME_BASE;
        Self { backoff, gave_up }
    }
}

impl SseRetryPolicy for Resumes {
    fn retry(&self, current_times: usize) -> Option<Duration> {
        let wait = self.backoff.retry(current_times);
        if wait.is_none() {
            self.gave_up.cancel();
        }
        wait
    }
}

/// The client rmcp's transport sends through: reqwest's, as rmcp drives it,
/// but for two requests.
///
/// - The `initialize` POST is sent here. rmcp reads an answer that is not 2xx
///   and whose body is a JSON-RPC error as that error, its status dropped,
///   which at `initialize` would make a gateway's 503 or 429 a refusal, never
///   tried again; here every answer to `initialize` that is not 2xx is its
///   status, as [`status_class`] reads it. Every other request keeps rmcp's
///   reading, under which a call's JSON-RPC error is its server's answer.
/// - The standing stream's opening: rmcp asks for it once, and one that fails
///   it only logs, the session left up and deaf. Here an opening that fails is
///   a stream that opened and ended at once, which rmcp resumes as it resumes
///   one that ended -- by [`Resumes`], whose giving up disconnects the
///   service. A server that says it has no stream to give, with 405, is taken
///   at its word, as rmcp takes it.
#[derive(Clone)]
struct Dialer {
    client: reqwest::Client,
    /// When `initialize` is to be answered by.
    by: Instant,
    /// Whether the standing stream has been asked for. Its opening is the
    /// first `GET` with no event to resume after; a response stream is resumed
    /// only after one.
    standing: Arc<AtomicBool>,
}

impl Dialer {
    /// `initialize`, POSTed as rmcp POSTs it, its answer read as rmcp reads a
    /// 2xx one.
    async fn initialize(
        &self,
        uri: &str,
        message: &ClientJsonRpcMessage,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
        let mut request = self
            .client
            .post(uri)
            .header(
                ACCEPT,
                format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
            )
            .header(CONTENT_TYPE, JSON_MIME_TYPE);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let body = serde_json::to_vec(message).map_err(StreamableHttpError::Deserialize)?;
        let response = request.body(body).send().await?;
        if !response.status().is_success() {
            return Err(self.unsuccessful(response).await);
        }
        let session = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|id| id.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned());
        match content_type.as_deref() {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME_TYPE) => {
                let events = sse_stream::SseStream::from_bytes_stream(response.bytes_stream());
                Ok(StreamableHttpPostResponse::Sse(events.boxed(), session))
            }
            Some(ct) if ct.starts_with(JSON_MIME_TYPE) => {
                let body = response.bytes().await?;
                let answer =
                    serde_json::from_slice(&body).map_err(StreamableHttpError::Deserialize)?;
                Ok(StreamableHttpPostResponse::Json(answer, session))
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    /// What an answer to `initialize` that is not 2xx says, as rmcp reads one:
    /// a 401 or 403 with a challenge at once, its body unread, and any other
    /// with what of its body arrives within [`BODY_WAIT`].
    async fn unsuccessful(
        &self,
        response: reqwest::Response,
    ) -> StreamableHttpError<reqwest::Error> {
        let status = response.status();
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .map(|said| String::from_utf8_lossy(said.as_bytes()).into_owned());
        match (status, challenge) {
            (StatusCode::UNAUTHORIZED, Some(challenge)) => {
                StreamableHttpError::AuthRequired(AuthRequiredError::new(challenge))
            }
            (StatusCode::FORBIDDEN, Some(challenge)) => {
                StreamableHttpError::InsufficientScope(InsufficientScopeError::new(challenge, None))
            }
            _ => {
                let left = self.by.saturating_duration_since(Instant::now());
                let body = tokio::time::timeout(BODY_WAIT.min(left / 2), response.text()).await;
                let body = body.ok().and_then(Result::ok).unwrap_or_default();
                StreamableHttpError::UnexpectedServerResponse(
                    format!("HTTP {status}: {body}").into(),
                )
            }
        }
    }
}

/// Whether `message` is the `initialize` request.
fn initializes(message: &ClientJsonRpcMessage) -> bool {
    matches!(
        message,
        ClientJsonRpcMessage::Request(request)
            if matches!(request.request, ClientRequest::InitializeRequest(_))
    )
}

impl StreamableHttpClient for Dialer {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            max_event_bytes(),
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        // No `auth_header` is ever set; a session id means a session that
        // has already been initialized.
        if session_id.is_none() && auth_header.is_none() && initializes(&message) {
            return self.initialize(&uri, &message, custom_headers).await;
        }
        self.client
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.client
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            max_event_bytes(),
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let opening = last_event_id.is_none() && !self.standing.swap(true, Ordering::Relaxed);
        let stream = self
            .client
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await;
        match stream {
            Err(error)
                if opening && !matches!(error, StreamableHttpError::ServerDoesNotSupportSse) =>
            {
                tracing::debug!("the standing stream did not open, so it is resumed: {error}");
                Ok(futures_util::stream::empty().boxed())
            }
            stream => stream,
        }
    }
}

/// What a failed handshake means: an HTTP failure by what the server or the
/// connection said, anything else as a stdio handshake's.
fn init_class(source: &ClientInitializeError) -> (McpFailureKind, StartupClass) {
    match http_failure(source) {
        Some(failure) => http_class(failure),
        None => init_error_class(source),
    }
}

/// What a failed handshake says, an HTTP failure with every cause it holds:
/// reqwest's own rendering names no cause, and the cause -- refused, reset, a
/// certificate it would not trust -- is what a reader needs.
fn init_message(source: &ClientInitializeError) -> String {
    let ClientInitializeError::TransportError { error, context } = source else {
        return source.to_string();
    };
    // rmcp's `Client` names reqwest's error as no source of its own.
    let said = match http_failure(source) {
        Some(StreamableHttpError::Client(e)) => rendered(e),
        _ => rendered(&*error.error),
    };
    format!("{context}: {said}")
}

/// The HTTP transport's own failure inside `source`, when it is one.
fn http_failure(source: &ClientInitializeError) -> Option<&StreamableHttpError<reqwest::Error>> {
    match source {
        ClientInitializeError::TransportError { error, .. } => error.error.downcast_ref(),
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => http_failure(fallback),
        _ => None,
    }
}

/// What the HTTP transport's failure under `source`, a request's, means, when
/// it was one, read as one at `initialize` is: a listing a server refuses is
/// refused, not a transport failure to try again.
pub(super) fn request_class(source: &ServiceError) -> Option<(McpFailureKind, StartupClass)> {
    let ServiceError::TransportSend(error) = source else {
        return None;
    };
    error.error.downcast_ref().map(http_class)
}

/// What an HTTP transport failure at `initialize` means for trying again:
/// `mcp-presenter.md`'s "Startup" table. A refusal by status or challenge is
/// refused; a certificate the client would not trust, a URL that serves no
/// endpoint, and a header rmcp refuses are misconfigured; a connection that
/// failed or broke, a timeout, 408, 429 and 5xx are transient; the rest is
/// unclassified, and says so.
fn http_class(failure: &StreamableHttpError<reqwest::Error>) -> (McpFailureKind, StartupClass) {
    use McpFailureKind as K;
    use StartupClass as C;
    use StreamableHttpError as E;

    // `StreamableHttpError` is `#[non_exhaustive]`, so the wildcard is
    // mandatory; `REVIEWED_RMCP`, in `tests/reviewed_rmcp.rs`, is what keeps
    // it from going unreviewed.
    match failure {
        E::AuthRequired(_) | E::InsufficientScope(_) => (K::Protocol, C::Refused),
        E::UnexpectedServerResponse(said) => match status_said(said) {
            Some(status) => status_class(status),
            None => (K::Protocol, C::Unclassified),
        },
        E::Client(e) => client_class(e),
        E::ReservedHeaderConflict(_) => (K::Other, C::Misconfigured),
        // A server that issues no session, to a declaration that needs one.
        E::MissingSessionIdInResponse => (K::Protocol, C::Misconfigured),
        E::Sse(_)
        | E::Io(_)
        | E::UnexpectedEndOfStream
        | E::TransportChannelClosed
        | E::SessionExpired => (K::Transport, C::Transient),
        E::ControlRequestTimeout | E::SessionRecoveryTimeout => (K::Timeout, C::Transient),
        E::UnexpectedContentType(_)
        | E::Deserialize(_)
        | E::ServerDoesNotSupportSse
        | E::ServerDoesNotSupportDeleteSession => (K::Protocol, C::Unclassified),
        E::TokioJoinError(_) => (K::Other, C::Unclassified),
        _ => (K::Other, C::Unclassified),
    }
}

/// The status rmcp read from a POST it could not use: it says it only in its
/// text, `HTTP 503 Service Unavailable: <body>`.
fn status_said(said: &str) -> Option<StatusCode> {
    let code = said.strip_prefix("HTTP ")?.get(..3)?;
    StatusCode::from_bytes(code.as_bytes()).ok()
}

/// What a status the server answered `initialize` with means.
fn status_class(status: StatusCode) -> (McpFailureKind, StartupClass) {
    use McpFailureKind as K;
    use StartupClass as C;

    match status.as_u16() {
        401 | 403 => (K::Protocol, C::Refused),
        // Asked of a URL that serves no MCP endpoint: rmcp reads a 404 to a
        // request that carried a session as `SessionExpired`, so this one
        // carried none.
        404 => (K::Protocol, C::Misconfigured),
        408 | 429 | 500..=599 => (K::Transport, C::Transient),
        _ => (K::Protocol, C::Unclassified),
    }
}

/// What a request reqwest could not make or finish means.
fn client_class(e: &reqwest::Error) -> (McpFailureKind, StartupClass) {
    use McpFailureKind as K;
    use StartupClass as C;

    if certificate_refused(e) {
        return (K::Transport, C::Misconfigured);
    }
    if let Some(status) = e.status() {
        return status_class(status);
    }
    if e.is_timeout() {
        return (K::Timeout, C::Transient);
    }
    if e.is_connect() || e.is_request() || e.is_body() {
        return (K::Transport, C::Transient);
    }
    if e.is_builder() {
        return (K::Other, C::Misconfigured);
    }
    (K::Other, C::Unclassified)
}

/// Whether `e` failed because the server's certificate is not one the client
/// trusts: rustls's own certificate error among its causes.
fn certificate_refused(e: &(dyn std::error::Error + 'static)) -> bool {
    causes(e).any(|cause| {
        matches!(
            cause.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(_))
        )
    })
}

/// `e` and each of its causes, as one line: each said once, where a cause
/// repeats the text of the error it causes.
fn rendered(e: &(dyn std::error::Error + 'static)) -> String {
    let mut said: Vec<String> = Vec::new();
    for cause in causes(e) {
        let text = cause.to_string();
        if !said.iter().any(|earlier| earlier.contains(&text)) {
            said.push(text);
        }
    }
    said.join(": ")
}

/// `e` and each of its causes, in order. An I/O error is looked inside, since
/// its `source` skips the error it wraps -- where TLS keeps its own.
fn causes<'a>(
    e: &'a (dyn std::error::Error + 'static),
) -> impl Iterator<Item = &'a (dyn std::error::Error + 'static)> {
    std::iter::successors(Some(e), |cause| {
        let wrapped = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref());
        match wrapped {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => cause.source(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `StreamableHttpError` variant rmcp 3.4.1 declares without its
    /// `auth` feature, named one by one, because the classifier's wildcard
    /// means the list is the only record of what was classified on purpose.
    /// `Client` holds a reqwest error a test cannot build by hand, and is
    /// driven against real sockets in `harness/mcp_http_tests.rs`.
    #[rustfmt::skip]
    #[test]
    fn every_rmcp_http_error_is_classified() {
        use rmcp::transport::streamable_http_client::{AuthRequiredError, InsufficientScopeError};
        use StreamableHttpError as E;
        use McpFailureKind as K;
        use StartupClass as C;

        let said = |text: &'static str| E::UnexpectedServerResponse(text.into());
        let join = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let task = tokio::spawn(std::future::pending::<()>());
            task.abort();
            task.await.unwrap_err()
        });
        let sse = rmcp::transport::streamable_http_client::SseError::InvalidLine;
        for (failure, expected) in [
            (E::<reqwest::Error>::Sse(sse), (K::Transport, C::Transient)),
            (E::Io(std::io::Error::other("reset")), (K::Transport, C::Transient)),
            (E::UnexpectedEndOfStream, (K::Transport, C::Transient)),
            (said("HTTP 401 Unauthorized: go away"), (K::Protocol, C::Refused)),
            (said("HTTP 403 Forbidden: "), (K::Protocol, C::Refused)),
            (said("HTTP 404 Not Found: "), (K::Protocol, C::Misconfigured)),
            (said("HTTP 408 Request Timeout: "), (K::Transport, C::Transient)),
            (said("HTTP 429 Too Many Requests: "), (K::Transport, C::Transient)),
            (said("HTTP 503 Service Unavailable: "), (K::Transport, C::Transient)),
            (said("HTTP 418 I'm a teapot: "), (K::Protocol, C::Unclassified)),
            (said("could not parse JSON response"), (K::Protocol, C::Unclassified)),
            (E::UnexpectedContentType(Some("text/html".into())), (K::Protocol, C::Unclassified)),
            (E::ServerDoesNotSupportSse, (K::Protocol, C::Unclassified)),
            (E::ServerDoesNotSupportDeleteSession, (K::Protocol, C::Unclassified)),
            (E::TokioJoinError(join), (K::Other, C::Unclassified)),
            (E::Deserialize(serde_json::from_str::<u8>("x").unwrap_err()), (K::Protocol, C::Unclassified)),
            (E::TransportChannelClosed, (K::Transport, C::Transient)),
            (E::MissingSessionIdInResponse, (K::Protocol, C::Misconfigured)),
            (E::AuthRequired(AuthRequiredError::new("Bearer".into())), (K::Protocol, C::Refused)),
            (E::InsufficientScope(InsufficientScopeError::new("Bearer".into(), None)), (K::Protocol, C::Refused)),
            (E::ReservedHeaderConflict("accept".into()), (K::Other, C::Misconfigured)),
            (E::SessionExpired, (K::Transport, C::Transient)),
            (E::SessionRecoveryTimeout, (K::Timeout, C::Transient)),
            (E::ControlRequestTimeout, (K::Timeout, C::Transient)),
        ] {
            let rendered = failure.to_string();
            assert_eq!(http_class(&failure), expected, "classifying {rendered:?}");
        }
    }

    #[test]
    fn a_status_is_read_from_what_rmcp_said() {
        assert_eq!(
            status_said("HTTP 503 Service Unavailable: busy"),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(status_said("HTTP 5"), None);
        assert_eq!(status_said("unexpected"), None);
    }
}
