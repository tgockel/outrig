//! End-to-end smoke for MCP sidecar containers. Gated behind `--features e2e`.
//!
//! Exercises task 0079's acceptance criteria against real podman:
//!
//! - A declared sidecar hosts servers visible over `outrig mcp` stdio, with
//!   read-only workspace access, session/sidecar labels, and a session record
//!   listing the sidecar container; clean EOF teardown reaps every container.
//! - `podman kill` of the primary reaps sidecars and ends the session with a
//!   non-zero exit.
//! - `outrig clean` sweeps stopped, record-less labeled containers.
//! - `on-failure = "warn"` serves a reduced tool set; the default `abort`
//!   fails fast without leftovers.
//! - `--network audit` attaches interception to the sidecar (loopback
//!   resolver installed), not just the primary.
//!
//! And task 0080's entrypoint-stdio criteria:
//!
//! - An off-the-shelf image whose ENTRYPOINT is the server serves tools with
//!   env baked in at create; the `--rm` container reaps on clean EOF.
//! - In audit mode, the entrypoint's *startup* network touch lands in
//!   `network.jsonl` attributed to the sidecar container -- policy attached
//!   before its first packet.
//! - Stopping the entrypoint container mid-session degrades its tools to
//!   errors while the session and primary-hosted tools survive.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p outrig-cli --features e2e mcp_sidecar_smoke -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use outrig_cli::session::{SessionId, SessionStore};
use rmcp::model::CallToolRequestParams;
use rmcp::service::serve_client;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

mod common;
use common::stream_lines;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("outrig-cli is under crates/")
        .join("outrig/tests/fixtures")
        .join(name)
}

/// `[images.entry]` block for the entrypoint-stdio fixture, whose ENTRYPOINT
/// touches the network and then serves mcp-server-filesystem over stdio.
fn entry_image_block() -> String {
    let dir = fixture_dir("mcp-entrypoint");
    format!(
        r#"
  [images.entry]
  dockerfile = "{dockerfile}"
  context = "{context}"
"#,
        dockerfile = dir.join("Dockerfile").display(),
        context = dir.display(),
    )
}

/// Repo config: primary `fs` server plus `fs2` in a named sidecar built from
/// the same fixture image (read-only workspace so its filesystem server can
/// see the repo). `extra` is appended inside the `[images.smoke]` scope.
fn write_sidecar_config(repo: &Path, sidecar_blocks: &str) {
    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");

    let dockerfile = fixture_dir("mcp-fs").join("Dockerfile");
    let context = fixture_dir("mcp-fs");
    let config_toml = format!(
        r#"
default-image = "smoke"

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

{sidecar_blocks}

[images.sidekick]
dockerfile = "{dockerfile}"
context = "{context}"
"#,
        dockerfile = dockerfile.display(),
        context = context.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
}

struct McpChild {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr_buf: Arc<Mutex<String>>,
    stderr_task: tokio::task::JoinHandle<()>,
}

fn spawn_mcp(repo: &Path, session_root: &Path, extra_args: &[&str]) -> McpChild {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .arg("--session-root")
        .arg(session_root)
        .arg("mcp")
        .args(extra_args)
        .current_dir(repo)
        .env("OUTRIG_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn outrig mcp");

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));
    McpChild {
        child,
        stdin,
        stdout,
        stderr_buf,
        stderr_task,
    }
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

