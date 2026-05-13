//! End-to-end smoke for the curated library surface (`Outrig`,
//! `LaunchSpec`, `WorkspaceSpec`, `ToolHandle`, `McpTool`,
//! `McpToolResult`, `load_project`, plus the always-public `config`,
//! `error`, and `mcp_proxy` modules).
//!
//! Compiles only when `internal` is off and `e2e` is on -- that double
//! gate is what proves the curated surface is self-contained: with
//! `internal` off, every import in this file must resolve through the
//! curated `pub use` re-exports above, never through a now-private
//! module.
//!
//! Run with:
//!
//! ```sh
//! cargo test --no-default-features --features e2e --test library_surface -- --nocapture
//! ```

#![cfg(all(not(feature = "internal"), feature = "e2e"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use outrig::config::McpServerSpec;
use outrig::{LaunchSpec, MountAccess, MountSpec, Outrig};

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs")
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn build_fixture_image(tag: &str) {
    let status = std::process::Command::new("podman")
        .arg("build")
        .arg("-t")
        .arg(tag)
        .arg(fixture_dir())
        .status()
        .expect("spawn podman build");
    assert!(status.success(), "podman build exited non-zero: {status:?}");
}

#[tokio::test]
async fn launch_lists_tools_calls_one_and_shuts_down() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let dockerfile = fixture_dir().join("Dockerfile");
    let context = fixture_dir();

    let mut mcp = BTreeMap::new();
    mcp.insert(
        "fs".to_string(),
        McpServerSpec::Short(vec![
            "mcp-server-filesystem".to_string(),
            "/workspace".to_string(),
        ]),
    );

    let spec = LaunchSpec::build(
        dockerfile,
        context,
        BTreeMap::new(),
        outrig::WorkspaceSpec {
            host: host_ws.path().to_path_buf(),
            container: PathBuf::from("/workspace"),
        },
        mcp,
        log_dir.clone(),
    );

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    let tools = outrig.tools();
    assert!(
        tools.len() >= 3,
        "filesystem MCP should advertise at least 3 tools, got {}: {:?}",
        tools.len(),
        tools
            .iter()
            .map(|t| (&t.server, &t.name))
            .collect::<Vec<_>>(),
    );
    assert!(
        tools.iter().all(|t| t.server == "fs"),
        "every tool should be tagged with the only server we configured",
    );
    assert!(
        tools.iter().any(|t| t.name == "list_directory"),
        "filesystem MCP should advertise list_directory",
    );

    let result = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool list_directory");
    assert!(
        !result.is_error,
        "list_directory should not be an error: {result:?}"
    );
    assert!(
        result.content_text.contains("MARKER.txt"),
        "list_directory output should mention MARKER.txt, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn from_image_launches_with_extra_read_only_mount() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-mounts-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let resources = tempfile::tempdir().expect("tempdir resources");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");
    std::fs::write(resources.path().join("REFERENCE.txt"), "mounted\n")
        .expect("write REFERENCE.txt");

    let mut mcp = BTreeMap::new();
    mcp.insert(
        "fs".to_string(),
        McpServerSpec::Short(vec![
            "mcp-server-filesystem".to_string(),
            "/resources/readonly".to_string(),
        ]),
    );

    let spec = LaunchSpec::from_image(tag, mcp, log_dir).with_mount(MountSpec {
        host: resources.path().to_path_buf(),
        container: PathBuf::from("/resources/readonly"),
        access: MountAccess::ReadOnly,
    });

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    let result = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/resources/readonly" }),
        )
        .await
        .expect("call_tool list_directory");
    assert!(
        result.content_text.contains("REFERENCE.txt"),
        "list_directory output should mention REFERENCE.txt, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
}
