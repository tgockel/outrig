//! `--config <path>` through the binary, without a container runtime.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: each assertion
//! is settled before the first podman call, or by the stub
//! [`common::run_outrig`] puts in podman's place.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{ABSENT_IMAGE, GLOBAL_WITH_MODEL, run_outrig, utf8};
use outrig_cli::session::SessionStore;

/// #323's fixture, under `tmp`: `home/u/proj/ci/outrig.toml` declares
/// `named`, and `home/u/.agents/outrig/config.toml` -- three levels above
/// it, where `--config` used to look -- declares `planted`. Neither image's
/// Dockerfile exists, so a build stops at validation, naming whichever
/// file was read. Returns `(proj, named_file)`.
fn issue_fixture(tmp: &Path) -> (PathBuf, PathBuf) {
    let proj = tmp.join("home/u/proj");
    fs::create_dir_all(proj.join("ci")).unwrap();
    let named = proj.join("ci/outrig.toml");
    fs::write(
        &named,
        "[images.named]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    let planted = tmp.join("home/u/.agents/outrig");
    fs::create_dir_all(&planted).unwrap();
    fs::write(
        planted.join("config.toml"),
        "[images.planted]\ndockerfile = \"planted/Dockerfile\"\ncontext = \"planted\"\n",
    )
    .unwrap();
    (proj, named)
}

/// The issue's reproduction, absolute and relative: `build` reads the named
/// file, and its failure names that file rather than one it never opened.
#[tokio::test]
async fn build_reads_the_named_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (proj, named) = issue_fixture(tmp.path());
    let absent = tmp.path().join("no-such-global.toml");

    for config in [utf8(&named), "ci/outrig.toml"] {
        let (ok, stderr) = run_outrig(
            &proj,
            &[
                "--global-config",
                utf8(&absent),
                "--config",
                config,
                "build",
                "--image",
                "named",
            ],
        )
        .await;

        assert!(!ok, "the named file's Dockerfile is missing:\n{stderr}");
        assert!(
            stderr.contains(&format!(
                "image \"named\" dockerfile path \"Dockerfile\" does not exist \
                 (declared in {named:?})"
            )),
            "--config {config} must read {}:\n{stderr}",
            named.display(),
        );
        assert!(
            !stderr.contains("planted"),
            "--config {config} must not read the config three levels up:\n{stderr}",
        );
    }
}

/// A `--config` that names nothing is a mistake to report, not a config-less
/// session to start, in every command that reads the flag.
#[tokio::test]
async fn a_missing_config_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (proj, _named) = issue_fixture(tmp.path());
    let absent = tmp.path().join("no-such-global.toml");
    let typo = proj.join("ci/outrig-typo.toml");

    for cmd in ["build", "run", "mcp"] {
        let (ok, stderr) = run_outrig(
            &proj,
            &[
                "--global-config",
                utf8(&absent),
                "--config",
                utf8(&typo),
                cmd,
            ],
        )
        .await;

        assert!(
            !ok,
            "`outrig {cmd}` must refuse a missing --config:\n{stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "--config {} is not an existing file",
                typo.display()
            )),
            "`outrig {cmd}` must name the missing path:\n{stderr}",
        );
    }
}

/// An out-of-tree `--config` runs against the repo the command runs from.
/// The session record's working directory is the project, not the directory
/// three levels above the file -- which here holds a repo config of its own,
/// so the old derivation would have taken it for a repo. With no repo above
/// the project either, the working directory is a default, and says so.
#[tokio::test]
async fn run_under_an_out_of_tree_config_works_in_the_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let file = tmp.path().join("other/deep/ci/outrig.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "").unwrap();
    let decoy = tmp.path().join("other/.agents/outrig");
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("config.toml"), "").unwrap();
    let global = tmp.path().join("global.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let (ok, stderr) = run_outrig(
        &proj,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&sessions),
            "--config",
            utf8(&file),
            "run",
            "--image",
            ABSENT_IMAGE,
        ],
    )
    .await;
    assert!(
        !ok,
        "the stubbed podman cannot start a container:\n{stderr}"
    );

    assert!(
        stderr.contains(&format!(
            "no repo config found; using current directory as workspace ({})",
            proj.display()
        )),
        "a working directory taken by default is announced:\n{stderr}",
    );

    let recorded = SessionStore::new(sessions).list().unwrap().sessions;
    assert_eq!(
        recorded.len(),
        1,
        "the session is recorded before the container starts:\n{stderr}"
    );
    assert_eq!(recorded[0].working_dir, proj);
}
