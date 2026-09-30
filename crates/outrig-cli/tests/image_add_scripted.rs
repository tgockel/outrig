//! Integration tests for `outrig image add` driven through scripted
//! stdin (`tokio::io::duplex`) against tempdir-rooted repo configs. Asserts
//! the resulting Dockerfile, the appended `[images.<name>]` block,
//! idempotency without `--force`, `toml_edit`-style preservation of
//! surrounding comments, an inline `images` table, the refusal of an
//! `images` that is not a table before any prompt, and that a config that
//! can't be written leaves no Dockerfile behind.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig::error::OutrigError;
use outrig_cli::error::CliError;
use outrig_cli::image_setup::add::run_with;
use outrig_cli::init::repo::resolve_or_bootstrap;

use common::scripted_prompt;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn seed_repo(root: &Path, initial_config: &str) {
    let cfg_dir = root.join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), initial_config).unwrap();
}

fn read_config(root: &Path) -> String {
    std::fs::read_to_string(root.join(".agents/outrig/config.toml")).unwrap()
}

fn coding_dockerfile(root: &Path) -> PathBuf {
    root.join(".agents/outrig/images/coding/Dockerfile")
}

const STANDARD_SEED: &str = "[images.base]\nimage-name = \"debian\"\n";

/// The config from #186: valid, and spelling `images` as an inline table.
const INLINE_SEED: &str = "default-image = \"base\"\n\
     images = { base = { image-name = \"docker.io/library/debian:bookworm-slim\" } }\n";

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

    let dockerfile_path = tmp.path().join(".agents/outrig/images/coding/Dockerfile");
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
    assert!(cfg_text.contains("[images.coding]"), "{cfg_text}");
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/images/coding/Dockerfile\""),
        "{cfg_text}",
    );
    assert!(cfg_text.contains("[images.coding.mcp]"));
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
    let dockerfile_path = tmp.path().join(".agents/outrig/images/coding/Dockerfile");
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
        "[images.coding]\n\
         dockerfile = \".agents/outrig/images/coding/Dockerfile\"\n\
         context    = \".agents/outrig/images/coding\"\n\
         \n\
         [images.coding.mcp]\n",
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
        msg.contains("[images.coding]") && msg.contains("--force"),
        "unexpected error: {msg}",
    );
}

#[tokio::test]
async fn force_replaces_both_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(
        tmp.path(),
        "[images.coding]\n\
         dockerfile = \"old/Dockerfile\"\n\
         context    = \"old\"\n\
         \n\
         [images.coding.mcp]\n\
         fs = { command = [\"old-cmd\"] }\n",
    );
    let dockerfile_path = tmp.path().join(".agents/outrig/images/coding/Dockerfile");
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
        "old [images.coding] not replaced:\n{cfg_text}",
    );
    assert!(
        !cfg_text.contains("old-cmd"),
        "old mcp entry not replaced:\n{cfg_text}",
    );
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/images/coding/Dockerfile\""),
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
    let dockerfile = cwd.join(".agents/outrig/images/myproj-standard/Dockerfile");
    assert!(cfg_path.is_file(), "repo config not bootstrapped");
    assert!(dockerfile.is_file(), "container Dockerfile not written");

    // Parse-only: the standalone repo config can't validate cross-references
    // that depend on the global config (e.g. `default-model`). The merged
    // case is exercised in `tests/init_scripted.rs::fresh_state_*`.
    let cfg = Config::load_from_str(&std::fs::read_to_string(&cfg_path).unwrap())
        .expect("repo config must parse");

    assert_eq!(cfg.default_image.as_deref(), Some("myproj-standard"));
    assert_eq!(cfg.default_agent.as_deref(), Some("coder"));
    assert!(cfg.images.contains_key("myproj-standard"));
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
        matches!(err, CliError::Outrig(OutrigError::NoRepoConfig)),
        "expected NoRepoConfig, got: {err:?}"
    );

    // Nothing was written.
    assert!(!tmp.path().join(".agents").exists());
}

#[tokio::test]
async fn force_preserves_unrelated_blocks_and_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let initial = "# top-level comment\n\
                   default-image = \"coding\"\n\
                   \n\
                   [images.coding]\n\
                   # inline comment for coding\n\
                   dockerfile = \"old/Dockerfile\"\n\
                   context    = \"old\"\n\
                   \n\
                   [images.coding.mcp]\n\
                   \n\
                   [images.planning]\n\
                   dockerfile = \".agents/outrig/images/planning/Dockerfile\"\n\
                   context    = \".agents/outrig/images/planning\"\n\
                   \n\
                   [images.planning.mcp]\n";
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
        cfg_text.contains("default-image = \"coding\""),
        "default-image key lost:\n{cfg_text}",
    );
    assert!(
        cfg_text.contains("[images.planning]"),
        "unrelated [images.planning] block lost:\n{cfg_text}",
    );
    // The replaced block updated to the new dockerfile path.
    assert!(
        cfg_text.contains("dockerfile = \".agents/outrig/images/coding/Dockerfile\""),
        "replaced [images.coding] missing new dockerfile path:\n{cfg_text}",
    );
}

