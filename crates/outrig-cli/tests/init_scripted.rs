//! Integration tests for `outrig init`.
//!
//! Drives `init::run_with` end-to-end through scripted stdin against a
//! tempdir-rooted cwd + global-config path. Covers fresh-state setup
//! (everything written, parses + validates), the idempotent re-run case
//! (existing files left alone), and the repo phase's checks: a workspace
//! path a load would refuse is asked for again, and a file the next load
//! would refuse is not written.

mod common;

use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig_cli::init::run_with;

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
    let dockerfile = cwd.join(".agents/outrig/images/repo-standard/Dockerfile");
    assert!(dockerfile.is_file(), "container Dockerfile not written");

    // Merged validation (with repo_root) succeeds: containers reference
    // existing dockerfile/context paths, default-* keys resolve, etc.
    let merged = Config::load(&cwd, Some(&global)).expect("merged config must load");
    assert_eq!(merged.default_image.as_deref(), Some("repo-standard"));
    assert_eq!(merged.default_agent.as_deref(), Some("coder"));
    assert_eq!(merged.default_model.as_deref(), Some("fast"));

    // Repo config carries the agent + workspace + container-loop output.
    let repo_text = std::fs::read_to_string(&repo_cfg_path).unwrap();
    assert!(repo_text.contains("[agents.coder]"), "{repo_text}");
    assert!(repo_text.contains("[workspace]"), "{repo_text}");
    assert!(repo_text.contains("[images.repo-standard]"), "{repo_text}");
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
    let dockerfile_path = cwd.join(".agents/outrig/images/repo-standard/Dockerfile");
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
        cwd.join(".agents/outrig/images/repo-standard/Dockerfile")
            .is_file()
    );

    // Touch path: the merged load works against the pre-seeded global.
    let _merged: Config = Config::load(&cwd, Some(&global)).expect("merged config must load");
}

/// #330: the repo phase may define models but never a provider -- a provider
/// carries an API key, and a repo config may not declare one. A provider name
/// the global config lacks is re-asked rather than created, so the file this
/// phase writes is one its own next load accepts.
#[tokio::test]
async fn the_repo_phase_refuses_a_provider_the_global_config_lacks() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        "default-model = \"fast\"\n\
         \n\
         [providers.openai]\n\
         style = \"openai\"\n\
         base-url = \"https://api.openai.com/v1\"\n\
         api-key = \"${OPENAI_API_KEY}\"\n\
         \n\
         [models.fast]\n\
         provider = \"openai\"\n\
         identifier = \"gpt-4o-mini\"\n",
    )
    .unwrap();

    // Phase 1 is skipped. Phase 2: configure-repo-models (Y), model name
    // (default), provider `nowhere` -- unknown, so the prompt comes straight
    // back without offering to define it -- then `openai`, identifier
    // (default), add-another (N), use-as-default (Y), agent x2, container
    // name, workspace x2; phase 3 takes 4. One line more than the all-defaults
    // walk, for the re-asked provider: 16.
    let script = b"\n\nnowhere\nopenai\n\n\n\n\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect("init must succeed");

    let repo_cfg = std::fs::read_to_string(cwd.join(".agents/outrig/config.toml")).unwrap();
    assert!(
        !repo_cfg.contains("[providers."),
        "the repo phase must not write a provider:\n{repo_cfg}"
    );
    assert!(
        repo_cfg.contains("provider = \"openai\""),
        "the model names the global provider:\n{repo_cfg}"
    );
    let _merged: Config = Config::load(&cwd, Some(&global)).expect("merged config must load");
}

/// #348: each workspace path is asked for until the rules a load applies take
/// it, rather than written as typed and refused by the next `outrig run`.
/// `src` names no directory in the repo, `workspace` is relative, and `/`
/// would cover the image's root filesystem (#341).
#[tokio::test]
async fn the_repo_phase_asks_again_for_a_workspace_path_the_load_would_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        "default-model = \"fast\"\n\
         \n\
         [providers.openai]\n\
         style = \"openai\"\n\
         base-url = \"https://api.openai.com/v1\"\n\
         api-key = \"${OPENAI_API_KEY}\"\n\
         \n\
         [models.fast]\n\
         provider = \"openai\"\n\
         identifier = \"gpt-4o-mini\"\n",
    )
    .unwrap();

    // Phase 1 is skipped. Phase 2 walks the defaults up to the workspace --
    // configure-repo-models (Y), model name/provider/identifier, add-another
    // (N), use-as-default (Y), agent name, preamble, image name -- then
    // host-path `src`, refused, and the default; container-path `workspace`
    // and `/`, both refused, and the default. Phase 3 takes 4. Three lines
    // more than the all-defaults walk, one per refusal: 18.
    let script = b"\n\n\n\n\n\n\n\n\nsrc\n\nworkspace\n/\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect("init must succeed");

    let merged = Config::load(&cwd, Some(&global)).expect("merged config must load");
    assert_eq!(merged.workspace.declared_host_path(), Some(Path::new(".")));
    assert_eq!(
        merged.workspace.declared_container_path(),
        Some(Path::new("/workspace"))
    );
}

