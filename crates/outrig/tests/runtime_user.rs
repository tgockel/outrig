//! End-to-end smoke for `Container::bootstrap_user` and `exec_stdio`. Gated
//! behind `--features e2e` because it shells out to a real `podman` and
//! starts an `alpine:latest` container.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e runtime_user -- --nocapture
//! ```
//!
//! `alpine:latest` is missing `useradd`/`groupadd` out of the box, so each
//! test installs the `shadow` package via `apk` after start and before
//! bootstrap.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::ImageTag;
use outrig::process::{self, Cmd};
use tokio::io::AsyncReadExt;

const ALPINE: &str = "docker.io/library/alpine:latest";

async fn pull_alpine() {
    process::run_capture(Cmd::new("podman").arg("pull").arg(ALPINE))
        .await
        .expect("podman pull alpine");
}

async fn install_shadow(name: &str) {
    process::run_capture(
        Cmd::new("podman")
            .args(["exec", "--user=0:0"])
            .arg(name)
            .args(["apk", "add", "--no-cache", "shadow"]),
    )
    .await
    .expect("apk add shadow");
}

async fn start_alpine(host_ws: &Path) -> Container {
    let tag = ImageTag(ALPINE.to_string());
    Container::start(
        &tag,
        ContainerLaunchSpec::workspace(host_ws, Path::new("/workspace")),
    )
    .await
    .expect("start")
}

async fn read_stdout(child: &mut tokio::process::Child) -> String {
    let mut out = String::new();
    child
        .stdout
        .as_mut()
        .expect("stdout was piped")
        .read_to_string(&mut out)
        .await
        .expect("read stdout");
    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "child exited non-zero: {status:?}");
    out
}

#[tokio::test]
async fn bootstrap_then_id_matches_host() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path()).await;
    install_shadow(&container.name).await;

    container.bootstrap_user().await.expect("bootstrap_user");
    assert!(container.user_name.is_some());
    assert!(container.group_name.is_some());

    let mut child = container
        .exec_stdio(&["id".to_string()], &BTreeMap::new())
        .await
        .expect("exec_stdio id");
    let out = read_stdout(&mut child).await;

    let expect_uid = format!("uid={}", container.uid);
    let expect_gid = format!("gid={}", container.gid);
    assert!(
        out.contains(&expect_uid),
        "expected `{expect_uid}` in `id` output, got: {out}"
    );
    assert!(
        out.contains(&expect_gid),
        "expected `{expect_gid}` in `id` output, got: {out}"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn workspace_writes_have_host_ownership() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path()).await;
    install_shadow(&container.name).await;
    container.bootstrap_user().await.expect("bootstrap_user");

    let mut child = container
        .exec_stdio(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo hello > /workspace/test.txt".to_string(),
            ],
            &BTreeMap::new(),
        )
        .await
        .expect("exec_stdio sh");
    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "writer exited non-zero: {status:?}");

    let host_file = host_ws.path().join("test.txt");
    let meta = fs::metadata(&host_file).expect("stat host file");
    assert_eq!(
        meta.uid(),
        container.uid,
        "file should be owned by host UID"
    );
    assert_eq!(
        meta.gid(),
        container.gid,
        "file should be owned by host GID"
    );
    assert_eq!(
        fs::read_to_string(&host_file).expect("read"),
        "hello\n",
        "file contents should round-trip via the bind-mount"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// Look up the first-field name of an existing `getent <db> <id>` entry, or
/// `None` if none exists. Used by the reuse test to decide whether the
/// container already has an entry at the host UID/GID (e.g. `--userns=keep-id`
/// auto-injects one in modern podman) or whether the test needs to plant one.
async fn first_name_in_db(name: &str, db: &str, id: u32) -> Option<String> {
    let probe = process::try_capture(
        Cmd::new("podman")
            .args(["exec", "--user=0:0"])
            .arg(name)
            .arg("getent")
            .arg(db)
            .arg(id.to_string()),
    )
    .await
    .ok()?;
    if !probe.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&probe.stdout);
    Some(stdout.lines().next()?.split(':').next()?.to_string())
}

#[tokio::test]
async fn bootstrap_reuses_existing_entry() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path()).await;
    install_shadow(&container.name).await;

    // Bootstrap should reuse whatever entry is already at the host UID/GID,
    // whether that's an auto-injection from `--userns=keep-id` or one we
    // manually plant here. Probe first; plant only if absent.
    let expected_grp = match first_name_in_db(&container.name, "group", container.gid).await {
        Some(existing) => existing,
        None => {
            let planted = "preexisting_grp";
            process::run_capture(
                Cmd::new("podman")
                    .args(["exec", "--user=0:0"])
                    .arg(&container.name)
                    .arg("groupadd")
                    .arg("--gid")
                    .arg(container.gid.to_string())
                    .arg(planted),
            )
            .await
            .expect("plant group");
            planted.to_string()
        }
    };
    let expected_usr = match first_name_in_db(&container.name, "passwd", container.uid).await {
        Some(existing) => existing,
        None => {
            let planted = "preexisting_usr";
            process::run_capture(
                Cmd::new("podman")
                    .args(["exec", "--user=0:0"])
                    .arg(&container.name)
                    .arg("useradd")
                    .arg("-u")
                    .arg(container.uid.to_string())
                    .arg("-g")
                    .arg(container.gid.to_string())
                    .arg(planted),
            )
            .await
            .expect("plant user");
            planted.to_string()
        }
    };

    container.bootstrap_user().await.expect("bootstrap_user");
    assert_eq!(
        container.group_name.as_deref(),
        Some(expected_grp.as_str()),
        "bootstrap should reuse the pre-existing group at the host GID"
    );
    assert_eq!(
        container.user_name.as_deref(),
        Some(expected_usr.as_str()),
        "bootstrap should reuse the pre-existing user at the host UID"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}
