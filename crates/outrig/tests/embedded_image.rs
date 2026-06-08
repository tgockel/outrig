//! End-to-end coverage for OCI-label embedded MCP config.
//! Gated behind `--features e2e` because it builds fixture images and starts
//! real podman containers.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use outrig::McpClient;
use outrig::config::{ImageConfig, McpServerSpec};
use outrig::container::{Container, ContainerLaunchSpec, embedded};
use outrig::error::OutrigError;
use outrig::image::{self, ImageTag};

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture_mcp_fs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs")
}

fn fs_spec() -> McpServerSpec {
    McpServerSpec::Short(vec![
        "mcp-server-filesystem".to_string(),
        "/workspace".to_string(),
    ])
}

fn crash_spec() -> McpServerSpec {
    McpServerSpec::Short(vec![
        "node".to_string(),
        "-e".to_string(),
        "process.exit(42)".to_string(),
    ])
}

/// Dockerfile-escape a label value (backslashes first, then double quotes) so a
/// JSON value survives `LABEL "key"="value"` parsing.
fn dockerfile_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn label_line(key: &str, value: &str) -> String {
    format!("LABEL \"{key}\"=\"{}\"\n", dockerfile_escape(value))
}

fn mcp_label_json(mcp: &BTreeMap<String, McpServerSpec>) -> String {
    serde_json::to_string(mcp).expect("serialize mcp label json")
}

/// Build an alpine + filesystem-MCP fixture image carrying `labels` (stamped via
/// Dockerfile `LABEL`, mirroring what `outrig image build` stamps via
/// `--label`). An empty slice builds a label-free image.
async fn ensure_image_with_labels(labels: &[(&str, &str)]) -> ImageTag {
    let ctx = tempfile::tempdir().expect("tempdir image context");
    let mut dockerfile = String::from(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n",
    );
    for &(key, value) in labels {
        dockerfile.push_str(&label_line(key, value));
    }
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let cfg = ImageConfig {
        image_name: None,
        dockerfile: Some("Dockerfile".into()),
        context: Some(".".into()),
        build_args: BTreeMap::new(),
        security: Default::default(),
        mcp: BTreeMap::new(),
    };
    image::ensure_image(&cfg, ctx.path(), false)
        .await
        .expect("ensure embedded fixture image")
        .tag
}

/// Convenience over [`ensure_image_with_labels`] for the common case: stamp just
/// the `org.outrig.mcp` label from an mcp table.
async fn ensure_image_with_mcp(mcp: &BTreeMap<String, McpServerSpec>) -> ImageTag {
    let json = mcp_label_json(mcp);
    ensure_image_with_labels(&[(embedded::LABEL_MCP, json.as_str())]).await
}

/// Build a label-free image (the committed mcp-fs fixture has the tool binaries
/// but no `org.outrig.*` labels) -- runtime must fall back to repo config.
async fn ensure_label_free_image() -> ImageTag {
    let cfg = ImageConfig {
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

async fn start_and_bootstrap(image: &ImageTag, host_ws: &Path) -> Container {
    let mut container = Container::start(
        image,
        ContainerLaunchSpec::workspace(host_ws, Path::new("/workspace")),
    )
    .await
    .expect("Container::start");
    container.bootstrap_user().await.expect("bootstrap_user");
    container
}

async fn assert_servers_boot(
    container: &Container,
    mcp: &BTreeMap<String, McpServerSpec>,
    expected: &[&str],
) {
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");
    let mut clients = Vec::new();

    for name in expected {
        let spec = mcp.get(*name).expect("expected merged server");
        let client =
            McpClient::connect_via_podman_exec(container, spec, name, &log_dir, &BTreeMap::new())
                .await
                .unwrap_or_else(|e| panic!("connect {name}: {e:?}"));
        let tools = client
            .list_tools()
            .await
            .unwrap_or_else(|e| panic!("list tools for {name}: {e:?}"));
        assert!(
            tools.iter().any(|tool| tool.name == "list_directory"),
            "{name} should expose list_directory, got {tools:?}",
        );
        clients.push(client);
    }

    for client in clients {
        client.shutdown().await.expect("shutdown mcp client");
    }
}

#[tokio::test]
async fn image_only_embedded_mcp_boots() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), fs_spec());
    let image = ensure_image_with_mcp(&mcp).await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let merged = embedded::merged_mcp(&container, &BTreeMap::new())
        .await
        .expect("merged mcp");
    assert_eq!(
        merged.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["fs"]
    );
    assert_servers_boot(&container, &merged, &["fs"]).await;

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn config_entry_overrides_image_entry_whole() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), crash_spec());
    let image = ensure_image_with_mcp(&mcp).await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let mut config = BTreeMap::new();
    config.insert("fs".to_string(), fs_spec());
    let merged = embedded::merged_mcp(&container, &config)
        .await
        .expect("merged mcp");
    assert_eq!(merged["fs"], fs_spec());
    assert_ne!(merged["fs"], crash_spec());
    assert_servers_boot(&container, &merged, &["fs"]).await;

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn image_and_config_entries_are_additive() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), fs_spec());
    mcp.insert("shell".to_string(), fs_spec());
    let image = ensure_image_with_mcp(&mcp).await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let mut config = BTreeMap::new();
    config.insert("build".to_string(), fs_spec());
    let merged = embedded::merged_mcp(&container, &config)
        .await
        .expect("merged mcp");
    assert_eq!(
        merged.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["build", "fs", "shell"],
    );
    assert_servers_boot(&container, &merged, &["build", "fs", "shell"]).await;

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn missing_label_falls_back_to_config() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_label_free_image().await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let mut config = BTreeMap::new();
    config.insert("fs".to_string(), fs_spec());
    let merged = embedded::merged_mcp(&container, &config)
        .await
        .expect("missing org.outrig.mcp label should be empty");
    assert_eq!(merged, config);
    assert_servers_boot(&container, &merged, &["fs"]).await;

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn malformed_mcp_label_is_hard_error() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_image_with_labels(&[(embedded::LABEL_MCP, r#"{"fs": ["#)]).await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let err = embedded::merged_mcp(&container, &BTreeMap::new())
        .await
        .expect_err("malformed org.outrig.mcp label should fail");
    assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn extra_labels_do_not_disturb_mcp_read() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), fs_spec());
    let json = mcp_label_json(&mcp);
    // Metadata labels, a forward-looking schema bump, and an unrelated label
    // must all be ignored: runtime reads only `org.outrig.mcp`.
    let image = ensure_image_with_labels(&[
        (embedded::LABEL_DESCRIPTION, "Fixture image"),
        (embedded::LABEL_SCHEMA, "2"),
        (embedded::LABEL_MCP, json.as_str()),
        ("org.example.unknown", "ignored"),
    ])
    .await;
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&image, host_ws.path()).await;

    let merged = embedded::merged_mcp(&container, &BTreeMap::new())
        .await
        .expect("extra labels should be ignored");
    assert_eq!(
        merged.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["fs"]
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}
