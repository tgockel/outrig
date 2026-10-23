//! A scripted LLM endpoint on loopback, for driving rounds without a provider.
//!
//! A copy of `outrig-cli`'s `tests/common/mod.rs` mock and the Anthropic
//! envelope helpers in its `tests/anthropic_mock.rs` -- the first in this
//! crate, which a unit test cannot reach across the crate boundary for. It
//! asserts on what was *sent*, which is how a round proves what reached the
//! model.
//!
//! It speaks Anthropic's Messages API by default and OpenAI's chat completions
//! through [`Style`], and accepts any request either way. What a provider
//! would refuse is [`check_wire`]'s to find -- except as [`start_strict`], a
//! provider that requires the user's and the model's turns to alternate, which
//! refuses a request whose roles repeat as a gateway in front of one would.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};

/// The identifier every canned response names. Recognized by rig, with a
/// published ceiling of 64 000.
pub(crate) const MODEL: &str = "claude-sonnet-4-6";

/// The signature on the thinking an Anthropic canned response carries.
pub(super) const SIGNATURE: &str = "signed-by-the-mock";

/// One request as the mock saw it, before any client library is asked to
/// interpret it.
#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    pub(super) path: String,
    /// Each header as sent, its name lowercased.
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Value,
}

impl RecordedRequest {
    /// The value of the header `name`, matched without regard to case.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One canned response.
#[derive(Clone)]
pub(crate) struct CannedResponse {
    status: u16,
    body: Value,
    /// Sent besides the content type, length, and `Connection: close`.
    headers: Vec<(&'static str, String)>,
}

impl CannedResponse {
    fn new(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
        }
    }

    /// The same response, reporting `usage` -- in the provider's own shape --
    /// rather than the fixed counts every canned response starts with.
    pub(super) fn usage(mut self, usage: Value) -> Self {
        self.body["usage"] = usage;
        self
    }

    /// The body it is sent with.
    pub(super) fn body(&self) -> &Value {
        &self.body
    }

    /// The same response, reporting no `usage` at all.
    pub(super) fn without_usage(mut self) -> Self {
        if let Some(body) = self.body.as_object_mut() {
            body.remove("usage");
        }
        self
    }

    /// The same reply, with the model's reasoning `text` before the rest: as
    /// OpenAI-compatible providers send it, a `reasoning_content` beside the
    /// message, or as Anthropic does, a thinking block signed [`SIGNATURE`].
    pub(super) fn with_reasoning(mut self, text: &str) -> Self {
        if self.body.get("choices").is_some() {
            self.body["choices"][0]["message"]["reasoning_content"] = json!(text);
        } else if let Some(content) = self.body["content"].as_array_mut() {
            content.insert(
                0,
                json!({ "type": "thinking", "thinking": text, "signature": SIGNATURE }),
            );
        }
        self
    }

    /// The same response, carrying the header `name: value` -- a
    /// `Retry-After`, say.
    pub(super) fn header(mut self, name: &'static str, value: impl ToString) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

/// A provider failing: `status` with Anthropic's error envelope.
pub(super) fn failure(status: u16) -> CannedResponse {
    CannedResponse::new(
        status,
        json!({
            "type": "error",
            "error": { "type": "api_error", "message": "the mock failed on purpose" },
        }),
    )
}

/// A `200` whose body is not a message, so rig's decode of it fails.
pub(super) fn undecodable() -> CannedResponse {
    CannedResponse::new(200, json!({ "not": "a message" }))
}

/// An Anthropic message envelope in the shape rig's `ApiResponse`
/// deserializes.
pub(super) fn message(content: Value, stop_reason: &str) -> CannedResponse {
    CannedResponse::new(
        200,
        json!({
            "type": "message",
            "id": "msg_mock",
            "model": MODEL,
            "role": "assistant",
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "content": content,
            "usage": { "input_tokens": 12, "output_tokens": 7 },
        }),
    )
}

/// A `200 OK` rig cannot make a completion of: no content at all. Its
/// `stop_reason` is not `end_turn`, which rig would read as an empty reply.
pub(super) fn unusable() -> CannedResponse {
    Style::Anthropic.unusable()
}

/// One text block, round over.
pub(super) fn text_reply(text: &str) -> CannedResponse {
    message(json!([{ "type": "text", "text": text }]), "end_turn")
}

/// A turn that asks for `submit_python` with `source`.
pub(super) fn submit(id: &str, source: &str) -> CannedResponse {
    Style::Anthropic.submit(id, source)
}

/// Which provider's protocol a canned response speaks, and a config names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Style {
    Anthropic,
    OpenAi,
}

impl Style {
    pub(super) const ALL: [Style; 2] = [Style::Anthropic, Style::OpenAi];

