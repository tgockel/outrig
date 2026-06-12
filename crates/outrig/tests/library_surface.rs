//! End-to-end smoke for the curated library surface (`Outrig`,
//! `LaunchSpec`, `WorkspaceSpec`, `ToolHandle`, `McpTool`,
//! `McpToolResult`, `load_project`, plus the always-public `config`,
//! `error`, and `mcp_proxy` modules).
//!
//! Run with:
//!
//! ```sh
//! cargo test -p outrig --features e2e --test library_surface -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use outrig::config::McpServerSpec;
use outrig::{
    CapabilityProfile, CapabilitySpec, EmbeddedMcpPolicy, LaunchSpec, MountAccess, MountSpec,
    NetworkAction, NetworkMode, NetworkPolicy, Outrig,
};

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

fn dockerfile_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn label_line(key: &str, value: &str) -> String {
    format!("LABEL \"{key}\"=\"{}\"\n", dockerfile_escape(value))
}

fn build_fixture_image_with_mcp_label(tag: &str, mcp: &BTreeMap<String, McpServerSpec>) {
    let ctx = tempfile::tempdir().expect("tempdir image context");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         {}",
        label_line(
            outrig::container::embedded::LABEL_MCP,
            &serde_json::to_string(mcp).expect("serialize mcp label json"),
        ),
    );
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let status = std::process::Command::new("podman")
        .arg("build")
        .arg("-t")
        .arg(tag)
        .arg(ctx.path())
        .status()
        .expect("spawn podman build");
    assert!(status.success(), "podman build exited non-zero: {status:?}");
}

fn fs_spec(path: &str) -> McpServerSpec {
    McpServerSpec::Short(vec!["mcp-server-filesystem".to_string(), path.to_string()])
}

#[test]
fn network_filter_builder_is_on_curated_surface() {
    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Deny)
        .allow_host_port("github.com", 443)
        .deny_host_port("*", 22)
        .build()
        .expect("policy builds");
    let spec = LaunchSpec::from_image(
        "localhost/outrig-unused:latest",
        BTreeMap::new(),
        tempfile::tempdir().expect("session").path().join("logs"),
    )
    .with_network_filter(policy);

    assert_eq!(spec.network.mode, NetworkMode::Filter);
    assert!(spec.network.policy.is_some());
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

    let spec = LaunchSpec::from_image(tag, mcp, log_dir)
        .with_mount(MountSpec {
            host: resources.path().to_path_buf(),
            container: PathBuf::from("/resources/readonly"),
            access: MountAccess::ReadOnly,
        })
        .with_capabilities(CapabilitySpec {
            profile: CapabilityProfile::NoNetRaw,
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
        })
        .with_network_mode(NetworkMode::Default);

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

#[tokio::test]
async fn from_image_can_ignore_embedded_mcp_label() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-embedded-policy-{}:latest",
        std::process::id(),
    );
    let mut embedded_mcp = BTreeMap::new();
    embedded_mcp.insert(
        "bad".to_string(),
        McpServerSpec::Short(vec![
            "node".to_string(),
            "-e".to_string(),
            "process.exit(42)".to_string(),
        ]),
    );
    build_fixture_image_with_mcp_label(&tag, &embedded_mcp);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let mut explicit_mcp = BTreeMap::new();
    explicit_mcp.insert("fs".to_string(), fs_spec("/workspace"));

    let merge_session_dir = tempfile::tempdir().expect("tempdir merge session");
    let merge_spec = LaunchSpec::from_image(
        tag.clone(),
        explicit_mcp.clone(),
        merge_session_dir.path().join("logs"),
    )
    .with_workspace(outrig::WorkspaceSpec {
        host: host_ws.path().to_path_buf(),
        container: PathBuf::from("/workspace"),
    });

    let merge_err = match Outrig::launch(&merge_spec).await {
        Ok(outrig) => {
            let _ = outrig.shutdown().await;
            panic!("default Merge policy should start embedded bad MCP and fail");
        }
        Err(err) => err,
    };
    let merge_err = merge_err.to_string();
    assert!(
        merge_err.contains("mcp server \"bad\" from image label org.outrig.mcp failed to start"),
        "startup error should identify image-label source, got: {merge_err}",
    );

    let ignore_session_dir = tempfile::tempdir().expect("tempdir ignore session");
    let ignore_spec =
        LaunchSpec::from_image(tag, explicit_mcp, ignore_session_dir.path().join("logs"))
            .with_workspace(outrig::WorkspaceSpec {
                host: host_ws.path().to_path_buf(),
                container: PathBuf::from("/workspace"),
            })
            .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);

    let outrig = Outrig::launch(&ignore_spec)
        .await
        .expect("Ignore policy should launch explicit MCP only");
    assert!(
        outrig.tools().iter().all(|tool| tool.server == "fs"),
        "only explicit fs tools should be exposed: {:?}",
        outrig
            .tools()
            .iter()
            .map(|tool| (&tool.server, &tool.name))
            .collect::<Vec<_>>(),
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
        result.content_text.contains("MARKER.txt"),
        "list_directory output should mention MARKER.txt, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
}
