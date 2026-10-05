//! A workspace outrig picks by default is never the home directory or one
//! above it (#411). Run against a `HOME` in a tempdir, with the stubbed
//! runtime of [`common::run_outrig_with_env`], so nothing real is mounted.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{GLOBAL_WITH_MODEL, run_outrig_with_env};
use outrig_cli::session::{Session, SessionStore};

/// A tempdir holding `home/u` (the `HOME` for each run), a global config that
/// resolves a model, and a session root, all outside `HOME`.
struct Fixture {
    tmp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("home/u")).unwrap();
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        fs::write(tmp.path().join("global.toml"), GLOBAL_WITH_MODEL).unwrap();
        Self { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home/u")
    }

    /// `outrig <cmd> ...` from `cwd`, after the global flags every run here
    /// shares. The image is a raw local ref, so nothing is pulled.
    async fn run(&self, cwd: &Path, cmd: &[&str]) -> (bool, String) {
        let global = self.tmp.path().join("global.toml");
        let sessions = self.tmp.path().join("sessions");
        let mut args = vec![
            "--global-config",
            global.to_str().unwrap(),
            "--session-root",
            sessions.to_str().unwrap(),
        ];
        args.extend_from_slice(cmd);
        args.extend_from_slice(&["--image", "localhost/outrig-test-absent:latest"]);
        run_outrig_with_env(cwd, &args, &[("HOME", &self.home())]).await
    }

    fn sessions(&self) -> Vec<Session> {
        SessionStore::new(self.tmp.path().join("sessions"))
            .list()
            .unwrap()
            .sessions
    }
}

fn write_repo_config(root: &Path, body: &str) {
    let dir = root.join(".agents/outrig");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), body).unwrap();
}

/// Refused before the session is recorded, naming the directory.
fn assert_refused(fx: &Fixture, ok: bool, stderr: &str, workspace: &Path) {
    assert!(!ok, "must be refused:\n{stderr}");
    assert!(
        stderr.contains(&format!(
            "refusing to mount {} as the workspace",
            workspace.display()
        )),
        "the refusal names {}:\n{stderr}",
        workspace.display(),
    );
    assert!(fx.sessions().is_empty(), "nothing is recorded:\n{stderr}");
}

/// Past the guard: the stubbed podman fails the container start, after the
/// session is recorded with the workspace it would have mounted.
fn assert_allowed(fx: &Fixture, stderr: &str, workspace: &Path) {
    assert!(
        !stderr.contains("refusing to mount"),
        "must not be refused:\n{stderr}"
    );
    let sessions = fx.sessions();
    assert_eq!(sessions.len(), 1, "the session is recorded:\n{stderr}");
    assert_eq!(sessions[0].working_dir, workspace);
}

/// With no repo config, the working directory is the default workspace:
/// refused at `HOME`, and above it, for both commands that mount one.
#[tokio::test]
async fn config_less_runs_from_home_or_above_are_refused() {
    let fx = Fixture::new();
    let home = fx.home();
    let above = home.parent().unwrap().to_path_buf();

    for (cwd, cmd) in [(&home, "run"), (&home, "mcp"), (&above, "run")] {
        let (ok, stderr) = fx.run(cwd, &[cmd]).await;
        assert_refused(&fx, ok, &stderr, cwd);
        assert!(
            stderr.contains("no repo config was found, so it is the current directory"),
            "{stderr}"
        );
    }
}

/// A repo config at `HOME` makes it the repo root of every config-less
/// directory below; the refusal names that file.
#[tokio::test]
async fn a_stray_home_repo_config_is_refused_from_below() {
    let fx = Fixture::new();
    let home = fx.home();
    write_repo_config(&home, "");
    let scratch = home.join("scratch");
    fs::create_dir_all(&scratch).unwrap();

    let (ok, stderr) = fx.run(&scratch, &["run"]).await;
    assert_refused(&fx, ok, &stderr, &home);
    assert!(
        stderr.contains(&format!(
            "{} makes it the repo root",
            home.join(".agents/outrig/config.toml").display()
        )),
        "{stderr}"
    );
}

/// An out-of-tree `--config` runs against the working directory, so from
/// `HOME` it is held to the same rule.
#[tokio::test]
async fn an_out_of_tree_config_from_home_is_refused() {
    let fx = Fixture::new();
    let home = fx.home();
    let file = fx.tmp.path().join("configs/outrig.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "").unwrap();

    let (ok, stderr) = fx
        .run(&home, &["--config", file.to_str().unwrap(), "run"])
        .await;
    assert_refused(&fx, ok, &stderr, &home);
}

/// Declaring `host-path` mounts the home directory on purpose.
#[tokio::test]
async fn a_declared_host_path_may_name_home() {
    let fx = Fixture::new();
    let home = fx.home();
    write_repo_config(&home, "[workspace]\nhost-path = \".\"\n");

    let (_, stderr) = fx.run(&home, &["run"]).await;
    assert_allowed(&fx, &stderr, &home);
}

/// A project below `HOME` is the ordinary case, and untouched.
#[tokio::test]
async fn a_config_less_run_below_home_is_allowed() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    fs::create_dir_all(&proj).unwrap();

    let (_, stderr) = fx.run(&proj, &["run"]).await;
    assert_allowed(&fx, &stderr, &proj);
}
