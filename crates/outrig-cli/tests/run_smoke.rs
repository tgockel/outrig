//! End-to-end smoke for `outrig run`. Gated behind `--features e2e`.
//!
//! Sets up a fixture repo in a tempdir, plus a global config under a private
//! `XDG_CONFIG_HOME` whose OpenAI provider points at a local hand-rolled mock
//! server (a repo config may not declare a keyed provider), then runs the
//! `outrig` binary as a subprocess, pipes one prompt to stdin, and asserts on
//! captured stdout/stderr.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test run_smoke -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

mod common;
use common::{
    CannedResponse, E2E_TIMEOUT, fixture_mcp_fs_dir, next_recorded, podman_names, start_mock_http,
    stream_lines, wait_for_stderr_value,
};

fn write_smoke_config(repo: &Path, mock_addr: &str) {
    write_config(
        repo,
        mock_addr,
        r#"
default-agent = "smoke"
"#,
        r#"
[agents.smoke]
model = "fast"
preamble = "test"
"#,
    );
}

/// The same fixture with no agent surface at all: no `default-agent`, no
/// `[agents]` table. `default-model` is what the session resolves against.
fn write_agentless_config(repo: &Path, mock_addr: &str) {
    write_config(
        repo,
        mock_addr,
        r#"
default-model = "fast"
"#,
        "",
    );
}

/// Where the fixture's global config lives. `run_child` points
/// `XDG_CONFIG_HOME` here, so the provider and its model are read from the
/// operator's side of the merge -- a repo config may not declare a provider
/// that carries an `api-key` -- and the developer's real global config is
/// never read.
fn xdg_config_home(repo: &Path) -> PathBuf {
    repo.join("xdg")
}