/// #186: a config spelling `images` as an inline table, which the loader
/// accepts, is read by the duplicate check and extended by the writer
/// alike. The two disagreed, and the writer panicked with the Dockerfile
/// already written.
#[tokio::test]
async fn an_inline_images_is_checked_and_extended() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path(), INLINE_SEED);

    let (mut prompt, _stderr) = scripted_prompt(b"").await;
    let err = timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("base".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect_err("run_with must refuse an entry the inline table has");
    let msg = format!("{err}");
    assert!(
        msg.contains("[images.base] already exists") && msg.contains("--force"),
        "unexpected error: {msg}",
    );

    let (mut prompt, _stderr) = scripted_prompt(b"\n\n\n").await;
    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with must succeed");

    assert!(coding_dockerfile(tmp.path()).is_file(), "no Dockerfile");
    let text = read_config(tmp.path());
    let mut cfg = Config::load_from_str(&text).expect("config must parse");
    cfg.validate(None).expect("config must validate");
    let coding = cfg.images.remove("coding").expect("coding is added");
    assert!(coding.mcp.contains_key("fs"), "{text}");
    assert_eq!(
        cfg.images,
        Config::load_from_str(INLINE_SEED).unwrap().images,
        "the seed's entries changed:\n{text}",
    );
}

/// An `images` that is not a table is refused before the first prompt, even
/// with `--force`, and nothing is written. These shapes reached the same
/// panic as an inline table, after the Dockerfile was on disk.
#[tokio::test]
async fn an_images_that_is_not_a_table_is_refused_before_any_prompt() {
    for (seed, found) in [
        ("images = \"legacy\"\n", "string"),
        ("images = 3\n", "integer"),
        ("images = []\n", "array"),
        ("[[images]]\nimage-name = \"debian\"\n", "array of tables"),
    ] {
        assert!(
            Config::load_from_str(seed).is_err(),
            "the loader must reject {seed:?} too"
        );
        let tmp = tempfile::tempdir().unwrap();
        seed_repo(tmp.path(), seed);
        // No name and no input: had the name prompt come first, it would
        // have failed on EOF instead.
        let (mut prompt, _stderr) = scripted_prompt(b"").await;

        let err = timeout(TEST_TIMEOUT, run_with(tmp.path(), None, true, &mut prompt))
            .await
            .expect("run_with must not hang")
            .expect_err("an images that is not a table must be refused");

        let msg = format!("{err}");
        assert!(
            msg.contains("`images`") && msg.ends_with(&format!("; got {found}")),
            "{seed:?}: unexpected error: {msg}"
        );
        assert!(
            !tmp.path().join(".agents/outrig/images").exists(),
            "{seed:?}: wrote under images/"
        );
        assert_eq!(read_config(tmp.path()), seed);
    }
}

/// Runs `image add coding` with `.agents/outrig` read-only, so the config
/// can't be written while the Dockerfile's directory, made up front, can.
/// `None` when this user isn't held to the mode bits, as root isn't: the
/// run would succeed and prove nothing.
async fn add_with_unwritable_config(
    root: &Path,
    force: bool,
) -> Option<outrig_cli::error::Result<()>> {
    use std::os::unix::fs::PermissionsExt as _;

    let agents = root.join(".agents/outrig");
    std::fs::create_dir_all(agents.join("images/coding")).unwrap();
    let writable = std::fs::metadata(&agents).unwrap().permissions();
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o555)).unwrap();

    let result = if tempfile::NamedTempFile::new_in(&agents).is_err() {
        let (mut prompt, _stderr) = scripted_prompt(b"\n\n\n").await;
        Some(
            timeout(
                TEST_TIMEOUT,
                run_with(root, Some("coding".to_string()), force, &mut prompt),
            )
            .await,
        )
    } else {
        eprintln!("skipping: permissions not enforced for this user");
        None
    };
    // Before anything can panic: the tempdir can't be removed while read-only.
    std::fs::set_permissions(&agents, writable).unwrap();
    result.map(|r| r.expect("run_with must not hang"))
}

/// A Dockerfile written without its config block would refuse the retry,
/// so a config that can't be written keeps the Dockerfile from being
/// written too.
#[tokio::test]
async fn a_config_that_cannot_be_written_leaves_no_dockerfile() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path(), STANDARD_SEED);
    let Some(result) = add_with_unwritable_config(tmp.path(), false).await else {
        return;
    };

    result.expect_err("an unwritable config must fail the run");
    assert!(
        !coding_dockerfile(tmp.path()).exists(),
        "the Dockerfile was written without its config block"
    );
    assert_eq!(read_config(tmp.path()), STANDARD_SEED);
}

/// Under `--force`, a config that can't be written leaves the Dockerfile it
/// would have replaced as it was.
#[tokio::test]
async fn a_config_that_cannot_be_written_leaves_a_forced_dockerfile_alone() {
    let tmp = tempfile::tempdir().unwrap();
    seed_repo(tmp.path(), STANDARD_SEED);
    let dockerfile_path = coding_dockerfile(tmp.path());
    std::fs::create_dir_all(dockerfile_path.parent().unwrap()).unwrap();
    std::fs::write(&dockerfile_path, "# mine\n").unwrap();
    let Some(result) = add_with_unwritable_config(tmp.path(), true).await else {
        return;
    };

    result.expect_err("an unwritable config must fail the run");
    assert_eq!(
        std::fs::read_to_string(&dockerfile_path).unwrap(),
        "# mine\n"
    );
    assert_eq!(read_config(tmp.path()), STANDARD_SEED);
}
