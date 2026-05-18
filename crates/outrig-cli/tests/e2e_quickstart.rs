//! End-to-end quickstart test: `outrig init` -> `outrig build` -> `outrig run`
//! against a tempdir-rooted HOME and an empty repo. Gated behind `--features e2e`.
//!
//! Two variants share an inner driver:
//!
//! - `quickstart_mocked` always runs. After `outrig init` writes the global
//!   config, the test surgically patches `[providers.openai].base-url` to
//!   point at a hand-rolled mock OpenAI server (the same one `run_smoke.rs`
//!   uses). Deterministic, free, fast — the always-on signal that the
//!   binary's piped-stdin / build / run paths agree end-to-end.
//! - `quickstart_real_api` is opt-in via `OUTRIG_E2E_REAL_API=1`. Skips the
//!   patch and runs against the real OpenAI endpoint, retrying the run leg
//!   up to 2x to defend against LLM non-determinism.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e quickstart_mocked -- --nocapture
//! OUTRIG_E2E_REAL_API=1 OPENAI_API_KEY=... \
//!   cargo test --features e2e quickstart_real_api -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::timeout;
use toml_edit::{DocumentMut, value};

use outrig::config::Config;

const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

// 26 newlines: every prompt of the all-defaults init walk takes its default.
// See tests/init_scripted.rs:41 for the breakdown.
const INIT_DEFAULTS: &[u8] = b"\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n";

// Pre-seeded into the workspace so we can assert it round-trips through the
// MCP filesystem listing into the assistant's stdout reply.
const MARKER: &str = "outrig_e2e_marker_8a2f.txt";

const PROMPT_TEXT: &[u8] = b"Use the filesystem MCP tool to list every file under /workspace. \
      Then echo each file's relative path on its own line.\n";

#[derive(Copy, Clone)]
enum Backend {
    Mocked,
    RealApi,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quickstart_mocked() {
    run_quickstart(Backend::Mocked).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quickstart_real_api() {
    if std::env::var("OUTRIG_E2E_REAL_API").ok().as_deref() != Some("1") {
        eprintln!(
            "[outrig-e2e] skipping quickstart_real_api (set OUTRIG_E2E_REAL_API=1 to enable)"
        );
        return;
    }
    run_quickstart(Backend::RealApi).await;
}

async fn run_quickstart(backend: Backend) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&home).expect("mkdir home");
    std::fs::create_dir_all(&repo).expect("mkdir repo");

