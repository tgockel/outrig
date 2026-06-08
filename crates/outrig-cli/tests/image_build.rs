//! End-to-end coverage for `outrig image build`. Gated behind `--features e2e`
//! because it shells out to real `buildah`/`podman`, pulls base images, and
//! starts containers.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e image_build -- --nocapture
//! ```
//!
//! The scaffold case builds the real `outrig image init rust-dev` Dockerfile
//! (debian-slim + npm), so it gets the e2e_quickstart-sized 600s budget. The
//! validation / `--no-test` cases use tiny alpine hand-fixtures that fail (or
//! succeed) before any heavyweight install, keeping total wall-clock sane.

#![cfg(feature = "e2e")]

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

use outrig_cli::image_setup::init;

// Serialize the cases: they share the host podman/buildah image store, and a
// single heavyweight build at a time keeps resource use predictable.
static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SCAFFOLD_TIMEOUT: Duration = Duration::from_secs(600);
const LIGHT_TIMEOUT: Duration = Duration::from_secs(300);

/// Drive `outrig image build <extra...>` as a child process and capture it.
/// `image build` resolves no repo config, so it runs fine from any cwd; we pass
/// an absolute project dir.
async fn outrig_image_build(extra: &[&str], budget: Duration) -> Output {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut cmd = Command::new(bin);
    cmd.args(["image", "build"])
        .args(extra)
        .stdin(Stdio::null());
    timeout(budget, cmd.output())
        .await
        .expect("outrig image build timed out")
        .expect("spawn outrig image build")
}

async fn outrig_image_inspect(image_ref: &str, budget: Duration) -> Output {
    let bin = env!("CARGO_BIN_EXE_outrig");
    let mut cmd = Command::new(bin);
    cmd.args(["image", "inspect", image_ref])
        .stdin(Stdio::null());
    timeout(budget, cmd.output())
        .await
        .expect("outrig image inspect timed out")
        .expect("spawn outrig image inspect")
}

