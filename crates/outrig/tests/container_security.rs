//! End-to-end smoke for the two security primitives a nested container
//! runtime needs: device passthrough and the `no-new-privileges` opt-out.
//! Gated behind `--features e2e` because it shells out to a real `podman` and
//! starts an `alpine:latest` container.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e container_security -- --nocapture
//! ```
//!
//! The unit tests in `container/mod.rs` prove the flags are *emitted*; these
//! prove they survive the trip through podman into the kernel.

#![cfg(feature = "e2e")]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::ImageTag;

const ALPINE: &str = "docker.io/library/alpine:latest";

/// `/dev/fuse` is the device a nested podman actually wants (for
/// `fuse-overlayfs`), and it is not in podman's default device set, which is
/// what makes it a real passthrough probe. A host without the `fuse` module
/// loaded cannot pass it through at all, so the device half is skipped there
/// rather than failed.
const FUSE: &str = "/dev/fuse";

async fn pull_alpine() {
    let output = Command::new("podman")
        .arg("pull")
        .arg(ALPINE)
        .output()
        .expect("spawn podman pull");
    assert!(
        output.status.success(),
        "podman pull exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
}

async fn start_alpine(launch: ContainerLaunchSpec) -> Container {
    Container::start(&ImageTag::new(ALPINE), launch)
        .await
        .expect("start")
}

/// Run `sh -c <script>` in the container and hand back its stdout. This goes
/// through `podman exec` rather than [`Container::exec_stdio`] to keep the
/// probe independent of `bootstrap_user` (which alpine cannot satisfy without
/// first installing `shadow`). The caller asserts on the text rather than the
/// exit status so that a failing probe reports what it actually saw.
fn sh(container: &Container, script: &str) -> String {
    let output = Command::new("podman")
        .args(["exec", "--user=0:0"])
        .arg(container.name())
        .args(["sh", "-c", script])
        .output()
        .expect("spawn podman exec");
    assert!(
        output.status.success(),
        "podman exec exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `NoNewPrivs` in `/proc/1/status` is the kernel's own view of the flag on
/// the process the container's security options were applied to, so this reads
/// it from both sides: a default container has it set, and one that opts out
/// does not. Asserting only the opted-out case would pass just as happily
/// against a build that dropped the flag everywhere.
#[tokio::test]
async fn no_new_privileges_reaches_the_kernel_in_both_directions() {
    common::init_tracing();
    pull_alpine().await;

    let hardened = start_alpine(ContainerLaunchSpec::default()).await;
    let value = sh(&hardened, "grep NoNewPrivs /proc/1/status");
    hardened.stop(Duration::from_secs(2)).await.expect("stop");
    assert!(
        value.ends_with('1'),
        "a default container should run under no_new_privs, got: {value:?}",
    );

    let mut opted_out_spec = ContainerLaunchSpec::default();
    opted_out_spec.no_new_privileges = false;
    let opted_out = start_alpine(opted_out_spec).await;
    let value = sh(&opted_out, "grep NoNewPrivs /proc/1/status");
    opted_out.stop(Duration::from_secs(2)).await.expect("stop");
    assert!(
        value.ends_with('0'),
        "no_new_privileges = false should clear the kernel flag, got: {value:?}",
    );
}

#[tokio::test]
async fn declared_device_appears_inside_the_container() {
    common::init_tracing();

    if !Path::new(FUSE).exists() {
        eprintln!("skipping: host has no {FUSE} (the fuse module is not loaded)");
        return;
    }
    pull_alpine().await;

    // Baseline first: if podman started handing out /dev/fuse by default, the
    // passthrough assertion below would pass for the wrong reason.
    let bare = start_alpine(ContainerLaunchSpec::default()).await;
    let bare_probe = sh(&bare, "test -c /dev/fuse && echo present || echo absent");
    bare.stop(Duration::from_secs(2)).await.expect("stop");
    assert_eq!(
        bare_probe, "absent",
        "podman should not provide {FUSE} without --device",
    );

    let mut with_device_spec = ContainerLaunchSpec::default();
    with_device_spec.devices = vec![FUSE.to_string()];
    let with_device = start_alpine(with_device_spec).await;
    let probe = sh(
        &with_device,
        "test -c /dev/fuse && echo present || echo absent",
    );
    with_device
        .stop(Duration::from_secs(2))
        .await
        .expect("stop");
    assert_eq!(
        probe, "present",
        "devices = [{FUSE:?}] should pass the node through",
    );
}
