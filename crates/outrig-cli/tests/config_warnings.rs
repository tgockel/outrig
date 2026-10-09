//! #507 through the binary: a key a config load sets aside is reported on
//! stderr, naming the file and line, and the command goes on.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: each assertion
//! is settled at config load, before the first podman call, or by the stub
//! [`common::run_outrig`] puts in podman's place.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{GLOBAL_WITH_MODEL, run_outrig, run_outrig_output, utf8};

/// A repo at `<tmp>/repo` whose `.agents/outrig/config.toml` holds `repo_body`,
/// beside a global config holding `global_body` and a session root, so a run
/// that gets past config load leaves its record under `tmp`. Returns
/// `(repo, global, sessions)`.
fn fixture(tmp: &Path, global_body: &str, repo_body: &str) -> (PathBuf, PathBuf, PathBuf) {
    let global = tmp.join("global.toml");
    fs::write(&global, global_body).unwrap();
    let repo = tmp.join("repo");
    let agents = repo.join(".agents/outrig");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("config.toml"), repo_body).unwrap();
    let sessions = tmp.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    (repo, global, sessions)
}

fn unknown_key(file: &Path, line: usize, key: &str) -> String {
    format!(
        "[outrig] warning: {}:{line}: unknown key `{key}`, ignored",
        file.display()
    )
}

/// The issue's shape: one global config shared with a newer outrig, which
/// added a root table and a model key. `ls` reads only that file, and lists.
#[tokio::test]
async fn ls_warns_about_a_newer_outrigs_key_and_lists() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, global, _) = fixture(
        tmp.path(),
        "session-root = \"sessions\"\n\n[events]\nmode = \"record\"\n",
        "",
    );

    let out = run_outrig_output(tmp.path(), &["--global-config", utf8(&global), "ls"], &[]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ls must go on:\n{stderr}");
    assert!(
        stderr.contains(&unknown_key(&global, 3, "events")),
        "ls must name the key, the file, and the line:\n{stderr}"
    );
}

/// `run` gets past config load with the same file, one warning per key, and
/// on to the image cascade, where the stub podman ends it for an unrelated
/// reason.
#[tokio::test]
async fn run_goes_on_past_a_newer_outrigs_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(
        tmp.path(),
        &format!("{GLOBAL_WITH_MODEL}context-window = 128000\n\n[events]\nmode = \"record\"\n"),
        "",
    );

    let (_ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&sessions),
            "run",
        ],
    )
    .await;

    for warning in [
        unknown_key(&global, 12, "models.fast.context-window"),
        unknown_key(&global, 14, "events"),
    ] {
        assert!(stderr.contains(&warning), "missing {warning:?}:\n{stderr}");
    }
    assert!(
        stderr.contains("config loaded"),
        "the load must go on past the keys it set aside:\n{stderr}"
    );
}

/// A misspelled `provider` leaves its model with none, which validation then
/// refuses. The warning naming the typo comes first, so the error that
/// follows it has its explanation on screen.
#[tokio::test]
async fn a_typos_warning_comes_before_the_error_it_causes() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(
        tmp.path(),
        GLOBAL_WITH_MODEL,
        "default-model = \"typo\"\n\n[models.typo]\nprovder    = \"openai\"\nidentifier = \"x\"\n",
    );

    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&sessions),
            "run",
        ],
    )
    .await;

    assert!(!ok, "a model with no provider must still fail:\n{stderr}");
    let warning = stderr
        .find(":4: unknown key `models.typo.provder`, ignored")
        .unwrap_or_else(|| panic!("no warning for the typo:\n{stderr}"));
    let error = stderr
        .find("error:")
        .unwrap_or_else(|| panic!("no error:\n{stderr}"));
    assert!(
        warning < error,
        "the warning must come before the error:\n{stderr}"
    );
}