    /// What `[providers.<name>].style` says for it.
    pub(super) fn name(self) -> &'static str {
        match self {
            Style::Anthropic => "anthropic",
            Style::OpenAi => "openai",
        }
    }

    /// One text reply, round over.
    pub(super) fn text(self, text: &str) -> CannedResponse {
        match self {
            Style::Anthropic => text_reply(text),
            Style::OpenAi => chat(json!({"role": "assistant", "content": text}), "stop"),
        }
    }

    /// A `200 OK` rig cannot make a completion of: no content at all, cut off.
    pub(super) fn unusable(self) -> CannedResponse {
        match self {
            Style::Anthropic => self.empty("max_tokens"),
            Style::OpenAi => self.empty("length"),
        }
    }

    /// A `200 OK` with no content at all, whose finish reason is
    /// `finish_reason`.
    pub(super) fn empty(self, finish_reason: &str) -> CannedResponse {
        match self {
            Style::Anthropic => message(json!([]), finish_reason),
            Style::OpenAi => chat(json!({"role": "assistant", "content": ""}), finish_reason),
        }
    }

    /// A model with nothing to add ending its turn: no content, and the
    /// finish reason a provider gives for the model stopping of its own accord.
    pub(super) fn empty_stop(self) -> CannedResponse {
        match self {
            Style::Anthropic => self.empty("end_turn"),
            Style::OpenAi => self.empty("stop"),
        }
    }

    /// A turn that asks for `submit_python` with `source`.
    pub(super) fn submit(self, id: &str, source: &str) -> CannedResponse {
        self.batch(&[(id, source)])
    }

    /// A turn that asks for `submit_python` once per `(id, source)`.
    pub(super) fn batch(self, calls: &[(&str, &str)]) -> CannedResponse {
        match self {
            Style::Anthropic => message(
                calls
                    .iter()
                    .map(|(id, source)| {
                        json!({
                            "type": "tool_use",
                            "id": id,
                            "name": super::tool::NAME,
                            "input": { "source": source },
                        })
                    })
                    .collect(),
                "tool_use",
            ),
            Style::OpenAi => chat(
                json!({
                    "role": "assistant",
                    "tool_calls": calls
                        .iter()
                        .map(|(id, source)| json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": super::tool::NAME,
                                "arguments": json!({"source": source}).to_string(),
                            },
                        }))
                        .collect::<Vec<_>>(),
                }),
                "tool_calls",
            ),
        }
    }

    /// The provider refusing a request with `status`.
    pub(super) fn failure(self, status: u16) -> CannedResponse {
        match self {
            Style::Anthropic => failure(status),
            Style::OpenAi => CannedResponse::new(
                status,
                json!({
                    "error": {
                        "type": "invalid_request_error",
                        "message": "the mock failed on purpose",
                    },
                }),
            ),
        }
    }

    /// A `200` carrying the provider's error envelope, as a gateway answers a
    /// request it accepted and that then failed from an overload or a timeout.
    /// rig reads it as a provider response with a success status.
    pub(super) fn failed_after_accepting(self) -> CannedResponse {
        match self {
            Style::Anthropic => {
                CannedResponse::new(200, json!({ "type": "error", "message": "Overloaded" }))
            }
            Style::OpenAi => CannedResponse::new(
                200,
                json!({ "error": { "code": 502, "message": "Provider returned error" } }),
            ),
        }
    }
}

/// A chat completion choosing `message`, in the shape rig's OpenAI
/// `CompletionResponse` deserializes.
pub(super) fn chat(message: Value, finish_reason: &str) -> CannedResponse {
    CannedResponse::new(
        200,
        json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-mock",
            "system_fingerprint": null,
            "choices": [{
                "index": 0,
                "message": message,
                "logprobs": null,
                "finish_reason": finish_reason,
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 },
        }),
    )
}

