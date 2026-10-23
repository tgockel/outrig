//! End-to-end coverage for OCI-label embedded MCP config.
//! Gated behind `--features e2e` because it builds fixture images and starts
//! real podman containers.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use outrig::config::{EnvValue, ImageConfig, McpServerSpec};
use outrig::container::{Container, ContainerLaunchSpec, embedded};
use outrig::error::OutrigError;
use outrig::image::{self, ImageTag};
use outrig::{EmbeddedMcpPolicy, LaunchSpec, McpClient, Outrig};

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

/// Tempdir image context whose Dockerfile carries only a malformed
/// `org.outrig.mcp` label on the alpine base -- the parse failure under test
/// never touches the layer contents, so no tooling is installed.
fn malformed_label_context() -> tempfile::TempDir {
    let ctx = tempfile::tempdir().expect("tempdir image context");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n{}",
        label_line(embedded::LABEL_MCP, r#"{"fs": ["#),
    );
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    ctx
}

/// `ensure_image` over a Build-source config rooted at `project_dir` (which
/// must contain a `Dockerfile`). Returns the result so tests can assert on
/// build-time failures.
async fn try_ensure_built_image(project_dir: &Path) -> outrig::error::Result<ImageTag> {
    let cfg = common::fixture_build_config();
    Ok(image::ensure_image(&cfg, project_dir, false).await?.tag)
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
    try_ensure_built_image(ctx.path())
        .await
        .expect("ensure embedded fixture image")
}

/// Convenience over [`ensure_image_with_labels`] for the common case: stamp just
/// the `org.outrig.mcp` label from an mcp table.
async fn ensure_image_with_mcp(mcp: &BTreeMap<String, McpServerSpec>) -> ImageTag {
    let json = mcp_label_json(mcp);
    ensure_image_with_labels(&[(embedded::LABEL_MCP, json.as_str())]).await
}

/// Build the fixture image under a *fixed* `tag`, carrying `mcp_label` as its
/// `org.outrig.mcp`. Called twice with the same tag, the second build moves
/// that tag to a different image, as a pull of a re-pushed ref would.
///
/// Deliberately not [`ensure_image_with_mcp`], whose tags are content-addressed
/// and so can never move: the point here is the tag that can.
async fn build_labeled_image(tag: &ImageTag, mcp_label: &str) {
    let ctx = tempfile::tempdir().expect("tempdir image context");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache nodejs npm shadow\n\
         RUN npm install -g @modelcontextprotocol/server-filesystem\n\
         {}",
        label_line(embedded::LABEL_MCP, mcp_label),
    );
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    image::build_standalone(
        ctx.path(),
        Path::new("Dockerfile"),
        Path::new("."),
        tag,
        false,
        &BTreeMap::new(),
    )
    .await
    .expect("build fixture image under a fixed tag");
}

/// Build a label-free image (the committed mcp-fs fixture has the tool binaries
/// but no `org.outrig.*` labels) -- runtime must fall back to repo config.
async fn ensure_label_free_image() -> ImageTag {
    try_ensure_built_image(&fixture_mcp_fs_dir())
        .await
        .expect("ensure mcp-fs fixture image")
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
async fn malformed_mcp_label_fails_ensure_image() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // Repo builds re-merge inherited labels into the cache tag
    // (stamp_repo_image_labels), so a malformed org.outrig.mcp label fails the
    // build itself rather than the later runtime read.
    let ctx = malformed_label_context();

    let err = try_ensure_built_image(ctx.path())
        .await
        .expect_err("malformed org.outrig.mcp label should fail ensure_image");
    assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
    assert_names_the_image_and_the_override(&err, "<image>");
}

#[tokio::test]
async fn malformed_mcp_label_error_names_the_image_config() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // The session path builds through `ensure_tagged_image_for`, a separate
    // builder from `ensure_image`'s, and is the one that knows the
    // image-config by name.
    let ctx = malformed_label_context();
    let cfg = common::fixture_build_config();
    let tag = image::compute_tag_for("coding", &cfg, ctx.path())
        .await
        .expect("compute tag");

    let err = image::ensure_tagged_image_for("coding", &cfg, ctx.path(), &tag, false, None)
        .await
        .expect_err("malformed org.outrig.mcp label should fail the build");
    assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));
    assert_names_the_image_and_the_override(&err, "coding");
}

