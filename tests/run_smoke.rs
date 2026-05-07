//! End-to-end smoke for `outrig run`. Gated behind `--features e2e`.
//!
//! Sets up a fixture repo in a tempdir whose `.agents/outrig/config.toml`
//! points its OpenAI base-url at a local hand-rolled mock server, then runs
//! the `outrig` binary as a subprocess, pipes one prompt to stdin, and
//! asserts on captured stdout/stderr.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test run_smoke -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture_mcp_fs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs")
}

fn write_smoke_config(repo: &Path, mock_addr: &str) {
    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = fixture_mcp_fs_dir().join("Dockerfile");
    let context = fixture_mcp_fs_dir();
    let config_toml = format!(
        r#"
default-agent = "smoke"
default-container = "smoke"

[providers.openai]
style = "openai"
base-url = "http://{addr}/v1"
api-key = "${{OUTRIG_TEST_KEY}}"
request-timeout-secs = 10

[models.fast]
provider = "openai"
identifier = "gpt-4o-mini"

[agents.smoke]
model = "fast"
preamble = "test"

[containers.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

  [containers.smoke.mcp]
  fs = ["mcp-server-filesystem", "/workspace"]
"#,
        addr = mock_addr,
        dockerfile = dockerfile.display(),
        context = context.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_drives_one_tool_call_and_prints_reply() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    // 1. Spin up the mock OpenAI server.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let mock_addr = listener.local_addr().expect("mock addr");
    let server_handle = tokio::spawn(run_mock_openai(listener));

    // 2. Build the fixture repo in a tempdir.
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_smoke_config(repo_dir.path(), &mock_addr.to_string());

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let sessions_s = sessions.path().to_str().expect("sessions path utf-8");
    let session_dir_s = session_dir.path().to_str().expect("session dir path utf-8");

    let Captured {
        status,
        stdout: stdout_str,
        stderr: stderr_str,
    } = run_child(
        &[
            "--session-root",
            sessions_s,
            "run",
            "--session-dir",
            session_dir_s,
        ],
        repo_dir.path(),
    )
    .await;
    server_handle.abort();

    assert!(
        status.success(),
        "outrig run exited with {status:?}; stderr was: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] agent:"),
        "stderr lacked banner: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] tool call: fs__list_directory"),
        "stderr lacked tool-call trace: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("[buildah] $"),
        "plain run should not print buildah command transcripts: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("[podman] $"),
        "plain run should not print podman command transcripts: {stderr_str}"
    );
    assert!(
        stdout_str.contains("listed the workspace"),
        "stdout lacked canned reply: {stdout_str}"
    );
    assert!(
        !session_dir.path().join("logs/container.log").exists(),
        "plain run should not create container.log"
    );

    // Verify our specific container was cleaned up (other tests / external
    // processes may have unrelated outrig-* containers running, so we only
    // check the session-id captured from this run's banner).
    let session_line = stderr_str
        .lines()
        .find(|l| l.contains("[outrig] container started:"))
        .expect("banner must include `container started` line");
    let our_container = session_line
        .split("started:")
        .nth(1)
        .expect("container name after `started:`")
        .trim();
    let ps = Command::new("podman")
        .args(["ps", "-a", "--filter"])
        .arg(format!("name={our_container}"))
        .args(["--format", "{{.Names}}"])
        .output()
        .await
        .expect("podman ps");
    let leftovers = String::from_utf8_lossy(&ps.stdout);
    assert!(
        leftovers.trim().is_empty(),
        "this run's container `{our_container}` is still alive: {leftovers}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verbose_run_writes_container_log_and_enables_trace() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let mock_addr = listener.local_addr().expect("mock addr");
    let server_handle = tokio::spawn(run_mock_openai(listener));

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_smoke_config(repo_dir.path(), &mock_addr.to_string());

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let sessions_s = sessions.path().to_str().expect("sessions path utf-8");
    let session_dir_s = session_dir.path().to_str().expect("session dir path utf-8");

    let captured = run_child(
        &[
            "--session-root",
            sessions_s,
            "run",
            "--session-dir",
            session_dir_s,
            "-vv",
        ],
        repo_dir.path(),
    )
    .await;
    server_handle.abort();

    assert!(
        captured.status.success(),
        "outrig run -vv exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr
    );
    assert!(
        captured.stdout.contains("listed the workspace"),
        "stdout lacked canned reply: {}",
        captured.stdout
    );
    assert!(
        captured.stderr.contains("verbose tracing enabled"),
        "-vv should enable trace-level outrig logs: {}",
        captured.stderr
    );
    assert!(
        captured.stderr.contains("[buildah] $ buildah images"),
        "stderr lacked buildah command transcript: {}",
        captured.stderr
    );
    assert!(
        captured.stderr.contains("[podman] $ podman run"),
        "stderr lacked podman run transcript: {}",
        captured.stderr
    );

    let log_path = session_dir.path().join("logs/container.log");
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", log_path.display()));
    assert!(
        log.contains("[buildah] $ buildah images"),
        "container.log lacked buildah probe: {log}"
    );
    assert!(
        log.contains("[podman] $ podman run"),
        "container.log lacked podman run: {log}"
    );
    assert!(
        log.contains("[podman] $ podman exec"),
        "container.log lacked podman exec: {log}"
    );

    let session_line = captured
        .stderr
        .lines()
        .find(|l| l.contains("[outrig] container started:"))
        .expect("banner must include `container started` line");
    let our_container = session_line
        .split("started:")
        .nth(1)
        .expect("container name after `started:`")
        .trim();
    let ps = Command::new("podman")
        .args(["ps", "-a", "--filter"])
        .arg(format!("name={our_container}"))
        .args(["--format", "{{.Names}}"])
        .output()
        .await
        .expect("podman ps");
    let leftovers = String::from_utf8_lossy(&ps.stdout);
    assert!(
        leftovers.trim().is_empty(),
        "this run's container `{our_container}` is still alive: {leftovers}"
    );
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_child(args: &[&str], repo: &Path) -> Captured {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(repo)
        .env("OUTRIG_TEST_KEY", "test-key")
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig");

    let mut stdin = child.stdin.take().expect("stdin piped");
    stdin.write_all(b"hello\n").await.expect("write prompt");
    stdin.flush().await.expect("flush stdin");
    drop(stdin);

    let output = timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    eprintln!("--- subprocess stderr ---\n{stderr}");
    eprintln!("--- subprocess stdout ---\n{stdout}");
    Captured {
        status: output.status,
        stdout,
        stderr,
    }
}

/// Hand-rolled mock OpenAI server. Reads HTTP/1.1 requests, parses
/// Content-Length, and returns canned chat-completions responses. The first
/// request is answered with a tool-call; the second with a final text reply.
async fn run_mock_openai(listener: TcpListener) {
    let mut request_count = 0u32;
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        request_count += 1;
        let body = if request_count == 1 {
            json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 0,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "fs__list_directory",
                                "arguments": "{\"path\":\"/workspace\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
        } else {
            json!({
                "id": "chatcmpl-2",
                "object": "chat.completion",
                "created": 0,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "I listed the workspace."
                    },
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
        };
        let _ = drain_request(&mut sock).await;
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body_str.len(),
            body_str
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

/// Read the request headers + body out of the socket so the client doesn't
/// stall waiting for us to consume its payload. Returns the JSON body if it
/// parsed (unused here, but a useful debugging hook).
async fn drain_request(sock: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buf = vec![0u8; 8192];
    let mut total = Vec::new();
    let mut content_length: usize = 0;

    let header_end = loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        total.extend_from_slice(&buf[..n]);
        if let Some(idx) = find_subseq(&total, b"\r\n\r\n") {
            break idx + 4;
        }
        if total.len() > 1 << 20 {
            return None;
        }
    };
    let header_buf = String::from_utf8_lossy(&total[..header_end]).into_owned();
    for line in header_buf.lines() {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    while total.len() < header_end + content_length {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        total.extend_from_slice(&buf[..n]);
    }
    let body = &total[header_end..(header_end + content_length).min(total.len())];
    serde_json::from_slice(body).ok()
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