/// What [`check_wire`] found in one request's `messages`.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Wire {
    /// Tool-call ids without their result where the provider requires it, and
    /// results without the call they answer.
    pub(super) unpaired: Vec<String>,
    /// Where a user or assistant message follows another of its role, by index
    /// into `messages`, with the role.
    pub(super) adjacent: Vec<(usize, String)>,
}

/// Check a request `body` against what `style`'s provider requires of the
/// history every request we send relies on, and say where roles repeat.
///
/// - Anthropic: each `tool_use` block is answered by a `tool_result` in the
///   very next message, and each `tool_result` answers a `tool_use` in the one
///   before.
/// - OpenAI: each of an assistant message's `tool_calls` is answered by a
///   `tool` message before the next message of any other role, and each
///   `tool` message answers a call of the assistant message before its run.
pub(super) fn check_wire(style: Style, body: &Value) -> Wire {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let role = |n: usize| messages[n]["role"].as_str().unwrap_or("").to_string();
    let mut wire = Wire::default();
    match style {
        Style::Anthropic => {
            let blocks = |n: usize, kind: &str, key: &str| -> Vec<String> {
                messages[n]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|block| block["type"] == kind)
                    .filter_map(|block| block[key].as_str().map(str::to_string))
                    .collect()
            };
            for n in 0..messages.len() {
                let answered = if n + 1 < messages.len() {
                    blocks(n + 1, "tool_result", "tool_use_id")
                } else {
                    Vec::new()
                };
                for id in blocks(n, "tool_use", "id") {
                    if !answered.contains(&id) {
                        wire.unpaired.push(id);
                    }
                }
                let asked = if n > 0 {
                    blocks(n - 1, "tool_use", "id")
                } else {
                    Vec::new()
                };
                for id in blocks(n, "tool_result", "tool_use_id") {
                    if !asked.contains(&id) {
                        wire.unpaired.push(id);
                    }
                }
            }
        }
        Style::OpenAi => {
            let mut asked: Vec<String> = Vec::new();
            for message in &messages {
                if message["role"] == "tool" {
                    let id = message["tool_call_id"].as_str().unwrap_or("").to_string();
                    match asked.iter().position(|asked| *asked == id) {
                        Some(at) => {
                            asked.remove(at);
                        }
                        None => wire.unpaired.push(id),
                    }
                    continue;
                }
                wire.unpaired.append(&mut asked);
                asked = message["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call["id"].as_str().map(str::to_string))
                    .collect();
            }
            wire.unpaired.append(&mut asked);
        }
    }
    for n in 1..messages.len() {
        let here = role(n);
        if (here == "user" || here == "assistant") && role(n - 1) == here {
            wire.adjacent.push((n, here));
        }
    }
    wire
}

/// The roles of a request `body`'s `messages` as a gateway in front of a
/// strict provider sees them: the system prompt apart, and a run of OpenAI
/// `tool` messages -- one turn's results -- as the one user message it
/// becomes. Anthropic's wire already carries results in user messages, so its
/// roles come through as they are.
fn gateway_roles(body: &Value) -> Vec<&str> {
    let mut roles: Vec<&str> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["role"].as_str())
        .filter(|role| *role != "system")
        .collect();
    roles.dedup_by(|a, b| *a == "tool" && *b == "tool");
    roles
        .into_iter()
        .map(|role| if role == "tool" { "user" } else { role })
        .collect()
}

/// How a provider that requires the user's and the model's turns to alternate
/// refuses `body`, in the words Bedrock's Claude uses: a 400 naming the role
/// that repeats, or the first message when it is not the user's. `None` when
/// the request is one it takes.
pub(super) fn strict_refusal(body: &Value) -> Option<CannedResponse> {
    let roles = gateway_roles(body);
    let message = match roles.first() {
        Some(&first) if first != "user" => {
            r#"messages: first message must use the "user" role"#.to_string()
        }
        _ => {
            let repeated = roles.windows(2).find(|pair| pair[0] == pair[1])?;
            format!(
                r#"messages: roles must alternate between "user" and "assistant", but found multiple "{}" roles in a row"#,
                repeated[0]
            )
        }
    };
    Some(CannedResponse::new(
        400,
        json!({
            "error": {
                "type": "invalid_request_error",
                "message": message,
            },
        }),
    ))
}

