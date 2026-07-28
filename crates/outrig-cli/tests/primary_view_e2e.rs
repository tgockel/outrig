//! End-to-end for `view = "primary"` sidecars (task 0090). Gated behind
//! `--features e2e`; needs real podman and network to pull images.
//!
//! Drives an unmodified `docker.io/mcp/filesystem` (Alpine/musl) against a
//! Debian/glibc primary that carries a real `cargo`, from config alone -- no
//! Dockerfile change, no `podman exec`, no bind mount of the workspace into the
//! sidecar. It asserts task 0090's container-level acceptance:
//!
//! - The sidecar completes an MCP handshake and lists the primary's workspace.
//! - It sees `/usr/local/cargo/bin/cargo`, a path only the primary image
//!   provides and no bind mount could -- what distinguishes this from
//!   `workspace = "rw"`.
//! - The graft stays invisible to the primary: `podman exec <primary> ls -A
//!   /mnt` is empty.
//! - Killing the primary reaps its `view = "primary"` sidecar.
//!
//! Plus task 0102's: what the payload writes into the workspace belongs to the
//! invoking user, and a root-owned path in the primary is refused -- the
//! launcher drops its capabilities with the uid once the graft is in place.
//!
//! The served root is `/` (not the doc's `/workspace`) so the filesystem
//! server can reach the primary-only cargo path through the view; both the
//! workspace and the cargo path are then provable through the real server.
//!
//! Run with (targeting this test binary specifically -- the rest of the e2e
//! suite has unrelated compile bit-rot, see
//! `plan/next/e2e-imageconfig-sidecars-bitrot.md`):
//!
//! ```sh
//! cargo test -p outrig-cli --features e2e --test primary_view_e2e -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::serve_client;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

mod common;
use common::stream_lines;

const TEST_TIMEOUT: Duration = Duration::from_secs(180);
const MCP_FS_IMAGE: &str = "docker.io/mcp/filesystem:latest";

fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("outrig-cli is under crates/")
        .join("outrig/tests/fixtures")
        .join(name)
}

/// Ensure the off-the-shelf MCP image is present locally; outrig launches it
/// `--pull=never`, so the test pulls it on demand.
async fn ensure_mcp_fs_image() {
    let present = Command::new("podman")
        .args(["image", "exists", MCP_FS_IMAGE])
        .status()
        .await
        .expect("podman image exists")
        .success();
    if !present {
        let status = Command::new("podman")
            .args(["pull", MCP_FS_IMAGE])
            .status()
            .await
            .expect("podman pull");
        assert!(status.success(), "failed to pull {MCP_FS_IMAGE}");
    }
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
                alive.extend(podman_names(&format!("name={name}")).await);
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

fn tool_body(call: &rmcp::model::CallToolResult) -> String {
    call.content
        .iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `tools/call` against the sidecar's server. A transport failure panics;
/// a result the *server* marked as an error is returned for the caller to
/// judge, since some of these calls are expected to be refused.
async fn call_tool(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    args: serde_json::Value,
) -> rmcp::model::CallToolResult {
    let args = args
        .as_object()
        .expect("tool arguments are an object")
        .clone();
    service
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args.clone()))
        .await
        .unwrap_or_else(|e| panic!("tools/call {tool} {args:?}: {e}"))
}

async fn list_dir(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    path: &str,
) -> String {
    let call = call_tool(
        service,
        "fs__list_directory",
        serde_json::json!({ "path": path }),
    )
    .await;
    assert!(
        call.is_error != Some(true),
        "listing {path} through the primary view failed: {call:?}"
    );
    tool_body(&call)
}

