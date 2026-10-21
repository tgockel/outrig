//! A workspace outrig picks by default is never the home directory or one
//! above it (#411), nor one that contains the global config (#609). Run
//! against a `HOME` in a tempdir, with the stubbed runtime of
//! [`common::run_outrig_with_env`], so nothing real is mounted.

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
        fs::create_dir_all(tmp.path().join("cfg")).unwrap();
        fs::write(tmp.path().join("cfg/global.toml"), GLOBAL_WITH_MODEL).unwrap();
        Self { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home/u")
    }

    /// `outrig <cmd> ...` from `cwd`, after the global flags every run here
    /// shares. The image is a raw local ref, so nothing is pulled.
    async fn run(&self, cwd: &Path, cmd: &[&str]) -> (bool, String) {
        let global = self.tmp.path().join("cfg/global.toml");
        self.run_with_global(cwd, &global, cmd).await
    }

    /// [`run`](Self::run) reading the global config from `global`.
    async fn run_with_global(&self, cwd: &Path, global: &Path, cmd: &[&str]) -> (bool, String) {
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

    /// [`run`](Self::run) with no `--global-config`: the global config is the
    /// `~/.outrig/config.toml` of the fixture's `HOME`, since an empty
    /// `XDG_CONFIG_HOME` names no location.
    async fn run_default_global(&self, cwd: &Path, cmd: &[&str]) -> (bool, String) {
        let sessions = self.tmp.path().join("sessions");
        let mut args = vec!["--session-root", sessions.to_str().unwrap()];
        args.extend_from_slice(cmd);
        args.extend_from_slice(&["--image", "localhost/outrig-test-absent:latest"]);
        let home = self.home();
        let env = [("HOME", home.as_path()), ("XDG_CONFIG_HOME", Path::new(""))];
        run_outrig_with_env(cwd, &args, &env).await
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

/// A repo config body declaring one extra mount of `host` at `/g`.
fn mount_of(host: &Path, access: &str) -> String {
    format!(
        "[[workspace.mounts]]\nhost-path = \"{}\"\ncontainer-path = \"/g\"\n\
         access = \"{access}\"\n",
        host.display()
    )
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

/// Refused before the session is recorded, naming the repo file that declared it.
fn assert_repo_refused(fx: &Fixture, ok: bool, stderr: &str, declared_in: &Path) {
    assert!(!ok, "must be refused:\n{stderr}");
    assert!(
        stderr.contains(&format!("declared in {}", declared_in.display())),
        "the refusal names {}:\n{stderr}",
        declared_in.display(),
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

/// A repo config is inside the workspace an agent can write, so it may not
/// declare the home directory as the workspace (#445).
#[tokio::test]
async fn a_repo_declared_host_path_may_not_name_home() {
    let fx = Fixture::new();
    let home = fx.home();
    write_repo_config(&home, "[workspace]\nhost-path = \".\"\n");

    let (ok, stderr) = fx.run(&home, &["run"]).await;
    assert_repo_refused(&fx, ok, &stderr, &home.join(".agents/outrig/config.toml"));
}

/// Nor may it mount the global config's directory read-write, which would let
/// the agent rewrite the operator's config for the next session -- but a
/// read-only mount of it is no such path.
#[tokio::test]
async fn a_repo_mount_covering_the_global_config_is_refused_when_read_write() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    let global_dir = fx.tmp.path().join("cfg");

    write_repo_config(&proj, &mount_of(&global_dir, "read-write"));
    let (ok, stderr) = fx.run(&proj, &["run"]).await;
    assert_repo_refused(&fx, ok, &stderr, &proj.join(".agents/outrig/config.toml"));
    assert!(stderr.contains("global config"), "{stderr}");

    write_repo_config(&proj, &mount_of(&global_dir, "read-only"));
    let (_, stderr) = fx.run(&proj, &["run"]).await;
    assert_allowed(&fx, &stderr, &proj);
}

/// The path to the global config is checked as it was given, so a symlink in
/// it makes the directory holding the link off limits read-write (#605).
#[tokio::test]
async fn a_repo_mount_covering_a_symlink_to_the_global_config_is_refused() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    let links = fx.tmp.path().join("links");
    fs::create_dir_all(&links).unwrap();
    std::os::unix::fs::symlink(fx.tmp.path().join("cfg"), links.join("cfg")).unwrap();

    write_repo_config(&proj, &mount_of(&links, "read-write"));
    let global = links.join("cfg/global.toml");
    let (ok, stderr) = fx.run_with_global(&proj, &global, &["run"]).await;
    assert_repo_refused(&fx, ok, &stderr, &proj.join(".agents/outrig/config.toml"));
    assert!(stderr.contains("global config path"), "{stderr}");
}

/// A repo sidecar's mount is held to the same rule as the workspace's.
#[tokio::test]
async fn a_repo_sidecar_mount_covering_the_global_config_is_refused() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    write_repo_config(
        &proj,
        &format!(
            "[sidecars.tools]\nimage = \"localhost/tools:latest\"\n\n\
             [[sidecars.tools.mounts]]\nhost-path = \"{}\"\ncontainer-path = \"/g\"\n\
             access = \"read-write\"\n",
            fx.tmp.path().join("cfg").display()
        ),
    );
    let (ok, stderr) = fx.run(&proj, &["run"]).await;
    assert_repo_refused(&fx, ok, &stderr, &proj.join(".agents/outrig/config.toml"));
    assert!(stderr.contains("sidecar \"tools\" mount"), "{stderr}");
}

/// A global-declared host path is the operator's own choice.
#[tokio::test]
async fn a_global_declared_host_path_may_name_home() {
    let fx = Fixture::new();
    let home = fx.home();
    fs::write(
        fx.tmp.path().join("cfg/global.toml"),
        format!(
            "{GLOBAL_WITH_MODEL}\n[workspace]\nhost-path = \"{}\"\n",
            home.display()
        ),
    )
    .unwrap();

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

/// The default workspace is read-write, so a global config inside it is one the
/// agent can rewrite for the next session. With no repo config the working
/// directory is the workspace, and one holding the file `--global-config` names
/// is refused (#609), as a repo-declared mount of it is (#445).
#[tokio::test]
async fn a_config_less_run_over_the_global_config_is_refused() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let global = proj.join("g.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();

    for cmd in ["run", "mcp"] {
        let (ok, stderr) = fx.run_with_global(&proj, &global, &[cmd]).await;
        assert_refused(&fx, ok, &stderr, &proj);
        assert!(
            stderr.contains(&format!("covers the global config ({})", global.display())),
            "{stderr}"
        );
        assert!(
            stderr.contains("no repo config was found, so it is the current directory"),
            "{stderr}"
        );
    }
}

/// A repo config at the root makes the root the workspace from anywhere below
/// it, so the global config's directory inside the repo is refused from there
/// too, and the refusal names the repo config that chose it.
#[tokio::test]
async fn a_repo_root_over_the_global_configs_directory_is_refused() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    write_repo_config(&proj, "");
    let global = proj.join("conf/global.toml");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let nested = proj.join("src");
    fs::create_dir_all(&nested).unwrap();

    let (ok, stderr) = fx.run_with_global(&nested, &global, &["run"]).await;
    assert_refused(&fx, ok, &stderr, &proj);
    assert!(
        stderr.contains(&format!(
            "{} makes it the repo root",
            proj.join(".agents/outrig/config.toml").display()
        )),
        "{stderr}"
    );
}

/// A symlink in the global config's path, inside the workspace, lets the agent
/// redirect where the next session reads it (#605); the directory holding the
/// link is refused as a default workspace, spelled absolutely or not.
#[tokio::test]
async fn a_symlink_in_the_global_config_path_inside_the_workspace_is_refused() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    fs::create_dir_all(&proj).unwrap();
    std::os::unix::fs::symlink(fx.tmp.path().join("cfg"), proj.join("link")).unwrap();

    for global in [
        proj.join("link/global.toml"),
        PathBuf::from("link/global.toml"),
    ] {
        let (ok, stderr) = fx.run_with_global(&proj, &global, &["run"]).await;
        assert_refused(&fx, ok, &stderr, &proj);
        assert!(
            stderr.contains(&format!("global config path ({})", global.display())),
            "{stderr}"
        );
    }
}

/// The default location is the same hole from its own directory: with no repo
/// config above `~/.outrig`, running there makes it the workspace. A project
/// beside it, or a directory below it, does not contain the file.
#[tokio::test]
async fn the_default_global_config_directory_is_refused_as_a_default_workspace() {
    let fx = Fixture::new();
    let dir = fx.home().join(".outrig");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), GLOBAL_WITH_MODEL).unwrap();

    let (ok, stderr) = fx.run_default_global(&dir, &["run"]).await;
    assert_refused(&fx, ok, &stderr, &dir);
    assert!(
        stderr.contains(&format!(
            "covers the global config ({})",
            dir.join("config.toml").display()
        )),
        "{stderr}"
    );

    for cwd in ["proj", ".outrig/work"] {
        let fx = Fixture::new();
        let (dir, cwd) = (fx.home().join(".outrig"), fx.home().join(cwd));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), GLOBAL_WITH_MODEL).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        let (_, stderr) = fx.run_default_global(&cwd, &["run"]).await;
        assert_allowed(&fx, &stderr, &cwd);
    }
}

