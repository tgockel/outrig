//! The repo config a session reads, and the workspace it mounts, are named
//! before anything starts (#329), so one picked up from an unexpected
//! ancestor is on screen. Not `e2e`-gated: both lines print before the
//! first podman call, and [`common::run_outrig`] stubs podman out.
//!
//! The walk's other half, refusing a config another user owns, needs a
//! second uid to plant one; `paths.rs` covers it with the uid injected.

mod common;

use std::fs;

use common::{GLOBAL_WITH_MODEL, run_outrig};

/// From below a repo, `run` and `mcp` name the config the walk found and the
/// repo it makes the workspace.
#[tokio::test]
async fn sessions_name_the_repo_config_and_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let config = repo.join(".agents/outrig/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "").unwrap();
    let nested = repo.join("src/deep");
    fs::create_dir_all(&nested).unwrap();
    let global = tmp.path().join("global.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    for cmd in ["run", "mcp"] {
        let (ok, stderr) = run_outrig(
            &nested,
            &[
                "--global-config",
                global.to_str().unwrap(),
                "--session-root",
                sessions.to_str().unwrap(),
                cmd,
                "--image",
                "localhost/outrig-test-absent:latest",
            ],
        )
        .await;
        assert!(
            !ok,
            "the stubbed podman cannot start a container:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!("[outrig] config loaded: {} (", config.display())),
            "`outrig {cmd}` names the config it read:\n{stderr}",
        );
        assert!(
            stderr.contains(&format!("[outrig] workspace: {}\n", repo.display())),
            "`outrig {cmd}` names the workspace it mounts:\n{stderr}",
        );
        assert!(
            !stderr.contains("no repo config found"),
            "`outrig {cmd}` found one:\n{stderr}",
        );
    }
}
