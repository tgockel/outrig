//! A host's `log_tag` does not stop an entrypoint-stdio container from being created.
//!
//! The container log `Container::create_initialized` asks podman for (#422) is a diagnostic, so it
//! must not turn a launch that worked into one that does not. podman copies the host's
//! `containers.conf` `log_tag` into a container, and conmon refuses a tag with a file log: with
//! `log_driver = "journald"` and a `log_tag` in the host config, a container created with
//! `--log-driver=k8s-file` failed at `podman init` with `conmon failed`, and podman 6 fails the
//! `podman create` itself. Two flags keep the tag out, and each is the one that works on some
//! engines: `--log-driver=json-file`, podman's alias of `k8s-file`, is not given the host's tag
//! through podman 5.8, and an empty `--log-opt=tag=` counts as a tag already given from podman
//! 5.5.1 on, which podman 6, where `json-file` is given the tag too, needs. The engine this runs
//! against decides which one it checks: CI's podman 4.9 the first, a podman 6 host the second.
//!
//! A binary of its own because it sets `CONTAINERS_CONF_OVERRIDE` in the process environment,
//! which every `podman` this binary spawns inherits.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test host_log_tag_e2e -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::process::Command;
use std::time::Duration;

use outrig::container::{Container, ContainerCreateOptions, ContainerLaunchSpec};
use outrig::image::ImageTag;

/// An SELinux host gets no container log, so there is nothing here to check.
fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce").is_ok_and(|enforce| enforce.trim() == "1")
}

#[tokio::test]
async fn a_host_log_tag_does_not_stop_an_entrypoint_container_from_initializing() {
    common::init_tracing();
    if selinux_enforcing() {
        eprintln!("skipped: no container log is made where SELinux is enforcing");
        return;
    }
    common::pull_alpine();

    let conf = tempfile::tempdir().expect("tempdir");
    let override_conf = conf.path().join("containers.conf");
    std::fs::write(
        &override_conf,
        "[containers]\nlog_driver = \"journald\"\nlog_tag = \"{{.Name}}\"\n",
    )
    .expect("write the host config");
    // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread races. This is the
    // only test in this binary, and nothing has been spawned on its runtime yet, so no thread can
    // read the environment while this writes it.
    unsafe { std::env::set_var("CONTAINERS_CONF_OVERRIDE", &override_conf) };

    let name = format!("outrig-test-host-log-tag-{}", std::process::id());
    // `true` replaces the image's CMD, so the container is one that starts and exits at once.
    let created = Container::create_initialized(
        ContainerCreateOptions::new(
            ImageTag::new(common::ALPINE),
            ContainerLaunchSpec::default(),
            &name,
        )
        .with_args(vec!["true".to_string()]),
    )
    .await
    .expect("a host log_tag must not stop the container from being created and initialized");

    // And it was not made by giving up the log: the container has one, and none of the host's tag.
    let inspected = common::run_capture(Command::new("podman").args([
        "inspect",
        "--format",
        "{{.HostConfig.LogConfig.Path}}|{{.HostConfig.LogConfig.Tag}}",
        &name,
    ]));
    let inspected = String::from_utf8_lossy(&inspected.stdout);
    let (path, tag) = inspected.trim().split_once('|').expect("a path and a tag");
    assert!(
        path.ends_with("/ctr.log"),
        "the container has no log: {path:?}"
    );
    assert_eq!(tag, "", "the host's log_tag reached the container");

    created.stop(Duration::from_secs(2)).await.expect("stop");
}
