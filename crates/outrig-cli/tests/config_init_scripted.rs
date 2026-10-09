//! Integration tests for `outrig config init`.
//!
//! Drive `config::init::run_with` through scripted stdin (via `tokio::io::duplex`)
//! against a tempdir-rooted target path. Each test verifies the resulting file
//! parses + validates with the 0001-05 loader.

mod common;

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;
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

/// Run `config init` on `script` against `hf`, and return the written config
/// along with everything the prompt printed.
async fn write_config(
    target: &Path,
    script: &[u8],
    hf: &mut StubHfTreeFetcher,
) -> (String, String) {
    let (mut prompt, mut stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(false, target, &mut prompt, hf))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    // Dropping the prompt closes its end of the stderr pipe, so the read ends.
    drop(prompt);
    let mut shown = String::new();
    stderr_r.read_to_string(&mut shown).await.unwrap();
    (std::fs::read_to_string(target).unwrap(), shown)
}

/// A listing that succeeds but holds no GGUF reaches the same free-form
/// prompt rather than ending setup, and its answer is saved like a pick. The
/// prompt line asks for a path inside the repo, which is what a quantization
/// kept in a directory of its own needs.
#[tokio::test]
async fn mistralrs_falls_back_to_free_form_when_listing_has_no_gguf() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // The script of the error fallback above, answering the free-form prompt
    // with a nested path.
    let script =
        b"mistralrs\nlocal\nn\n\nphi\nlocal\n\nsome/repo\n\nQ4_K_M/model-Q4_K_M.gguf\n\nn\n\n";
    let mut hf = StubHfTreeFetcher::with_files(["README.md", "config.json"]);

    let (text, shown) = write_config(&target, script, &mut hf).await;

    assert!(
        shown.contains("? GGUF model-file (path inside the HF repo): "),
        "prompt line does not ask for a path inside the repo:\n{shown}"
    );
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();
    assert!(
        text.contains("model-file = [\"Q4_K_M/model-Q4_K_M.gguf\"]"),
        "missing free-form-prompt model-file:\n{text}"
    );
}

/// A quantization kept in a directory of its own is offered at its full
/// path, beside the repo's root files, and written with its directory
/// whether it was picked by number or by the path as listed.
#[tokio::test]
async fn writes_mistralrs_config_with_nested_model_file_pick() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Path order puts the `Q4_K_M/` shards ahead of the root file ('Q' sorts
    // before 'm'), so `1` and the second shard's path pick both shards.
    let script = b"mistralrs\nlocal\nn\n\nqwen\nlocal\n\nsome/repo\n\n\
                   1,Q4_K_M/model-Q4_K_M-00002-of-00002.gguf\n\nn\n\n";
    let mut hf = StubHfTreeFetcher::with_files([
        "model-Q8_0.gguf",
        "Q4_K_M/model-Q4_K_M-00001-of-00002.gguf",
        "Q4_K_M/model-Q4_K_M-00002-of-00002.gguf",
        "README.md",
    ]);

    let (text, _) = write_config(&target, script, &mut hf).await;

    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();
    assert_eq!(
        cfg.models["qwen"].model_file,
        Some(vec![
            "Q4_K_M/model-Q4_K_M-00001-of-00002.gguf".into(),
            "Q4_K_M/model-Q4_K_M-00002-of-00002.gguf".into(),
        ]),
        "picker didn't write both nested shards:\n{text}"
    );
}

/// Enter at the picker takes the first file that is a whole model by itself.
/// A directory such as `BF16/` sorts ahead of the root files, and the shard it
/// would put first is no model alone: saved by itself, it downloads in full
/// and then fails to load.
#[tokio::test]
async fn picker_default_skips_split_shards() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Enter at the picker, after the blank revision.
    let script = b"mistralrs\nlocal\nn\n\ngemma\nlocal\n\nsome/repo\n\n\n\nn\n\n";
    let mut hf = StubHfTreeFetcher::with_files([
        "BF16/model-BF16-00001-of-00002.gguf",
        "BF16/model-BF16-00002-of-00002.gguf",
        "model-IQ4_NL.gguf",
        "model-Q4_K_M.gguf",
    ]);

    let (text, shown) = write_config(&target, script, &mut hf).await;

    assert!(
        shown.contains("? Pick GGUF file(s) from the repo [default: model-IQ4_NL.gguf]: "),
        "the default is not the first whole model:\n{shown}"
    );
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();
    assert_eq!(
        cfg.models["gemma"].model_file,
        Some(vec!["model-IQ4_NL.gguf".into()]),
        "Enter didn't take the first whole model:\n{text}"
    );
}

