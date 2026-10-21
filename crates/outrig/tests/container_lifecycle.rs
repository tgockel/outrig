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

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

use outrig::container::{
    self, Container, ContainerCapabilities, ContainerLaunchSpec, ContainerMount,
};
use outrig::image::ImageTag;
use outrig::{CapabilityProfile, MountAccess};

const ALPINE: &str = "docker.io/library/alpine:latest";

async fn pull_alpine() {
    run_capture(Command::new("podman").arg("pull").arg(ALPINE));
}

async fn podman_ps_lists(name: &str, include_stopped: bool) -> bool {
    let mut cmd = Command::new("podman");
    cmd.arg("ps");
    if include_stopped {
        cmd.arg("-a");
    }
    let out = run_capture(
        cmd.arg("--filter")
            .arg(format!("name={name}"))
            .args(["--format", "{{.Names}}"]),
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim() == name)
}

async fn podman_inspect_json(name: &str) -> Value {
    let out = run_capture(Command::new("podman").arg("inspect").arg(name));
    let value: Value = serde_json::from_slice(&out.stdout).expect("podman inspect JSON");
    value
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .expect("podman inspect should return a non-empty array")
}

fn run_capture(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("spawn command");
    assert!(
        output.status.success(),
        "command exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn try_capture(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn command")
}

fn inspect_string_array(value: &Value, paths: &[&[&str]]) -> Option<Vec<String>> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for segment in *path {
            current = current.get(*segment)?;
        }
        current.as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
    })
}