async fn podman_names(filter: &str) -> Vec<String> {
    let out = Command::new("podman")
        .args(["ps", "-a", "--filter", filter, "--format", "{{.Names}}"])
        .output()
        .await
        .expect("podman ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

async fn wait_until_gone(names: &[String]) {
    timeout(TEST_TIMEOUT, async {
        loop {
            let mut alive = Vec::new();
            for name in names {
                let found = podman_names(&format!("name={name}")).await;
                alive.extend(found);
            }
            if alive.is_empty() {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("containers {names:?} were not reaped within {TEST_TIMEOUT:?}"));
}

fn tool_names(listing: &rmcp::model::ListToolsResult) -> Vec<String> {
    listing
        .tools
        .iter()
        .map(|t| t.name.as_ref().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_hosts_servers_with_labels_record_and_clean_reap() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        r#"
  [images.smoke.mcp]
  fs  = ["mcp-server-filesystem", "/workspace"]
  fs2 = { command = ["mcp-server-filesystem", "/workspace"], sidecar = "tools" }

  [images.smoke.sidecars.tools]
  image     = "sidekick"
  workspace = "ro"
"#,
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client handshake");

        let names = tool_names(
            &service
                .list_tools(Default::default())
                .await
                .expect("tools/list"),
        );
        assert!(
            names.iter().any(|n| n == "fs__list_directory"),
            "primary server tools missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "fs2__list_directory"),
            "sidecar server tools missing: {names:?}"
        );

        // The sidecar's read-only workspace shows the same repo content.
        let call_args = serde_json::json!({"path": "/workspace"})
            .as_object()
            .unwrap()
            .clone();
        let call = service
            .call_tool(
                CallToolRequestParams::new("fs2__list_directory".to_string())
                    .with_arguments(call_args),
            )
            .await
            .expect("tools/call fs2__list_directory");
        assert!(
            call.is_error != Some(true),
            "sidecar-hosted tool call failed: {call:?}"
        );
        let body = call
            .content
            .iter()
            .filter_map(|c| match c {
                rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("HELLO.txt"),
            "sidecar workspace should show HELLO.txt: {body}"
        );

        // While serving: both containers alive with the session label; the
        // sidecar carries the sidecar label.
        let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
        let labeled = podman_names(&format!("label=org.outrig.session={sid}")).await;
        assert_eq!(
            labeled.len(),
            2,
            "expected primary + sidecar with session label, got {labeled:?}"
        );
        assert!(
            labeled.contains(&format!("outrig-{sid}-tools")),
            "sidecar name should be outrig-<sid>-tools: {labeled:?}"
        );
        let sidecar_labeled = podman_names("label=org.outrig.sidecar=tools").await;
        assert!(
            sidecar_labeled.contains(&format!("outrig-{sid}-tools")),
            "sidecar label missing: {sidecar_labeled:?}"
        );

        // The session record lists the sidecar container.
        let store = SessionStore::new(sessions.path().to_path_buf());
        let (_, session) = store
            .get_by_id(&SessionId(sid.clone()))
            .expect("session record");
        assert_eq!(
            session.sidecar_container_names,
            vec![format!("outrig-{sid}-tools")],
            "session.json should list the sidecar container"
        );

        let _ = service.cancel().await;
        sid
    };
    let sid = timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("MCP work did not finish within {TEST_TIMEOUT:?}"));

    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(status.success(), "clean EOF exit expected: {stderr}");
    let leftovers = podman_names(&format!("label=org.outrig.session={sid}")).await;
    assert!(
        leftovers.is_empty(),
        "teardown should reap primary and sidecar: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn podman_kill_of_primary_reaps_sidecars_and_exits_nonzero() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        r#"
  [images.smoke.mcp]
  fs  = ["mcp-server-filesystem", "/workspace"]
  fs2 = { command = ["mcp-server-filesystem", "/workspace"], sidecar = "tools" }

  [images.smoke.sidecars.tools]
  image     = "sidekick"
  workspace = "ro"
"#,
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");

    // The stdio server only reports ready once a client completes the
    // initialize handshake; keep the service alive across the kill.
    let service = timeout(TEST_TIMEOUT, serve_client((), (child_stdout, child_stdin)))
        .await
        .expect("handshake within timeout")
        .expect("serve_client handshake");
    wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] mcp server ready").await;
    let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
    let primary = format!("outrig-{sid}");
    let sidecar = format!("outrig-{sid}-tools");

    let kill = Command::new("podman")
        .args(["kill", &primary])
        .status()
        .await
        .expect("podman kill");
    assert!(kill.success(), "podman kill failed: {kill}");

    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?} after kill"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(
        !status.success(),
        "external primary death must end the session with an error: {stderr}"
    );
    assert!(
        stderr.contains("exited unexpectedly"),
        "stderr should explain the primary death: {stderr}"
    );
    wait_until_gone(&[primary, sidecar]).await;
    drop(service);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_sweeps_stopped_recordless_labeled_containers() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    // Plant a stray: labeled, stopped, no --rm, no session record.
    let suffix = format!("{:08x}", rand_u32());
    let stray = format!("outrig-straytest-{suffix}");
    let sid_label = format!("straytest-{suffix}");
    let create = Command::new("podman")
        .args([
            "run",
            "-d",
            "--name",
            &stray,
            "--label",
            &format!("org.outrig.session={sid_label}"),
            "--pull=never",
            "docker.io/library/alpine:3.20",
            "sleep",
            "infinity",
        ])
        .output()
        .await
        .expect("podman run stray");
    if !create.status.success() {
        // Pull alpine once if absent, then retry.
        let pull = Command::new("podman")
            .args(["pull", "docker.io/library/alpine:3.20"])
            .status()
            .await
            .expect("podman pull alpine");
        assert!(pull.success(), "podman pull alpine failed");
        let retry = Command::new("podman")
            .args([
                "run",
                "-d",
                "--name",
                &stray,
                "--label",
                &format!("org.outrig.session={sid_label}"),
                "--pull=never",
                "docker.io/library/alpine:3.20",
                "sleep",
                "infinity",
            ])
            .status()
            .await
            .expect("podman run stray retry");
        assert!(retry.success(), "planting stray container failed");
    }
    let stop = Command::new("podman")
        .args(["stop", "-t", "0", &stray])
        .status()
        .await
        .expect("podman stop stray");
    assert!(stop.success(), "podman stop stray failed");

    // Age past the 2s cutoff (podman Created has second resolution).
    sleep(Duration::from_secs(3)).await;

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let bin = env!("CARGO_BIN_EXE_outrig");
    let out = Command::new(bin)
        .args([
            "--session-root",
            sessions.path().to_str().expect("utf-8 root"),
            "clean",
            "-y",
            "--older-than",
            "2s",
        ])
        .output()
        .await
        .expect("outrig clean");
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- outrig clean stderr ---\n{stderr}");
    assert!(out.status.success(), "outrig clean failed: {stderr}");
    assert!(
        stderr.contains(&stray),
        "clean should report removing the stray: {stderr}"
    );

    let leftovers = podman_names(&format!("name={stray}")).await;
    assert!(
        leftovers.is_empty(),
        "stray container should be removed: {leftovers:?}"
    );
}

fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .subsec_nanos();
    nanos ^ std::process::id()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warn_on_failure_serves_reduced_toolset() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        r#"
  [images.smoke.mcp]
  fs  = ["mcp-server-filesystem", "/workspace"]
  fs2 = { command = ["mcp-server-filesystem", "/workspace"], sidecar = "broken" }

  [images.smoke.sidecars.broken]
  image      = "no-such-image-outrig-e2e:1"
  on-failure = "warn"
"#,
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client handshake");
        let names = tool_names(
            &service
                .list_tools(Default::default())
                .await
                .expect("tools/list"),
        );
        assert!(
            names.iter().any(|n| n.starts_with("fs__")),
            "primary tools should survive: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("fs2__")),
            "warn'd sidecar's tools must be skipped: {names:?}"
        );
        let _ = service.cancel().await;
    };
    timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("MCP work did not finish within {TEST_TIMEOUT:?}"));

    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(
        status.success(),
        "warn on-failure should keep the session usable: {stderr}"
    );
    assert!(
        stderr.contains("warning: sidecar broken"),
        "stderr should carry the sidecar warning: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_on_failure_fails_fast_without_leftovers() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        r#"
  [images.smoke.mcp]
  fs  = ["mcp-server-filesystem", "/workspace"]
  fs2 = { command = ["mcp-server-filesystem", "/workspace"], sidecar = "broken" }

  [images.smoke.sidecars.broken]
  image = "no-such-image-outrig-e2e:1"
"#,
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    drop(run.stdin.take());
    drop(run.stdout.take());

    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(
        !status.success(),
        "abort on-failure must fail the session: {stderr}"
    );
    assert!(
        stderr.contains("no-such-image-outrig-e2e"),
        "stderr should name the failing image: {stderr}"
    );

    // The primary was started (its progress line names it) and must be gone.
    let primary = stderr
        .lines()
        .find_map(|l| l.strip_prefix("[outrig] starting container "))
        .map(str::to_string);
    if let Some(primary) = primary {
        wait_until_gone(&[primary]).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entrypoint_stdio_server_serves_tools_and_reaps() {
    common::init_tracing();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        &format!(
            r#"
  [images.smoke.mcp]
  fs    = ["mcp-server-filesystem", "/workspace"]
  fetch = {{ image = "entry", env = {{ MARKER = "smoke-value" }} }}
{}"#,
            entry_image_block()
        ),
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client handshake");

        let names = tool_names(
            &service
                .list_tools(Default::default())
                .await
                .expect("tools/list"),
        );
        assert!(
            names.iter().any(|n| n == "fs__list_directory"),
            "primary server tools missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "fetch__list_directory"),
            "entrypoint server tools missing: {names:?}"
        );

        // The entrypoint server works: it serves /tmp inside its container.
        let call_args = serde_json::json!({"path": "/tmp"})
            .as_object()
            .unwrap()
            .clone();
        let call = service
            .call_tool(
                CallToolRequestParams::new("fetch__list_directory".to_string())
                    .with_arguments(call_args),
            )
            .await
            .expect("tools/call fetch__list_directory");
        assert!(
            call.is_error != Some(true),
            "entrypoint-hosted tool call failed: {call:?}"
        );

        let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
        let sidecar = format!("outrig-{sid}-fetch");

        // Labels + session record, exactly as exec-stdio sidecars get.
        let labeled = podman_names(&format!("label=org.outrig.session={sid}")).await;
        assert_eq!(
            labeled.len(),
            2,
            "expected primary + entrypoint sidecar with session label, got {labeled:?}"
        );
        assert!(labeled.contains(&sidecar), "sidecar missing: {labeled:?}");
        let store = SessionStore::new(sessions.path().to_path_buf());
        let (_, session) = store
            .get_by_id(&SessionId(sid.clone()))
            .expect("session record");
        assert_eq!(
            session.sidecar_container_names,
            vec![sidecar.clone()],
            "session.json should list the entrypoint sidecar"
        );

        // env from the MCP entry was baked in at `podman create --env`.
        let inspect = Command::new("podman")
            .args(["inspect", "--format", "{{.Config.Env}}", &sidecar])
            .output()
            .await
            .expect("podman inspect env");
        let env = String::from_utf8_lossy(&inspect.stdout);
        assert!(
            env.contains("MARKER=smoke-value"),
            "container env should carry the config entry env: {env}"
        );

        let _ = service.cancel().await;
        sid
    };
    let sid = timeout(TEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| panic!("MCP work did not finish within {TEST_TIMEOUT:?}"));

    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("subprocess did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");

    assert!(status.success(), "clean EOF exit expected: {stderr}");
    // Lifetime equals server lifetime: EOF closed the server, `--rm` reaped
    // the container, and teardown tolerated it already being gone.
    let leftovers = podman_names(&format!("label=org.outrig.session={sid}")).await;
    assert!(
        leftovers.is_empty(),
        "teardown should reap primary and entrypoint sidecar: {leftovers:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entrypoint_stdio_audit_covers_first_packet() {
    common::init_tracing();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        &format!(
            r#"
  [images.smoke.mcp]
  fetch = {{ image = "entry" }}
{}"#,
            entry_image_block()
        ),
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &["--network", "audit"]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");
    let service = timeout(TEST_TIMEOUT, serve_client((), (child_stdout, child_stdin)))
        .await
        .expect("handshake within timeout")
        .expect("serve_client handshake");
    wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] mcp server ready").await;
    let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
    let sidecar = format!("outrig-{sid}-fetch");

    // The loopback resolver was baked in via `--dns` at create time -- the
    // exec-based install can't reach a container that starts as the server.
    let resolv = Command::new("podman")
        .args(["exec", &sidecar, "cat", "/etc/resolv.conf"])
        .output()
        .await
        .expect("podman exec cat resolv.conf");
    assert!(
        resolv.status.success(),
        "podman exec into entrypoint sidecar failed: {}",
        String::from_utf8_lossy(&resolv.stderr)
    );
    let resolv = String::from_utf8_lossy(&resolv.stdout);
    assert!(
        resolv.contains("nameserver 127.0.0.1"),
        "entrypoint sidecar resolv.conf should point at the interceptor: {resolv}"
    );

    let (session_dir, _) = SessionStore::new(sessions.path().to_path_buf())
        .get_by_id(&SessionId(sid.clone()))
        .expect("session record");

    let _ = service.cancel().await;
    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");
    assert!(status.success(), "clean shutdown expected: {stderr}");

    // The entrypoint touched example.com *before* serving MCP; an audit
    // record attributed to the sidecar proves interception was live ahead of
    // the entrypoint's first packet.
    let log = std::fs::read_to_string(session_dir.join("logs/network.jsonl"))
        .expect("network.jsonl exists");
    assert!(
        log.lines()
            .any(|l| l.contains(&sidecar) && l.contains("example.com")),
        "audit log should attribute the entrypoint's startup traffic to the \
         sidecar container:\n{log}"
    );
    wait_until_gone(&[format!("outrig-{sid}"), sidecar]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entrypoint_server_exit_surfaces_as_tool_errors_session_survives() {
    common::init_tracing();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        &format!(
            r#"
  [images.smoke.mcp]
  fs    = ["mcp-server-filesystem", "/workspace"]
  fetch = {{ image = "entry" }}
{}"#,
            entry_image_block()
        ),
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &[]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");
    let service = timeout(TEST_TIMEOUT, serve_client((), (child_stdout, child_stdin)))
        .await
        .expect("handshake within timeout")
        .expect("serve_client handshake");
    wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] mcp server ready").await;
    let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
    let sidecar = format!("outrig-{sid}-fetch");

    // Kill the entrypoint server's container out from under the session.
    let stop = Command::new("podman")
        .args(["stop", "-t", "1", &sidecar])
        .status()
        .await
        .expect("podman stop");
    assert!(stop.success(), "podman stop failed: {stop}");
    wait_for_stderr_value(
        run.stderr_buf.clone(),
        &format!("[outrig] sidecar container {sidecar} exited"),
    )
    .await;

    // Its tools now error; the session and primary-hosted tools survive.
    let call_args = serde_json::json!({"path": "/tmp"})
        .as_object()
        .unwrap()
        .clone();
    let dead = timeout(
        TEST_TIMEOUT,
        service.call_tool(
            CallToolRequestParams::new("fetch__list_directory".to_string())
                .with_arguments(call_args),
        ),
    )
    .await
    .expect("dead-server call returned")
    .expect("dead-server call is an in-band tool error, not a protocol error");
    assert_eq!(
        dead.is_error,
        Some(true),
        "call against the dead entrypoint server must surface as a tool \
         error: {dead:?}"
    );

    let alive_args = serde_json::json!({"path": "/workspace"})
        .as_object()
        .unwrap()
        .clone();
    let alive = service
        .call_tool(
            CallToolRequestParams::new("fs__list_directory".to_string()).with_arguments(alive_args),
        )
        .await
        .expect("primary-hosted call");
    assert!(
        alive.is_error != Some(true),
        "primary-hosted tools must keep working: {alive:?}"
    );

    let _ = service.cancel().await;
    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");
    assert!(
        status.success(),
        "session must end cleanly despite the dead sidecar: {stderr}"
    );
    wait_until_gone(&[format!("outrig-{sid}")]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_audit_attaches_interceptor_to_sidecar() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    write_sidecar_config(
        repo_dir.path(),
        r#"
  [images.smoke.mcp]
  fs  = ["mcp-server-filesystem", "/workspace"]
  fs2 = { command = ["mcp-server-filesystem", "/workspace"], sidecar = "tools" }

  [images.smoke.sidecars.tools]
  image     = "sidekick"
  workspace = "ro"
"#,
    );
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let mut run = spawn_mcp(repo_dir.path(), sessions.path(), &["--network", "audit"]);
    let child_stdin = run.stdin.take().expect("stdin piped");
    let child_stdout = run.stdout.take().expect("stdout piped");
    let service = timeout(TEST_TIMEOUT, serve_client((), (child_stdout, child_stdin)))
        .await
        .expect("handshake within timeout")
        .expect("serve_client handshake");
    wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] mcp server ready").await;
    let sid = wait_for_stderr_value(run.stderr_buf.clone(), "[outrig] session id:").await;
    let sidecar = format!("outrig-{sid}-tools");

    // The interceptor's audit resolver must be installed in the *sidecar*,
    // proving per-container attachment (0078 covers the mechanics; this
    // covers the session wiring).
    let resolv = Command::new("podman")
        .args(["exec", &sidecar, "cat", "/etc/resolv.conf"])
        .output()
        .await
        .expect("podman exec cat resolv.conf");
    assert!(
        resolv.status.success(),
        "podman exec into sidecar failed: {}",
        String::from_utf8_lossy(&resolv.stderr)
    );
    let resolv = String::from_utf8_lossy(&resolv.stdout);
    assert!(
        resolv.contains("nameserver 127.0.0.1"),
        "sidecar resolv.conf should point at the interceptor: {resolv}"
    );

    // Orderly shutdown: cancel the client so the server sees EOF.
    let _ = service.cancel().await;
    let status = timeout(TEST_TIMEOUT, run.child.wait())
        .await
        .unwrap_or_else(|_| panic!("child did not exit within {TEST_TIMEOUT:?}"))
        .expect("child.wait");
    let _ = run.stderr_task.await;
    let stderr = run.stderr_buf.lock().unwrap().clone();
    eprintln!("--- subprocess stderr ---\n{stderr}");
    assert!(
        status.success(),
        "SIGTERM shutdown should be clean: {stderr}"
    );
    wait_until_gone(&[format!("outrig-{sid}"), sidecar]).await;
}