/// The build-time failure names the image-config, not the temporary tag it
/// read the label from, and says how to replace the label.
fn assert_names_the_image_and_the_override(err: &OutrigError, image: &str) {
    let message = err.to_string();
    assert!(
        message.starts_with(&format!("image {image:?} embedded config:")),
        "{message}"
    );
    assert!(!message.contains("outrig-tmp-"), "{message}");
    assert!(
        message.contains("LABEL org.outrig.mcp='{}'"),
        "the message should say how to override the label: {message}"
    );
}

#[tokio::test]
async fn malformed_mcp_label_on_raw_image_fails_runtime_read() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    // A raw (non-built) image ref skips the build path's label re-merge, so a
    // malformed label must still hard-error at the runtime read. The standalone
    // build stamps nothing beyond the Dockerfile's own labels, and the fixed
    // tag is rebuilt on every run, so nothing accumulates in local storage.
    let ctx = malformed_label_context();
    let tag = ImageTag::new("outrig-raw-malformed:e2e");
    image::build_standalone(
        ctx.path(),
        Path::new("Dockerfile"),
        Path::new("."),
        &tag,
        false,
        &BTreeMap::new(),
    )
    .await
    .expect("build raw fixture image");

    // ensure_image on an image-ref config only probes/pulls -- it must not
    // re-validate labels, so the malformed image passes here.
    let cfg = ImageConfig::from_image_name(tag.into_string());
    let image = image::ensure_image(&cfg, ctx.path(), false)
        .await
        .expect("raw image ref should pass ensure_image without label validation")
        .tag;

    // merged_mcp reads the label off the image the container is *running*, by
    // the engine's id for it, so this needs a real container rather than a
    // handle to a name. Nothing is exec'd into it; `sleep infinity` on the
    // alpine base is enough, and no user bootstrap is needed for a label read.
    let container = Container::start(&image, ContainerLaunchSpec::default())
        .await
        .expect("start raw malformed-label container");
    let err = embedded::merged_mcp(&container, &BTreeMap::new())
        .await
        .expect_err("malformed org.outrig.mcp label should fail the runtime read");
    assert!(matches!(err, OutrigError::EmbeddedImageConfigParse { .. }));

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// Issue #370: the merge answers for the image the container is *running*,
/// not for whatever its tag names by the time the merge runs.
///
/// A tag is a pointer. `outrig mcp --attach` borrows a container someone else
/// started, possibly long ago, from a ref like `registry/x:latest` -- and a
/// pull in between re-points that ref at another image while the container
/// goes on running the one it started from. Reading the label by tag then
/// merges servers the running container cannot serve: commands that are not
/// installed in it, and `env` from an image its operator never approved.
///
/// Pulling is not simulated here; a local rebuild under the same tag moves the
/// pointer exactly as a pull does, which is the only part of a pull this read
/// can tell apart.
#[tokio::test]
async fn a_tag_moved_after_the_container_started_does_not_change_its_servers() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let tag = ImageTag::new("outrig-moving-tag:e2e");

    // What the container will run: `fs`, which the fixture image can serve.
    let mut started_with = BTreeMap::new();
    started_with.insert("fs".to_string(), fs_spec());
    build_labeled_image(&tag, &mcp_label_json(&started_with)).await;

    let host_ws = tempfile::tempdir().expect("tempdir host_ws");
    let container = start_and_bootstrap(&tag, host_ws.path()).await;

    // The pull. The new image declares a different server, under a name the
    // running container has never heard of.
    let mut moved_to = BTreeMap::new();
    moved_to.insert("intruder".to_string(), crash_spec());
    build_labeled_image(&tag, &mcp_label_json(&moved_to)).await;

    let merged = embedded::merged_mcp(&container, &BTreeMap::new())
        .await
        .expect("merged mcp");

    assert_eq!(
        merged.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["fs"],
        "the servers are the running image's, not the ones the tag now names",
    );
    assert_eq!(merged["fs"], fs_spec());
    // And they are servers this container can actually run.
    assert_servers_boot(&container, &merged, &["fs"]).await;

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

