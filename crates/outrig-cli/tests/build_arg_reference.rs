//! A `${VAR}` build-arg through the binary, without a container runtime.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: the stub
//! [`common::run_outrig_with_env`] puts in buildah's place fails the build,
//! which is the whole of what this needs.

mod common;

use std::fs;
use std::path::Path;

use common::run_outrig_with_env;

const SECRET: &str = "ghp_FAKE_SECRET_DO_NOT_USE_1234";

/// #324's reproduction: a build that fails printed the token on stderr, with
/// no flag given -- in CI, into the job log. The error now names the
/// reference the config wrote.
#[tokio::test]
async fn a_failed_build_shows_a_referenced_build_arg_as_the_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
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

    let (ok, stderr) = run_outrig_with_env(
        &repo,
        &["--global-config", "/dev/null", "build", "--image", "coding"],
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
