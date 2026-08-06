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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use outrig::config::{Config, McpServerSpec};
use outrig::{
    CapabilityProfile, CapabilitySpec, EmbeddedMcpPolicy, ExecOptions, LaunchSpec, MountAccess,
    MountSpec, NetworkAction, NetworkMode, NetworkPolicy, Outrig, SidecarSpec, SidecarView,
    SidecarWorkspaceAccess,
};

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The off-the-shelf MCP image the `view = "primary"` case runs -- the whole
/// point of that placement is that an unmodified third-party image works.
const MCP_FS_IMAGE: &str = "docker.io/mcp/filesystem:latest";

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

fn podman_build(tag: &str, context: &Path) {
    let status = std::process::Command::new("podman")
        .arg("build")
        .arg("-t")
        .arg(tag)
        .arg(context)
        .status()
        .expect("spawn podman build");
    assert!(status.success(), "podman build exited non-zero: {status:?}");
}

fn build_fixture_image(tag: &str) {
    podman_build(tag, &fixture_dir());
}

/// Ensure the off-the-shelf MCP image is present locally; outrig launches it
/// `--pull=never`, so the test pulls it on demand.
fn ensure_mcp_fs_image() {
    let present = std::process::Command::new("podman")
        .args(["image", "exists", MCP_FS_IMAGE])
        .status()
        .expect("podman image exists")
        .success();
    if !present {
        let status = std::process::Command::new("podman")
            .args(["pull", MCP_FS_IMAGE])
            .status()
            .expect("podman pull");
        assert!(status.success(), "failed to pull {MCP_FS_IMAGE}");
    }
}

/// A one-layer derivative of `base` that declares `entrypoint` instead of its
/// own -- the cheap way to put a chosen program shape in front of
/// `outrig-enter`, which decides how to open it before anything in the image
/// runs.
fn build_image_with_entrypoint(tag: &str, base: &str, entrypoint: &[&str]) {
    let entrypoint = entrypoint
        .iter()
        .map(|e| format!("\"{}\"", dockerfile_escape(e)))
        .collect::<Vec<_>>()
        .join(", ");
    build_image_from_dockerfile(tag, &format!("FROM {base}\nENTRYPOINT [{entrypoint}]\n"));
}

/// Build `dockerfile` as `tag` in a throwaway context. The tempdir has to
/// outlive `podman build`, which is why every caller goes through here rather
/// than handing a path around.
fn build_image_from_dockerfile(tag: &str, dockerfile: &str) {
    let ctx = tempfile::tempdir().expect("tempdir image context");
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    podman_build(tag, ctx.path());
}

/// An image with no shell anywhere on `PATH`. alpine -- the base every other
/// e2e fixture uses -- is busybox underneath, with `sh`, `cat`, and `pwd` all
/// symlinks to one static binary, so deleting the shell's names leaves the
/// rest working. The argv exec form exists precisely so an image like this
/// stays usable, and nothing else in the suite has one. No `CMD` is needed:
/// `build_podman_run_cmd` appends `sleep infinity` after the image ref.
fn build_shell_less_image(tag: &str) {
    build_image_from_dockerfile(
        tag,
        "FROM docker.io/library/alpine:latest\n\
         RUN [\"/bin/busybox\", \"rm\", \"-f\", \
         \"/bin/sh\", \"/bin/ash\", \"/bin/bash\", \"/usr/bin/sh\", \"/usr/bin/ash\"]\n",
    );
}

fn dockerfile_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn label_line(key: &str, value: &str) -> String {
    format!("LABEL \"{key}\"=\"{}\"\n", dockerfile_escape(value))
}