/// `top` lands with the other scalar keys, above every table; `agents` lands
/// at the bottom, after the image-config. The provider and the model it serves
/// go to the global config under [`xdg_config_home`].
fn write_config(repo: &Path, mock_addr: &str, top: &str, agents: &str) {
    write_global_config(repo, mock_addr);

    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = fixture_mcp_fs_dir().join("Dockerfile");
    let context = fixture_mcp_fs_dir();
    let config_toml = format!(
        r#"
default-image = "smoke"
{top}

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

  [images.smoke.mcp]
  fs = ["mcp-server-filesystem", "/workspace"]
{agents}
"#,
        dockerfile = dockerfile.display(),
        context = context.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
}

/// The provider and the model every fixture resolves against, served by the
/// mock at `mock_addr`, in the global config under [`xdg_config_home`].
fn write_global_config(repo: &Path, mock_addr: &str) {
    let global_dir = xdg_config_home(repo).join("outrig");
    std::fs::create_dir_all(&global_dir).expect("mkdir xdg/outrig");
    let global_toml = format!(
        r#"
[providers.openai]
style = "openai"
base-url = "http://{addr}/v1"
api-key = "${{OUTRIG_TEST_KEY}}"
request-timeout-secs = 10

[models.fast]
provider = "openai"
identifier = "gpt-4o-mini"
"#,
        addr = mock_addr,
    );
    std::fs::write(global_dir.join("config.toml"), global_toml).expect("write global config");
}

/// A repo whose image the content-hash cache has never seen, so the session
/// has to build it: `dockerfile` is written under the repo and should carry
/// [`miss_nonce`]. No MCP table, so the REPL opens with nothing to connect,
/// and no agent, as [`write_agentless_config`] has none.
fn write_miss_config(repo: &Path, mock_addr: &str, dockerfile: &str) {
    write_global_config(repo, mock_addr);

    let image_dir = repo.join("image");
    std::fs::create_dir_all(&image_dir).expect("mkdir image");
    std::fs::write(image_dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    let config_toml = format!(
        r#"
default-image = "smoke-miss"
default-model = "fast"

[images.smoke-miss]
dockerfile = "{dockerfile}"
context = "{context}"
"#,
        dockerfile = image_dir.join("Dockerfile").display(),
        context = image_dir.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
}

/// What makes a miss fixture a miss: the repo tempdir's name, which no earlier
/// run of this binary can have used, reduced to what a shell word is happy
/// with.
fn miss_nonce(repo: &Path) -> String {
    repo.file_name()
        .and_then(|name| name.to_str())
        .expect("tempdir name is UTF-8")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// A final chat-completions reply saying `content`, for a session with no
/// tools to call.
fn plain_reply(content: &str) -> Value {
    json!({
        "id": "chatcmpl-plain",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// The image a cache-miss run built, removed on drop so a failed assertion
/// cannot leave it behind. Best-effort, and blocking because `Drop` is.
struct BuiltImage(Option<String>);

impl BuiltImage {
    /// The tag off the `[outrig] image ready: <tag> (built) (..)` line, if the
    /// run got that far.
    fn from_stderr(stderr: &str) -> Self {
        let tag = stderr
            .lines()
            .find_map(|line| line.split("[outrig] image ready: ").nth(1))
            .and_then(|rest| rest.split(' ').next())
            .map(str::to_string);
        Self(tag)
    }
}

impl Drop for BuiltImage {
    fn drop(&mut self) {
        if let Some(tag) = &self.0 {
            let _ = std::process::Command::new("buildah")
                .args(["rmi", tag])
                .output();
        }
    }
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
    assert_stderr_lines_in_order(
        &stderr_str,
        &[
            "[outrig] loading config",
            "[outrig] config loaded",
            "[outrig] resolving agent and container",
            "[outrig] agent/container resolved",
            "[outrig] computing image tag",
            "[outrig] image tag computed",
            "[outrig] ensuring image",
            "[outrig] image ready:",
            "[outrig] starting container",
            "[outrig] container ready:",
            "[outrig] bootstrapping container user",
            "[outrig] container user ready",
            "[outrig] MCP fs: initializing",
            "[outrig] MCP fs: initialized",
            "[outrig] MCP fs: listing tools",
            "[outrig] MCP fs: tools ready",
            "[outrig] building agent",
            "[outrig] agent ready",
            "[outrig] agent:",
            "[outrig] base-url:          http://127.0.0.1:",
            "[outrig] entering REPL",
        ],
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
    assert!(
        !session_dir.path().join("logs/network.jsonl").exists(),
        "network audit is opt-in and should be absent on a plain run"
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

/// `outrig run` with no agent anywhere in config: it starts, the banner leads
/// with the model instead of an agent name, and the request carries no
/// `system` message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_without_an_agent_sends_no_preamble() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let mock_addr = listener.local_addr().expect("mock addr");
    let (request_tx, mut request_rx) = mpsc::unbounded_channel();
    let server_handle = tokio::spawn(run_mock_openai_capturing(listener, Some(request_tx)));

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_agentless_config(repo_dir.path(), &mock_addr.to_string());

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
        ],
        repo_dir.path(),
    )
    .await;
    server_handle.abort();

    assert!(
        captured.status.success(),
        "agentless run exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr,
    );
    assert!(
        captured.stderr.contains("[outrig] model:"),
        "banner should lead with the model when no agent is configured: {}",
        captured.stderr,
    );
    assert!(
        !captured.stderr.contains("[outrig] agent:"),
        "banner should not print an agent line when none is configured: {}",
        captured.stderr,
    );
    assert!(
        captured.stdout.contains("listed the workspace"),
        "stdout lacked canned reply: {}",
        captured.stdout,
    );

    let request = timeout(Duration::from_secs(5), request_rx.recv())
        .await
        .expect("mock server did not capture the first request")
        .expect("mock server request channel closed");
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .expect("first request has messages array");
    assert!(
        !messages
            .iter()
            .any(|m| m.get("role").and_then(Value::as_str) == Some("system")),
        "an agentless session must send no system message; got: {}",
        serde_json::to_string(messages).expect("messages serialize"),
    );
}

/// A cache miss shows its build as it runs. Both of buildah's streams reach
/// stderr -- a `RUN`'s stdout and its stderr alike -- as `[buildah]` lines
/// between `ensuring image` and `image ready`, each once, with no command
/// echo and no `container.log`, which stay behind `--verbose`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_miss_shows_the_build_on_a_plain_run() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let (mock_addr, _requests) =
        start_mock_http(vec![CannedResponse::ok(plain_reply("built fine"))]).await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let nonce = miss_nonce(repo_dir.path());
    write_miss_config(
        repo_dir.path(),
        &mock_addr.to_string(),
        &format!(
            "FROM docker.io/library/alpine:latest\n\
             # cache-bust: {nonce}\n\
             RUN echo outrig-e2e-miss-{nonce}\n\
             RUN echo outrig-e2e-miss-err-{nonce} 1>&2\n"
        ),
    );

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
        ],
        repo_dir.path(),
    )
    .await;
    let _built = BuiltImage::from_stderr(&captured.stderr);

    assert!(
        captured.status.success(),
        "outrig run exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr
    );
    assert_stderr_lines_in_order(
        &captured.stderr,
        &[
            "[outrig] ensuring image smoke-miss:",
            "[buildah] STEP 1/",
            &format!("[buildah] outrig-e2e-miss-{nonce}"),
            &format!("[buildah] outrig-e2e-miss-err-{nonce}"),
            "[outrig] image ready: smoke-miss:",
            "[outrig] starting container",
        ],
    );
    let ready = captured
        .stderr
        .lines()
        .find(|line| line.contains("[outrig] image ready:"))
        .expect("image ready line");
    assert!(
        ready.contains("(built)"),
        "a miss is reported as built: {ready}"
    );
    assert_eq!(
        captured.stderr.matches("[buildah] STEP 1/").count(),
        1,
        "each build line is shown once: {}",
        captured.stderr
    );
    assert!(
        !captured.stderr.contains("[buildah] $"),
        "the command echo stays behind --verbose: {}",
        captured.stderr
    );
    assert!(
        !captured.stderr.contains("[podman] $"),
        "the command echo stays behind --verbose: {}",
        captured.stderr
    );
    assert!(
        !session_dir.path().join("logs/container.log").exists(),
        "a plain run writes no container.log"
    );
    assert!(
        captured.stdout.contains("built fine"),
        "stdout lacked canned reply: {}",
        captured.stdout
    );
}

/// Under `--verbose` the transcript already mirrors every line to stderr, so
/// the shown build adds nothing: each line once, plus the command echo, and
/// all of it in `container.log`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_miss_under_verbose_prints_each_build_line_once() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let (mock_addr, _requests) =
        start_mock_http(vec![CannedResponse::ok(plain_reply("built fine"))]).await;
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let nonce = miss_nonce(repo_dir.path());
    write_miss_config(
        repo_dir.path(),
        &mock_addr.to_string(),
        &format!(
            "FROM docker.io/library/alpine:latest\n\
             # cache-bust: {nonce}\n\
             RUN echo outrig-e2e-miss-{nonce}\n"
        ),
    );

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
            "-v",
        ],
        repo_dir.path(),
    )
    .await;
    let _built = BuiltImage::from_stderr(&captured.stderr);

    assert!(
        captured.status.success(),
        "outrig run -v exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr
    );
    assert_eq!(
        captured.stderr.matches("[buildah] STEP 1/").count(),
        1,
        "a mirrored transcript is not doubled by the shown build: {}",
        captured.stderr
    );
    assert_eq!(
        captured
            .stderr
            .matches(&format!("[buildah] outrig-e2e-miss-{nonce}"))
            .count(),
        1,
        "a mirrored transcript is not doubled by the shown build: {}",
        captured.stderr
    );
    assert!(
        captured.stderr.contains("[buildah] $ buildah build"),
        "--verbose adds the command echo: {}",
        captured.stderr
    );

    let log_path = session_dir.path().join("logs/container.log");
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", log_path.display()));
    assert!(
        log.contains("[buildah] $ buildah build"),
        "container.log lacked the build command: {log}"
    );
    assert!(
        log.contains("[buildah] STEP 1/"),
        "container.log lacked the build output: {log}"
    );
}

/// A failing build's lines were on stderr as they arrived, so the error that
/// ends the session carries no tail to repeat them with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_build_on_a_plain_run_does_not_repeat_its_output() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let nonce = miss_nonce(repo_dir.path());
    // The model is never reached; any closed port will do.
    write_miss_config(
        repo_dir.path(),
        "127.0.0.1:1",
        &format!(
            "FROM docker.io/library/alpine:latest\n\
             # cache-bust: {nonce}\n\
             RUN false\n"
        ),
    );

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
        ],
        repo_dir.path(),
    )
    .await;

    assert!(
        !captured.status.success(),
        "a failed build must fail the run; stderr was: {}",
        captured.stderr
    );
    assert_eq!(
        captured
            .stderr
            .matches("[buildah] STEP 2/2: RUN false")
            .count(),
        1,
        "the failing step is shown once: {}",
        captured.stderr
    );
    assert!(
        captured
            .stderr
            .contains("error: process `buildah` exited with code 1"),
        "the error names the exit: {}",
        captured.stderr
    );
    assert_eq!(
        captured
            .stderr
            .matches("building at STEP \"RUN false\"")
            .count(),
        1,
        "buildah's own error line is shown once and not repeated in the error's tail: {}",
        captured.stderr
    );
    for absent in [
        "[outrig] image ready:",
        "[outrig] starting container",
        "[buildah] $",
    ] {
        assert!(
            !captured.stderr.contains(absent),
            "{absent:?} must not appear: {}",
            captured.stderr
        );
    }
    assert!(
        !session_dir.path().join("logs/container.log").exists(),
        "a plain run writes no container.log"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_tool_calls_retains_partial_history_for_continue() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let mock_addr = listener.local_addr().expect("mock addr");
    let (request_tx, mut request_rx) = mpsc::unbounded_channel();
    // A second tool call, which the max refuses.
    let second = (
        200,
        json!({
            "id": "chatcmpl-2",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_2",
                        "type": "function",
                        "function": {
                            "name": "fs__read_text_file",
                            "arguments": "{\"path\":\"/workspace/README.md\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }),
    );
    let server_handle = tokio::spawn(run_mock_openai_resume(listener, request_tx, second));

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_smoke_config(repo_dir.path(), &mock_addr.to_string());

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let sessions_s = sessions.path().to_str().expect("sessions path utf-8");
    let session_dir_s = session_dir.path().to_str().expect("session dir path utf-8");

    let captured = run_child_with_input(
        &[
            "--session-root",
            sessions_s,
            "run",
            "--session-dir",
            session_dir_s,
            "--max-tool-calls",
            "1",
        ],
        repo_dir.path(),
        b"start\ncontinue\n",
    )
    .await;
    server_handle.abort();

    let mut requests = Vec::new();
    for i in 0..3 {
        let req = timeout(Duration::from_secs(5), request_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("mock server did not capture request {}", i + 1))
            .expect("mock server request channel closed");
        requests.push(req);
    }

    assert!(
        captured.status.success(),
        "outrig run exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr
    );
    assert!(
        captured
            .stderr
            .contains("[outrig] tool-call iteration max (1) reached; ending turn"),
        "stderr lacked max message: {}",
        captured.stderr
    );
    assert!(
        captured
            .stderr
            .contains("[outrig] partial history retained -- send another prompt"),
        "stderr lacked continuation hint: {}",
        captured.stderr
    );
    assert!(
        captured
            .stdout
            .contains("(turn ended: tool-call iteration max (1) reached)"),
        "stdout lacked the ended-turn reply: {}",
        captured.stdout
    );
    assert!(
        captured.stdout.contains("continued from retained history"),
        "stdout lacked continuation reply: {}",
        captured.stdout
    );

    let third_messages = requests[2]
        .get("messages")
        .and_then(Value::as_array)
        .expect("third request has messages array");
    let third_messages_json = serde_json::to_string(third_messages).expect("messages serialize");
    assert!(
        third_messages_json.contains("fs__list_directory"),
        "third request lacked prior completed tool call: {third_messages_json}",
    );

    let cancelled_tool_call_index =
        assistant_tool_call_index(third_messages, "call_2", "fs__read_text_file").unwrap_or_else(
            || panic!("third request lacked cancelled tool call: {third_messages_json}"),
        );
    let synthetic_tool_result_index = cancelled_tool_call_index + 1;
    let synthetic_tool_result = third_messages
        .get(synthetic_tool_result_index)
        .unwrap_or_else(|| panic!("cancelled tool call had no following message"));
    assert!(
        synthetic_tool_result.get("role").and_then(Value::as_str) == Some("tool"),
        "cancelled tool call was not followed by a tool result: {third_messages_json}",
    );
    assert!(
        synthetic_tool_result
            .get("tool_call_id")
            .and_then(Value::as_str)
            == Some("call_2"),
        "synthetic tool result did not match cancelled tool call: {third_messages_json}",
    );
    assert!(
        synthetic_tool_result
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| content.contains("per-turn tool-call max (1)")),
        "synthetic tool result lacked max marker: {third_messages_json}",
    );

    let continue_index = third_messages
        .iter()
        .position(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && json_contains_str(message, "continue")
        })
        .unwrap_or_else(|| panic!("third request lacked follow-up prompt: {third_messages_json}"));
    assert!(
        continue_index > synthetic_tool_result_index,
        "follow-up prompt did not come after synthetic tool result: {third_messages_json}",
    );
}

/// A model call that fails for good after a tool call ran keeps that call, and
/// says so: the advice is to continue, not to send the prompt again, which
/// would run the tool call a second time (#197).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_failure_after_a_tool_call_retains_it_for_continue() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let mock_addr = listener.local_addr().expect("mock addr");
    let (request_tx, mut request_rx) = mpsc::unbounded_channel();
    let rate_limited = (
        429,
        json!({"error": {"message": "slow down", "type": "rate_limit_error"}}),
    );
    let server_handle = tokio::spawn(run_mock_openai_resume(listener, request_tx, rate_limited));

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    // Retries off, so the 429 ends the turn at once.
    write_config(
        repo_dir.path(),
        &mock_addr.to_string(),
        r#"
default-agent = "smoke"
retry-budget-secs = 0
"#,
        r#"
[agents.smoke]
model = "fast"
preamble = "test"
"#,
    );

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let sessions_s = sessions.path().to_str().expect("sessions path utf-8");
    let session_dir_s = session_dir.path().to_str().expect("session dir path utf-8");

    let captured = run_child_with_input(
        &[
            "--session-root",
            sessions_s,
            "run",
            "--session-dir",
            session_dir_s,
        ],
        repo_dir.path(),
        b"start\ncontinue\n",
    )
    .await;
    server_handle.abort();

    let mut requests = Vec::new();
    for i in 0..3 {
        let req = timeout(Duration::from_secs(5), request_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("mock server did not capture request {}", i + 1))
            .expect("mock server request channel closed");
        requests.push(req);
    }

    assert!(
        captured.status.success(),
        "outrig run exited with {:?}; stderr was: {}",
        captured.status,
        captured.stderr
    );
    assert!(
        captured
            .stderr
            .contains("[outrig] LLM endpoint failed and did not recover (HTTP 429"),
        "stderr lacked the failure: {}",
        captured.stderr
    );
    assert!(
        captured
            .stderr
            .contains("[outrig] partial history retained -- send another prompt"),
        "stderr lacked the advice to continue: {}",
        captured.stderr
    );
    assert!(
        !captured.stderr.contains("history unchanged"),
        "advising a resend would run the tool call again: {}",
        captured.stderr
    );
    assert!(
        captured.stdout.contains("continued from retained history"),
        "stdout lacked continuation reply: {}",
        captured.stdout
    );

    let third_messages = requests[2]
        .get("messages")
        .and_then(Value::as_array)
        .expect("third request has messages array");
    let third_messages_json = serde_json::to_string(third_messages).expect("messages serialize");
    let tool_call_index = assistant_tool_call_index(third_messages, "call_1", "fs__list_directory")
        .unwrap_or_else(|| {
            panic!("third request lacked the tool call that ran: {third_messages_json}")
        });
    let tool_result = third_messages
        .get(tool_call_index + 1)
        .unwrap_or_else(|| panic!("the tool call had no following message: {third_messages_json}"));
    assert!(
        tool_result.get("role").and_then(Value::as_str) == Some("tool")
            && tool_result.get("tool_call_id").and_then(Value::as_str) == Some("call_1"),
        "the tool call was not answered by its result: {third_messages_json}",
    );
    let continue_index = third_messages
        .iter()
        .position(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && json_contains_str(message, "continue")
        })
        .unwrap_or_else(|| panic!("third request lacked follow-up prompt: {third_messages_json}"));
    assert!(
        continue_index > tool_call_index + 1,
        "follow-up prompt did not come after the kept tool result: {third_messages_json}",
    );
}

