//! Integration tests for `outrig init`.
//!
//! Drives `init::run_with` end-to-end through scripted stdin against a
//! tempdir-rooted cwd + global-config path. Covers fresh-state setup
//! (everything written, parses + validates) and the idempotent re-run case
//! (existing files left alone).

mod common;

use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig::init::run_with;

use common::{StubHfTreeFetcher, scripted_prompt};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Scripted stdin for an "all defaults" walk through every prompt.
///
/// Phase 1 -- global config (`config::init::run_with`, 11 prompts):
///   provider style/name/url/env-var, no extra provider,
///   define-a-model + model name/provider/identifier, no extra model,
///   use-as-default-model.
///
/// Phase 2 -- repo config (`init::repo::ensure`, 11 prompts when global
/// has providers + models):
///   configure-repo-models (Y) -> model name/provider/identifier,
///   add-another-model (N), use-as-default-model (Y),
///   agent name, preamble,
///   container name, workspace host-path, container-path.
///
/// Phase 3 -- container loop (4 prompts): base image, toolchains, mcp
/// servers, add-another (N). The container name was asked during
/// bootstrap and threaded into `container::add::run_with`; the
/// "add-first" gate is skipped when phase 2 bootstrapped a container.
///
/// Total: 11 + 11 + 4 = 26 newlines.
const ALL_DEFAULTS: &[u8] = b"\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n";

#[tokio::test]
async fn fresh_state_writes_global_repo_and_container() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");

    let (mut prompt, _stderr_r) = scripted_prompt(ALL_DEFAULTS).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect("init must succeed");

    // Both files exist + parse + validate.
    assert!(global.is_file(), "global config not written");
    let global_text = std::fs::read_to_string(&global).unwrap();
    Config::load_from_str(&global_text)
        .unwrap()
        .validate(None)
        .unwrap();

    let repo_cfg_path = cwd.join(".agents/outrig/config.toml");
    assert!(repo_cfg_path.is_file(), "repo config not written");

    // Container name default = `<repo-folder-kebab>-standard` (== "repo-standard").
    // Agent name default = "coder" (constant, role-based).
    let dockerfile = cwd.join(".agents/outrig/containers/repo-standard/Dockerfile");
    assert!(dockerfile.is_file(), "container Dockerfile not written");

    // Merged validation (with repo_root) succeeds: containers reference
    // existing dockerfile/context paths, default-* keys resolve, etc.
    let merged = Config::load(&cwd, Some(&global)).expect("merged config must load");
    assert_eq!(merged.default_container.as_deref(), Some("repo-standard"));
    assert_eq!(merged.default_agent.as_deref(), Some("coder"));
    assert_eq!(merged.default_model.as_deref(), Some("fast"));

    // Repo config carries the agent + workspace + container-loop output.
    let repo_text = std::fs::read_to_string(&repo_cfg_path).unwrap();
    assert!(repo_text.contains("[agents.coder]"), "{repo_text}");
    assert!(repo_text.contains("[workspace]"), "{repo_text}");
    assert!(
        repo_text.contains("[containers.repo-standard]"),
        "{repo_text}"
    );
}

#[tokio::test]
async fn idempotent_rerun_leaves_files_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");

    // Pre-seed everything: do a fresh run first.
    let (mut prompt, _stderr_r) = scripted_prompt(ALL_DEFAULTS).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());
    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("seed init must not hang")
    .expect("seed init must succeed");

    let global_before = std::fs::read_to_string(&global).unwrap();
    let repo_cfg_path = cwd.join(".agents/outrig/config.toml");
    let repo_before = std::fs::read_to_string(&repo_cfg_path).unwrap();
    let dockerfile_path = cwd.join(".agents/outrig/containers/repo-standard/Dockerfile");
    let dockerfile_before = std::fs::read_to_string(&dockerfile_path).unwrap();

    // Re-run: only the container-loop prompt fires (answer "no") -- both
    // the global and repo phases short-circuit on "exists".
    let (mut prompt, _stderr_r) = scripted_prompt(b"n\n").await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());
    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("re-run must not hang")
    .expect("re-run must succeed");

    assert_eq!(
        std::fs::read_to_string(&global).unwrap(),
        global_before,
        "global config rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(&repo_cfg_path).unwrap(),
        repo_before,
        "repo config rewritten"
    );
    assert_eq!(
        std::fs::read_to_string(&dockerfile_path).unwrap(),
        dockerfile_before,
        "Dockerfile rewritten"
    );
}

#[tokio::test]
async fn skips_global_phase_when_global_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");

    // Pre-seed a minimal global config. `default-model` lives at the top
    // because anything after a `[section]` header gets nested into that
    // section.
    let pre_global = "default-model = \"fast\"\n\
                      \n\
                      [providers.openai]\n\
                      style = \"openai\"\n\
                      base-url = \"https://api.openai.com/v1\"\n\
                      api-key = \"${OPENAI_API_KEY}\"\n\
                      \n\
                      [models.fast]\n\
                      provider = \"openai\"\n\
                      identifier = \"gpt-4o-mini\"\n";
    std::fs::write(&global, pre_global).unwrap();

    // Script: phase 1 is skipped entirely. Phase 2 takes 11 defaults
    // (configure-repo-models=Y + model name/provider/identifier +
    // add-another=N + use-as-default=Y, agent x2, container name,
    // workspace x2); phase 3 takes 4 (base image, toolchains, mcp,
    // add-another -- the add-first gate is skipped after bootstrap).
    // Total 15.
    let script = b"\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect("init must succeed");

    assert_eq!(
        std::fs::read_to_string(&global).unwrap(),
        pre_global,
        "existing global was rewritten",
    );
    assert!(cwd.join(".agents/outrig/config.toml").is_file());
    assert!(
        cwd.join(".agents/outrig/containers/repo-standard/Dockerfile")
            .is_file()
    );

    // Touch path: the merged load works against the pre-seeded global.
    let _merged: Config = Config::load(&cwd, Some(&global)).expect("merged config must load");
}
