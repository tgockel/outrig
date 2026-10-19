//! #444 through the binary: a repo config file may reference only the host
//! variables the global config's `[repo] env-allow` lists.
//!
//! Not `e2e`-gated, for the reason `builtin_default.rs` gives: each assertion
//! is settled at config load, before the first podman call, or by the stub
//! [`common::run_outrig`] puts in podman's place. `build_arg_reference.rs`
//! holds the build-arg half to the same line; this holds the MCP half and the
//! global file's side of it.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{ABSENT_IMAGE, GLOBAL_WITH_MODEL, run_outrig, utf8};

/// A repo at `<tmp>/repo` whose `.agents/outrig/config.toml` holds
/// `repo_body`, beside a global config holding `global_body` and a session
/// root, so a run that gets past config load leaves its record under `tmp`
/// rather than in the user's store. Returns `(repo, global, sessions)`.
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

/// A repo config whose one image runs an MCP server with `GITHUB_TOKEN`
/// handed to it, which the global config may or may not allow.
fn mcp_env_repo() -> String {
    format!(
        "default-image = \"coding\"\n\n[images.coding]\nimage-name = \"{ABSENT_IMAGE}\"\n\n\
         [images.coding.mcp]\nsrv = {{ command = [\"srv\"], env = {{ TOKEN = \"${{GITHUB_TOKEN}}\" }} }}\n"
    )
}

/// `outrig <subcommand>` against the fixture, as `(ok, stderr)`.
async fn run_in(repo: &Path, global: &Path, sessions: &Path, subcommand: &str) -> (bool, String) {
    run_outrig(
        repo,
        &[
            "--global-config",
            utf8(global),
            "--session-root",
            utf8(sessions),
            subcommand,
        ],
    )
    .await
}

/// The issue's second case: the global config lists nothing, so the repo's
/// `env = { TOKEN = "${GITHUB_TOKEN}" }` is refused -- by `run` and by `mcp`
/// alike -- naming the image, the server, the key, the variable and the repo
/// file, at config load, before anything is resolved or started.
#[tokio::test]
async fn an_mcp_env_reference_the_global_config_does_not_list_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(tmp.path(), GLOBAL_WITH_MODEL, &mcp_env_repo());
    let repo_file = repo.join(".agents/outrig/config.toml");

    for subcommand in ["run", "mcp"] {
        let (ok, stderr) = run_in(&repo, &global, &sessions, subcommand).await;

        assert!(!ok, "{subcommand} must refuse the reference:\n{stderr}");
        assert!(
            stderr.contains(&format!(
                "repo config may not reference ${{GITHUB_TOKEN}} in image \"coding\" mcp \
                 server \"srv\" env key \"TOKEN\" (declared in {repo_file:?}): list \
                 \"GITHUB_TOKEN\" in [repo] env-allow in global config to allow it"
            )),
            "{subcommand}: the error names what was referenced, where, and the remedy:\n{stderr}"
        );
        assert!(
            !stderr.contains("resolving agent and container"),
            "{subcommand}: the refusal comes at config load:\n{stderr}"
        );
    }
}

/// The same repo under a global config that lists the variable gets past
/// config load and on to the image cascade, where the stub podman ends the run
/// for an unrelated reason.
#[tokio::test]
async fn a_listed_mcp_env_reference_loads() {
    let tmp = tempfile::tempdir().unwrap();
    let global_body = format!("{GLOBAL_WITH_MODEL}\n[repo]\nenv-allow = [\"GITHUB_TOKEN\"]\n");
    let (repo, global, sessions) = fixture(tmp.path(), &global_body, &mcp_env_repo());

    let (_ok, stderr) = run_in(&repo, &global, &sessions, "run").await;

    assert!(
        !stderr.contains("repo config may not reference"),
        "a listed variable is the supported shape:\n{stderr}"
    );
    assert!(
        stderr.contains("config loaded"),
        "the load must get past the reference rule:\n{stderr}"
    );
}

/// The reason the rule is a validation rather than a load-time one: a global
/// `env-allow` of the wrong type is set aside with a warning and leaves the
/// list empty, so the reference is refused -- and the warning that explains
/// why has to be on screen first.
#[tokio::test]
async fn a_set_aside_list_is_warned_about_before_the_refusal_it_causes() {
    let tmp = tempfile::tempdir().unwrap();
    let global_body = format!("{GLOBAL_WITH_MODEL}\n[repo]\nenv-allow = \"GITHUB_TOKEN\"\n");
    let (repo, global, sessions) = fixture(tmp.path(), &global_body, &mcp_env_repo());

    let (ok, stderr) = run_in(&repo, &global, &sessions, "run").await;

    assert!(!ok, "the reference is refused:\n{stderr}");
    let warning_line = format!("[outrig] warning: {}:", global.display());
    let warning = stderr
        .find(&warning_line)
        .unwrap_or_else(|| panic!("the set-aside list is warned about:\n{stderr}"));
    let refusal = stderr
        .find("repo config may not reference ${GITHUB_TOKEN}")
        .unwrap_or_else(|| panic!("the reference is refused:\n{stderr}"));
    assert!(
        warning < refusal,
        "the warning that explains the refusal comes first:\n{stderr}"
    );
    assert!(
        stderr[warning..refusal].contains("`repo.env-allow` ignored"),
        "the warning names the key it set aside:\n{stderr}"
    );
}

/// Only the operator's file says what a repo file may reference, so a repo
/// file declaring the list is refused.
#[tokio::test]
async fn a_repo_config_declaring_the_list_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, global, sessions) = fixture(
        tmp.path(),
        GLOBAL_WITH_MODEL,
        "[repo]\nenv-allow = [\"GITHUB_TOKEN\"]\n",
    );

    let (ok, stderr) = run_in(&repo, &global, &sessions, "run").await;

    assert!(!ok, "the repo file must be refused:\n{stderr}");
    assert!(
        stderr.contains("repo config may not set [repo].env-allow; it belongs in global config"),
        "the error says where the list belongs:\n{stderr}"
    );
    assert!(
        !stderr.contains("resolving agent and container"),
        "the refusal comes at config load:\n{stderr}"
    );
}
