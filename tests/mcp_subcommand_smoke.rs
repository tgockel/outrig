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
//! cargo test --features e2e mcp_subcommand_smoke -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::serve_client;
use tokio::process::Command;
use tokio::time::timeout;

mod common;
use common::stream_lines;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture_mcp_fs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs")
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
    let agents_dir = repo_dir.path().join(".agents/outrig");
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
