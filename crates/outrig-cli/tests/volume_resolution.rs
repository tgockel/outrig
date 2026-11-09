//! A `--volume` host path is resolved against the directory the flag was
//! typed in, not against the repo root (#367).
//!
//! The flag comes from a shell, which reads a relative path from the current
//! directory; a config mount is read against the file that declared it, and a
//! `--volume` was declared in no file. From a subdirectory of a repo the two
//! bases differ, and the old one silently mounted a sibling of the directory
//! the user meant. Not `e2e`-gated: the mounts are validated before the first
//! podman call, and [`common::run_outrig`] stubs podman out.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{GLOBAL_WITH_MODEL, run_outrig};

/// A repo with a `sub/` to run from, a global config that resolves a model,
/// and a session root -- all a `--volume` needs to get as far as validation.
struct Fixture {
    tmp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("repo/.agents/outrig/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "").unwrap();
        fs::create_dir_all(tmp.path().join("repo/sub")).unwrap();
        fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        fs::write(tmp.path().join("global.toml"), GLOBAL_WITH_MODEL).unwrap();
        Self { tmp }
    }

    fn repo(&self) -> PathBuf {
        self.tmp.path().join("repo")
    }

    /// The subdirectory every run here is issued from.
    fn sub(&self) -> PathBuf {
        self.repo().join("sub")
    }

    /// `outrig <cmd> --volume <spec>` from `cwd`, against an image no
    /// registry holds, so a run that passes validation stops at the image.
    async fn run(&self, cmd: &str, cwd: &Path, spec: &str) -> (bool, String) {
        let global = self.tmp.path().join("global.toml");
        let sessions = self.tmp.path().join("sessions");
        run_outrig(
            cwd,
            &[
                "--global-config",
                global.to_str().unwrap(),
                "--session-root",
                sessions.to_str().unwrap(),
                cmd,
                "--image",
                common::ABSENT_IMAGE,
                "--volume",
                spec,
            ],
        )
        .await
    }
}

/// Past the mount rules: the run reached the image, which is the next thing
/// that stops it.
fn assert_mounted(ok: bool, stderr: &str) {
    assert!(!ok, "the absent image stops the run:\n{stderr}");
    assert!(
        !stderr.contains("host-path"),
        "the mount was refused:\n{stderr}"
    );
    assert!(
        stderr.contains("[outrig] ensuring image"),
        "the run did not get past the mount rules:\n{stderr}",
    );
}

/// From `<repo>/sub`, `--volume data:/data` means `<repo>/sub/data`, which
/// is the only `data/` there is: the repo root has none, so the old base
/// refused a directory that was right there.
#[tokio::test]
async fn a_relative_volume_names_a_directory_under_the_current_one() {
    let fx = Fixture::new();
    fs::create_dir(fx.sub().join("data")).unwrap();

    for cmd in ["run", "mcp"] {
        let (ok, stderr) = fx.run(cmd, &fx.sub(), "data:/data:rw").await;
        assert_mounted(ok, &stderr);
    }
}

/// The same spelling with only the repo root's `data/` present is refused,
/// and the refusal names the path the flag means. Before, it mounted
/// `<repo>/data` without a word.
#[tokio::test]
async fn a_relative_volume_does_not_fall_back_to_the_repo_root() {
    let fx = Fixture::new();
    fs::create_dir(fx.repo().join("data")).unwrap();

    for cmd in ["run", "mcp"] {
        let (ok, stderr) = fx.run(cmd, &fx.sub(), "data:/data:rw").await;
        assert!(!ok, "`outrig {cmd}` must refuse the mount:\n{stderr}");
        assert!(
            stderr.contains(&format!(
                "host-path {:?} does not exist",
                fx.sub().join("data").display().to_string()
            )),
            "`outrig {cmd}` names the directory the flag means:\n{stderr}",
        );
    }
}

/// A `.`-relative spelling is the same path, written the way a shell
/// completion offers it.
#[tokio::test]
async fn a_dot_relative_volume_resolves_the_same_way() {
    let fx = Fixture::new();
    fs::create_dir(fx.sub().join("data")).unwrap();

    let (ok, stderr) = fx.run("run", &fx.sub(), "./data:/data").await;
    assert_mounted(ok, &stderr);
}

/// An absolute host path is used as written, from any directory.
#[tokio::test]
async fn an_absolute_volume_is_unaffected() {
    let fx = Fixture::new();
    let outside = fx.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();

    let (ok, stderr) = fx
        .run("run", &fx.sub(), &format!("{}:/data", outside.display()))
        .await;
    assert_mounted(ok, &stderr);
}
