//! Integration tests for `outrig config init`.
//!
//! Drive `config::init::run_with` through scripted stdin (via `tokio::io::duplex`)
//! against a tempdir-rooted target path. Each test verifies the resulting file
//! parses + validates with the 0005 loader.

use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, duplex};
use tokio::time::timeout;

use outrig::config::Config;
use outrig::config::init::run_with;
use outrig::init::prompt::TerminalPrompt;

const BUF: usize = 4096;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

type ScriptedPrompt = TerminalPrompt<BufReader<DuplexStream>, DuplexStream>;

async fn scripted_prompt(script: &[u8]) -> (ScriptedPrompt, DuplexStream) {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stderr_w, stderr_r) = duplex(BUF);
    stdin_w.write_all(script).await.unwrap();
    drop(stdin_w);
    (
        TerminalPrompt::new(BufReader::new(stdin_r), stderr_w),
        stderr_r,
    )
}

#[tokio::test]
async fn writes_minimal_openai_config() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // 11 prompts, all defaults: style=openai, name=openai, base-url, env-var,
    // no extra provider, define a model, model name + identifier + provider,
    // no extra models, use as default-model.
    let script = b"\n\n\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();

    assert!(
        text.contains("default-model = \"fast\""),
        "missing default-model:\n{text}"
    );
    assert!(
        text.contains("[providers.openai]"),
        "missing providers.openai:\n{text}"
    );
    assert!(
        text.contains("api-key = \"${OPENAI_API_KEY}\""),
        "missing api-key:\n{text}"
    );
    assert!(
        text.contains("base-url = \"https://api.openai.com/v1\""),
        "missing base-url:\n{text}"
    );
    assert!(
        text.contains("[models.fast]"),
        "missing models.fast:\n{text}"
    );
    assert!(
        !text.contains("[workspace]"),
        "global config should not emit [workspace]:\n{text}"
    );
}

#[tokio::test]
async fn refuses_to_clobber_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    std::fs::write(&target, "# pre-existing\n").unwrap();

    // No prompts should be consumed; an empty script is fine.
    let (mut prompt, _stderr_r) = scripted_prompt(b"").await;

    let err = timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect_err("run_with must error when target exists and force=false");

    let msg = format!("{err}");
    assert!(
        msg.contains("already exists") && msg.contains("--force"),
        "unexpected error: {msg}"
    );
    // File untouched.
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "# pre-existing\n"
    );
}

#[tokio::test]
async fn force_overwrites_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    std::fs::write(&target, "# stale\n").unwrap();

    let script = b"\n\n\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(true, &target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed with force=true");

    let text = std::fs::read_to_string(&target).unwrap();
    assert!(!text.contains("# stale"), "stale content remained:\n{text}");
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();
}

#[tokio::test]
async fn writes_mistralrs_config_with_model_id() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Provider: pick mistralrs style by value, name "local". Mistralrs
    // provider has no follow-up prompts.
    // Add another provider? n.
    // Define a model now? (Y default).
    // Model name "phi" -> provider "local" -> auto-download (Y default) ->
    // model-id -> revision blank -> context-length blank.
    // Add another model? n.
    // Use as default-model? (Y default).
    let script =
        b"mistralrs\nlocal\nn\n\nphi\nlocal\n\nmicrosoft/Phi-3-mini-4k-instruct-gguf\n\n\nn\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();

    assert!(
        text.contains("default-model = \"phi\""),
        "missing default-model:\n{text}"
    );
    assert!(
        text.contains("[providers.local]"),
        "missing providers.local:\n{text}"
    );
    assert!(
        text.contains("style = \"mistralrs\""),
        "missing style:\n{text}"
    );
    // Weight fields land under [models.<name>], not under the provider.
    assert!(text.contains("[models.phi]"), "missing models.phi:\n{text}");
    assert!(
        text.contains("model-id = \"microsoft/Phi-3-mini-4k-instruct-gguf\""),
        "missing model-id:\n{text}"
    );
    // mistralrs models don't carry `identifier`.
    assert!(
        !text.contains("identifier ="),
        "unexpected identifier on mistralrs model:\n{text}"
    );
}

#[tokio::test]
async fn no_models_writes_providers_only() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // openai style (default), default name, default base-url, default env-var,
    // no extra provider, NO model.
    let script = b"\n\n\n\n\nn\nn\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();

    assert!(
        !text.contains("default-model"),
        "unexpected default-model:\n{text}"
    );
    assert!(
        !text.contains("[models."),
        "unexpected models table:\n{text}"
    );
    assert!(
        text.contains("[providers.openai]"),
        "missing provider:\n{text}"
    );
}
