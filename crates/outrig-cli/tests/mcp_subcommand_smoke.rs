//! End-to-end smoke for `outrig mcp`. Gated behind `--features e2e`.
//!
//! Spawns the binary with `mcp` against a fixture repo whose config has one
//! backing MCP (`mcp-server-filesystem`) and *no* agent / providers /
//! models. Drives the spawned child as an MCP client over its stdio using
//! rmcp's [`serve_client`], then asserts on:
//!
//! - `tools/list` returns the namespaced union (`fs__*`) matching what
//!   `outrig run`'s banner reports.
//! - `tools/call fs__list_directory` returns the planted `HELLO.txt`.
//! - Closing the client side (drops stdin → child sees EOF) drives a clean
//!   teardown and exit-zero from the child.
//! - The child's container is reaped (no leftovers in `podman ps`).
//!
//! Run with:
//!
//! ```sh
//! cargo test -p outrig-cli --features e2e mcp_subcommand_smoke -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use outrig::config::{ContainerConfig, McpServerSpec};
use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::{self, ImageTag};
use outrig::mcp::McpClient;
use outrig_cli::session::{Session, SessionId, SessionStore};
use rmcp::model::CallToolRequestParams;
use rmcp::service::serve_client;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

mod common;
use common::stream_lines;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture_mcp_fs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("outrig-cli is under crates/")
        .join("outrig/tests/fixtures/mcp-fs")
}

fn fs_spec() -> McpServerSpec {
    McpServerSpec::Short(vec![
        "mcp-server-filesystem".to_string(),
        "/workspace".to_string(),
    ])
}

async fn ensure_fixture_image() -> ImageTag {
    let cfg = ContainerConfig {
        image_name: None,
        dockerfile: Some("Dockerfile".into()),
        context: Some(".".into()),
        build_args: BTreeMap::new(),
        security: Default::default(),
        mcp: BTreeMap::new(),
    };
    image::ensure_image(&cfg, &fixture_mcp_fs_dir(), false)
        .await
        .expect("ensure mcp-fs fixture image")
        .tag
}

fn write_mcp_config(repo: &Path) {
    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = fixture_mcp_fs_dir().join("Dockerfile");
    let context = fixture_mcp_fs_dir();
    let config_toml = format!(
        r#"
default-container = "smoke"

[containers.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

  [containers.smoke.mcp]
  fs = ["mcp-server-filesystem", "/workspace"]
"#,
        dockerfile = dockerfile.display(),
        context = context.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
}

async fn start_fixture_container(image: &ImageTag, repo: &Path) -> Container {
    let mut container = Container::start(
        image,
        ContainerLaunchSpec::workspace(repo, Path::new("/workspace")),
    )
    .await
    .expect("start fixture container");
    container.bootstrap_user().await.expect("bootstrap user");
    container
}

fn create_host_session(
    session_root: &Path,
    repo: &Path,
    container: &Container,
    image: &ImageTag,
) -> SessionId {
    let store = SessionStore::new(session_root.to_path_buf());
    let sid = SessionId(container.session_suffix().to_string());
    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: container.name().to_string(),
        image_tag: image.to_string(),
        container_config_name: "smoke".to_string(),
        agent_name: Some("smoke".to_string()),
        working_dir: repo.to_path_buf(),
        session_dir: PathBuf::new(),
        exit_code: None,
        link_target: None,
    };
    store
        .create(&sid, None, &mut session)
        .expect("host session");
    sid
}

struct McpRun {
    status: std::process::ExitStatus,
    stderr: String,
}

async fn run_mcp_client(args: &[String], repo: &Path) -> McpRun {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(repo)
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp");

    let child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    let service = serve_client((), (child_stdout, child_stdin))
        .await
        .expect("serve_client (initialize handshake)");
    let listing = service
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    let names: Vec<String> = listing
        .tools
        .iter()
        .map(|t| t.name.as_ref().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "fs__list_directory"),
        "expected `fs__list_directory` in {names:?}"
    );

    let call_args = serde_json::json!({"path": "/workspace"})
        .as_object()
        .unwrap()
        .clone();
    let call = service
        .call_tool(
            CallToolRequestParams::new("fs__list_directory".to_string()).with_arguments(call_args),
        )
        .await
        .expect("tools/call fs__list_directory");
    assert!(
        call.is_error != Some(true),
        "fs__list_directory should not be an error: {call:?}"
    );

    let _ = service.cancel().await;

    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = stderr_task.await;
    let stderr = stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");
    McpRun { status, stderr }
}

