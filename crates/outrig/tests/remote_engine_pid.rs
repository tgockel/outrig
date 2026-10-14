//! End-to-end check that the user bootstrap refuses a container PID that is
//! not the container's init on this machine (#254). Gated behind
//! `--features e2e`: it shells out to a real `podman`.
//!
//! Its own binary because it prepends a `podman` wrapper to `PATH`, which
//! would otherwise leak into every other test in the process.
//!
//! ```sh
//! cargo test --features e2e --test remote_engine_pid -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use outrig::error::OutrigError;

use common::{ALPINE, init_tracing, pull_alpine, root_stdout, run_capture, start_alpine};

/// Put a `podman` on `PATH` that answers `inspect` of `{{.State.Pid}}` with
/// `pid` -- a remote engine's number, standing in for a PID that happens to be
/// taken locally -- and forwards everything else to the real one.
fn install_lying_podman(dir: &Path, pid: &str) {
    let real = String::from_utf8(run_capture(Command::new("which").arg("podman")).stdout)
        .expect("utf-8 path")
        .trim()
        .to_string();
    let script = dir.join("podman");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *'inspect --format {{{{.State.Pid}}}}'*) echo '{pid} 1'; exit 0;;\n\
             esac\n\
             exec {real} \"$@\"\n"
        ),
    )
    .expect("write wrapper");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod wrapper");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&path));
    // SAFETY: the only test in this binary, and it sets this before spawning
    // anything.
    unsafe { std::env::set_var("PATH", std::env::join_paths(paths).expect("join PATH")) };
}

#[tokio::test]
async fn a_pid_that_is_another_local_container_is_refused_and_left_alone() {
    init_tracing();
    pull_alpine();

    let bystander = format!("outrig-test-bystander-{}", std::process::id());
    run_capture(
        Command::new("podman")
            .args(["run", "-d", "--rm", "--name", &bystander, ALPINE])
            .arg("sleep")
            .arg("900"),
    );
    let before = root_stdout(&bystander, &["cat", "/etc/passwd"]);
    let bystander_pid = String::from_utf8(
        run_capture(Command::new("podman").args([
            "inspect",
            "--format",
            "{{.State.Pid}}",
            &bystander,
        ]))
        .stdout,
    )
    .expect("utf-8")
    .trim()
    .to_string();

    let wrapper_dir = tempfile::tempdir().expect("tempdir");
    install_lying_podman(wrapper_dir.path(), &bystander_pid);

    let ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(ws.path(), None).await;
    let err = container
        .bootstrap_user()
        .await
        .expect_err("a foreign pid must be refused");
    let after = root_stdout(&bystander, &["cat", "/etc/passwd"]);
    run_capture(Command::new("podman").args(["rm", "-f", &bystander]));

    assert!(
        matches!(&err, OutrigError::ContainerPidNotLocal { pid, .. } if pid.to_string() == bystander_pid),
        "unexpected error: {err}"
    );
    assert_eq!(before, after, "the bystander's /etc/passwd was written");
}
