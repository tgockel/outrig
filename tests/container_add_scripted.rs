//! Integration tests for `outrig container add` driven through scripted
//! stdin (`tokio::io::duplex`) against tempdir-rooted repo configs. Asserts
//! the resulting Dockerfile, the appended `[containers.<name>]` block,
//! idempotency without `--force`, and `toml_edit`-style preservation of
//! surrounding comments.

mod common;

use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig::container::add::run_with;
use outrig::error::OutrigError;
use outrig::init::repo::resolve_or_bootstrap;

use common::scripted_prompt;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn seed_repo(root: &Path, initial_config: &str) {
    let cfg_dir = root.join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), initial_config).unwrap();
}

#[tokio::test]
async fn defaults_write_dockerfile_and_config_block() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path(), "");

    // Three prompts after the name (which we pass as Some): base image
    // (default = first), toolchains (default = []), MCP servers (default = [fs]).
    let script = b"\n\n\n";
    let (mut prompt, _stderr) = scripted_prompt(script).await;

    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with must succeed");

    let dockerfile_path = tmp
        .path()
        .join(".agents/outrig/containers/coding/Dockerfile");
    let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    assert!(
        dockerfile.starts_with("FROM docker.io/library/debian:bookworm-slim"),
        "unexpected Dockerfile:\n{dockerfile}",
    );
    assert!(dockerfile.contains("@modelcontextprotocol/server-filesystem"));
    assert!(
        dockerfile
            .trim_end()
            .ends_with("CMD [\"sleep\", \"infinity\"]"),
    );

    let cfg_text = std::fs::read_to_string(tmp.path().join(".agents/outrig/config.toml")).unwrap();
    assert!(cfg_text.contains("[containers.coding]"), "{cfg_text}");
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/containers/coding/Dockerfile\""),
        "{cfg_text}",
    );
    assert!(cfg_text.contains("[containers.coding.mcp]"));
    assert!(
        cfg_text.contains("fs = { command = [\"mcp-server-filesystem\", \"/workspace\"] }"),
        "{cfg_text}",
    );

    // Round-trip: parses + structurally validates.
    let cfg = Config::load_from_str(&cfg_text).expect("config must parse");
    cfg.validate(None).expect("config must validate");
}

#[tokio::test]
async fn refuses_when_dockerfile_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path(), "");
    let dockerfile_path = tmp
        .path()
        .join(".agents/outrig/containers/coding/Dockerfile");
    std::fs::create_dir_all(dockerfile_path.parent().unwrap()).unwrap();
    std::fs::write(&dockerfile_path, "# stale\n").unwrap();

    let (mut prompt, _stderr) = scripted_prompt(b"").await;

    let err = timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect_err("run_with must error when Dockerfile exists and force=false");

    let msg = format!("{err}");
    assert!(
        msg.contains("already exists") && msg.contains("--force"),
        "unexpected error: {msg}",
    );
    // Untouched.
    assert_eq!(
        std::fs::read_to_string(&dockerfile_path).unwrap(),
        "# stale\n"
    );
}

#[tokio::test]
async fn refuses_when_config_block_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(
        tmp.path(),
        "[containers.coding]\n\
         dockerfile = \".agents/outrig/containers/coding/Dockerfile\"\n\
         context    = \".agents/outrig/containers/coding\"\n\
         \n\
         [containers.coding.mcp]\n",
    );

    let (mut prompt, _stderr) = scripted_prompt(b"").await;

    let err = timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect_err("run_with must error when block exists and force=false");

    let msg = format!("{err}");
    assert!(
        msg.contains("[containers.coding]") && msg.contains("--force"),
        "unexpected error: {msg}",
    );
}

#[tokio::test]
async fn force_replaces_both_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(
        tmp.path(),
        "[containers.coding]\n\
         dockerfile = \"old/Dockerfile\"\n\
         context    = \"old\"\n\
         \n\
         [containers.coding.mcp]\n\
         fs = { command = [\"old-cmd\"] }\n",
    );
    let dockerfile_path = tmp
        .path()
        .join(".agents/outrig/containers/coding/Dockerfile");
    std::fs::create_dir_all(dockerfile_path.parent().unwrap()).unwrap();
    std::fs::write(&dockerfile_path, "# stale dockerfile\n").unwrap();

    let script = b"\n\n\n"; // base, toolchains, mcp -- all defaults.
    let (mut prompt, _stderr) = scripted_prompt(script).await;

    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), true, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with --force must succeed");

    let dockerfile = std::fs::read_to_string(&dockerfile_path).unwrap();
    assert!(
        !dockerfile.contains("stale"),
        "Dockerfile not replaced:\n{dockerfile}",
    );
    assert!(dockerfile.starts_with("FROM docker.io/library/debian:bookworm-slim"));

    let cfg_text = std::fs::read_to_string(tmp.path().join(".agents/outrig/config.toml")).unwrap();
    assert!(
        !cfg_text.contains("old/Dockerfile"),
        "old [containers.coding] not replaced:\n{cfg_text}",
    );
    assert!(
        !cfg_text.contains("old-cmd"),
        "old mcp entry not replaced:\n{cfg_text}",
    );
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/containers/coding/Dockerfile\""),
        "{cfg_text}",
    );
}