fn build_fixture_image_with_mcp_label(tag: &str, mcp: &BTreeMap<String, McpServerSpec>) {
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
    build_image_from_dockerfile(tag, &dockerfile);
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
        outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"),
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
        .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"));

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
    .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"))
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

/// A `LaunchSpec` built entirely in code, hosting an off-the-shelf image whose
/// ENTRYPOINT is the server: no `command` is written anywhere by the caller,
/// and the served directory rides `args`.
///
/// Deliberately the real off-the-shelf image rather than a fixture: "an
/// unmodified MCP image needs no repo-side command knowledge" is the whole
/// claim, and a fixture with a hand-written ENTRYPOINT would not test it. The
/// config path's equivalent (`mcp_sidecar_smoke.rs`) uses the `mcp-entrypoint`
/// fixture, whose `entry.sh` exits 64 on an empty argv, so the louder
/// "the args did not arrive" signal is covered there.
#[tokio::test]
async fn launch_with_entrypoint_sidecar_serves_tools() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    ensure_mcp_fs_image();
    let primary_tag = format!(
        "localhost/outrig-library-surface-entrypoint-primary-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&primary_tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    )
    .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"))
    .with_sidecar(
        SidecarSpec::from_image("served", MCP_FS_IMAGE)
            .with_workspace_access(SidecarWorkspaceAccess::Ro)
            .with_entrypoint_server("ws", ["/workspace"]),
    );

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    assert!(
        outrig.tools().iter().any(|t| t.server == "ws"),
        "entrypoint-stdio server should be in tools(): {:?}",
        outrig
            .tools()
            .iter()
            .map(|t| (&t.server, &t.name))
            .collect::<Vec<_>>(),
    );

    let result = outrig
        .call_tool(
            "ws",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool via entrypoint-stdio server");
    assert!(
        result.content_text.contains("MARKER.txt"),
        "the served directory came from `args`, got: {}",
        result.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("served"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// `view = "primary"` from the library: an off-the-shelf Alpine MCP image
/// serving the *primary* container's tree, at the primary's paths. What
/// distinguishes this from `workspace = "rw"` is that a file written into the
/// primary's own rootfs -- which no bind mount provides -- is visible too.
///
/// The upstream image declares `ENTRYPOINT ["node", "/app/dist/index.js"]` and
/// runs that way in `crates/outrig-cli/tests/primary_view_e2e.rs`; restating the
/// same program absolutely here covers the launcher's other branch, where the
/// program is opened as written and `PATH` is never consulted.
#[tokio::test]
async fn primary_view_sidecar_from_library_sees_the_primary_filesystem() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-sidecar-{}:latest",
        std::process::id(),
    );
    ensure_mcp_fs_image();
    build_image_with_entrypoint(
        &sidecar_tag,
        MCP_FS_IMAGE,
        &["/usr/local/bin/node", "/app/dist/index.js"],
    );
    let primary_tag = format!(
        "localhost/outrig-library-surface-view-primary-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&primary_tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");

    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    )
    .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"));

    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // Written into the primary's own rootfs, outside every bind mount.
    let touched = outrig
        .exec_capture(
            &[
                "sh".into(),
                "-lc".into(),
                "echo hi > /tmp/IN-PRIMARY.txt".into(),
            ],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_capture in the primary");
    assert!(
        touched.status.success(),
        "writing into the primary failed: {}",
        String::from_utf8_lossy(&touched.stderr),
    );

    outrig
        .add_sidecar(
            SidecarSpec::from_image("fs", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_entrypoint_server("fs", ["/"]),
        )
        .await
        .expect("add a view = \"primary\" sidecar");

    let workspace = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("list the primary's workspace through the view");
    assert!(
        workspace.content_text.contains("MARKER.txt"),
        "the view should show the primary's workspace, got: {}",
        workspace.content_text,
    );

    let tmp = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/tmp" }),
        )
        .await
        .expect("list the primary's own rootfs through the view");
    assert!(
        tmp.content_text.contains("IN-PRIMARY.txt"),
        "the view should show a file only the primary container has, got: {}",
        tmp.content_text,
    );

    // The launcher drops to the session's uid/gid once the graft is in place,
    // so what the server writes is the invoking user's -- not a host subuid
    // that user could not chown back. Without the drop this file lands owned by
    // the container root's mapping and the workspace tempdir cannot clean up.
    let written = outrig
        .call_tool(
            "fs",
            "write_file",
            serde_json::json!({
                "path": "/workspace/FROM-SIDECAR.txt",
                "content": "written through the primary view\n",
            }),
        )
        .await
        .expect("write into the primary's workspace through the view");
    assert!(
        !written.is_error,
        "writing into the workspace through the view failed: {}",
        written.content_text,
    );
    let host_file = host_ws.path().join("FROM-SIDECAR.txt");
    let meta = std::fs::metadata(&host_file).expect("stat the file the sidecar wrote");
    let ws_meta = std::fs::metadata(host_ws.path()).expect("stat the host workspace");
    assert_eq!(
        (meta.uid(), meta.gid()),
        (ws_meta.uid(), ws_meta.gid()),
        "a file written through the view should belong to the invoking user",
    );

    // And the capabilities are gone, not merely unused: the served root is `/`,
    // so the server's own policy allows this path and only the kernel refuses.
    let denied = outrig
        .call_tool(
            "fs",
            "write_file",
            serde_json::json!({
                "path": "/etc/outrig-privilege-probe",
                "content": "should never be written\n",
            }),
        )
        .await
        .expect("attempt a privileged write through the view");
    assert!(
        denied.is_error,
        "a root-owned path must be unwritable after the drop, got: {}",
        denied.content_text,
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("fs"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// A `view = "primary"` sidecar whose `ENTRYPOINT` names a program no `PATH`
/// entry provides fails saying exactly that. The bare `open` errno this used to
/// be ("open frobnicate: No such file or directory") reads like a missing file
/// at a path the user never wrote; naming the searched `PATH` is what turns it
/// into an actionable "that program is not in this image".
///
/// Both images are the local fixture: the launcher gives up in its `open` loop,
/// before the namespace join and long before anything in either image runs, so
/// what they contain is beside the point.
#[tokio::test]
async fn primary_view_sidecar_with_an_unresolvable_entrypoint_names_the_path() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let primary_tag = format!(
        "localhost/outrig-library-surface-view-badentry-primary-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&primary_tag);
    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-badentry-{}:latest",
        std::process::id(),
    );
    build_image_with_entrypoint(&sidecar_tag, &primary_tag, &["outrig-no-such-program"]);

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );
    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    let err = outrig
        .add_sidecar(
            SidecarSpec::from_image("badentry", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_entrypoint_server("badentry", ["/"]),
        )
        .await
        .expect_err("an entrypoint no PATH entry provides must fail the add");
    let message = err.to_string();
    assert!(
        message.contains("outrig-no-such-program") && message.contains("PATH=/"),
        "the failure should name the program and the PATH searched, got: {message}",
    );

    assert_eq!(
        sidecar_containers_labeled("badentry"),
        Vec::<String>::new(),
        "a failed add should leave no container"
    );
    outrig.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn exec_capture_runs_a_command_in_the_primary() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-exec-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(tag, BTreeMap::new(), session_dir.path().join("logs"));
    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    let out = outrig
        .exec_capture(
            &["sh".into(), "-lc".into(), "printf %s \"$GREETING\"".into()],
            &ExecOptions::new().with_env(BTreeMap::from([(
                "GREETING".to_string(),
                "hello".to_string(),
            )])),
        )
        .await
        .expect("exec_capture");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");

    // A non-zero exit is data on the Output, not an error.
    let failed = outrig
        .exec_capture(
            &["sh".into(), "-lc".into(), "exit 3".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("a failing command still ran");
    assert_eq!(failed.status.code(), Some(3));

    // The streaming form hands back the child with all three pipes open.
    let mut child = outrig
        .exec_stdio(
            &["sh".into(), "-lc".into(), "cat".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_stdio");
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(b"ping\n").await.expect("write to stdin");
        drop(stdin);
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout piped")
            .read_to_string(&mut out)
            .await
            .expect("read stdout");
        assert_eq!(out, "ping\n");
    }
    assert!(child.wait().await.expect("wait").success());

    outrig.shutdown().await.expect("shutdown");
}

/// `ExecOptions::with_workdir` against an image with no shell -- the case the
/// knob exists for. Without it a caller would have to wrap the command in
/// `sh -c 'cd ... && ...'`, which this image cannot run at all.
#[tokio::test]
async fn exec_honors_a_working_directory_without_a_shell() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-noshell-{}:latest",
        std::process::id(),
    );
    build_shell_less_image(&tag);

    // The marker goes in a *subdirectory*. The run path already sets
    // `-w /workspace`, so a test that asked for `/workspace` would pass with
    // `--workdir` deleted entirely -- it has to name a directory the container
    // would not otherwise be in.
    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    std::fs::create_dir(host_ws.path().join("sub")).expect("mkdir sub");
    std::fs::write(host_ws.path().join("sub/MARKER.txt"), "in-the-subdir\n")
        .expect("write MARKER.txt");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(tag, BTreeMap::new(), session_dir.path().join("logs"))
        .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"));
    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // The image really has no shell, which is what makes the rest meaningful.
    let no_shell = outrig
        .exec_capture(
            &["sh".into(), "-c".into(), "pwd".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("podman ran; the command inside it is what fails");
    assert!(
        !no_shell.status.success(),
        "this image is supposed to have no shell, but `sh -c pwd` succeeded: {}",
        String::from_utf8_lossy(&no_shell.stdout),
    );

    // Omitting it leaves the container where it already was -- the workspace,
    // which the run path set with `-w`. This is the baseline the next two
    // assertions have to differ from.
    let default_dir = outrig
        .exec_capture(&["pwd".into()], &ExecOptions::new())
        .await
        .expect("exec_capture pwd with no workdir");
    assert_eq!(
        String::from_utf8_lossy(&default_dir.stdout).trim(),
        "/workspace",
    );

    // The working directory takes effect: `pwd` reports the subdirectory, not
    // the `-w` the container was started with.
    let pwd = outrig
        .exec_capture(
            &["pwd".into()],
            &ExecOptions::new().with_workdir("/workspace/sub"),
        )
        .await
        .expect("exec_capture pwd");
    assert!(pwd.status.success(), "exit: {:?}", pwd.status);
    assert_eq!(
        String::from_utf8_lossy(&pwd.stdout).trim(),
        "/workspace/sub"
    );

    // ...and a relative path in the argv resolves against it, with no shell in
    // the picture to have expanded it. `MARKER.txt` exists only in the
    // subdirectory, so this fails outright if the working directory did not
    // apply.
    let marker = outrig
        .exec_capture(
            &["cat".into(), "MARKER.txt".into()],
            &ExecOptions::new().with_workdir("/workspace/sub"),
        )
        .await
        .expect("exec_capture cat");
    assert!(
        marker.status.success(),
        "reading a relative path failed: {}",
        String::from_utf8_lossy(&marker.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&marker.stdout), "in-the-subdir\n");

    // A directory the container does not have is podman's error to report,
    // and it arrives the way every other failing exec does: a non-zero status
    // with the message on stderr, not an `Err`.
    let missing = outrig
        .exec_capture(
            &["pwd".into()],
            &ExecOptions::new().with_workdir("/no/such/dir"),
        )
        .await
        .expect("podman itself ran");
    assert!(
        !missing.status.success(),
        "a nonexistent working directory should fail the exec"
    );
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("/no/such/dir"),
        "the failure should name the directory, got: {stderr}"
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

[sidecars.tools]
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
        .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"));

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
        .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"))
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
        .with_mount(MountSpec::new(
            resources.path(),
            "/resources/readonly",
            MountAccess::ReadOnly,
        ))
        .with_capabilities(CapabilitySpec::new(CapabilityProfile::NoNetRaw))
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
    .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"));

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
            .with_workspace(outrig::WorkspaceSpec::new(host_ws.path(), "/workspace"))
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
