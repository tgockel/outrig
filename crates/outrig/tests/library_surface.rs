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

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use outrig::config::{Config, EnvValue, McpServerSpec, ResolvedEnvValue};
use outrig::{
    CapabilityProfile, CapabilitySpec, EmbeddedMcpPolicy, ExecOptions, LaunchSpec, MountAccess,
    MountSpec, NetworkAction, NetworkMode, NetworkPolicy, Outrig, SecuritySpec, SidecarSpec,
    SidecarView, SidecarWorkspaceAccess,
};

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The off-the-shelf MCP image the `view = "primary"` case runs -- the whole
/// point of that placement is that an unmodified third-party image works.
const MCP_FS_IMAGE: &str = "docker.io/mcp/filesystem:latest";

/// A primary whose glibc (2.39) differs from the Debian-based sidecar's (2.36)
/// in `primary_view_sidecar_on_glibc_runs_its_own_dynamic_loader`. The mismatch
/// is the test; see there.
const UBUNTU_IMAGE: &str = "docker.io/library/ubuntu:24.04";

/// Ships no Python at all, which is what makes it the image to prove the
/// payload against.
const ALPINE_IMAGE: &str = "docker.io/library/alpine:latest";

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

/// Ensure an off-the-shelf image is present locally; outrig launches every
/// image `--pull=never`, so a stock ref has to be pulled on demand before it
/// can be named directly in a spec.
fn ensure_image(image: &str) {
    let present = std::process::Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .expect("podman image exists")
        .success();
    if !present {
        let status = std::process::Command::new("podman")
            .args(["pull", image])
            .status()
            .expect("podman pull");
        assert!(status.success(), "failed to pull {image}");
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

/// The filesystem server on a glibc base (`node:22-slim`): a `view = "primary"`
/// payload whose loader the launcher has to run by name.
///
/// The `ENTRYPOINT` names `node` absolutely and the server by its real path
/// rather than the `mcp-server-filesystem` shim, which is a shebang script and
/// so refused before any of this is reached.
fn build_glibc_fs_sidecar_image(tag: &str) {
    build_image_from_dockerfile(
        tag,
        "FROM docker.io/library/node:22-slim\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         ENTRYPOINT [\"/usr/local/bin/node\", \
         \"/usr/local/lib/node_modules/@modelcontextprotocol/server-filesystem/dist/index.js\"]\n",
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

/// Names of the volumes mounted into `container`, read off the engine rather
/// than the argv outrig sent: an image's `VOLUME` gets one from podman without
/// any flag asking for it.
fn volume_mounts_of(container: &str) -> Vec<String> {
    let output = std::process::Command::new("podman")
        .args([
            "container",
            "inspect",
            "--format",
            "{{range .Mounts}}{{if eq .Type \"volume\"}}{{.Name}} {{end}}{{end}}",
            container,
        ])
        .output()
        .expect("podman container inspect");
    assert!(
        output.status.success(),
        "podman container inspect {container} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
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
        result.render_text().contains("MARKER.txt"),
        "list_directory output should mention MARKER.txt, got: {}",
        result.render_text(),
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
        result.render_text().contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.render_text(),
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
        result.render_text().contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.render_text(),
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

    ensure_image(MCP_FS_IMAGE);
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
        result.render_text().contains("MARKER.txt"),
        "the served directory came from `args`, got: {}",
        result.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("served"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// An image's `VOLUME` gets no volume. Podman would make an anonymous one per
/// declaration, and an entrypoint-stdio container's outlived it: the
/// `podman start --attach` client that acts on `--rm` removes the container
/// without its volumes, and each one left behind held a podman lock until no
/// launch could get one. The sidecar is #214's repro, the stock filesystem
/// image plus `VOLUME /data`.
#[tokio::test]
async fn entrypoint_sidecar_gets_no_volume_for_its_image_volume() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let sidecar_tag = format!(
        "localhost/outrig-library-surface-volume-sidecar-{}:latest",
        std::process::id(),
    );
    build_image_from_dockerfile(
        &sidecar_tag,
        &format!("FROM {MCP_FS_IMAGE}\nVOLUME /data\n"),
    );
    // Stock: nothing here asks anything of the primary but that it run.
    ensure_image(UBUNTU_IMAGE);

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        UBUNTU_IMAGE,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    )
    .with_sidecar(
        SidecarSpec::from_image("volumed", sidecar_tag.as_str())
            .with_entrypoint_server("fs", ["/tmp"]),
    );

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    let sidecars = sidecar_containers_labeled("volumed");
    assert_eq!(sidecars.len(), 1, "one sidecar container: {sidecars:?}");
    assert_eq!(
        volume_mounts_of(&sidecars[0]),
        Vec::<String>::new(),
        "the image's `VOLUME /data` got a volume, which outlives the sidecar"
    );

    outrig.shutdown().await.expect("shutdown");
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
    ensure_image(MCP_FS_IMAGE);
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
        workspace.render_text().contains("MARKER.txt"),
        "the view should show the primary's workspace, got: {}",
        workspace.render_text(),
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
        tmp.render_text().contains("IN-PRIMARY.txt"),
        "the view should show a file only the primary container has, got: {}",
        tmp.render_text(),
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
        written.render_text(),
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
        denied.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("fs"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// `view = "primary"` with a **glibc** sidecar against a primary carrying a
/// *differently versioned* glibc -- the pairing that caught `outrig-enter`
/// exec'ing the wrong dynamic loader.
///
/// `setns(CLONE_NEWNS)` moves the launcher's root directory, so an absolute
/// symlink met under the graft afterwards resolves in the *primary's* rootfs.
/// Debian's `/lib64/ld-linux-x86-64.so.2` is exactly that (Ubuntu's is
/// relative), so `execv("/mnt/lib64/ld-linux-x86-64.so.2")` ran Ubuntu's
/// ld.so 2.39 against Debian's libc 2.36 -- one version-locked unit, mismatched
/// -- and the payload died on SIGSEGV before writing a byte to stderr.
///
/// Nothing else in the suite can fail this way. Every other `view = "primary"`
/// test runs an Alpine sidecar, whose `/lib/ld-musl-x86_64.so.1` is a real file
/// with no link to escape through, and a Debian sidecar over the existing
/// `rust:1-slim` primary would pair 2.36 with 2.36 and work by accident. The
/// two glibcs have to differ, which is why the primary here is `ubuntu:24.04`
/// rather than a fixture.
///
/// The assertion is on the *success*: the server handshakes and serves the
/// primary's tree. Asserting on exit 139 would pass for the wrong reason.
///
/// What the view itself shows is the test above's job, so this one asserts only
/// the part that used to segfault plus one listing to prove the payload really
/// is serving the primary.
#[tokio::test]
async fn primary_view_sidecar_on_glibc_runs_its_own_dynamic_loader() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-glibc-sidecar-{}:latest",
        std::process::id(),
    );
    // The absolute `/lib64` interpreter symlink comes free with the base image.
    build_glibc_fs_sidecar_image(&sidecar_tag);
    // Stock and unmodified: the primary contributes only its glibc version, so
    // a derived image would be a build that adds no layer.
    ensure_image(UBUNTU_IMAGE);

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        UBUNTU_IMAGE,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );

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

    // The add is where the segfault surfaced: the payload died during the MCP
    // handshake, so `add_sidecar` failed with "connection closed" and exit 139.
    let tools = outrig
        .add_sidecar(
            SidecarSpec::from_image("glibcfs", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_entrypoint_server("glibcfs", ["/"]),
        )
        .await
        .expect("a glibc sidecar must run its own loader, not the primary's");
    assert!(
        !tools.is_empty(),
        "the glibc sidecar should have served at least one tool",
    );

    let tmp = outrig
        .call_tool(
            "glibcfs",
            "list_directory",
            serde_json::json!({ "path": "/tmp" }),
        )
        .await
        .expect("list the primary's own rootfs through the view");
    assert!(
        tmp.render_text().contains("IN-PRIMARY.txt"),
        "the view should show a file only the primary container has, got: {}",
        tmp.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("glibcfs"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// What glibc's loader prints, on stderr and before `main`, for each entry of
/// `/etc/ld.so.preload` it cannot load. The `from` and the path pin it to that
/// file; an `LD_PRELOAD` entry says `from LD_PRELOAD`.
const PRELOAD_FILE_ERROR: &str = "from /etc/ld.so.preload";

/// What the filesystem server has written to `path` once it has announced
/// itself, which it does only after its transport is connected. The dynamic
/// loader's own reports come before `main`, so by then they are in the file if
/// they are coming at all -- which is what lets the caller assert one is
/// absent. Polled: the file is filled by a separate `podman start --attach`.
async fn stderr_once_running(path: &Path) -> String {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains("running on stdio")
        {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "{} never showed the server's startup line: {:?}",
        path.display(),
        std::fs::read_to_string(path),
    );
}

/// `view = "primary"` with a **glibc** sidecar over a primary that ships
/// `/etc/ld.so.preload` (#268).
///
/// glibc's loader opens that file on every start, by that absolute name, and no
/// loader option turns that off. `outrig-enter` runs the payload through the
/// sidecar's own loader *inside* the primary's mount namespace, so the name is
/// the primary's file: every object it lists was loaded into the sidecar's
/// server before `main`, with the server's environment and network, and one
/// built for a newer glibc than the sidecar's stopped the server from starting
/// at all.
///
/// The primary here lists an object that does not exist. The loader reports
/// that on stderr and carries on, so it is the same read, visible without
/// compiling a library. That makes the primary's own processes the control --
/// they must print it, or the fixture proves nothing -- and the server's stderr
/// the thing under test.
///
/// The pairing is the neighbor test's, `ubuntu:24.04` against `node:22-slim`: a
/// glibc that differs from the sidecar's is the realistic shape of the failure.
#[tokio::test]
async fn primary_view_glibc_sidecar_does_not_load_the_primarys_ld_so_preload() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-preload-sidecar-{}:latest",
        std::process::id(),
    );
    build_glibc_fs_sidecar_image(&sidecar_tag);
    let primary_tag = format!(
        "localhost/outrig-library-surface-view-preload-primary-{}:latest",
        std::process::id(),
    );
    ensure_image(UBUNTU_IMAGE);
    build_image_from_dockerfile(
        &primary_tag,
        &format!(
            "FROM {UBUNTU_IMAGE}\n\
             RUN echo /opt/from-the-primary/missing.so > /etc/ld.so.preload\n"
        ),
    );

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );
    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // The control. Without it a pass could mean the fixture never took effect,
    // or that the loader words its report differently from what the assertions
    // below look for.
    let control = outrig
        .exec_capture(&["true".into()], &ExecOptions::new())
        .await
        .expect("exec_capture in the primary");
    assert!(
        String::from_utf8_lossy(&control.stderr).contains(PRELOAD_FILE_ERROR),
        "the primary's own loader should report its preload file, got: {}",
        String::from_utf8_lossy(&control.stderr),
    );

    outrig
        .add_sidecar(
            SidecarSpec::from_image("preloadfs", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_entrypoint_server("preloadfs", ["/"]),
        )
        .await
        .expect("a glibc sidecar must start over a primary that ships /etc/ld.so.preload");

    // What the server's loader said about it.
    let stderr = stderr_once_running(&session_dir.path().join("logs/preloadfs.stderr")).await;
    assert!(
        !stderr.contains(PRELOAD_FILE_ERROR),
        "the server's loader should not have read the primary's preload file, got: {stderr}",
    );

    // And what the server itself finds at that path.
    let seen = outrig
        .call_tool(
            "preloadfs",
            "read_file",
            serde_json::json!({ "path": "/etc/ld.so.preload" }),
        )
        .await
        .expect("read the preload file through the view");
    assert!(
        !seen.is_error && seen.render_text().trim().is_empty(),
        "the server should read the primary's preload file as empty, got: {}",
        seen.render_text(),
    );

    // The mask is the launcher's own, in its own mount namespace: the primary
    // keeps its file.
    let kept = outrig
        .exec_capture(
            &["cat".into(), "/etc/ld.so.preload".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_capture in the primary");
    assert!(
        String::from_utf8_lossy(&kept.stdout).contains("/opt/from-the-primary/missing.so"),
        "the primary should still have its own preload file, got: {}",
        String::from_utf8_lossy(&kept.stdout),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("preloadfs"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// The mask over `/etc/ld.so.preload` does not depend on the launcher being
/// able to read the file (#600).
///
/// The launcher is the sidecar image's user, here root, holding only the
/// capabilities the sidecar was given. With `DAC_OVERRIDE` dropped it cannot
/// open a file owned by the session user with mode `0600`, while the payload,
/// which runs as that user, can -- so a probe that read "cannot open" as "not
/// there" skipped the mask and the server preloaded the primary's file.
#[tokio::test]
async fn primary_view_sidecar_without_dac_override_masks_an_unreadable_ld_so_preload() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-preload-eacces-sidecar-{}:latest",
        std::process::id(),
    );
    build_glibc_fs_sidecar_image(&sidecar_tag);
    let primary_tag = format!(
        "localhost/outrig-library-surface-view-preload-eacces-primary-{}:latest",
        std::process::id(),
    );
    ensure_image(UBUNTU_IMAGE);

    // The session user is the invoking host user, so the file is theirs by id.
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let meta = std::fs::metadata(session_dir.path()).expect("stat session dir");
    build_image_from_dockerfile(
        &primary_tag,
        &format!(
            "FROM {UBUNTU_IMAGE}\n\
             RUN echo /opt/from-the-primary/missing.so > /etc/ld.so.preload \
             && chown {}:{} /etc/ld.so.preload && chmod 0600 /etc/ld.so.preload\n",
            meta.uid(),
            meta.gid(),
        ),
    );

    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );
    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // The control: the session user's own loader reads the file.
    let control = outrig
        .exec_capture(&["true".into()], &ExecOptions::new())
        .await
        .expect("exec_capture in the primary");
    assert!(
        String::from_utf8_lossy(&control.stderr).contains(PRELOAD_FILE_ERROR),
        "the primary's own loader should report its preload file, got: {}",
        String::from_utf8_lossy(&control.stderr),
    );

    let mut security = SecuritySpec::default();
    security.capabilities.cap_drop = vec!["DAC_OVERRIDE".to_string()];
    outrig
        .add_sidecar(
            SidecarSpec::from_image("preloadfs", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_security(security)
                .with_entrypoint_server("preloadfs", ["/"]),
        )
        .await
        .expect("a sidecar without DAC_OVERRIDE must start over the primary");

    let stderr = stderr_once_running(&session_dir.path().join("logs/preloadfs.stderr")).await;
    assert!(
        !stderr.contains(PRELOAD_FILE_ERROR),
        "the server's loader should not have read the primary's preload file, got: {stderr}",
    );

    outrig.shutdown().await.expect("shutdown");
}

/// The musl counterpart of the test above, pinning what musl's loader does with
/// the primary's files: nothing, for the one that matters.
///
/// musl has no `/etc/ld.so.preload`. Its one startup file is
/// `/etc/ld-musl-<arch>.path`, found relative to the *loader's own path* --
/// `/mnt/lib/ld-musl-<arch>.so.1` here, so `/mnt/etc/ld-musl-<arch>.path`, the
/// sidecar's -- and not the primary's.
///
/// The payload is `xz`, which needs `liblzma.so.5`. The sidecar moves that
/// library out of every directory its loader searches, so on its own `xz` dies
/// with `Error loading shared library liblzma.so.5`. The primary has the
/// library only in `/opt/pmark`, which its path file names, and nowhere the
/// loader's built-in directories reach. Were the primary's path file read, the
/// loader would find the library there and `xz` would start instead.
#[tokio::test]
async fn primary_view_musl_sidecar_does_not_read_the_primarys_library_path_file() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let primary_tag = format!(
        "localhost/outrig-library-surface-view-pathfile-primary-{}:latest",
        std::process::id(),
    );
    // The arch in the file's name is musl's own spelling, which `uname -m`
    // matches on both x86-64 and aarch64.
    build_image_from_dockerfile(
        &primary_tag,
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache xz-libs \
         && mkdir -p /opt/pmark \
         && mv /usr/lib/liblzma.so.5* /opt/pmark/ \
         && echo /opt/pmark:/lib:/usr/local/lib:/usr/lib > /etc/ld-musl-$(uname -m).path\n",
    );
    let sidecar_tag = format!(
        "localhost/outrig-library-surface-view-pathfile-sidecar-{}:latest",
        std::process::id(),
    );
    build_image_from_dockerfile(
        &sidecar_tag,
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache xz \
         && mkdir /opt/hidden \
         && mv /usr/lib/liblzma.so.5* /opt/hidden/\n\
         ENTRYPOINT [\"/usr/bin/xz\"]\n",
    );

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        primary_tag,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );
    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // `xz` is not an MCP server, so the add fails either way; what the
    // container wrote says why. The loader's error is the pass, `xz` having
    // started is the fail.
    outrig
        .add_sidecar(
            SidecarSpec::from_image("pathfile", sidecar_tag.as_str())
                .with_view(SidecarView::Primary)
                .with_entrypoint_server("pathfile", ["/"]),
        )
        .await
        .expect_err("a library only the primary's path file names must not be found");
    let stderr = std::fs::read_to_string(session_dir.path().join("logs/pathfile.stderr"))
        .expect("the server's stderr file");
    assert!(
        stderr.contains("Error loading shared library liblzma.so.5"),
        "the payload should fail to find liblzma.so.5, got: {stderr:?}",
    );

    assert_eq!(
        sidecar_containers_labeled("pathfile"),
        Vec::<String>::new(),
        "a failed add should leave no container"
    );
    outrig.shutdown().await.expect("shutdown");
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
    // The launcher exits within milliseconds, which is when `podman start --attach` can drop
    // what the container wrote (#422), so this is what the container's own log is there for.
    // The file the error cites, and `outrig logs` reads, holds the same text.
    let stderr = std::fs::read_to_string(session_dir.path().join("logs/badentry.stderr"))
        .expect("the server's stderr file");
    assert!(
        stderr.contains("outrig-no-such-program") && stderr.contains("PATH=/"),
        "badentry.stderr should hold the launcher's line, got: {stderr:?}",
    );

    assert_eq!(
        sidecar_containers_labeled("badentry"),
        Vec::<String>::new(),
        "a failed add should leave no container"
    );
    outrig.shutdown().await.expect("shutdown");
}

/// `argv` in the primary, which must at least have started.
async fn capture(outrig: &Outrig, argv: &[&str]) -> std::process::Output {
    let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
    outrig
        .exec_capture(&argv, &ExecOptions::new())
        .await
        .unwrap_or_else(|e| panic!("exec {argv:?}: {e}"))
}

/// Every session gets OutRig's interpreter whatever the image holds. The
/// first assertion is what makes the rest mean anything: an image that
/// carried its own Python would pass them without the payload.
#[tokio::test]
async fn the_python_payload_runs_in_an_image_with_no_python() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();
    ensure_image(ALPINE_IMAGE);

    let session_dir = tempfile::tempdir().expect("tempdir session");
    let spec = LaunchSpec::from_image(
        ALPINE_IMAGE,
        BTreeMap::new(),
        session_dir.path().join("logs"),
    );
    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    let image_python = capture(
        &outrig,
        &[
            "sh",
            "-c",
            "for p in python3 python; do command -v \"$p\" && exit 1; done; \
             for d in /usr/lib/python3* /usr/local/lib/python3*; do [ -e \"$d\" ] && exit 1; done; \
             exit 0",
        ],
    )
    .await;
    assert!(
        image_python.status.success(),
        "the image has a Python of its own: {}",
        String::from_utf8_lossy(&image_python.stdout)
    );

    let printed = capture(
        &outrig,
        &["/outrig/python/bin/python3", "-I", "-c", "print(1)"],
    )
    .await;
    assert!(
        printed.status.success(),
        "payload python failed: {}",
        String::from_utf8_lossy(&printed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&printed.stdout), "1\n");

    let written = capture(&outrig, &["touch", "/outrig/python/probe"]).await;
    assert!(
        !written.status.success(),
        "the payload mount accepted a write"
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

    // A value resolved from a `${VAR}` reference goes to podman as a bare
    // `--env KEY`, which podman fills from its own environment.
    let var = "OUTRIG_E2E_LIBRARY_SURFACE_REFERENCED";
    // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
    // races; no other test reads or writes this name.
    unsafe { std::env::set_var(var, "by-name") };
    let referenced =
        ResolvedEnvValue::resolve(EnvValue::EnvRef(var.to_string())).expect("the variable is set");
    let out = outrig
        .exec_capture(
            &["sh".into(), "-lc".into(), "printf %s \"$GREETING\"".into()],
            &ExecOptions::new()
                .with_resolved_env(BTreeMap::from([("GREETING".to_string(), referenced)])),
        )
        .await
        .expect("exec_capture");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "by-name");

    // A non-zero exit is data on the Output, not an error.
    let failed = outrig
        .exec_capture(
            &["sh".into(), "-lc".into(), "exit 3".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("a failing command still ran");
    assert_eq!(failed.status.code(), Some(3));

    // The command's stdin is empty: one that reads it gets end-of-file.
    let read_stdin = outrig
        .exec_capture(&["cat".into()], &ExecOptions::new())
        .await
        .expect("a command reading stdin still ran");
    assert!(read_stdin.status.success(), "exit: {:?}", read_stdin.status);
    assert_eq!(String::from_utf8_lossy(&read_stdin.stdout), "");

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
        result.render_text().contains("MARKER.txt"),
        "sidecar list_directory should see the workspace, got: {}",
        result.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("tools"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// A primary image whose `org.outrig.mcp` label declares `fs` -- the server the
/// config below moves into a sidecar -- and `docs`, which it leaves alone. The
/// label's `fs` serves `/tmp` and the sidecar's serves `/workspace`, so which of
/// the two answered a call shows in what it can list.
fn build_primary_image_embedding_fs_and_docs(tag: &str) {
    let embedded_mcp = BTreeMap::from([
        ("fs".to_string(), fs_spec("/tmp")),
        ("docs".to_string(), fs_spec("/workspace")),
    ]);
    build_fixture_image_with_mcp_label(tag, &embedded_mcp);
}

/// The config both placement tests lower: the primary's `fs` is placed in
/// `[sidecars.tools]`, whose `start` is the one thing they differ in.
fn placed_fs_config(host_ws: &Path, primary_tag: &str, tools_tag: &str, start: &str) -> Config {
    let config_toml = format!(
        r#"
[workspace]
host-path = "{host_ws}"
container-path = "/workspace"

[images.primary]
image-name = "{primary_tag}"

[images.toolsimg]
image-name = "{tools_tag}"

[images.primary.mcp]
fs = {{ command = ["mcp-server-filesystem", "/workspace"], sidecar = "tools" }}

[sidecars.tools]
image = "toolsimg"
workspace = "ro"
start = "{start}"
"#,
        host_ws = host_ws.display(),
    );
    let config: Config = toml::from_str(&config_toml).expect("parse config");
    config
        .validate(None)
        .expect("the placement config is valid");
    config
}

/// A server the config places in a sidecar is served from that sidecar alone:
/// the primary image's own `org.outrig.mcp` entry of the same name is not
/// imported beside it, which used to abort the launch at `add_sidecar` with
/// "already connected". The image's unrelated entries still merge.
#[tokio::test]
async fn from_config_placement_wins_over_the_primary_images_embedded_server() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let pid = std::process::id();
    let primary_tag = format!("localhost/outrig-library-surface-placed-primary-{pid}:latest");
    let tools_tag = format!("localhost/outrig-library-surface-placed-tools-{pid}:latest");
    build_primary_image_embedding_fs_and_docs(&primary_tag);
    build_fixture_image(&tools_tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");
    let config = placed_fs_config(host_ws.path(), &primary_tag, &tools_tag, "auto");

    let spec = LaunchSpec::from_config(
        &config,
        "primary",
        repo_root.path(),
        session_dir.path().join("logs"),
    )
    .await
    .expect("from_config");
    // The premise: the config moved `fs` out of the primary, into a sidecar the
    // spec carries.
    assert!(
        !spec.mcp.contains_key("fs"),
        "`fs` is sidecar-placed, so not in the primary map: {:?}",
        spec.mcp.keys().collect::<Vec<_>>(),
    );
    assert_eq!(
        spec.sidecars
            .iter()
            .filter(|sidecar| sidecar.servers.contains_key("fs"))
            .count(),
        1,
        "exactly one sidecar hosts `fs`",
    );

    let outrig = Outrig::launch(&spec).await.expect(
        "launch must not import the image's own `fs` when the config placed `fs` in a sidecar",
    );
    let servers: BTreeSet<&str> = outrig
        .tools()
        .iter()
        .map(|tool| tool.server.as_str())
        .collect();
    assert_eq!(
        servers,
        BTreeSet::from(["docs", "fs"]),
        "the sidecar's `fs` and the image's unrelated `docs`",
    );

    // `fs` can list `/workspace` only if the one connected is the sidecar's: the
    // image's own entry serves `/tmp`. `docs` is the primary's and serves it too.
    for server in ["fs", "docs"] {
        let result = outrig
            .call_tool(
                server,
                "list_directory",
                serde_json::json!({ "path": "/workspace" }),
            )
            .await
            .unwrap_or_else(|e| panic!("call_tool {server}: {e}"));
        assert!(
            result.render_text().contains("MARKER.txt"),
            "{server} list_directory should see the workspace, got: {}",
            result.render_text(),
        );
    }

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("tools"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// A `start = "manual"` placement is not imported into the primary either.
/// `from_config` neither starts nor carries that sidecar, but its server still
/// belongs to it: the primary image's entry of the same name stays out, and the
/// name is free when `add_sidecar` starts the sidecar later.
#[tokio::test]
async fn from_config_manual_placement_is_not_imported_into_the_primary() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let pid = std::process::id();
    let primary_tag = format!("localhost/outrig-library-surface-manual-primary-{pid}:latest");
    let tools_tag = format!("localhost/outrig-library-surface-manual-tools-{pid}:latest");
    build_primary_image_embedding_fs_and_docs(&primary_tag);
    build_fixture_image(&tools_tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");
    std::fs::write(host_ws.path().join("MARKER.txt"), "hi\n").expect("write MARKER.txt");
    let config = placed_fs_config(host_ws.path(), &primary_tag, &tools_tag, "manual");

    let spec = LaunchSpec::from_config(
        &config,
        "primary",
        repo_root.path(),
        session_dir.path().join("logs"),
    )
    .await
    .expect("from_config");
    assert!(spec.sidecars.is_empty(), "a manual sidecar is not carried");

    let mut outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    let servers: BTreeSet<&str> = outrig
        .tools()
        .iter()
        .map(|tool| tool.server.as_str())
        .collect();
    assert_eq!(
        servers,
        BTreeSet::from(["docs"]),
        "the image's `fs` belongs to the manual sidecar, so only `docs` is in the primary",
    );
    outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .map(|_| ())
        .expect_err("nothing serves `fs` until the manual sidecar is added");

    outrig
        .add_sidecar(
            SidecarSpec::from_image("tools", tools_tag.as_str())
                .with_workspace_access(SidecarWorkspaceAccess::Ro)
                .with_server("fs", fs_command("/workspace")),
        )
        .await
        .expect("add_sidecar: the name `fs` is still free");
    let result = outrig
        .call_tool(
            "fs",
            "list_directory",
            serde_json::json!({ "path": "/workspace" }),
        )
        .await
        .expect("call_tool via the added sidecar");
    assert!(
        result.render_text().contains("MARKER.txt"),
        "the added sidecar's `fs` should see the workspace, got: {}",
        result.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
    assert_eq!(
        sidecar_containers_labeled("tools"),
        Vec::<String>::new(),
        "shutdown should remove the sidecar container"
    );
}

/// The other half of `tests/launch_spec_from_config.rs`: that file asserts
/// `from_config` puts the config's `[network]` mode on the spec, this one
/// asserts the spec reaches actual enforcement. A config declaring `audit`
/// used to lower as `default`, so the session ran with no interceptor at all
/// and nothing said so.
#[tokio::test]
async fn from_config_audit_mode_starts_the_interceptor() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let tag = format!(
        "localhost/outrig-library-surface-from-config-audit-{}:latest",
        std::process::id(),
    );
    build_fixture_image(&tag);

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");

    let config_toml = format!(
        r#"
[workspace]
host-path = "{host_ws}"
container-path = "/workspace"

[network]
mode = "audit"

[images.primary]
image-name = "{tag}"
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
    .expect("from_config");
    assert_eq!(
        spec.network.mode,
        NetworkMode::Audit,
        "the config's mode must survive the lowering"
    );

    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    // The same marker `added_sidecar_egress_obeys_network_policy` uses for
    // "interception is live": the container resolves through the interceptor
    // rather than the host's resolver.
    let resolv = outrig
        .exec_capture(
            &["cat".into(), "/etc/resolv.conf".into()],
            &ExecOptions::new(),
        )
        .await
        .expect("cat /etc/resolv.conf");
    let text = String::from_utf8_lossy(&resolv.stdout);
    assert!(
        text.contains("nameserver 127.0.0.1"),
        "a config-declared audit mode should have attached the interceptor, \
         but resolv.conf points elsewhere: {text}",
    );

    outrig.shutdown().await.expect("shutdown");
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
    assert!(result.render_text().contains("MARKER.txt"));

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
        result.render_text().contains("REFERENCE.txt"),
        "list_directory output should mention REFERENCE.txt, got: {}",
        result.render_text(),
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
        result.render_text().contains("MARKER.txt"),
        "list_directory output should mention MARKER.txt, got: {}",
        result.render_text(),
    );

    outrig.shutdown().await.expect("shutdown");
}

/// `from_config` pulls an `image-name` the host does not have, as `outrig
/// run` would, instead of leaving `launch` to fail with `image not known`.
#[tokio::test]
async fn from_config_pulls_an_absent_image_name_primary() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let image = "docker.io/library/alpine:3.20";
    // Absent is the precondition. No `--force`: that would also remove any
    // container using the image, and this is the contributor's own store. A
    // plain `rmi` refuses when one does, and the test then has nothing to say.
    let _ = std::process::Command::new("podman")
        .args(["rmi", image])
        .output();
    let present = std::process::Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .expect("spawn podman image exists")
        .success();
    if present {
        eprintln!("skipping: {image} is in use and cannot be removed without --force");
        return;
    }

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");
    let config: Config = toml::from_str(&format!(
        r#"
[workspace]
host-path = "{host_ws}"
container-path = "/workspace"

[images.probe]
image-name = "{image}"
"#,
        host_ws = host_ws.path().display(),
    ))
    .expect("parse config");

    let spec = LaunchSpec::from_config(
        &config,
        "probe",
        repo_root.path(),
        session_dir.path().join("logs"),
    )
    .await
    .expect("from_config pulls the image");
    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");
    outrig.shutdown().await.expect("shutdown");
}

/// A Dockerfile image-config is built under its own name -- the tag `outrig
/// run` and `build` use -- not under `outrig-cache`.
#[tokio::test]
async fn from_config_builds_a_dockerfile_primary_under_its_config_name() {
    let _guard = E2E_LOCK.lock().await;
    init_tracing();

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let session_dir = tempfile::tempdir().expect("tempdir session");
    let repo_root = tempfile::tempdir().expect("tempdir repo_root");
    std::fs::write(
        repo_root.path().join("Dockerfile"),
        format!(
            "FROM docker.io/library/alpine:latest\n# marker: {}\n",
            std::process::id()
        ),
    )
    .expect("write Dockerfile");
    let config: Config = toml::from_str(&format!(
        r#"
[workspace]
host-path = "{host_ws}"
container-path = "/workspace"

[images.shared]
dockerfile = "Dockerfile"
context = "."
"#,
        host_ws = host_ws.path().display(),
    ))
    .expect("parse config");

    let spec = LaunchSpec::from_config(
        &config,
        "shared",
        repo_root.path(),
        session_dir.path().join("logs"),
    )
    .await
    .expect("from_config builds the image");
    let outrig = Outrig::launch(&spec).await.expect("Outrig::launch");

    let cfg = config.images.get("shared").expect("shared image-config");
    let tag = outrig::image::compute_tag_for("shared", cfg, repo_root.path())
        .await
        .expect("compute_tag_for");
    let status = std::process::Command::new("podman")
        .args(["image", "exists", tag.as_str()])
        .status()
        .expect("spawn podman image exists");
    assert!(status.success(), "{tag} should exist after from_config");

    outrig.shutdown().await.expect("shutdown");
}