/// Write through the view. The served root is `/`, so whether the result comes
/// back as an error is the kernel's answer about what the payload's ids may
/// touch, not the server's own policy -- which is why the caller judges it.
async fn write_file(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    path: &str,
    content: &str,
) -> rmcp::model::CallToolResult {
    let args = serde_json::json!({ "path": path, "content": content });
    call_tool(service, "fs__write_file", args).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_view_sidecar_sees_the_primary_filesystem() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    ensure_mcp_fs_image().await;

    let repo_dir = tempfile::tempdir().expect("tempdir repo");
    let agents_dir = repo_dir.path().join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    std::fs::write(repo_dir.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let primary = fixture_dir("primary-cargo");
    let config_toml = format!(
        r#"
default-image = "primary"

[images.primary]
dockerfile = "{dockerfile}"
context = "{context}"

  [images.primary.mcp]
  # One-liner primary-view form. Served root is "/" so the server can reach
  # both /workspace (a bind mount) and /usr/local/cargo (primary-image only).
  fs = {{ image = "{MCP_FS_IMAGE}", view = "primary", args = ["/"] }}
"#,
        dockerfile = primary.join("Dockerfile").display(),
        context = primary.display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");

    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut child = Command::new(bin)
        .arg("--session-root")
        .arg(sessions.path())
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
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let _stderr_task = tokio::spawn(stream_lines(stderr, stderr_buf.clone(), "stderr"));

    let work = async {
        let service = serve_client((), (child_stdout, child_stdin))
            .await
            .expect("serve_client handshake");

        let tools = service
            .list_tools(Default::default())
            .await
            .expect("tools/list");
        assert!(
            tools
                .tools
                .iter()
                .any(|t| t.name.as_ref() == "fs__list_directory"),
            "primary-view sidecar tools missing: {tools:?}"
        );

        // The primary's workspace, through the view.
        let workspace = list_dir(&service, "/workspace").await;
        assert!(
            workspace.contains("HELLO.txt"),
            "the view should show the primary's workspace: {workspace}"
        );

        // A path only the primary *image* provides -- no bind mount does. This
        // is what separates view = "primary" from workspace = "rw".
        let cargo_dir = list_dir(&service, "/usr/local/cargo/bin").await;
        assert!(
            cargo_dir.contains("cargo"),
            "the view should show the primary image's cargo: {cargo_dir}"
        );

        // The payload runs as the session user: the launcher holds its
        // capabilities only until the graft is in place, then drops to the
        // `--uid`/`--gid` it was given. So a file it writes belongs to the
        // invoking user rather than to a host subuid, and a root-owned path
        // is out of reach.
        let wrote = write_file(
            &service,
            "/workspace/FROM-SIDECAR.txt",
            "through the view\n",
        )
        .await;
        assert!(
            wrote.is_error != Some(true),
            "writing into the primary's workspace through the view failed: {wrote:?}"
        );
        let written = std::fs::metadata(repo_dir.path().join("FROM-SIDECAR.txt"))
            .expect("stat the file the sidecar wrote");
        let repo_meta = std::fs::metadata(repo_dir.path()).expect("stat the host repo dir");
        assert_eq!(
            (written.uid(), written.gid()),
            (repo_meta.uid(), repo_meta.gid()),
            "a file written through the view should belong to the invoking user"
        );
        let denied = write_file(&service, "/etc/outrig-privilege-probe", "never\n").await;
        assert!(
            denied.is_error == Some(true),
            "a root-owned path must be unwritable after the privilege drop: {denied:?}"
        );

        let sid = wait_for_stderr_value(stderr_buf.clone(), "[outrig] session id:").await;
        let primary_name = format!("outrig-{sid}");
        let sidecar_name = format!("outrig-{sid}-fs");

        // The graft is invisible to the primary: its /mnt is untouched.
        let mnt = Command::new("podman")
            .args(["exec", &primary_name, "ls", "-A", "/mnt"])
            .output()
            .await
            .expect("podman exec ls /mnt");
        assert!(
            mnt.status.success(),
            "podman exec ls /mnt failed: {}",
            String::from_utf8_lossy(&mnt.stderr)
        );
        assert!(
            String::from_utf8_lossy(&mnt.stdout).trim().is_empty(),
            "the graft must stay invisible to the primary; /mnt held: {}",
            String::from_utf8_lossy(&mnt.stdout)
        );

        // Killing the primary reaps its view sidecar.
        drop(service);
        let killed = Command::new("podman")
            .args(["kill", &primary_name])
            .status()
            .await
            .expect("podman kill primary");
        assert!(killed.success(), "podman kill {primary_name} failed");
        wait_until_gone(&[sidecar_name]).await;
    };

    timeout(TEST_TIMEOUT, work)
        .await
        .expect("primary-view e2e timed out");
    let _ = child.kill().await;
}