fn write_project(dir: &Path, dockerfile: &str, image_toml: &str) {
    std::fs::create_dir_all(dir).expect("mkdir project");
    std::fs::write(dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");
    std::fs::write(dir.join("image.toml"), image_toml).expect("write image.toml");
}

/// Read the OCI labels off a built image as a JSON string (or `null` when the
/// image carries none).
async fn podman_image_labels(tag: &str) -> String {
    let out = Command::new("podman")
        .args([
            "image",
            "inspect",
            tag,
            "--format",
            "{{json .Config.Labels}}",
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("spawn podman image inspect");
    assert!(
        out.status.success(),
        "podman image inspect {tag} failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn mcp_label_json(labels: &str) -> Value {
    let labels: Value = serde_json::from_str(labels).expect("podman labels JSON");
    let mcp = labels
        .get("org.outrig.mcp")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("built image should carry org.outrig.mcp: {labels}"));
    serde_json::from_str(mcp).expect("org.outrig.mcp label JSON")
}

/// Acceptance: building the `outrig image init rust-dev` scaffold succeeds, and
/// its declared MCP server is live-tested (initialize + tools/list).
#[tokio::test]
async fn generated_scaffold_builds_and_mcp_boots() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    init::run(tmp.path(), Some(Path::new("rust-dev")), false).expect("init scaffold");
    let proj = tmp.path().join("rust-dev");

    let out = outrig_image_build(&[proj.to_str().unwrap()], SCAFFOLD_TIMEOUT).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "build should succeed:\n{stderr}");
    assert!(stderr.contains("[outrig] image ready"), "{stderr}");
    assert!(
        stderr.contains("[outrig] mcp fs: initialized ("),
        "live fs server should be tested:\n{stderr}"
    );
    assert!(stderr.contains("[outrig] image ok"), "{stderr}");

    // The build stamps the config into OCI labels; confirm they read back off
    // the built image (acceptance: "reads them back off the built image").
    let labels = podman_image_labels("rust-dev").await;
    let mcp = mcp_label_json(&labels);
    assert_eq!(
        mcp["fs"]["command"],
        json!(["mcp-server-filesystem", "/workspace"]),
        "org.outrig.mcp should declare the fs server: {labels}",
    );

    let inspect = outrig_image_inspect("rust-dev", LIGHT_TIMEOUT).await;
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let stderr = String::from_utf8_lossy(&inspect.stderr);
    assert!(
        inspect.status.success(),
        "inspect should succeed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("image: rust-dev"), "{stdout}");
    assert!(
        stdout.contains("command: [\"mcp-server-filesystem\",\"/workspace\"]"),
        "{stdout}"
    );
}

/// Acceptance: `--no-test` succeeds without running the live probe, and a
/// `--tag` override does not rewrite the project's `image.toml`.
#[tokio::test]
async fn no_test_succeeds_and_tag_override_preserves_image_toml() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let proj = tmp.path().join("label-stamped");
    write_project(
        &proj,
        "FROM docker.io/library/alpine:latest\n\
         CMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-label-stamped\"\n\
         description = \"Label stamped test image\"\n\
         version = \"0.2.0\"\n\
         tags = [\"test\", \"inspect\"]\n\
         [mcp]\nfs = [\"mcp-server-filesystem\", \"/workspace\"]\n",
    );
    let before = std::fs::read(proj.join("image.toml")).expect("read image.toml");

    let out = outrig_image_build(
        &[
            proj.to_str().unwrap(),
            "--no-test",
            "--tag",
            "outrig-e2e-custom-ref",
        ],
        LIGHT_TIMEOUT,
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "expected success:\n{stderr}");
    assert!(
        !stderr.contains("[outrig] mcp "),
        "--no-test must not run the live probe:\n{stderr}"
    );
    assert!(stderr.contains("skipping live mcp test"), "{stderr}");

    let after = std::fs::read(proj.join("image.toml")).expect("read image.toml");
    assert_eq!(before, after, "--tag must not rewrite image.toml");

    let inspect = outrig_image_inspect("outrig-e2e-custom-ref", LIGHT_TIMEOUT).await;
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let stderr = String::from_utf8_lossy(&inspect.stderr);
    assert!(
        inspect.status.success(),
        "inspect should report stamped metadata:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("image: outrig-e2e-custom-ref"), "{stdout}");
    assert!(
        stdout.contains("description: Label stamped test image"),
        "{stdout}"
    );
    assert!(stdout.contains("version: 0.2.0"), "{stdout}");
    assert!(stdout.contains("tags: [\"test\",\"inspect\"]"), "{stdout}");
}

#[tokio::test]
async fn inspect_missing_local_image_is_clear_and_does_not_pull() {
    let _guard = E2E_LOCK.lock().await;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let image_ref = format!("outrig-e2e-missing-inspect-{suffix}");

    let out = outrig_image_inspect(&image_ref, LIGHT_TIMEOUT).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "missing local image must fail:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "stdout should stay scriptable: {stdout:?}"
    );
    assert!(stderr.contains("local image"), "{stderr}");
    assert!(stderr.contains("not found"), "{stderr}");
    assert!(stderr.contains("local-only"), "{stderr}");
    assert!(stderr.contains("does not pull"), "{stderr}");
}

/// Acceptance: a declared MCP server that cannot start fails the build (this is
/// the failure `--no-test` exists to skip). The stamped label declares a server
/// whose command does not exist, so the live probe cannot initialize it.
/// `shadow` is installed so the UID/GID bootstrap (which precedes the probe)
/// succeeds and the failure is squarely the server, not the container.
#[tokio::test]
async fn build_fails_when_mcp_server_cannot_start() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let proj = tmp.path().join("broken-mcp");
    write_project(
        &proj,
        "FROM docker.io/library/alpine:latest\n\
         RUN apk add --no-cache shadow\n\
         CMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-broken-mcp\"\n\
         [mcp]\nbroken = [\"outrig-no-such-mcp-binary\"]\n",
    );

    let inspectable = outrig_image_build(
        &[
            proj.to_str().unwrap(),
            "--no-test",
            "--tag",
            "outrig-e2e-broken-mcp-inspect",
        ],
        LIGHT_TIMEOUT,
    )
    .await;
    let stderr = String::from_utf8_lossy(&inspectable.stderr);
    assert!(
        inspectable.status.success(),
        "--no-test should produce an inspectable broken declaration:\n{stderr}"
    );

    let inspect = outrig_image_inspect("outrig-e2e-broken-mcp-inspect", LIGHT_TIMEOUT).await;
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let stderr = String::from_utf8_lossy(&inspect.stderr);
    assert!(
        inspect.status.success(),
        "inspect must not start the broken MCP server:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("command: [\"outrig-no-such-mcp-binary\"]"),
        "{stdout}"
    );

    let out = outrig_image_build(&[proj.to_str().unwrap()], LIGHT_TIMEOUT).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a server that cannot start must fail the build:\n{stderr}"
    );
    assert!(
        stderr.contains("broken"),
        "the error should name the failing server:\n{stderr}"
    );
}
