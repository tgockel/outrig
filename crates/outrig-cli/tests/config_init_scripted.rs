//! Integration tests for `outrig config init`.
//!
//! Drive `config::init::run_with` through scripted stdin (via `tokio::io::duplex`)
//! against a tempdir-rooted target path. Each test verifies the resulting file
//! parses + validates with the 0005 loader.

mod common;

use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig_cli::config_init::run_with;

use common::{StubHfTreeFetcher, scripted_prompt};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn writes_minimal_openai_config() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // 11 prompts, all defaults: style=openai, name=openai, base-url, env-var,
    // no extra provider, define a model, model name + identifier + provider,
    // no extra models, use as default-model.
    let script = b"\n\n\n\n\n\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
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

/// The Anthropic style walks the same provider prompts as openai with its own
/// defaults, and adds the `max-tokens` its API requires. Accepting every
/// default has to yield a config that works on the first turn -- that is the
/// whole reason the ceiling is prompted for here.
#[tokio::test]
async fn writes_anthropic_config_with_max_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // style=anthropic, name=claude, then defaults: base-url, env-var, no
    // extra provider, define a model, name=sonnet, provider (default), then
    // identifier + max-tokens defaults, no extra models, use as default-model.
    let script = b"anthropic\nclaude\n\n\n\n\nsonnet\n\n\n\n\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();

    for expected in [
        "[providers.claude]",
        "style = \"anthropic\"",
        // The bare endpoint: rig appends `/v1/messages` itself.
        "base-url = \"https://api.anthropic.com\"",
        "api-key = \"${ANTHROPIC_API_KEY}\"",
        "[models.sonnet]",
        "identifier = \"claude-sonnet-4-6\"",
        "max-tokens = 64000",
        "default-model = \"sonnet\"",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
}

#[tokio::test]
async fn refuses_to_clobber_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    std::fs::write(&target, "# pre-existing\n").unwrap();

    // No prompts should be consumed; an empty script is fine.
    let (mut prompt, _stderr_r) = scripted_prompt(b"").await;
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    let err = timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
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
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(TEST_TIMEOUT, run_with(true, &target, &mut prompt, &mut hf))
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
    // model-id -> revision blank -> (HF stub returns one file -> auto-pick,
    // no prompt consumed) -> context-length blank.
    // Add another model? n.
    // Use as default-model? (Y default).
    let script =
        b"mistralrs\nlocal\nn\n\nphi\nlocal\n\nmicrosoft/Phi-3-mini-4k-instruct-gguf\n\n\nn\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files(["Phi-3-mini-4k-instruct-q4.gguf"]);

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
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
    // model-file always serializes as an array (single-shard configs
    // are length-1; the deserializer accepts either form, but the
    // writer canonicalizes).
    assert!(
        text.contains("model-file = [\"Phi-3-mini-4k-instruct-q4.gguf\"]"),
        "missing auto-picked model-file:\n{text}"
    );
    // mistralrs models don't carry `identifier`.
    assert!(
        !text.contains("identifier ="),
        "unexpected identifier on mistralrs model:\n{text}"
    );
}

#[tokio::test]
async fn writes_mistralrs_config_with_model_file_picker() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Same script shape as the auto-pick test, with one extra answer
    // ("2") between `revision` and `context-length` for the GGUF picker.
    let script = b"mistralrs\nlocal\nn\n\nqwen\nlocal\n\nQwen/Qwen2.5-Coder-1.5B-Instruct-GGUF\n\n2\n\nn\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_files([
        "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        "qwen2.5-coder-1.5b-instruct-q5_k_m.gguf",
        "qwen2.5-coder-1.5b-instruct-q8_0.gguf",
    ]);

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();
    // Picker accepts a 1-based index. "2" -> the q5_k_m file.
    assert!(
        text.contains("model-file = [\"qwen2.5-coder-1.5b-instruct-q5_k_m.gguf\"]"),
        "picker didn't write the chosen file:\n{text}"
    );
}

#[tokio::test]
async fn writes_mistralrs_config_with_multi_shard_pick() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Pick three shards via comma-separated input: "1,2,3".
    let script = b"mistralrs\nlocal\nn\n\nllama\nlocal\n\nsplit/repo\n\n1,2,3\n\nn\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::with_sized_files([
        ("llama-q4-00001-of-00003.gguf", 5_000_000_000),
        ("llama-q4-00002-of-00003.gguf", 5_000_000_000),
        ("llama-q4-00003-of-00003.gguf", 4_000_000_000),
    ]);

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();
    assert!(
        text.contains("00001-of-00003.gguf")
            && text.contains("00002-of-00003.gguf")
            && text.contains("00003-of-00003.gguf"),
        "missing all three shards in model-file:\n{text}"
    );
}

#[tokio::test]
async fn mistralrs_falls_back_to_free_form_when_hf_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // After model-id + blank revision, the HF query errors and the flow
    // falls through to the free-form `MODEL_FILE_FIELD` text prompt --
    // hence the extra `phi.gguf` answer in the script.
    let script = b"mistralrs\nlocal\nn\n\nphi\nlocal\n\nsome/repo\n\nphi.gguf\n\nn\n\n";
    let (mut prompt, _stderr_r) = scripted_prompt(script).await;
    let mut hf = StubHfTreeFetcher::errors_with("simulated network failure");

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    let text = std::fs::read_to_string(&target).unwrap();
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();
    assert!(
        text.contains("model-file = [\"phi.gguf\"]"),
        "missing free-form-prompt model-file:\n{text}"
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
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt, &mut hf))
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