    // `git init` so the workspace is a real repo (matches the doc/quickstart.md
    // story). `outrig` itself doesn't need git, so its absence wouldn't fail
    // the test, but the v0 ship gate is "from an empty git repo".
    let git = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo)
        .status()
        .expect("invoke git init");
    assert!(git.success(), "git init failed: {git:?}");

    std::fs::write(repo.join(MARKER), b"hello\n").expect("seed marker");

    let (api_key, mock) = match backend {
        Backend::Mocked => {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
            let addr = listener.local_addr().expect("mock addr");
            let handle = tokio::spawn(run_mock_openai(listener));
            ("test-key".to_string(), Some((addr, handle)))
        }
        Backend::RealApi => {
            let key = std::env::var("OPENAI_API_KEY").expect(
                "OUTRIG_E2E_REAL_API=1 set without OPENAI_API_KEY -- export a key or unset the gate",
            );
            (key, None)
        }
    };

    // ---- 1. outrig init -------------------------------------------------
    let init = spawn_outrig(
        &["init"],
        &home,
        &repo,
        &api_key,
        Some(INIT_DEFAULTS),
        INIT_TIMEOUT,
        "init",
    )
    .await;
    assert!(
        init.status.success(),
        "outrig init exited with {:?}",
        init.status
    );

    let global_cfg_path = home.join(".outrig/config.toml");
    let global_text = std::fs::read_to_string(&global_cfg_path)
        .unwrap_or_else(|e| panic!("global config {global_cfg_path:?} not written: {e}"));
    Config::load_from_str(&global_text)
        .expect("global config parses")
        .validate(None)
        .expect("global config validates");

    let repo_cfg_path = repo.join(".agents/outrig/config.toml");
    assert!(
        repo_cfg_path.is_file(),
        "repo config {repo_cfg_path:?} not written"
    );

    // ---- 2. mock-only patch: redirect base-url at our test server -------
    if let Some((addr, _)) = &mock {
        let patched = patch_global_for_mock(&global_text, &addr.to_string());
        std::fs::write(&global_cfg_path, &patched).expect("rewrite global for mock");
        // Re-validate so a botched edit fails the test here, not deep inside run.
        Config::load_from_str(&patched)
            .expect("patched global parses")
            .validate(None)
            .expect("patched global validates");
    }

    // ---- 3. outrig build ------------------------------------------------
    let build = spawn_outrig(
        &["build"],
        &home,
        &repo,
        &api_key,
        None,
        BUILD_TIMEOUT,
        "build",
    )
    .await;
    assert!(
        build.status.success(),
        "outrig build exited with {:?}",
        build.status
    );

    // ---- 4. outrig run (with retry on real-api) -------------------------
    let attempts = match backend {
        Backend::Mocked => 1,
        Backend::RealApi => 2,
    };
    let mut last_failure: Option<String> = None;
    for attempt in 1..=attempts {
        match try_run_leg(&home, &repo, &sessions, &api_key).await {
            Ok(()) => {
                last_failure = None;
                break;
            }
            Err(msg) => {
                eprintln!("[outrig-e2e] run attempt {attempt}/{attempts} failed: {msg}");
                last_failure = Some(msg);
                // Wipe sessions so the next attempt's "exactly one session"
                // check works.
                let _ = std::fs::remove_dir_all(&sessions);
            }
        }
    }
    if let Some(msg) = last_failure {
        panic!("outrig run failed after {attempts} attempt(s): {msg}");
    }

    // ---- 5. session record present --------------------------------------
    let session_count = std::fs::read_dir(&sessions).map(|d| d.count()).unwrap_or(0);
    assert!(
        session_count >= 1,
        "no session record written under {sessions:?}"
    );

    if let Some((_, handle)) = mock {
        handle.abort();
    }
}

/// One `outrig run` attempt. Returns `Err(reason)` on failure so the caller
/// can retry against the real API. Mock backend always passes on attempt 1.
async fn try_run_leg(
    home: &Path,
    repo: &Path,
    sessions: &Path,
    api_key: &str,
) -> Result<(), String> {
    let _ = std::fs::create_dir_all(sessions);
    let captured = spawn_outrig(
        &[
            "--session-root",
            sessions.to_str().expect("sessions path utf-8"),
            "run",
        ],
        home,
        repo,
        api_key,
        Some(PROMPT_TEXT),
        RUN_TIMEOUT,
        "run",
    )
    .await;

    if !captured.status.success() {
        return Err(format!(
            "exit {:?}; stderr was: {}",
            captured.status, captured.stderr
        ));
    }
    if !captured.stderr.contains("[outrig] tool call:") {
        return Err(format!(
            "stderr lacked `[outrig] tool call:` trace; full stderr:\n{}",
            captured.stderr
        ));
    }
    if !captured.stdout.contains(MARKER) {
        return Err(format!(
            "stdout lacked marker {MARKER:?}; full stdout:\n{}",
            captured.stdout
        ));
    }
    Ok(())
}

/// Captured output from one binary invocation.
struct Captured {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Spawn `outrig <args...>` with HOME redirected at `home`, cwd at `repo`,
/// `OPENAI_API_KEY` set to `api_key`, and XDG vars cleared. Pipes `stdin`
/// (if any) and closes it. Streams stdout/stderr into shared buffers so a
/// timeout-kill still has content to report.
async fn spawn_outrig(
    args: &[&str],
    home: &Path,
    repo: &Path,
    api_key: &str,
    stdin_bytes: Option<&[u8]>,
    wait_timeout: Duration,
    label: &'static str,
) -> Captured {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(repo)
        .env("HOME", home)
        .env("OPENAI_API_KEY", api_key)
        .env("OUTRIG_LOG", "info")
        // Make sure HOME-based resolution wins (`~/.outrig/config.toml`).
        // XDG_CONFIG_HOME would otherwise route the global config under
        // the user's real config dir on Linux.
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("spawn outrig {label}: {e}"));

