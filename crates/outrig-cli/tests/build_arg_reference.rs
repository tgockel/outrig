//! A `${VAR}` build-arg through the binary, without a container runtime.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: the stub
//! [`common::run_outrig_with_env`] puts in buildah's place fails the build,
//! which is the whole of what this needs.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{run_outrig_with_env, utf8};

const SECRET: &str = "ghp_FAKE_SECRET_DO_NOT_USE_1234";

/// A repo at `<tmp>/repo` whose `coding` image takes `GH_TOKEN` from
/// `${GITHUB_TOKEN}`, beside a global config holding `global_body`. A repo
/// file may reference only the variables the global config lists (#444), so
/// what the global config says is what these tests vary. Returns
/// `(repo, global)`.
fn fixture(tmp: &Path, global_body: &str) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    let image_dir = repo.join(".agents/outrig/images/coding");
    fs::create_dir_all(&image_dir).unwrap();
    fs::write(
        repo.join(".agents/outrig/config.toml"),
        r#"default-image = "coding"
[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"
build-args = { GH_TOKEN = "${GITHUB_TOKEN}" }
"#,
    )
    .unwrap();
    fs::write(
        image_dir.join("Dockerfile"),
        "FROM scratch\nARG GH_TOKEN\nRUN false\n",
    )
    .unwrap();
    let global = tmp.join("global.toml");
    fs::write(&global, global_body).unwrap();
    (repo, global)
}

/// #324's reproduction: a build that fails printed the token on stderr, with
/// no flag given -- in CI, into the job log. The error now names the
/// reference the config wrote.
#[tokio::test]
async fn a_failed_build_shows_a_referenced_build_arg_as_the_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global) = fixture(tmp.path(), "[repo]\nenv-allow = [\"GITHUB_TOKEN\"]\n");

    let (ok, stderr) = run_outrig_with_env(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "build",
            "--image",
            "coding",
        ],
        &[("GITHUB_TOKEN", Path::new(SECRET))],
    )
    .await;

    assert!(!ok, "the stub buildah fails the build; stderr:\n{stderr}");
    assert!(
        stderr.contains("process `buildah` exited"),
        "the failure is the build's; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("GH_TOKEN=${GITHUB_TOKEN}"),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains(SECRET), "stderr:\n{stderr}");
}

/// #444: the same repo under a global config that lists nothing. The repo
/// file may not name `GITHUB_TOKEN`, so the build is refused at config load,
/// before buildah runs and before the variable is read.
#[tokio::test]
async fn a_build_arg_reference_the_global_config_does_not_list_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global) = fixture(tmp.path(), "");

    let (ok, stderr) = run_outrig_with_env(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "build",
            "--image",
            "coding",
        ],
        &[("GITHUB_TOKEN", Path::new(SECRET))],
    )
    .await;

    assert!(!ok, "the reference must be refused; stderr:\n{stderr}");
    assert!(
        stderr.contains(
            "repo config may not reference ${GITHUB_TOKEN} in image \"coding\" \
             build-arg \"GH_TOKEN\""
        ),
        "the error names the variable, the image, and the key; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("list \"GITHUB_TOKEN\" in [repo] env-allow in global config"),
        "the error says how to allow it; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("buildah"),
        "the refusal comes before any build; stderr:\n{stderr}"
    );
    assert!(!stderr.contains(SECRET), "stderr:\n{stderr}");
}
