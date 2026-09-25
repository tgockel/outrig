//! A scripted LLM endpoint on loopback, for driving rounds without a provider.
//!
//! A copy of `outrig-cli`'s `tests/common/mod.rs` mock and the Anthropic
//! envelope helpers in its `tests/anthropic_mock.rs` -- the first in this
//! crate, which a unit test cannot reach across the crate boundary for. It
//! asserts on what was *sent*, which is how a round proves what reached the
//! model.

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// The identifier every canned response names. Recognized by rig, with a
/// published ceiling of 64 000.
pub(super) const MODEL: &str = "claude-sonnet-4-6";

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
}

/// A provider failing: `status` with Anthropic's error envelope.
pub(super) fn failure(status: u16) -> CannedResponse {
    CannedResponse {
        status,
        body: json!({
            "type": "error",
            "error": { "type": "api_error", "message": "the mock failed on purpose" },
        }),
    }
}

/// An Anthropic message envelope in the shape rig's `ApiResponse`
/// deserializes.
pub(super) fn message(content: Value, stop_reason: &str) -> CannedResponse {
    CannedResponse {
        status: 200,
        body: json!({
            "type": "message",
            "id": "msg_mock",
            "model": MODEL,
            "role": "assistant",
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "content": content,
            "usage": { "input_tokens": 12, "output_tokens": 7 },
        }),
    }
}

/// One text block, round over.
pub(super) fn text_reply(text: &str) -> CannedResponse {
    message(json!([{ "type": "text", "text": text }]), "end_turn")
}

/// A turn that asks for `submit_python` with `source`.
pub(super) fn submit(id: &str, source: &str) -> CannedResponse {
    message(
        json!([{
            "type": "tool_use",
            "id": id,
            "name": super::tool::NAME,
            "input": { "source": source },
        }]),
        "tool_use",
    )
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local_addr");
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(serve(listener, script, tx));
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
) {
    let mut served = 0usize;
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        let Some(recorded) = read_request(&mut sock).await else {
            continue;
        };
        if tx.send(recorded).is_err() {
            return;
        }
        let Some(canned) = script.get(served).or(script.last()).cloned() else {
            return;
        };
        served += 1;

        let body = serde_json::to_string(&canned.body).expect("canned body serializes");
        let response = format!(
            "HTTP/1.1 {} MOCK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
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
