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

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// The identifier every canned response names. Recognized by rig, with a
/// published ceiling of 64 000.
pub(super) const MODEL: &str = "claude-sonnet-4-6";

/// The signature on the thinking an Anthropic canned response carries.
pub(super) const SIGNATURE: &str = "signed-by-the-mock";

/// One request as the mock saw it, before any client library is asked to
/// interpret it.
#[derive(Debug)]
pub(super) struct RecordedRequest {
    pub(super) path: String,
    pub(super) body: Value,
}

/// One canned response.
#[derive(Clone)]
pub(super) struct CannedResponse {
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
    message(json!([]), "max_tokens")
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
}

/// A chat completion choosing `message`, in the shape rig's OpenAI
/// `CompletionResponse` deserializes.
fn chat(message: Value, finish_reason: &str) -> CannedResponse {
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
pub(super) async fn start(
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

/// Everything the mock has recorded so far.
pub(super) fn drain(rx: &mut mpsc::UnboundedReceiver<RecordedRequest>) -> Vec<RecordedRequest> {
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
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
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

    Some(RecordedRequest { path, body })
}