/// A repo of nothing but shards offers no default: Enter asks again, and the
/// pick has to name the shards.
#[tokio::test]
async fn picker_has_no_default_when_every_file_is_a_shard() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // Enter at the picker, then both `Q4_K_M/` shards.
    let script = b"mistralrs\nlocal\nn\n\nds\nlocal\n\nsome/repo\n\n\n1,2\n\nn\n\n";
    let mut hf = StubHfTreeFetcher::with_files([
        "Q4_K_M/model-Q4_K_M-00001-of-00002.gguf",
        "Q4_K_M/model-Q4_K_M-00002-of-00002.gguf",
        "Q8_0/model-Q8_0-00001-of-00002.gguf",
        "Q8_0/model-Q8_0-00002-of-00002.gguf",
    ]);

    let (text, shown) = write_config(&target, script, &mut hf).await;

    assert_eq!(
        shown.matches("? Pick GGUF file(s) from the repo: ").count(),
        2,
        "Enter must ask again, with no default offered:\n{shown}"
    );
    let cfg = Config::load_from_str(&text).unwrap();
    cfg.validate(None).unwrap();
    assert_eq!(
        cfg.models["ds"].model_file,
        Some(vec![
            "Q4_K_M/model-Q4_K_M-00001-of-00002.gguf".into(),
            "Q4_K_M/model-Q4_K_M-00002-of-00002.gguf".into(),
        ]),
        "picker didn't write both shards:\n{text}"
    );
}

/// Drive the mistralrs local-path branch, giving `path_answers` at the
/// `model-path` prompt, and return the written config along with everything
/// the prompt printed.
async fn write_local_path_config(target: &Path, path_answers: &str) -> (String, String) {
    // style=mistralrs, name=local, no extra provider, define a model,
    // name=phi, provider=local, no auto-download, then the path answers,
    // blank context-length, no extra models, use as default-model.
    let script = format!("mistralrs\nlocal\nn\n\nphi\nlocal\nn\n{path_answers}\n\nn\n\n");
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());
    write_config(target, script.as_bytes(), &mut hf).await
}

/// Neither prompt backend prints a free-text field's description unasked, so
/// the base a relative `model-path` is read from has to be in the prompt line
/// itself. `?` adds the advice for a global config. The answer itself is
/// stored as typed: the global config has no single repo root to join it to.
#[tokio::test]
async fn model_path_prompt_names_its_base() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    let (text, shown) = write_local_path_config(&target, "?\nweights/local.gguf").await;

    assert!(
        shown.contains("? Local model-path (absolute, or relative to the repo root): "),
        "prompt line does not name the base:\n{shown}"
    );
    assert!(
        shown.contains("prefer an absolute path"),
        "`?` help does not recommend an absolute path:\n{shown}"
    );
    assert!(
        text.contains("model-path = \"weights/local.gguf\""),
        "relative answer not stored as typed:\n{text}"
    );
}

/// An absolute answer is the one a global config wants: it names the same
/// file from every repo, including one that holds no copy of it.
#[tokio::test]
async fn absolute_model_path_validates_under_any_repo_root() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    let weights = tmp.path().join("weights/local.gguf");
    std::fs::create_dir_all(weights.parent().unwrap()).unwrap();
    std::fs::write(&weights, b"").unwrap();
    let other_repo = tempfile::tempdir().unwrap();

    let (text, _) = write_local_path_config(&target, weights.to_str().unwrap()).await;

    Config::load_from_str(&text)
        .unwrap()
        .validate(Some(other_repo.path()))
        .unwrap();
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

/// #346: a second provider of one style is offered `<style>-2`, and a name
/// already given is asked for again rather than replacing the first entry.
#[tokio::test]
async fn a_repeated_provider_name_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    // style (openai), name (openai), base-url A, env-var, add another (y);
    // style (openai), name `openai` -- taken, so asked again -- then the
    // default (openai-2), base-url B, env-var, no extra provider, NO model.
    let script = b"\n\nhttps://a.example/v1\n\ny\n\nopenai\n\nhttps://b.example/v1\n\nn\nn\n";
    let (text, shown) = write_config(&target, script, &mut hf).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    for expected in [
        "[providers.openai]\nstyle = \"openai\"\nbase-url = \"https://a.example/v1\"",
        "[providers.openai-2]\nstyle = \"openai\"\nbase-url = \"https://b.example/v1\"",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert_eq!(
        shown
            .matches("? Provider name [default: openai-2]: ")
            .count(),
        2,
        "the taken name must be asked for again:\n{shown}"
    );
}

/// #346: a second model is offered `fast-2`, and a name already given is
/// asked for again rather than replacing the first entry.
#[tokio::test]
async fn a_repeated_model_name_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");
    let mut hf = StubHfTreeFetcher::with_files(Vec::<&str>::new());

    // Provider defaults (4), no extra provider, define a model; name (fast),
    // provider, identifier gpt-a, add another (y); name `fast` -- taken, so
    // asked again -- then the default (fast-2), provider, identifier gpt-b,
    // no extra model, default-model name (fast).
    let script = b"\n\n\n\n\n\n\n\ngpt-a\ny\nfast\n\n\ngpt-b\nn\n\n";
    let (text, shown) = write_config(&target, script, &mut hf).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    for expected in [
        "default-model = \"fast\"",
        "[models.fast]\nprovider = \"openai\"\nidentifier = \"gpt-a\"",
        "[models.fast-2]\nprovider = \"openai\"\nidentifier = \"gpt-b\"",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert_eq!(
        shown.matches("? Model name [default: fast-2]: ").count(),
        2,
        "the taken name must be asked for again:\n{shown}"
    );
}
