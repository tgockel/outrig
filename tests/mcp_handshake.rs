//! End-to-end smoke for `outrig::mcp`. Gated behind `--features e2e`
//! because it shells out to a real `buildah` (to build the fixture image),
//! a real `podman` (to start the container), and a real
//! `@modelcontextprotocol/server-filesystem` MCP server inside the
//! container.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e mcp_handshake -- --nocapture
//! ```
//!
//! The fixture image is built once via `outrig::image::ensure_image` and
//! re-used by every test in this file (and across re-runs) thanks to the
//! buildah-tag content cache.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use outrig::config::{ContainerConfig, McpServerSpec};
use outrig::container::Container;
use outrig::error::OutrigError;
use outrig::image::{self, ImageTag};
use outrig::mcp::McpClient;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs")
}

async fn ensure_fixture_image() -> ImageTag {
    let cfg = ContainerConfig {
        image_name: None,
        dockerfile: Some("Dockerfile".into()),
        context: Some(".".into()),
        build_args: BTreeMap::new(),
        mcp: BTreeMap::new(),
    };
    image::ensure_image(&cfg, &fixture_dir(), false)
        .await
        .expect("ensure_image for mcp-fs fixture")
        .tag
}

async fn start_and_bootstrap(image: &ImageTag, host_ws: &Path) -> Container {
    let mut container = Container::start(image, Some((host_ws, Path::new("/workspace"))))
        .await
        .expect("Container::start");
    container.bootstrap_user().await.expect("bootstrap_user");
    container
}

#[tokio::test]
async fn lists_tools_and_calls_list_directory() {
    common::init_tracing();

    let image = ensure_fixture_image().await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");

    // Plant a recognizable file in the workspace so list_directory can show it.
    std::fs::write(host_ws.path().join("HELLO.txt"), "hi\n").expect("write HELLO.txt");

    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let spec = McpServerSpec::Short(vec![
        "mcp-server-filesystem".to_string(),
        "/workspace".to_string(),
    ]);
    let client =
        McpClient::connect_via_podman_exec(&container, &spec, "fs", &log_dir, &BTreeMap::new())
            .await
            .expect("connect_via_podman_exec");

    let tools = client.list_tools().await.expect("list_tools");
    assert!(
        tools.len() >= 3,
        "filesystem MCP should advertise at least 3 tools, got {}: {:?}",
        tools.len(),
        tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
    );

    let result = client
        .call_tool(
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool list_directory");
    assert!(
        !result.is_error,
        "list_directory should not be an error: {:?}",
        result
    );
    assert!(
        result.content_text.contains("HELLO.txt"),
        "list_directory output should mention HELLO.txt, got: {}",
        result.content_text,
    );

    let stderr_path = log_dir.join("fs.stderr");
    assert!(
        stderr_path.exists(),
        "stderr file should exist at {}",
        stderr_path.display()
    );

    client.shutdown().await.expect("shutdown");
    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn stderr_captured_on_crash() {
    common::init_tracing();

    let image = ensure_fixture_image().await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");

    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let spec = McpServerSpec::Short(vec![
        "node".to_string(),
        "-e".to_string(),
        "console.error('boom-from-mcp'); process.exit(1)".to_string(),
    ]);
    let result =
        McpClient::connect_via_podman_exec(&container, &spec, "crashy", &log_dir, &BTreeMap::new())
            .await;

    match result {
        Err(OutrigError::McpStartupFailed(payload)) => {
            assert_eq!(payload.name, "crashy");
            assert_eq!(payload.stderr_path, log_dir.join("crashy.stderr"));
        }
        Err(other) => panic!("expected McpStartupFailed error, got: {other:?}"),
        Ok(_) => panic!("connect should have failed (server exits before initialize)"),
    }

    // Give the kernel a moment to flush the child's stderr to disk; on
    // unloaded systems this is usually instant, but `--features e2e` runs
    // can be CI-bound.
    let stderr_path = log_dir.join("crashy.stderr");
    let mut contents = String::new();
    for _ in 0..40 {
        if let Ok(text) = std::fs::read_to_string(&stderr_path)
            && text.contains("boom-from-mcp")
        {
            contents = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        contents.contains("boom-from-mcp"),
        "stderr file at {} should contain `boom-from-mcp`, got: {:?}",
        stderr_path.display(),
        contents,
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}