/// Start the mock on an ephemeral loopback port. Returns its address and the
/// channel on which every request arrives.
///
/// The server answers `script` in order, one response per connection. Past
/// the end of the script it repeats the last entry rather than hanging, so an
/// unexpected extra request fails a count assertion instead of a timeout.
pub(crate) async fn start(
    script: Vec<CannedResponse>,
) -> (
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<RecordedRequest>,
) {
    start_with(script, |_| None).await
}

/// [`start`], as a provider that requires the user's and the model's turns to
/// alternate: a request whose roles repeat as a gateway sees them, or that does
/// not open on the user's, is answered with [`strict_refusal`] rather than from
/// the script, which it does not advance. The request is recorded either way.
pub(super) async fn start_strict(
    script: Vec<CannedResponse>,
) -> (
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<RecordedRequest>,
) {
    start_with(script, strict_refusal).await
}

/// [`start`], refusing each request `refuses` answers for, which the script
/// does not advance past.
async fn start_with(
    script: Vec<CannedResponse>,
    refuses: fn(&Value) -> Option<CannedResponse>,
) -> (
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<RecordedRequest>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local_addr");
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(serve(listener, script, tx, refuses));
    (addr, rx)
}

/// The discard port: assigned to a service essentially nothing runs, and below
/// the ephemeral range so no test can be handed it. Every unreachable-endpoint
/// fixture points here.
pub(super) const REFUSED_PORT: u16 = 9;