/// A Ctrl-C mid-turn abandons the turn and nothing else: the next prompt's
/// tool call still reaches the MCP server (#335). The `SIGINT` goes to the
/// whole process group, as a terminal sends it. That used to reach every
/// `podman exec` transport too, and each tool call after it failed with
/// `Transport closed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_sigint_to_the_group_mid_turn_keeps_the_mcp_transports() {
    common::init_tracing();

    let (mock_addr, mut requests) = start_mock_http(vec![
        CannedResponse::held(),
        list_workspace_call(),
        text_reply("I listed the workspace."),
    ])
    .await;

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_smoke_config(repo_dir.path(), &mock_addr.to_string());
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let session_dir = tempfile::tempdir().expect("tempdir session");

    // A group of its own, as a shell gives a foreground job, so the signal
    // below reaches outrig and what shares its group, and not this test.
    let mut child = Command::new(env!("CARGO_BIN_EXE_outrig"))
        .arg("--session-root")
        .arg(sessions.path())
        .arg("run")
        .arg("--session-dir")
        .arg(session_dir.path())
        .current_dir(repo_dir.path())
        .env("OUTRIG_TEST_KEY", "test-key")
        .env("OUTRIG_LOG", "info")
        .env("XDG_CONFIG_HOME", xdg_config_home(repo_dir.path()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig");
    let group = Pid::from_raw(child.id().expect("outrig is running") as i32);
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = Arc::new(Mutex::new(String::new()));
    let stderr = Arc::new(Mutex::new(String::new()));
    let stdout_task = tokio::spawn(stream_lines(
        child.stdout.take().expect("stdout piped"),
        stdout.clone(),
        "stdout",
    ));
    let stderr_task = tokio::spawn(stream_lines(
        child.stderr.take().expect("stderr piped"),
        stderr.clone(),
        "stderr",
    ));

    let container = wait_for_stderr_value(stderr.clone(), "[outrig] container started:").await;
    wait_for_stderr_value(stderr.clone(), "[outrig] entering REPL").await;
    stdin
        .write_all(b"take your time\n")
        .await
        .expect("write prompt");
    // The model call is in flight, and the mock never answers it.
    next_recorded(&mut requests).await;

    killpg(group, Signal::SIGINT).expect("signal outrig's group");
    wait_for_stderr_value(stderr.clone(), "[outrig] interrupted").await;

    stdin
        .write_all(b"list the workspace\n")
        .await
        .expect("write prompt");
    next_recorded(&mut requests).await;
    let with_result = next_recorded(&mut requests).await;
    let messages = with_result.messages();
    let messages_json = serde_json::to_string(messages).expect("messages serialize");
    let result = messages
        .iter()
        .find(|message| {
            message.get("role").and_then(Value::as_str) == Some("tool")
                && message.get("tool_call_id").and_then(Value::as_str) == Some("call_1")
        })
        .unwrap_or_else(|| panic!("no result for call_1: {messages_json}"));
    assert!(
        !json_contains_str(result, "Transport closed"),
        "the transport did not survive the Ctrl-C: {messages_json}"
    );
    assert!(
        json_contains_str(result, ".agents"),
        "the result is not the workspace listing: {messages_json}"
    );
    wait_for_stderr_value(stdout.clone(), "I listed the workspace.").await;

    // EOF ends the session the way Ctrl-D does.
    drop(stdin);
    let status = timeout(E2E_TIMEOUT, child.wait())
        .await
        .expect("outrig exited in time")
        .expect("wait for outrig");
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    assert!(
        status.success(),
        "outrig run exited with {status:?}; stderr: {}",
        stderr.lock().unwrap()
    );
    let left = podman_names(&format!("name={container}")).await;
    assert!(left.is_empty(), "container still exists: {left:?}");
}

/// A completion asking for `fs__list_directory` on `/workspace`, as `call_1`.
fn list_workspace_call() -> CannedResponse {
    completion(
        json!({
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
        }),
        "tool_calls",
    )
}

/// A completion that ends the turn with `text`.
fn text_reply(text: &str) -> CannedResponse {
    completion(json!({ "role": "assistant", "content": text }), "stop")
}

fn completion(message: Value, finish_reason: &str) -> CannedResponse {
    CannedResponse::ok(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    }))
}