#[tokio::test]
async fn fallback_yes_bootstraps_repo_config() {
    // tempdir lands under TMPDIR, outside any outrig-configured tree, so
    // `find_repo_root_from` walks all the way up and returns NoRepoConfig.
    // Use a named subdirectory so the folder-derived default name is
    // deterministic (`myproj-standard`).
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("myproj");
    std::fs::create_dir_all(&cwd).unwrap();
    let global = tmp.path().join("global.toml");

    // Script: configure-now (Y) + 5 repo-config defaults (container name,
    // workspace x2, agent name, preamble; the model section is
    // informational only when the global config is missing) + 3
    // container-add defaults (base, toolchains, mcp -- name was already
    // asked during bootstrap) = 9 prompts.
    let script = b"\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr) = scripted_prompt(script).await;
    let mut hf = common::StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(TEST_TIMEOUT, async {
        let (repo_root, bootstrapped_name) =
            resolve_or_bootstrap(&cwd, &global, &mut prompt, &mut hf).await?;
        run_with(&repo_root, bootstrapped_name, false, &mut prompt).await
    })
    .await
    .expect("fallback flow must not hang")
    .expect("fallback flow must succeed");

    let cfg_path = cwd.join(".agents/outrig/config.toml");
    let dockerfile = cwd.join(".agents/outrig/containers/myproj-standard/Dockerfile");
    assert!(cfg_path.is_file(), "repo config not bootstrapped");
    assert!(dockerfile.is_file(), "container Dockerfile not written");

    // Parse-only: the standalone repo config can't validate cross-references
    // that depend on the global config (e.g. `default-model`). The merged
    // case is exercised in `tests/init_scripted.rs::fresh_state_*`.
    let cfg = Config::load_from_str(&std::fs::read_to_string(&cfg_path).unwrap())
        .expect("repo config must parse");

    assert_eq!(cfg.default_container.as_deref(), Some("myproj-standard"));
    assert_eq!(cfg.default_agent.as_deref(), Some("coder"));
    assert!(cfg.containers.contains_key("myproj-standard"));
    assert!(cfg.agents.contains_key("coder"));
}

#[tokio::test]
async fn fallback_no_returns_no_repo_config() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global.toml");

    // Script: configure now? -> n. No further prompts should be consumed.
    let script = b"n\n";
    let (mut prompt, _stderr) = scripted_prompt(script).await;
    let mut hf = common::StubHfTreeFetcher::with_files(Vec::<&str>::new());

    let err = timeout(
        TEST_TIMEOUT,
        resolve_or_bootstrap(tmp.path(), &global, &mut prompt, &mut hf),
    )
    .await
    .expect("fallback must not hang")
    .expect_err("declining the prompt must error");

    assert!(
        matches!(err, OutrigError::NoRepoConfig),
        "expected NoRepoConfig, got: {err:?}"
    );

    // Nothing was written.
    assert!(!tmp.path().join(".agents").exists());
}

#[tokio::test]
async fn force_preserves_unrelated_blocks_and_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let initial = "# top-level comment\n\
                   default-container = \"coding\"\n\
                   \n\
                   [containers.coding]\n\
                   # inline comment for coding\n\
                   dockerfile = \"old/Dockerfile\"\n\
                   context    = \"old\"\n\
                   \n\
                   [containers.coding.mcp]\n\
                   \n\
                   [containers.planning]\n\
                   dockerfile = \".agents/outrig/containers/planning/Dockerfile\"\n\
                   context    = \".agents/outrig/containers/planning\"\n\
                   \n\
                   [containers.planning.mcp]\n";
    seed_repo(tmp.path(), initial);

    let script = b"\n\n\n";
    let (mut prompt, _stderr) = scripted_prompt(script).await;

    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), true, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with --force must succeed");

    let cfg_text = std::fs::read_to_string(tmp.path().join(".agents/outrig/config.toml")).unwrap();

    assert!(
        cfg_text.contains("# top-level comment"),
        "top-level comment lost:\n{cfg_text}",
    );
    assert!(
        cfg_text.contains("default-container = \"coding\""),
        "default-container key lost:\n{cfg_text}",
    );
    assert!(
        cfg_text.contains("[containers.planning]"),
        "unrelated [containers.planning] block lost:\n{cfg_text}",
    );
    // The replaced block updated to the new dockerfile path.
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/containers/coding/Dockerfile\""),
        "replaced [containers.coding] missing new dockerfile path:\n{cfg_text}",
    );
}
