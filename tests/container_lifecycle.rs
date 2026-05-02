//! End-to-end smoke for `outrig::container`. Gated behind `--features e2e`
//! because it shells out to a real `podman` and uses `alpine:latest`.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e container_lifecycle -- --nocapture
//! ```
//!
//! The tests pre-pull `alpine:latest` because [`Container::start`] uses
//! `--pull=never`. Containers are uniquely named (`outrig-<sid>`), so
//! parallel test runs don't collide.

#![cfg(feature = "e2e")]

use std::path::Path;
use std::time::{Duration, Instant};

use outrig::container::{self, Container};
use outrig::image::ImageTag;
use outrig::process::{self, Cmd};

const ALPINE: &str = "docker.io/library/alpine:latest";

async fn pull_alpine() {
    process::run_capture(Cmd::new("podman").arg("pull").arg(ALPINE))
        .await
        .expect("podman pull alpine");
}

async fn podman_ps_lists(name: &str, include_stopped: bool) -> bool {
    let mut cmd = Cmd::new("podman").arg("ps");
    if include_stopped {
        cmd = cmd.arg("-a");
    }
    let out = process::run_capture(
        cmd.arg("--filter")
            .arg(format!("name={name}"))
            .args(["--format", "{{.Names}}"]),
    )
    .await
    .expect("podman ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim() == name)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[tokio::test]
async fn start_then_stop_leaves_no_container() {
    init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag(ALPINE.to_string());

    let container = Container::start(&tag, host_ws.path(), Path::new("/workspace"))
        .await
        .expect("start");
    let name = container.name.clone();

    assert!(
        podman_ps_lists(&name, false).await,
        "container should be running"
    );
    assert!(container::is_tracked(&name), "name should be tracked");

    container.stop(Duration::from_secs(2)).await.expect("stop");

    assert!(
        !podman_ps_lists(&name, true).await,
        "container should be gone after stop"
    );
    assert!(
        !container::is_tracked(&name),
        "name should be untracked after stop"
    );
}

#[tokio::test]
async fn drop_without_stop_cleans_up() {
    init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag(ALPINE.to_string());

    let name;
    {
        let container = Container::start(&tag, host_ws.path(), Path::new("/workspace"))
            .await
            .expect("start");
        name = container.name.clone();
        assert!(
            podman_ps_lists(&name, false).await,
            "container should be running"
        );
        // container drops here; Drop spawns detached `podman rm -f`.
    }

    // Poll up to 30s for the detached cleanup to finish. Rootless podman
    // with `--userns=keep-id` can be sluggish on busy hosts, and this test
    // is checking *eventual* cleanup, not latency.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if !podman_ps_lists(&name, true).await {
            break;
        }
        if Instant::now() >= deadline {
            panic!("container {name} not cleaned up after Drop within 30s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        !container::is_tracked(&name),
        "name should be untracked after Drop"
    );
}
