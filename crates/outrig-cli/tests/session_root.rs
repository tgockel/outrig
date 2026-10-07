//! #336: the session root comes from the global config alone, resolved beside
//! that file, so `run` and every session command agree on it from any
//! directory.
//!
//! Through the binary, without a container runtime. Not `e2e`-gated, for the
//! reason `builtin_default.rs` gives: each assertion is settled before the
//! first podman call, or by the stub [`common::run_outrig`] puts in podman's
//! place.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{
    ABSENT_IMAGE, GLOBAL_WITH_MODEL, run_outrig_output, run_outrig_with_env, sample_session, utf8,
};
use outrig_cli::session::{self, SessionId, SessionStore};

/// `<tmp>/repo`, with `body` as its config.
fn repo_with_config(tmp: &Path, body: &str) -> PathBuf {
    let repo = tmp.join("repo");
    fs::create_dir_all(repo.join(".agents/outrig")).unwrap();
    fs::write(repo.join(".agents/outrig/config.toml"), body).unwrap();
    repo
}

/// `<tmp>/global/config.toml`, holding `body`.
fn global_config(tmp: &Path, body: &str) -> PathBuf {
    let dir = tmp.join("global");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    fs::write(&path, body).unwrap();
    path
}

/// Record one session under `root`.
fn record(root: &Path, sid: &str) -> SessionId {
    let sid = SessionId(sid.into());
    SessionStore::new(root.to_path_buf())
        .create(&sid, None, &mut sample_session(&sid))
        .expect("record a session");
    sid
}

/// The issue's reproduction. The repo config's `session-root = "sessions"`
/// used to be read against the cwd, so `ls` from the repo and from `sub/`
/// listed two different roots. Now the repo's value is not read at all, and
/// the global one resolves beside the global file wherever `ls` runs.
#[tokio::test]
async fn ls_lists_the_global_root_from_any_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_config(tmp.path(), "session-root = \"sessions\"\n");
    fs::create_dir_all(repo.join("sub")).unwrap();
    let from_repo = record(&repo.join("sessions"), "20261003T100000-aaaa");
    let from_sub = record(&repo.join("sub/sessions"), "20261003T100001-bbbb");
    let global = global_config(tmp.path(), "session-root = \"sessions\"\n");
    let sid = record(&tmp.path().join("global/sessions"), "20261003T100002-cccc");

    for cwd in [repo.clone(), repo.join("sub")] {
        let out = run_outrig_output(&cwd, &["--global-config", utf8(&global), "ls"], &[]).await;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ls from {}:\n{stderr}", cwd.display());
        assert!(
            stdout.contains(sid.as_str()),
            "ls from {} must list the global root:\n{stdout}",
            cwd.display(),
        );
        for stale in [&from_repo, &from_sub] {
            assert!(
                !stdout.contains(stale.as_str()),
                "ls from {} must not read the repo's root:\n{stdout}",
                cwd.display(),
            );
        }
    }
}

/// A repo config that sets the root is refused where it used to be obeyed,
/// and `run` stops at load, before it creates a session anywhere.
#[tokio::test]
async fn run_refuses_a_repo_session_root() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_config(tmp.path(), "session-root = \"sessions\"\n");
    let global = global_config(tmp.path(), GLOBAL_WITH_MODEL);
    // Where the default root would go, were the load to get that far.
    let data = tmp.path().join("data");

    let (ok, stderr) = run_outrig_with_env(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "run",
            "--image",
            ABSENT_IMAGE,
        ],
        &[("XDG_DATA_HOME", &data)],
    )
    .await;

    assert!(!ok, "a repo session-root must be refused:\n{stderr}");
    assert!(
        stderr.contains("repo config may not set session-root; it belongs in global config"),
        "{stderr}",
    );
    assert!(!repo.join("sessions").exists());
    assert!(!data.exists());
}

/// A global `~` root is under the home directory, for `run`, which writes the
/// record, and for `ls`, which finds it -- not a directory named `~` beside
/// the global file or the repo.
#[tokio::test]
async fn a_tilde_root_is_under_home_for_run_and_ls() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let repo = repo_with_config(tmp.path(), "");
    let global = global_config(
        tmp.path(),
        &format!("session-root = \"~/outrig-sessions\"\n{GLOBAL_WITH_MODEL}"),
    );
    let env = [("HOME", home.as_path())];

    let (ok, stderr) = run_outrig_with_env(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "run",
            "--image",
            ABSENT_IMAGE,
        ],
        &env,
    )
    .await;
    assert!(
        !ok,
        "the stubbed podman cannot start a container:\n{stderr}"
    );
    let recorded = SessionStore::new(home.join("outrig-sessions"))
        .list()
        .unwrap()
        .sessions;
    assert_eq!(
        recorded.len(),
        1,
        "the session is recorded under home before the container starts:\n{stderr}"
    );
    assert!(!repo.join("~").exists());
    assert!(!tmp.path().join("global/~").exists());

    let out = run_outrig_output(&repo, &["--global-config", utf8(&global), "ls"], &env).await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains(recorded[0].id.as_str()), "{stdout}");
}

/// The flag is the whole answer when given: the global file is not read, so a
/// broken one cannot get in its way.
#[test]
fn the_flag_wins_without_reading_the_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    let broken = tmp.path().join("config.toml");
    fs::write(&broken, "session-root = [").unwrap();
    let flag = tmp.path().join("flagged");

    assert_eq!(
        session::resolve_session_root_for_cli(Some(&flag), &broken).unwrap(),
        flag,
    );
    assert!(
        session::resolve_session_root_for_cli(None, &broken).is_err(),
        "without the flag the global file is read",
    );
}