fn stderr_value<'a>(stderr: &'a str, prefix: &str) -> &'a str {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .unwrap_or_else(|| panic!("stderr lacked {prefix:?}: {stderr}"))
}

async fn wait_for_stderr_value(stderr: Arc<Mutex<String>>, prefix: &str) -> String {
    timeout(TEST_TIMEOUT, async {
        loop {
            {
                let snapshot = stderr.lock().unwrap().clone();
                if let Some(value) = snapshot
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix).map(str::trim))
                {
                    return value.to_string();
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("stderr lacked {prefix:?}: {}", stderr.lock().unwrap()))
}

async fn initialize_http_session(client: &reqwest::Client, url: &str, id: u64) -> String {
    let response = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "outrig-test",
                    "version": "1.0.0"
                }
            }
        }))
        .send()
        .await
        .expect("POST initialize");
    assert!(
        response.status().is_success(),
        "initialize failed with status {}",
        response.status()
    );
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("mcp-session-id header")
        .to_str()
        .expect("session id utf-8")
        .to_string();
    let _ = response.text().await.expect("initialize body");

    let response = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("mcp-session-id", &session_id)
        .header("Mcp-Protocol-Version", "2025-06-18")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("POST notifications/initialized");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::ACCEPTED,
        "initialized notification should be accepted"
    );

    session_id
}

async fn post_http_mcp(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let response = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("mcp-session-id", session_id)
        .header("Mcp-Protocol-Version", "2025-06-18")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {method}: {e}"));
    assert!(
        response.status().is_success(),
        "{method} failed with status {}",
        response.status()
    );
    let body = response.text().await.expect("response body");
    json_rpc_response(&body, id)
}

