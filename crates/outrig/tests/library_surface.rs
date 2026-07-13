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

use outrig::config::{Config, McpServerSpec};
use outrig::{
    CapabilityProfile, CapabilitySpec, EmbeddedMcpPolicy, LaunchSpec, MountAccess, MountSpec,
    NetworkAction, NetworkMode, NetworkPolicy, Outrig, SidecarSpec, SidecarWorkspaceAccess,
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

fn fs_command(path: &str) -> Vec<String> {
    vec!["mcp-server-filesystem".to_string(), path.to_string()]
}

/// Container names carrying the given `org.outrig.sidecar` label, running
/// or exited.
fn sidecar_containers_labeled(sidecar: &str) -> Vec<String> {
    let output = std::process::Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=org.outrig.sidecar={sidecar}"),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .expect("podman ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
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
async fn add_sidecar_extends_tools_and_serves_calls() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-add-sidecar-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), fs_spec("/workspace"));
    let spec = LaunchSpec::from_image(tag.clone(), mcp, session_dir.path().join("logs"))
        .with_workspace(outrig::WorkspaceSpec {
            host: host_ws.path().to_path_buf(),
            container: PathBuf::from("/workspace"),
        });

    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    let before = outrig.tools().len();

    let added = outrig
        .add_sidecar(
            SidecarSpec::from_image("tools", tag.as_str())
                .with_workspace_access(SidecarWorkspaceAccess::Ro)
                .with_server("sidefs", fs_command("/workspace")),
        )
        .await
        .expect("add_sidecar");

    assert!(
        !added.is_empty(),
        "sidecar filesystem MCP should advertise tools"
    );
    assert!(
        added.iter().all(|t| t.server == "sidefs"),
        "returned handles should belong to the sidecar's server: {added:?}"
    );
    assert_eq!(
        outrig.tools().len(),
        before + added.len(),
        "tools() should grow by exactly the returned handles"
    );

    // The sidecar sees the workspace read-only and serves calls.
    let result = outrig
        .call_tool(
            "sidefs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool via sidecar server");
    assert!(
        result.content_text.contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("tools"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

#[tokio::test]
async fn launch_with_sidecar_starts_it() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-launch-sidecar-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let spec = LaunchSpec::from_image(
        tag.clone(),
        BTreeMap::new(),
        session_dir.path().join("logs"),
    )
    .with_workspace(outrig::WorkspaceSpec {
        host: host_ws.path().to_path_buf(),
        container: PathBuf::from("/workspace"),
    })
    .with_sidecar(
        SidecarSpec::from_image("tools", tag.as_str())
            .with_workspace_access(SidecarWorkspaceAccess::Ro)
            .with_server("sidefs", fs_command("/workspace")),
    );

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    assert!(
        outrig.tools().iter().any(|t| t.server == "sidefs"),
        "launch-time sidecar servers should be in tools(): {:?}",
        outrig
            .tools()
            .iter()
            .map(|t| (&t.server, &t.name))
            .collect::<Vec<_>>(),
    );

    let result = outrig
        .call_tool(
            "sidefs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool via launch-time sidecar");
    assert!(
        result.content_text.contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn from_config_resolves_and_starts_a_config_sidecar() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-from-config-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    // `[sidecars.tools].image = "toolsimg"` names a sibling `[images.toolsimg]`
    // block, so `from_config` must resolve the config name like `--image`
    // (here to the already-local fixture tag) before it can start the sidecar.
    let config_toml = format!(
        r#"
[workspace]
host-path = "{host_ws}"
container-path = "/workspace"

[images.primary]
image-name = "{tag}"

[images.toolsimg]
image-name = "{tag}"

[images.primary.mcp]
sidefs = {{ command = ["mcp-server-filesystem", "/workspace"], sidecar = "tools" }}

[images.primary.sidecars.tools]
image = "toolsimg"
workspace = "ro"
"#,
        host_ws = host_ws.path().display(),
    );
    let config: Config = toml::from_str(&config_toml).expect("parse config");

    let spec = LaunchSpec::from_config(
        &config,
        "primary",
        repo_root.path(),
        session_dir.path().join("logs"),
    )
    .await
    .expect("from_config resolves the sidecar image and translates placement");

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    assert!(
        outrig.tools().iter().any(|t| t.server == "sidefs"),
        "config sidecar server should appear in tools(): {:?}",
        outrig
            .tools()
            .iter()
            .map(|t| (&t.server, &t.name))
            .collect::<Vec<_>>(),
    );

    let result = outrig
        .call_tool(
            "sidefs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool via config-declared sidecar server");
    assert!(
        result.content_text.contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("tools"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

#[tokio::test]
async fn failed_add_sidecar_leaves_session_usable() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-failed-add-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let mut mcp = BTreeMap::new();
    mcp.insert("fs".to_string(), fs_spec("/workspace"));
    let spec = LaunchSpec::from_image(tag.clone(), mcp, session_dir.path().join("logs"))
        .with_workspace(outrig::WorkspaceSpec {
            host: host_ws.path().to_path_buf(),
            container: PathBuf::from("/workspace"),
        });

    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    let before: Vec<(String, String)> = outrig
        .tools()
        .iter()
        .map(|t| (t.server.clone(), t.name.clone()))
        .collect();

    // A container that cannot start: nonexistent image ref.
    outrig
        .add_sidecar(
            SidecarSpec::from_image("badimg", "localhost/outrig-does-not-exist:nope")
                .with_server("s1", fs_command("/tmp")),
        )
        .await
        .expect_err("nonexistent image must fail the add");

    // A server that cannot connect: its command exits immediately.
    outrig
        .add_sidecar(SidecarSpec::from_image("badsrv", tag.as_str()).with_server(
            "s2",
            vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
        ))
        .await
        .expect_err("immediately-exiting server must fail the add");

    // The session is fully usable: tool set unchanged, calls still served.
    let after: Vec<(String, String)> = outrig
        .tools()
        .iter()
        .map(|t| (t.server.clone(), t.name.clone()))
        .collect();
    assert_eq!(before, after, "failed adds must not change tools()");
    let result = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("primary server still serves calls");
    assert!(result.content_text.contains("MARKER.txt"));

    // Failed adds leave no containers behind.
    for name in ["badimg", "badsrv"] {
        assert_eq!(
            sidecar_containers_labeled(name),
            Vec::<String>::new(),
            "failed add of {name:?} should leave no container"
        );
    }

    outrig.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn added_sidecar_egress_obeys_network_policy() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-sidecar-network-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let log_dir = session_dir.path().join("logs");

    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Deny)
        .allow_host_port("github.com", 443)
        .build()
        .expect("policy builds");
    let spec = LaunchSpec::from_image(tag.clone(), BTreeMap::new(), log_dir.clone())
        .with_workspace(outrig::WorkspaceSpec {
            host: host_ws.path().to_path_buf(),
            container: PathBuf::from("/workspace"),
        })
        .with_network_filter(policy);

    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    outrig
        .add_sidecar(
            SidecarSpec::from_image("tools", tag.as_str())
                .with_server("sidefs", fs_command("/tmp")),
        )
        .await
        .expect("add_sidecar under filter mode");

    let names = sidecar_containers_labeled("tools");
    assert_eq!(names.len(), 1, "expected one sidecar container: {names:?}");
    let sidecar = &names[0];

    // Interception attached: the sidecar resolves through the interceptor.
    let resolv = std::process::Command::new("podman")
        .args(["exec", sidecar, "cat", "/etc/resolv.conf"])
        .output()
        .expect("podman exec cat resolv.conf");
    assert!(
        String::from_utf8_lossy(&resolv.stdout).contains("nameserver 127.0.0.1"),
        "added sidecar resolv.conf should point at the interceptor: {}",
        String::from_utf8_lossy(&resolv.stdout),
    );

    // Drive denied egress from inside the added sidecar. A literal IP
    // avoids DNS; the deny-all policy intercepts the connect inside the
    // sidecar's netns (no packet leaves the host), fails the wget, and
    // writes an audit record attributed to the sidecar.
    let wget = std::process::Command::new("podman")
        .args([
            "exec",
            sidecar,
            "wget",
            "-T",
            "5",
            "-qO-",
            "http://192.0.2.1/",
        ])
        .output()
        .expect("podman exec wget");
    assert!(
        !wget.status.success(),
        "deny-all policy should fail the wget, stderr: {}",
        String::from_utf8_lossy(&wget.stderr),
    );

    let sidecar = sidecar.clone();
    outrig.shutdown().await.expect("shutdown");

    let log = std::fs::read_to_string(log_dir.join("network.jsonl")).expect("network.jsonl exists");
    assert!(
        log.lines()
            .any(|l| l.contains(&sidecar) && l.contains("192.0.2.1")),
        "network log should attribute the added sidecar's denied egress to \
         its container:\n{log}"
    );
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