fn assistant_tool_call_index(messages: &[Value], call_id: &str, tool_name: &str) -> Option<usize> {
    messages.iter().position(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|tool_calls| {
                    tool_calls.iter().any(|tool_call| {
                        tool_call.get("id").and_then(Value::as_str) == Some(call_id)
                            && tool_call
                                .get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                                == Some(tool_name)
                    })
                })
    })
}

fn json_contains_str(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(s) => s.contains(needle),
        Value::Array(items) => items.iter().any(|item| json_contains_str(item, needle)),
        Value::Object(map) => map.values().any(|item| json_contains_str(item, needle)),
        _ => false,
    }
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn assert_stderr_lines_in_order(stderr: &str, needles: &[&str]) {
    let mut offset = 0;
    for needle in needles {
        let haystack = &stderr[offset..];
        let Some(pos) = haystack.find(needle) else {
            panic!("stderr lacked ordered startup line {needle:?}: {stderr}");
        };
        offset += pos + needle.len();
    }
}

async fn run_child(args: &[&str], repo: &Path) -> Captured {
    run_child_with_input(args, repo, b"hello\n").await
}

async fn run_child_with_input(args: &[&str], repo: &Path, input: &[u8]) -> Captured {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(repo)
        .env("OUTRIG_TEST_KEY", "test-key")
        .env("OUTRIG_LOG", "info")
        .env("XDG_CONFIG_HOME", xdg_config_home(repo))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig");

    let mut stdin = child.stdin.take().expect("stdin piped");
    stdin.write_all(input).await.expect("write prompt");
    stdin.flush().await.expect("flush stdin");
    drop(stdin);

    let output = timeout(E2E_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {E2E_TIMEOUT:?}"))
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

async fn run_mock_openai(listener: TcpListener) {
    run_mock_openai_capturing(listener, None).await
}

/// Hand-rolled mock OpenAI server. Reads HTTP/1.1 requests, parses
/// Content-Length, and returns canned chat-completions responses. The first
/// request is answered with a tool-call; the second with a final text reply.
///
/// `request_tx`, when given, forwards each parsed request body so a test can
/// assert on what outrig put on the wire.
async fn run_mock_openai_capturing(
    listener: TcpListener,
    request_tx: Option<mpsc::UnboundedSender<Value>>,
) {
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
        let request = drain_request(&mut sock).await.unwrap_or(Value::Null);
        if let Some(tx) = &request_tx {
            let _ = tx.send(request);
        }
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

/// A turn that ends early, then a `continue` that resumes it. The first request
/// is answered with a tool call and every one after the second with a final
/// reply; `second`, a status and a body, is what ends the turn early.
async fn run_mock_openai_resume(
    listener: TcpListener,
    request_tx: mpsc::UnboundedSender<Value>,
    second: (u16, Value),
) {
    let mut request_count = 0u32;
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            return;
        };
        request_count += 1;
        let request = drain_request(&mut sock).await.unwrap_or(Value::Null);
        let _ = request_tx.send(request);
        let (status, body) = match request_count {
            1 => (
                200,
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
                }),
            ),
            2 => second.clone(),
            _ => (
                200,
                json!({
                    "id": "chatcmpl-3",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "gpt-4o-mini",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "I continued from retained history."
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }),
            ),
        };
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