/// #348: the repo phase reads the file it is about to write as the next load
/// will -- merged over the global config -- and writes nothing that load
/// would refuse. The global `default-model` here names no model, so the
/// summary drops it and the agent is asked for one; declining a repo
/// default-model still leaves the merge inheriting the dangling one, which is
/// a file `outrig run` used to be the first to refuse.
#[tokio::test]
async fn the_repo_phase_writes_nothing_the_next_load_would_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        "default-model = \"ghost\"\n\
         \n\
         [providers.openai]\n\
         style = \"openai\"\n\
         base-url = \"https://api.openai.com/v1\"\n\
         api-key = \"${OPENAI_API_KEY}\"\n\
         \n\
         [models.fast]\n\
         provider = \"openai\"\n\
         identifier = \"gpt-4o-mini\"\n",
    )
    .unwrap();

    // Phase 1 is skipped. Phase 2: configure-repo-models (n), set a repo
    // default-model (n; it defaults to yes, the global default having been
    // dropped), agent name, model for this agent (`fast`), preamble, image
    // name, workspace x2. The check refuses before phase 3 asks anything.
    let script = b"n\nn\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    let err = timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect_err("init must refuse a config the next load would refuse");

    let msg = err.to_string();
    assert!(msg.contains("default-model \"ghost\""), "{msg}");
    assert!(msg.contains(".agents/outrig/config.toml"), "{msg}");
    assert!(!cwd.join(".agents").exists(), "nothing may be written");
}

/// With no model anywhere, the agent is written without one and the wizard
/// says so, so the check leaves that agent out rather than refusing it. A
/// global row the summary dropped as unloadable is no model to choose, and
/// the refusal names that row -- not the agent it left without a model.
#[tokio::test]
async fn the_repo_phase_names_a_broken_global_model_rather_than_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        "[providers.openai]\n\
         style = \"openai\"\n\
         base-url = \"https://api.openai.com/v1\"\n\
         api-key = \"${OPENAI_API_KEY}\"\n\
         \n\
         [models.broken]\n\
         alias = \"ghost\"\n",
    )
    .unwrap();

    // Phase 1 is skipped. Phase 2: configure-repo-models (n); with no global
    // model left to pick, neither the repo default-model nor the agent's
    // model is asked for. Then agent name, preamble, image name, workspace x2.
    let script = b"n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    let err = timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect_err("init must refuse a config the next load would refuse");

    let msg = err.to_string();
    assert!(msg.contains("alias target \"ghost\""), "{msg}");
    assert!(!msg.contains("omits 'model'"), "{msg}");
    assert!(!cwd.join(".agents").exists(), "nothing may be written");
}

/// A global config whose agent `reviewer` names `image`, and that inherits
/// `fast` as its model.
fn global_with_reviewer_image(image: &str) -> String {
    format!(
        "default-model = \"fast\"\n\
         \n\
         [providers.openai]\n\
         style = \"openai\"\n\
         base-url = \"https://api.openai.com/v1\"\n\
         api-key = \"${{OPENAI_API_KEY}}\"\n\
         \n\
         [models.fast]\n\
         provider = \"openai\"\n\
         identifier = \"gpt-4o-mini\"\n\
         \n\
         [agents.reviewer]\n\
         image = \"{image}\"\n"
    )
}

/// The image the repo phase names is the one the `image add` after it
/// writes, so the check takes a reference to it as resolved: here a global
/// agent's `image`, which loads once that block lands.
#[tokio::test]
async fn the_repo_phase_takes_a_reference_to_the_image_it_is_about_to_add() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, global_with_reviewer_image("standard")).unwrap();

    // Phase 1 is skipped. Phase 2: configure-repo-models (n), set a repo
    // default-model (no; the global one is inherited), agent name, preamble,
    // image name `standard`, workspace x2. Phase 3 takes 4.
    let script = b"n\n\n\n\nstandard\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect("init must succeed");

    let merged = Config::load(&cwd, Some(&global)).expect("merged config must load");
    assert!(merged.images.contains_key("standard"));
}

/// Only the image about to be added is taken as resolved: a reference to an
/// image no config declares is refused as the next load would refuse it.
#[tokio::test]
async fn the_repo_phase_refuses_a_reference_to_an_image_nothing_declares() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, global_with_reviewer_image("elsewhere")).unwrap();

    // As above, through the workspace; the check refuses before phase 3.
    let script = b"n\n\n\n\nstandard\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    let err = timeout(
        TEST_TIMEOUT,
        run_with(false, Some(&global), &cwd, &mut prompt, &mut hf),
    )
    .await
    .expect("init must not hang")
    .expect_err("init must refuse a config the next load would refuse");

    let msg = err.to_string();
    assert!(msg.contains("image=\"elsewhere\""), "{msg}");
    assert!(!cwd.join(".agents").exists(), "nothing may be written");
}