fn json_rpc_response(body: &str, id: u64) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && value.get("id").and_then(Value::as_u64) == Some(id)
    {
        return value;
    }

    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.trim().strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data.trim())
            && value.get("id").and_then(Value::as_u64) == Some(id)
        {
            return value;
        }
    }

    panic!("no JSON-RPC response id {id} in body: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_subcommand_serves_namespaced_tools_and_exits_clean() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    // 1. Build the fixture repo: container-only config, no agent surface.
    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_mcp_config(repo_dir.path());

    // The default workspace mount is `repo_root -> /workspace`; planting the
    // file at the repo root makes it visible inside the container.
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    // 2. Spawn `outrig mcp` with all three pipes piped.
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .arg("mcp")
        .current_dir(repo_dir.path())
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp");

    let child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Stream stderr line-by-line so a hang dumps everything-so-far.
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    // 3. Drive the child as an MCP client over its stdio.
    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client (initialize handshake)");

        let listing = service
            .list_tools(Default::default())
            .await
            .expect("tools/list");
        let names: Vec<String> = listing
            .tools
            .iter()
            .map(|t| t.name.as_ref().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "fs__list_directory"),
            "expected `fs__list_directory` in {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "fs__read_file"),
            "expected `fs__read_file` in {names:?}"
        );
        assert!(
            names.iter().all(|n| n.starts_with("fs__")),
            "every tool should be namespaced under `fs__`, got {names:?}"
        );

        let call_args = serde_json::json!({"path": "/workspace"})
            .as_object()
            .unwrap()
            .clone();
        let call = service
            .call_tool(
                CallToolRequestParams::new("fs__list_directory".to_string())
                    .with_arguments(call_args),
            )
            .await
            .expect("tools/call fs__list_directory");
        assert!(
            call.is_error != Some(true),
            "fs__list_directory should not be an error: {call:?}"
        );
        let body = call
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("HELLO.txt"),
            "list_directory body should mention HELLO.txt, got: {body}"
        );

        // Cancel the rmcp client: closes the write side of child's stdin
        // and quiesces the dispatcher. Child's `serve_server` then sees
        // EOF, teardown runs, child exits 0.
        let _ = service.cancel().await;
    };

    // 4. Wait for everything with a wall-clock bound.
    timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("MCP work did not finish within {TEST_TIMEOUT:?}"));

    let wait_result = timeout(TEST_TIMEOUT, child.wait()).await;
    let status = match wait_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => panic!("child.wait() failed: {e}"),
        Err(_) => {
            eprintln!(
                "--- subprocess stderr (before timeout kill) ---\n{}",
                stderr_buf.lock().unwrap()
            );
            let _ = child.kill().await;
            panic!("subprocess did not exit within {TEST_TIMEOUT:?}");
        }
    };
    let _ = stderr_task.await;

    let stderr_str = stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr_str}");

    assert!(
        status.success(),
        "outrig mcp exited with {status:?}; stderr was: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] mcp fs:"),
        "stderr lacked per-server banner line: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] transport: stdio"),
        "stderr lacked `transport: stdio` line: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] mcp server ready"),
        "stderr lacked `mcp server ready` line: {stderr_str}"
    );

    // Verify our specific container was cleaned up.
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
async fn mcp_listen_http_serves_multiple_independent_sessions() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_mcp_config(repo_dir.path());
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args(["mcp", "--listen", "127.0.0.1:0"])
        .current_dir(repo_dir.path())
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp --listen");

    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    let url = wait_for_stderr_value(stderr_buf.clone(), "[outrig] listen: ").await;
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with("/mcp"),
        "unexpected listen URL: {url}"
    );
    wait_for_stderr_value(stderr_buf.clone(), "[outrig] mcp server ready").await;

    let client = reqwest::Client::new();
    let session_a = initialize_http_session(&client, &url, 1).await;
    let session_b = initialize_http_session(&client, &url, 2).await;
    assert_ne!(
        session_a, session_b,
        "each HTTP client should get a distinct MCP session"
    );

    for (idx, session_id) in [session_a.as_str(), session_b.as_str()]
        .into_iter()
        .enumerate()
    {
        let list = post_http_mcp(
            &client,
            &url,
            session_id,
            10 + idx as u64,
            "tools/list",
            serde_json::json!({}),
        )
        .await;
        let tool_names = list["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(
            tool_names.iter().any(|name| *name == "fs__list_directory"),
            "tools/list should include fs__list_directory: {list}"
        );

        let call = post_http_mcp(
            &client,
            &url,
            session_id,
            20 + idx as u64,
            "tools/call",
            serde_json::json!({
                "name": "fs__list_directory",
                "arguments": { "path": "/workspace" }
            }),
        )
        .await;
        assert!(
            call["result"]["isError"] != Value::Bool(true),
            "fs__list_directory should not error: {call}"
        );
        assert!(
            call.to_string().contains("HELLO.txt"),
            "list_directory should see HELLO.txt: {call}"
        );
    }

    let pid = child.id().expect("child pid").to_string();
    let term = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .await
        .expect("send SIGTERM");
    assert!(term.success(), "kill -TERM failed with {term}");

    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = stderr_task.await;
    let stderr_str = stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr_str}");

    assert!(
        status.success(),
        "outrig mcp --listen exited with {status:?}; stderr was: {stderr_str}"
    );
    assert!(
        stderr_str.contains("[outrig] transport: streamable-http"),
        "stderr lacked streamable transport banner: {stderr_str}"
    );
    assert!(
        !stderr_str.contains("outstanding refs"),
        "HTTP shutdown should release MCP client refs before teardown: {stderr_str}"
    );

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
async fn mcp_attach_by_session_id_reuses_container_and_writes_own_logs() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_mcp_config(repo_dir.path());
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let image = ensure_fixture_image().await;
    let container = start_fixture_container(&image, repo_dir.path()).await;
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let host_sid = create_host_session(sessions.path(), repo_dir.path(), &container, &image);

    let host_log_dir = sessions.path().join(host_sid.as_str()).join("logs");
    let host_client = McpClient::connect_via_podman_exec(
        &container,
        &fs_spec(),
        "fs",
        &host_log_dir,
        &BTreeMap::new(),
    )
    .await
    .expect("host MCP client");

    let args = vec![
        "--session-root".to_string(),
        sessions.path().display().to_string(),
        "mcp".to_string(),
        "--attach".to_string(),
        host_sid.to_string(),
    ];
    let run = run_mcp_client(&args, repo_dir.path()).await;
    assert!(
        run.status.success(),
        "outrig mcp --attach exited with {:?}; stderr was: {}",
        run.status,
        run.stderr
    );
    assert!(
        run.stderr.contains("[outrig] container attached:"),
        "stderr lacked attached banner: {}",
        run.stderr
    );

    let attached_sid = stderr_value(&run.stderr, "[outrig] session id:");
    assert_ne!(
        attached_sid,
        host_sid.as_str(),
        "attacher should write a fresh session row"
    );
    let attached_log = sessions
        .path()
        .join(attached_sid)
        .join("logs")
        .join("fs.stderr");
    assert!(
        attached_log.exists(),
        "attached MCP stderr should land at {}",
        attached_log.display()
    );

    let host_result = host_client
        .call_tool("list_directory", serde_json::json!({"path": "/workspace"}))
        .await
        .expect("host MCP still answers after attach exits");
    assert!(
        !host_result.is_error && host_result.content_text.contains("HELLO.txt"),
        "host MCP child should remain usable: {host_result:?}"
    );

    let bin = env!("CARGO_BIN_EXE_outrig");
    let logs = Command::new(bin)
        .args([
            "--session-root",
            sessions.path().to_str().expect("session root utf-8"),
            "logs",
            attached_sid,
            "fs",
        ])
        .output()
        .await
        .expect("outrig logs attached session");
    assert!(
        logs.status.success(),
        "`outrig logs` for attached session failed: stderr={}",
        String::from_utf8_lossy(&logs.stderr)
    );

    host_client.shutdown().await.expect("shutdown host mcp");
    assert!(
        Container::is_running(container.name())
            .await
            .expect("podman inspect"),
        "attacher shutdown must leave host container running"
    );
    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_attach_by_podman_name_requires_container_config_and_borrows_lifecycle() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_mcp_config(repo_dir.path());
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let image = ensure_fixture_image().await;
    let container = start_fixture_container(&image, repo_dir.path()).await;
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let args = vec![
        "--session-root".to_string(),
        sessions.path().display().to_string(),
        "mcp".to_string(),
        "--attach".to_string(),
        container.name().to_string(),
        "--container".to_string(),
        "smoke".to_string(),
    ];
    let run = run_mcp_client(&args, repo_dir.path()).await;
    assert!(
        run.status.success(),
        "direct attach exited with {:?}; stderr was: {}",
        run.status,
        run.stderr
    );
    assert!(
        Container::is_running(container.name())
            .await
            .expect("podman inspect"),
        "direct attacher must not stop the borrowed container"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_attach_exits_when_host_stops_container() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_mcp_config(repo_dir.path());
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let image = ensure_fixture_image().await;
    let container = start_fixture_container(&image, repo_dir.path()).await;
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let host_sid = create_host_session(sessions.path(), repo_dir.path(), &container, &image);

    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .args([
            "--session-root",
            sessions.path().to_str().expect("session root utf-8"),
            "mcp",
            "--attach",
            host_sid.as_str(),
        ])
        .current_dir(repo_dir.path())
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp --attach");

    let child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    let service = serve_client((), (child_stdout, child_stdin))
        .await
        .expect("serve_client (initialize handshake)");
    service
        .list_tools(Default::default())
        .await
        .expect("tools/list before host stop");

    container
        .stop(Duration::from_secs(2))
        .await
        .expect("host stop");

    let status = timeout(TEST_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    drop(service);
    let _ = stderr_task.await;
    let stderr = stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(
        !status.success(),
        "attacher should exit non-zero when host stops container; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("attached container") && stderr.contains("stopped"),
        "stderr should explain host container stop: {stderr}"
    );
}