    if let Some(bytes) = stdin_bytes {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(bytes)
            .await
            .unwrap_or_else(|e| panic!("write stdin to outrig {label}: {e}"));
        stdin
            .flush()
            .await
            .unwrap_or_else(|e| panic!("flush stdin to outrig {label}: {e}"));
        // Drop, not shutdown(): tokio's ChildStdin::poll_shutdown is a no-op
        // on Unix; only Drop closes the pipe and signals EOF to the child.
        drop(stdin);
    }

    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stdout_task = tokio::spawn(stream_lines(
        stdout_pipe,
        stdout_buf.clone(),
        label,
        "stdout",
    ));
    let stderr_task = tokio::spawn(stream_lines(
        stderr_pipe,
        stderr_buf.clone(),
        label,
        "stderr",
    ));

    let status = match timeout(wait_timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("outrig {label} wait failed: {e}"),
        Err(_) => {
            eprintln!(
                "--- outrig {label} stderr (timeout, before kill) ---\n{}",
                stderr_buf.lock().unwrap()
            );
            eprintln!(
                "--- outrig {label} stdout (timeout, before kill) ---\n{}",
                stdout_buf.lock().unwrap()
            );
            let _ = child.kill().await;
            panic!("outrig {label} did not exit within {wait_timeout:?}");
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let stdout = stdout_buf.lock().unwrap().clone();
    let stderr = stderr_buf.lock().unwrap().clone();
    eprintln!("--- outrig {label} stderr ---\n{stderr}");
    eprintln!("--- outrig {label} stdout ---\n{stdout}");
    Captured {
        status,
        stdout,
        stderr,
    }
}

async fn stream_lines<R>(
    reader: R,
    sink: Arc<Mutex<String>>,
    label: &'static str,
    kind: &'static str,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                eprintln!("[child {label}/{kind}] {}", line.trim_end_matches('\n'));
                sink.lock().unwrap().push_str(&line);
            }
            Err(_) => break,
        }
    }
}

/// Surgically rewrite `[providers.openai].base-url` to point at our mock
/// server. Leaves `api-key` alone (it's `${OPENAI_API_KEY}` from init, which
/// the spawned env resolves to whatever we pass; `Config::validate` requires
/// `${VAR}` form, so we can't replace it with a literal).
fn patch_global_for_mock(global_text: &str, mock_addr: &str) -> String {
    let mut doc: DocumentMut = global_text.parse().expect("global config parses as TOML");
    let providers = doc
        .get_mut("providers")
        .and_then(|p| p.as_table_mut())
        .expect("init-generated global must have [providers]");
    let openai = providers
        .get_mut("openai")
        .and_then(|p| p.as_table_mut())
        .expect("init-generated global must have [providers.openai]");
    openai.insert("base-url", value(format!("http://{mock_addr}/v1")));
    doc.to_string()
}

/// Hand-rolled mock OpenAI server: one canned tool-call, then a canned reply
/// that echoes MARKER. Mirrors `tests/run_smoke.rs:run_mock_openai`, with
/// the second response tuned to include the marker filename so the test's
/// stdout assertion finds it.
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
            // Echo the marker filename in the assistant's text reply so the
            // test's `stdout.contains(MARKER)` assertion passes regardless
            // of what the (real) MCP server actually returned to the agent.
            let content = format!("Workspace contents: {MARKER}");
            json!({
                "id": "chatcmpl-2",
                "object": "chat.completion",
                "created": 0,
                "model": "gpt-4o-mini",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": content },
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
