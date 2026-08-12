//! End-to-end smoke for the security primitives a nested container runtime
//! needs: device passthrough, path unmasking, and the `no-new-privileges`
//! opt-out. Gated behind `--features e2e` because it shells out to a real
//! `podman` and starts an `alpine:latest` container.
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

/// `/proc/acpi` is one of the paths podman hides behind a read-only tmpfs, and
/// that mount is the obstruction the kernel's "fully visible" procfs rule
/// refuses to mount a nested `procfs` over -- see `doc/concepts/containers.md`
/// for why a nested container runtime needs it gone. Reading
/// `/proc/self/mountinfo` measures that obstruction directly, without needing
/// an image carrying a whole second container runtime. Both directions,
/// because asserting only the unmasked case would pass just as happily against
/// a build that stopped masking everywhere.
#[tokio::test]
async fn unmask_removes_the_masking_mounts_in_both_directions() {
    common::init_tracing();
    pull_alpine().await;

    // The masking mounts are locked and created by a more privileged namespace,
    // so `grep`ing them out of mountinfo is the whole measurement.
    let probe = "grep ' /proc/acpi ' /proc/self/mountinfo | wc -l";

    let masked = start_alpine(ContainerLaunchSpec::default()).await;
    let baseline = sh(&masked, probe);
    masked.stop(Duration::from_secs(2)).await.expect("stop");
    if baseline == "0" {
        eprintln!("skipping: this podman does not mask /proc/acpi, so there is nothing to unmask");
        return;
    }

    let mut unmasked_spec = ContainerLaunchSpec::default();
    unmasked_spec.unmask = vec!["/proc/*".to_string()];
    let unmasked = start_alpine(unmasked_spec).await;
    let value = sh(&unmasked, probe);
    unmasked.stop(Duration::from_secs(2)).await.expect("stop");
    assert_eq!(
        value, "0",
        "unmask = [\"/proc/*\"] should leave no tmpfs over /proc/acpi, got: {value:?}",
    );
}

/// `ALL` does strictly more than a `/proc/*` glob: it lifts the *read-only*
/// paths too, which is what makes `/sys/fs/cgroup` writable. Config validation
/// rejects every other spelling -- lowercase, or `ALL` beside other entries --
/// because podman applies this half of the behavior only for exact uppercase
/// `ALL` in first position and says nothing when it does not. That rule is only
/// worth its strictness while the measurement holds, so pin the measurement:
/// if podman ever makes the spellings equivalent, this test keeps passing and
/// `validate_unmask_list` can be relaxed on purpose rather than by guess.
#[tokio::test]
async fn unmask_all_lifts_the_read_only_paths_too() {
    common::init_tracing();
    pull_alpine().await;

    // `mount` renders the cgroup line as `... type cgroup2 (ro,nosuid,...)`, so
    // the leading `(rw,` is the whole signal. Counted with `wc`, like the probe
    // above, because `sh` requires exit 0 and `grep -c` calls "no matches" 1.
    let probe = "mount | grep ' /sys/fs/cgroup ' | grep '(rw,' | wc -l";

    let masked = start_alpine(ContainerLaunchSpec::default()).await;
    let baseline = sh(&masked, probe);
    masked.stop(Duration::from_secs(2)).await.expect("stop");
    assert_eq!(
        baseline, "0",
        "a default container should get cgroup read-only, got: {baseline:?}",
    );

    let mut all_spec = ContainerLaunchSpec::default();
    all_spec.unmask = vec!["ALL".to_string()];
    let all = start_alpine(all_spec).await;
    let value = sh(&all, probe);
    all.stop(Duration::from_secs(2)).await.expect("stop");
    assert_ne!(
        value, "0",
        "unmask = [\"ALL\"] should make cgroup writable, got: {value:?}",
    );
}