/// The innermost `source()` text of a refused connect to [`REFUSED_PORT`],
/// which is what a failure line should name instead of reqwest's outer text.
/// Taken from the error itself because the OS wording varies.
///
/// `no_proxy` for the same reason `remote_http_client` sets it under
/// `cfg(test)`: on a machine behind a proxy the request would go to the proxy
/// and never be refused.
pub(super) async fn refused_cause() -> String {
    let err = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a bare client builds")
        .get(format!("http://127.0.0.1:{REFUSED_PORT}/"))
        .send()
        .await
        .expect_err("nothing listens");
    let mut cause: &(dyn std::error::Error + 'static) = &err;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

/// A chain of errors: each link's `Display` is its own text, and its `source()`
/// the next link.
#[derive(Debug)]
pub(super) struct Link(pub(super) &'static str, pub(super) Option<Box<Link>>);

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Link {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.1.as_deref().map(|link| link as _)
    }
}

/// A transport failure as reqwest raises one: the outer text says only that
/// sending failed, and the cause is in its `source()`.
pub(super) fn transport_failure() -> rig::completion::CompletionError {
    rig::completion::CompletionError::HttpError(rig::http_client::Error::Instance(Box::new(Link(
        "error sending request",
        Some(Box::new(Link("Connection refused", None))),
    ))))
}

/// Everything the mock has recorded so far.
pub(crate) fn drain(rx: &mut mpsc::UnboundedReceiver<RecordedRequest>) -> Vec<RecordedRequest> {
    let mut out = Vec::new();
    while let Ok(recorded) = rx.try_recv() {
        out.push(recorded);
    }
    out
}

async fn serve(
    listener: TcpListener,
    script: Vec<CannedResponse>,
    tx: mpsc::UnboundedSender<RecordedRequest>,
    refuses: fn(&Value) -> Option<CannedResponse>,
) {
    let mut served = 0usize;
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let Some(recorded) = read_request(&mut sock).await else {
            continue;
        };
        let refused = refuses(&recorded.body);
        if tx.send(recorded).is_err() {
            return;
        }
        let canned = match refused {
            Some(refusal) => refusal,
            None => {
                let Some(canned) = script.get(served).or(script.last()).cloned() else {
                    return;
                };
                served += 1;
                canned
            }
        };

        let body = serde_json::to_string(&canned.body).expect("canned body serializes");
        let headers: String = canned
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let response = format!(
            "HTTP/1.1 {} MOCK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n{headers}\r\n{}",
            canned.status,
            body.len(),
            body,
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

/// What the routed mock answers one request with: a response, ready when the
/// future is -- at once, or once the test lets it go.
pub(crate) type Responder = Arc<
    dyn Fn(&RecordedRequest) -> Pin<Box<dyn Future<Output = CannedResponse> + Send>> + Send + Sync,
>;

/// A responder answering `script` in order, one each request, repeating the
/// last past its end.
pub(crate) fn scripted(script: Vec<CannedResponse>) -> Responder {
    let next = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_| {
        let n = next.fetch_add(1, Ordering::SeqCst);
        let canned = script
            .get(n)
            .or(script.last())
            .cloned()
            .expect("a script of at least one response");
        Box::pin(async move { canned })
    })
}

/// How many requests the routed mock holds open at once, and the most it ever
/// has.
#[derive(Default)]
pub(crate) struct Gauge {
    open: AtomicUsize,
    most: AtomicUsize,
    changed: Notify,
}

impl Gauge {
    pub(crate) fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    pub(crate) fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// The system prompt `body` carries, whether as one string or as blocks.
pub(crate) fn system_text(body: &Value) -> String {
    match &body["system"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// The mock as several agents share it. Each request goes to the responder of
/// the first route whose marker its system prompt holds -- `""` holds in any --
/// and is answered on a task of its own, so requests overlap. With `hold`, each
/// waits until `hold` requests are open at once, or until a second has passed
/// with fewer, before its responder is asked.
pub(crate) async fn routed(
    routes: Vec<(&'static str, Responder)>,
    hold: Option<usize>,
) -> (
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<RecordedRequest>,
    Arc<Gauge>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local_addr");
    let (tx, rx) = mpsc::unbounded_channel();
    let gauge = Arc::new(Gauge::default());
    let routes = Arc::new(routes);
    tokio::spawn({
        let gauge = Arc::clone(&gauge);
        async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let (routes, tx, gauge) = (Arc::clone(&routes), tx.clone(), Arc::clone(&gauge));
                tokio::spawn(answer_routed(sock, routes, tx, gauge, hold));
            }
        }
    });
    (addr, rx, gauge)
}

async fn answer_routed(
    mut sock: TcpStream,
    routes: Arc<Vec<(&'static str, Responder)>>,
    tx: mpsc::UnboundedSender<RecordedRequest>,
    gauge: Arc<Gauge>,
    hold: Option<usize>,
) {
    let Some(recorded) = read_request(&mut sock).await else {
        return;
    };
    let open = gauge.open.fetch_add(1, Ordering::SeqCst) + 1;
    gauge.most.fetch_max(open, Ordering::SeqCst);
    gauge.changed.notify_waiters();
    if let Some(hold) = hold {
        loop {
            let changed = gauge.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if gauge.open() >= hold {
                gauge.changed.notify_waiters();
                break;
            }
            if tokio::time::timeout(Duration::from_secs(1), changed)
                .await
                .is_err()
            {
                break;
            }
        }
    }
    let system = system_text(&recorded.body);
    let responder = routes
        .iter()
        .find(|(marker, _)| system.contains(marker))
        .map(|(_, responder)| Arc::clone(responder))
        .expect("a route for every request");
    let _ = tx.send(recorded.clone());
    let canned = responder(&recorded).await;
    let body = serde_json::to_string(&canned.body).expect("canned body serializes");
    let headers: String = canned
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let response = format!(
        "HTTP/1.1 {} MOCK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n{headers}\r\n{}",
        canned.status,
        body.len(),
        body,
    );
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
    gauge.open.fetch_sub(1, Ordering::SeqCst);
    gauge.changed.notify_waiters();
}

/// Read one HTTP/1.1 request: the request line, its headers, and a
/// `Content-Length`-delimited JSON body.
async fn read_request(sock: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = vec![0u8; 8192];
    let mut total = Vec::new();

    let header_end = loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        total.extend_from_slice(&buf[..n]);
        if let Some(idx) = total.windows(4).position(|w| w == b"\r\n\r\n") {
            break idx + 4;
        }
        if total.len() > 1 << 20 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&total[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let _method = request_line.next()?;
    let path = request_line.next()?.to_string();

    let mut content_length = 0usize;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value.to_string()));
        }
    }

    while total.len() < header_end + content_length {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        total.extend_from_slice(&buf[..n]);
    }
    let body = serde_json::from_slice(&total[header_end..]).unwrap_or(Value::Null);

    Some(RecordedRequest {
        path,
        headers,
        body,
    })
}