#[tokio::test]
async fn start_then_stop_leaves_no_container() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag::new(ALPINE);

    let container = Container::start(
        &tag,
        ContainerLaunchSpec::workspace(host_ws.path(), Path::new("/workspace")),
    )
    .await
    .expect("start");
    let name = container.name().to_string();

    assert!(
        podman_ps_lists(&name, false).await,
        "container should be running"
    );
    assert!(container::is_tracked(&name), "name should be tracked");

    // The grace is a floor for a PID 1 that ignores SIGTERM, which is what
    // `sleep infinity` would be without `--stop-signal=SIGKILL`.
    let started = Instant::now();
    container.stop(Duration::from_secs(10)).await.expect("stop");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stop waited out its grace: {:?}",
        started.elapsed()
    );

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
async fn extra_mounts_enforce_access_modes() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir workspace");
    let ro_dir = tempfile::tempdir().expect("tempdir ro");
    let rw_dir = tempfile::tempdir().expect("tempdir rw");
    std::fs::write(ro_dir.path().join("MARKER.txt"), "read-only marker\n")
        .expect("write ro marker");
    let tag = ImageTag::new(ALPINE);

    let mut launch = ContainerLaunchSpec::workspace(host_ws.path(), "/workspace");
    launch.mounts = vec![
        ContainerMount::new(ro_dir.path(), "/resources/ro", MountAccess::ReadOnly),
        ContainerMount::new(rw_dir.path(), "/resources/rw", MountAccess::ReadWrite),
    ];
    let container = Container::start(&tag, launch).await.expect("start");

    let read = run_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["cat", "/resources/ro/MARKER.txt"]),
    );
    assert_eq!(String::from_utf8_lossy(&read.stdout), "read-only marker\n");

    let ro_write = try_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["sh", "-c", "echo nope > /resources/ro/out.txt"]),
    );
    assert!(
        !ro_write.status.success(),
        "read-only mount write should fail"
    );
    assert!(
        !ro_dir.path().join("out.txt").exists(),
        "read-only write must not create a host file"
    );

    run_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["sh", "-c", "echo yes > /resources/rw/out.txt"]),
    );
    assert_eq!(
        std::fs::read_to_string(rw_dir.path().join("out.txt")).expect("read rw output"),
        "yes\n"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn capability_flags_are_recorded_in_podman_create_command() {
    common::init_tracing();
    pull_alpine().await;

    let tag = ImageTag::new(ALPINE);
    let mut launch = ContainerLaunchSpec::default();
    launch.capabilities = ContainerCapabilities::new(CapabilityProfile::DropAll);
    launch.capabilities.cap_add = vec!["NET_BIND_SERVICE".to_string()];
    let container = Container::start(&tag, launch).await.expect("start");

    let inspect = podman_inspect_json(container.name()).await;
    let create_command = inspect_string_array(&inspect, &[&["Config", "CreateCommand"]])
        .expect("podman inspect should expose Config.CreateCommand");

    assert!(
        create_command.iter().any(|arg| arg == "--cap-drop=ALL"),
        "create command should contain --cap-drop=ALL, got {create_command:?}",
    );
    assert!(
        create_command
            .iter()
            .any(|arg| arg == "--cap-add=NET_BIND_SERVICE"),
        "create command should contain --cap-add=NET_BIND_SERVICE, got {create_command:?}",
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn drop_without_stop_cleans_up() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag::new(ALPINE);

    let name;
    {
        let container = Container::start(
            &tag,
            ContainerLaunchSpec::workspace(host_ws.path(), Path::new("/workspace")),
        )
        .await
        .expect("start");
        name = container.name().to_string();
        assert!(
            podman_ps_lists(&name, false).await,
            "container should be running"
        );
        // container drops here; Drop spawns detached `podman rm -f`.
    }

    // Poll for the detached cleanup to finish. Under podman's 10 s default
    // stop timeout, which `rm -f` waits out for a PID 1 that ignores SIGTERM,
    // so a regression of `--stop-signal=SIGKILL` cannot pass.
    let deadline = Instant::now() + Duration::from_secs(9);
    loop {
        if !podman_ps_lists(&name, true).await {
            break;
        }
        if Instant::now() >= deadline {
            panic!("container {name} not cleaned up after Drop within 9s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        !container::is_tracked(&name),
        "name should be untracked after Drop"
    );
}

#[tokio::test]
async fn attached_handle_does_not_stop_or_cleanup_container() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag::new(ALPINE);

    let container = Container::start(
        &tag,
        ContainerLaunchSpec::workspace(host_ws.path(), Path::new("/workspace")),
    )
    .await
    .expect("start");
    let name = container.name().to_string();

    {
        let _attached = Container::attach(
            name.clone(),
            tag.clone(),
            Some((host_ws.path(), Path::new("/workspace"))),
            None,
        );
    }
    assert!(
        podman_ps_lists(&name, false).await,
        "attached drop must leave the borrowed container running"
    );

    {
        let attached = Container::attach(
            name.clone(),
            tag.clone(),
            Some((host_ws.path(), Path::new("/workspace"))),
            None,
        );
        attached
            .stop(Duration::from_secs(2))
            .await
            .expect("attached stop is no-op");
    }

    assert!(
        podman_ps_lists(&name, false).await,
        "attached stop/drop must leave the borrowed container running"
    );
    assert!(
        container::is_tracked(&name),
        "owned handle should still track the container"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
    assert!(
        !podman_ps_lists(&name, true).await,
        "owned stop should remove the container"
    );
}

/// A handle whose container is gone reaches nothing: a container created under
/// its name afterward is not where its exec runs, and its stop leaves it be.
#[tokio::test]
async fn stale_handle_does_not_reach_a_container_that_took_its_name() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag::new(ALPINE);
    let mut container = Container::start(
        &tag,
        ContainerLaunchSpec::workspace(host_ws.path(), Path::new("/workspace")),
    )
    .await
    .expect("start");
    container.bootstrap_user().await.expect("bootstrap_user");
    let name = container.name().to_string();
    let hostname = ["cat".to_string(), "/etc/hostname".to_string()];

    let before = container
        .exec_capture(&hostname, &outrig::container::ExecOptions::new())
        .await
        .expect("exec before");
    assert!(before.status.success());

    run_capture(Command::new("podman").args(["kill", &name]));
    let deadline = Instant::now() + Duration::from_secs(10);
    while podman_ps_lists(&name, true).await {
        assert!(Instant::now() < deadline, "the name never freed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    run_capture(Command::new("podman").args([
        "run",
        "-d",
        "--rm",
        "--pull=never",
        "--name",
        &name,
        "--hostname",
        "replacement",
        ALPINE,
        "sleep",
        "infinity",
    ]));

    let after = container
        .exec_capture(&hostname, &outrig::container::ExecOptions::new())
        .await;
    let ran_in_replacement = after
        .as_ref()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("replacement"));
    let stopped = container.stop(Duration::from_secs(2)).await;
    let replacement_survived = podman_ps_lists(&name, false).await;
    // Clean up before asserting, so a failure leaves nothing running.
    try_capture(Command::new("podman").args(["rm", "-f", "-t", "0", &name]));

    assert!(!ran_in_replacement, "the stale exec ran in the replacement");
    assert!(
        after.map(|out| !out.status.success()).unwrap_or(true),
        "the stale exec should fail: no such container"
    );
    stopped.expect("stale stop");
    assert!(
        replacement_survived,
        "the stale stop removed the replacement"
    );
}