/// A workspace the global config declares is the operator's own choice, over
/// the global config itself included.
#[tokio::test]
async fn a_global_declared_host_path_may_cover_the_global_config() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let global = proj.join("g.toml");
    fs::write(
        &global,
        format!(
            "{GLOBAL_WITH_MODEL}\n[workspace]\nhost-path = \"{}\"\n",
            proj.display()
        ),
    )
    .unwrap();

    let (_, stderr) = fx.run_with_global(&proj, &global, &["run"]).await;
    assert_allowed(&fx, &stderr, &proj);
}

/// So is `--volume`: the global config's directory can be handed to the agent
/// on purpose, from a project that does not hold it.
#[tokio::test]
async fn a_volume_of_the_global_configs_directory_is_allowed() {
    let fx = Fixture::new();
    let proj = fx.home().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let volume = format!("{}:/g:rw", fx.tmp.path().join("cfg").display());

    let (_, stderr) = fx.run(&proj, &["run", "--volume", &volume]).await;
    assert_allowed(&fx, &stderr, &proj);
}

/// `HOME` holding the global config breaks both rules; the home refusal is the
/// more basic one, so it is the one reported.
#[tokio::test]
async fn the_home_refusal_comes_before_the_global_config_one() {
    let fx = Fixture::new();
    let home = fx.home();
    let global = home.join("g.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();

    let (ok, stderr) = fx.run_with_global(&home, &global, &["run"]).await;
    assert_refused(&fx, ok, &stderr, &home);
    assert!(stderr.contains("it is your home directory"), "{stderr}");
    assert!(!stderr.contains("covers the global config"), "{stderr}");
}
