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
use std::time::Duration;

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

fn write_project(dir: &Path, dockerfile: &str, image_toml: &str) {
    std::fs::create_dir_all(dir).expect("mkdir project");
    std::fs::write(dir.join("Dockerfile"), dockerfile).expect("write Dockerfile");
    std::fs::write(dir.join("image.toml"), image_toml).expect("write image.toml");
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
}

/// Acceptance: the command fails if the built image does not contain
/// `/etc/outrig/image.toml` (here the Dockerfile omits the COPY).
#[tokio::test]
async fn build_fails_when_built_image_lacks_image_toml() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let proj = tmp.path().join("no-copy");
    write_project(
        &proj,
        "FROM docker.io/library/alpine:latest\nCMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-no-copy\"\n\
         [mcp]\nfs = [\"mcp-server-filesystem\", \"/workspace\"]\n",
    );

    let out = outrig_image_build(&[proj.to_str().unwrap()], LIGHT_TIMEOUT).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected failure, got success:\n{stderr}"
    );
    assert!(stderr.contains("/etc/outrig/image.toml"), "{stderr}");
    assert!(stderr.to_lowercase().contains("missing"), "{stderr}");
}

/// Acceptance: `--no-test` skips ONLY the live MCP probe -- it still validates
/// the baked `image.toml`, so the missing-COPY image must still fail.
#[tokio::test]
async fn no_test_still_validates_embedded_image_toml() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let proj = tmp.path().join("no-copy-notest");
    write_project(
        &proj,
        "FROM docker.io/library/alpine:latest\nCMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-no-copy-notest\"\n\
         [mcp]\nfs = [\"mcp-server-filesystem\", \"/workspace\"]\n",
    );

    let out = outrig_image_build(&[proj.to_str().unwrap(), "--no-test"], LIGHT_TIMEOUT).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "--no-test must still validate the baked image.toml:\n{stderr}"
    );
    assert!(stderr.contains("/etc/outrig/image.toml"), "{stderr}");
}

/// Acceptance: `--no-test` succeeds without running the live probe, and a
/// `--tag` override does not rewrite the project's `image.toml`.
#[tokio::test]
async fn no_test_succeeds_and_tag_override_preserves_image_toml() {
    let _guard = E2E_LOCK.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let proj = tmp.path().join("with-copy");
    write_project(
        &proj,
        "FROM docker.io/library/alpine:latest\n\
         RUN mkdir -p /etc/outrig\n\
         COPY image.toml /etc/outrig/image.toml\n\
         CMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-with-copy\"\n\
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
}

/// Acceptance: a declared MCP server that cannot start fails the build (this is
/// the failure `--no-test` exists to skip). The baked image.toml declares a
/// server whose command does not exist, so the live probe cannot initialize it.
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
         RUN mkdir -p /etc/outrig\n\
         COPY image.toml /etc/outrig/image.toml\n\
         CMD [\"sleep\", \"infinity\"]\n",
        "[image]\nref = \"outrig-e2e-broken-mcp\"\n\
         [mcp]\nbroken = [\"outrig-no-such-mcp-binary\"]\n",
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
