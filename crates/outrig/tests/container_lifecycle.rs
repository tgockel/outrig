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

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

use outrig::container::{
    self, Container, ContainerCapabilities, ContainerLaunchSpec, ContainerMount, ContainerWorkspace,
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
    let tag = ImageTag(ALPINE.to_string());

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
async fn extra_mounts_enforce_access_modes() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir workspace");
    let ro_dir = tempfile::tempdir().expect("tempdir ro");
    let rw_dir = tempfile::tempdir().expect("tempdir rw");
    std::fs::write(ro_dir.path().join("MARKER.txt"), "read-only marker\n")
        .expect("write ro marker");
    let tag = ImageTag(ALPINE.to_string());

    let container = Container::start(
        &tag,
        ContainerLaunchSpec {
            workspace: Some(ContainerWorkspace {
                host: host_ws.path().to_path_buf(),
                container: PathBuf::from("/workspace"),
            }),
            mounts: vec![
                ContainerMount {
                    host: ro_dir.path().to_path_buf(),
                    container: PathBuf::from("/resources/ro"),
                    access: MountAccess::ReadOnly,
                },
                ContainerMount {
                    host: rw_dir.path().to_path_buf(),
                    container: PathBuf::from("/resources/rw"),
                    access: MountAccess::ReadWrite,
                },
            ],
            capabilities: ContainerCapabilities::default(),
        },
    )
    .await
    .expect("start");

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

    let tag = ImageTag(ALPINE.to_string());
    let container = Container::start(
        &tag,
        ContainerLaunchSpec {
            workspace: None,
            mounts: Vec::new(),
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::DropAll,
                cap_drop: Vec::new(),
                cap_add: vec!["NET_BIND_SERVICE".to_string()],
            },
        },
    )
    .await
    .expect("start");

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
    let tag = ImageTag(ALPINE.to_string());

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

#[tokio::test]
async fn attached_handle_does_not_stop_or_cleanup_container() {
    common::init_tracing();
    pull_alpine().await;

    let host_ws = tempfile::tempdir().expect("tempdir");
    let tag = ImageTag(ALPINE.to_string());

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
