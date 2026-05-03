//! End-to-end dispatch through `McpToolAdapter` as a Rig `ToolDyn`. Gated
//! behind `--features e2e` because it shells out to a real `buildah` /
//! `podman` and runs the `@modelcontextprotocol/server-filesystem` MCP
//! server inside the container.
//!
//! Reuses the `tests/fixtures/mcp-fs/` Dockerfile already built for
//! `tests/mcp_handshake.rs`. Run with:
//!
//! ```sh
//! cargo test --features e2e --test rig_tool_dispatch -- --nocapture
//! ```
//!
//! The test exercises the same code path an `Agent` would run: it calls
//! `<McpToolAdapter as ToolDyn>::call(adapter, args_string).await` directly
//! rather than spinning up an LLM. The LLM stack lands in tasks 0012-0016;
//! once it's available, an Agent-shaped test can be added in 0019.

#![cfg(feature = "e2e")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use outrig::config::{ContainerConfig, McpServerSpec};
use outrig::container::Container;
use outrig::image::{self, ImageTag};
use outrig::mcp::McpClient;
use outrig::rig_tool::McpToolAdapter;
use rig::tool::ToolDyn;

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

async fn ensure_fixture_image() -> ImageTag {
    let cfg = ContainerConfig {
        dockerfile: "Dockerfile".into(),
        context: ".".into(),
        build_args: BTreeMap::new(),
        mcp: BTreeMap::new(),
    };
    image::ensure_image(&cfg, &fixture_dir())
        .await
        .expect("ensure_image for mcp-fs fixture")
}

async fn start_and_bootstrap(image: &ImageTag, host_ws: &Path) -> Container {
    let mut container = Container::start(image, host_ws, Path::new("/workspace"))
        .await
        .expect("Container::start");
    container.bootstrap_user().await.expect("bootstrap_user");
    container
}

#[tokio::test]
async fn adapter_dispatches_tool_call_into_container() {
    init_tracing();

    let image = ensure_fixture_image().await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");

    let known_content = "rig-tool-dispatch e2e marker\n";
    std::fs::write(host_ws.path().join("HELLO.txt"), known_content).expect("write HELLO.txt");

    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let spec = McpServerSpec::Short(vec![
        "mcp-server-filesystem".to_string(),
        "/workspace".to_string(),
    ]);
    let client = McpClient::connect_via_podman_exec(&container, &spec, "fs", &log_dir)
        .await
        .expect("connect_via_podman_exec");
    let client = Arc::new(client);

    let adapters = McpToolAdapter::from_client_tools(client.clone())
        .await
        .expect("from_client_tools");
    assert!(
        adapters.len() >= 3,
        "filesystem MCP should advertise at least 3 tools, got {}",
        adapters.len()
    );

    let read_file = adapters
        .iter()
        .find(|a| a.mcp_tool_name == "read_file" || a.mcp_tool_name == "read_text_file")
        .expect("filesystem MCP should expose a read_file (or read_text_file) tool");

    // Sanity-check the LLM-facing name. The filesystem MCP renamed
    // `read_file` to `read_text_file` in newer versions; both are acceptable
    // because the test image pulls the latest published server.
    assert!(
        read_file.openai_name == "fs__read_file" || read_file.openai_name == "fs__read_text_file",
        "unexpected sanitized name: {}",
        read_file.openai_name
    );

    // Happy path: dispatch through ToolDyn::call exactly as a Rig agent would.
    let args = serde_json::json!({ "path": "/workspace/HELLO.txt" }).to_string();
    let body = ToolDyn::call(read_file, args)
        .await
        .expect("ToolDyn::call should succeed for an existing file");
    assert!(
        body.contains("rig-tool-dispatch e2e marker"),
        "tool output should echo the file content, got: {body}"
    );

    // Error path: missing file -> filesystem MCP reports `is_error: true`,
    // which the adapter maps to ToolError::ToolCallError.
    let bad_args = serde_json::json!({ "path": "/workspace/does-not-exist.txt" }).to_string();
    let err = ToolDyn::call(read_file, bad_args)
        .await
        .expect_err("missing file should surface as a tool-call error");
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "tool error should carry a non-empty message"
    );

    // Adapters each hold an `Arc<McpClient>`; drop them so the underlying
    // client can be reclaimed by `Arc::try_unwrap` for graceful shutdown.
    drop(adapters);
    let client =
        Arc::try_unwrap(client).expect("client Arc should be unique once adapters are dropped");
    client.shutdown().await.expect("shutdown");
    container.stop(Duration::from_secs(2)).await.expect("stop");
}
