//! #330 through the binary: a repo config may not declare a provider that
//! carries an `api-key`, and a model there may still name a global one.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: each assertion
//! is settled at config load, before the first podman call, or by the stub
//! [`common::run_outrig`] puts in podman's place.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{GLOBAL_WITH_MODEL, run_outrig};

/// A repo at `<tmp>/repo` whose `.agents/outrig/config.toml` holds `body`,
/// beside a global config holding [`GLOBAL_WITH_MODEL`] and a session root,
/// so a run that gets past config load leaves its record under `tmp` rather
/// than in the user's store. Returns `(repo, global, sessions)`.
fn fixture(tmp: &Path, body: &str) -> (PathBuf, PathBuf, PathBuf) {
    let global = tmp.join("global.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let repo = tmp.join("repo");
    let agents = repo.join(".agents/outrig");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("config.toml"), body).unwrap();
    let sessions = tmp.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    (repo, global, sessions)
}

/// The issue's shape: the global config binds `${OUTRIG_TEST_KEY}` to one
/// endpoint and the repo restates the provider under the same name at
/// another. `outrig run` refuses the repo file by name, before resolving an
/// agent or touching a container.
#[tokio::test]
async fn a_repo_config_declaring_a_keyed_provider_is_refused_before_any_container_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(
        tmp.path(),
        "[providers.openai]\nstyle = \"openai\"\nbase-url = \"http://127.0.0.1:2/v1\"\n\
         api-key = \"${OUTRIG_TEST_KEY}\"\n",
    );

    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            global.to_str().unwrap(),
            "--session-root",
            sessions.to_str().unwrap(),
            "run",
        ],
    )
    .await;

    assert!(!ok, "the repo provider must be refused:\n{stderr}");
    assert!(
        stderr.contains(
            "repo config may not declare [providers.openai]: a style=openai provider \
             carries api-key ${OUTRIG_TEST_KEY}, which belongs in global config"
        ),
        "the error names the provider and where it belongs:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolving agent and container"),
        "the refusal comes at config load, before anything is resolved:\n{stderr}"
    );
}

/// The recommended split loads: a repo model naming the global provider gets
/// past config load and on to the image cascade, where the stub podman ends
/// the run for an unrelated reason.
#[tokio::test]
async fn a_repo_model_naming_a_global_provider_still_loads() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(
        tmp.path(),
        "default-model = \"repo-fast\"\n\n[models.repo-fast]\nprovider = \"openai\"\n\
         identifier = \"test-model\"\n",
    );

    let (_ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            global.to_str().unwrap(),
            "--session-root",
            sessions.to_str().unwrap(),
            "run",
        ],
    )
    .await;

    assert!(
        !stderr.contains("belongs in global config"),
        "naming a global provider is the supported shape:\n{stderr}"
    );
    assert!(
        stderr.contains("config loaded"),
        "the load must get past the repo rules:\n{stderr}"
    );
}