/// Alpine plus an `org.outrig.mcp` label, nothing installed: the refusals under
/// test come before any server runs. The label is single-quoted because a
/// Dockerfile expands `${VAR}` inside double quotes, and the reference would
/// reach the image blank.
async fn ensure_label_only_image(label_json: &str) -> ImageTag {
    assert!(!label_json.contains('\''), "{label_json}");
    let ctx = tempfile::tempdir().expect("tempdir image context");
    let dockerfile = format!(
        "FROM docker.io/library/alpine:latest\nLABEL {}='{label_json}'\n",
        embedded::LABEL_MCP,
    );
    std::fs::write(ctx.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    try_ensure_built_image(ctx.path())
        .await
        .expect("ensure label-only fixture image")
}

/// The variable the probe label references, and what it is set to when a test
/// wants the reference to resolve.
const LABEL_REF_VAR: &str = "OUTRIG_E2E_LABEL_ENV_624";
const LABEL_REF_VALUE: &str = "label-value-624";

/// A label declaring `probe`, which prints `STOLEN` to stderr and exits. A
/// launch that gets past the policy fails at the handshake with the value in
/// the server's stderr, which is how a test sees the reference was resolved.
fn probe_label() -> String {
    format!(
        r#"{{"probe":{{"command":["sh","-c","printenv STOLEN >&2"],"env":{{"STOLEN":"${{{LABEL_REF_VAR}}}"}}}}}}"#
    )
}

/// What `Outrig::launch` fails with. `Outrig` is not `Debug`, so there is no
/// `expect_err`.
async fn launch_err(spec: &LaunchSpec) -> OutrigError {
    match Outrig::launch(spec).await {
        Err(err) => err,
        Ok(_) => panic!("the launch was expected to fail"),
    }
}

fn label_spec(image: &ImageTag, log_dir: &Path) -> LaunchSpec {
    LaunchSpec::from_image(image.as_str(), BTreeMap::new(), log_dir.join("logs"))
}

/// The issue's case: a label names a variable the spec lists nothing for, and
/// `launch` refuses it before a server is started or the variable is read.
#[tokio::test]
async fn launch_refuses_a_label_reference_the_spec_does_not_list() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_label_only_image(&probe_label()).await;
    let log_dir = tempfile::tempdir().expect("tempdir log dir");

    let err = launch_err(&label_spec(&image, log_dir.path())).await;

    let OutrigError::EmbeddedMcpEnvRef {
        image: named,
        server,
        key,
        var,
        ..
    } = &err
    else {
        panic!("expected EmbeddedMcpEnvRef, got {err:?}");
    };
    assert_eq!(named, image.as_str());
    assert_eq!((server.as_str(), key.as_str()), ("probe", "STOLEN"));
    assert_eq!(var, LABEL_REF_VAR);
}

#[tokio::test]
async fn launch_resolves_a_label_reference_the_spec_lists() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_label_only_image(&probe_label()).await;
    let log_dir = tempfile::tempdir().expect("tempdir log dir");
    // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
    // races; no other test reads or writes this name.
    unsafe { std::env::set_var(LABEL_REF_VAR, LABEL_REF_VALUE) };
    let spec = label_spec(&image, log_dir.path()).with_embedded_mcp_env_allow([LABEL_REF_VAR]);

    let err = launch_err(&spec).await;

    let OutrigError::McpStartupFailed(failure) = &err else {
        panic!("a listed reference gets past the policy to the handshake, got {err:?}");
    };
    assert!(
        failure.stderr_tail.contains(LABEL_REF_VALUE),
        "the probe server should have been handed the variable: {}",
        failure.stderr_tail,
    );
}

/// A spec entry replaces the label's of that name whole, so the label's
/// reference is never read; and the caller's own entry is not held to the list
/// either, though it references a variable the list omits.
#[tokio::test]
async fn a_spec_entry_replaces_the_label_entry_and_is_not_held_itself() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_label_only_image(&probe_label()).await;
    let log_dir = tempfile::tempdir().expect("tempdir log dir");
    let own =
        McpServerSpec::exec(["sh", "-c", "printenv STOLEN >&2"]).with_env(BTreeMap::from([(
            "STOLEN".to_string(),
            EnvValue::EnvRef("PATH".to_string()),
        )]));
    let spec = LaunchSpec::from_image(
        image.as_str(),
        BTreeMap::from([("probe".to_string(), own)]),
        log_dir.path().join("logs"),
    );

    let err = launch_err(&spec).await;

    assert!(
        matches!(err, OutrigError::McpStartupFailed(_)),
        "the caller's own entry runs, and its failure is the handshake's: {err:?}"
    );
}

/// `Ignore` never reads the label, so there is nothing to hold.
#[tokio::test]
async fn ignoring_the_label_ignores_its_references() {
    common::init_tracing();
    let _guard = E2E_LOCK.lock().await;
    let image = ensure_label_only_image(&probe_label()).await;
    let log_dir = tempfile::tempdir().expect("tempdir log dir");
    let spec =
        label_spec(&image, log_dir.path()).with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);

    let outrig = Outrig::launch(&spec)
        .await
        .expect("a label that is not merged cannot be refused");

    outrig.shutdown().await.expect("shutdown");
}
