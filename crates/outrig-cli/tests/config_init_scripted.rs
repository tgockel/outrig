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

use common::scripted_prompt;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn writes_minimal_openai_config() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // 11 prompts, all defaults: style=chat-completions, name=openai, base-url, env-var,
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

    timeout(TEST_TIMEOUT, run_with(false, &target, &mut prompt))
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
        "max-tokens = 128000",
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

/// #346: a second provider of one style is offered `<style>-2`, and a name
/// already given is asked for again rather than replacing the first entry.
#[tokio::test]
async fn a_repeated_provider_name_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // style (openai), name (openai), base-url A, env-var, add another (y);
    // style (openai), name `openai` -- taken, so asked again -- then the
    // default (openai-2), base-url B, env-var, no extra provider, NO model.
    let script = b"\n\nhttps://a.example/v1\n\ny\n\nopenai\n\nhttps://b.example/v1\n\nn\nn\n";
    let (text, shown) = write_config(&target, script).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    for expected in [
        "[providers.openai]\nstyle = \"chat-completions\"\nbase-url = \"https://a.example/v1\"",
        "[providers.openai-2]\nstyle = \"chat-completions\"\nbase-url = \"https://b.example/v1\"",
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

    // Provider defaults (4), no extra provider, define a model; name (fast),
    // provider, identifier gpt-a, add another (y); name `fast` -- taken, so
    // asked again -- then the default (fast-2), provider, identifier gpt-b,
    // no extra model, default-model name (fast).
    let script = b"\n\n\n\n\n\n\n\ngpt-a\ny\nfast\n\n\ngpt-b\nn\n\n";
    let (text, shown) = write_config(&target, script).await;
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

/// #347: a `base-url` a load would refuse is asked for again at its prompt,
/// rather than ending the wizard at its last check with nothing written.
#[tokio::test]
async fn a_base_url_that_is_not_http_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // style (openai), name (openai), base-url with a typo'd scheme -- refused,
    // so asked again -- then a good one, env-var, no extra provider, NO model.
    let script = b"\n\nhtps://api.example/v1\nhttps://api.example/v1\n\nn\nn\n";
    let (text, shown) = write_config(&target, script).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    assert!(
        text.contains("base-url = \"https://api.example/v1\""),
        "missing the second base-url:\n{text}"
    );
    assert_eq!(
        shown
            .matches("? Base URL [default: https://api.openai.com/v1]: ")
            .count(),
        2,
        "the refused base-url must be asked for again:\n{shown}"
    );
}

/// #347: an API key answer that is not an env var name -- the key itself
/// pasted in, or a name in lower case -- is asked for again, and the `${VAR}`
/// form the config stores is taken as typed. The pasted key lands nowhere.
#[tokio::test]
async fn an_api_key_answer_that_is_not_a_var_name_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // style (openai), name (openai), base-url, then three env-var answers: a
    // pasted key and a lower-case name, both refused, then `${MY_KEY}`; no
    // extra provider, NO model.
    let script = b"\n\n\nsk-not-a-real-key\nopenai_api_key\n${MY_KEY}\nn\nn\n";
    let (text, shown) = write_config(&target, script).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    assert!(
        text.contains("api-key = \"${MY_KEY}\""),
        "missing the ${{VAR}} answer as typed:\n{text}"
    );
    assert_eq!(
        shown
            .matches("? API key environment variable [default: OPENAI_API_KEY]: ")
            .count(),
        3,
        "each refused answer must be asked for again:\n{shown}"
    );
    for out in [&text, &shown] {
        assert!(
            !out.contains("sk-not-a-real-key"),
            "the pasted key was repeated:\n{out}"
        );
    }
}

/// #259: `0` at the `max-tokens` prompt is refused and asked for again, so the
/// scaffolder cannot write a value validation rejects.
#[tokio::test]
async fn a_zero_max_tokens_answer_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    let script = b"anthropic\nclaude\n\n\n\n\nsonnet\n\n\n0\n32000\n\n\n";
    let (text, shown) = write_config(&target, script).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    assert!(text.contains("max-tokens = 32000"), "got:\n{text}");
    assert_eq!(
        shown
            .matches("? max-tokens for this model [default: 128000]: ")
            .count(),
        2,
        "the refused 0 must be asked for again:\n{shown}"
    );
}

/// #347: a `max-tokens` answer that is not a number is asked for again,
/// rather than ending the wizard with every earlier answer lost.
#[tokio::test]
async fn a_max_tokens_answer_that_is_not_a_number_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("config.toml");

    // The script of `writes_anthropic_config_with_max_tokens`, answering
    // max-tokens with `64k` -- refused, so asked again -- and then `32000`.
    let script = b"anthropic\nclaude\n\n\n\n\nsonnet\n\n\n64k\n32000\n\n\n";
    let (text, shown) = write_config(&target, script).await;
    Config::load_from_str(&text)
        .unwrap()
        .validate(None)
        .unwrap();

    assert!(
        text.contains("max-tokens = 32000"),
        "missing the second max-tokens:\n{text}"
    );
    assert_eq!(
        shown
            .matches("? max-tokens for this model [default: 128000]: ")
            .count(),
        2,
        "the refused max-tokens must be asked for again:\n{shown}"
    );
}

/// Run `config init` on `script`, and return the written config along with
/// everything the prompt printed.
async fn write_config(target: &Path, script: &[u8]) -> (String, String) {
    let (mut prompt, mut stderr_r) = scripted_prompt(script).await;

    timeout(TEST_TIMEOUT, run_with(false, target, &mut prompt))
        .await
        .expect("run_with must not hang")
        .expect("run_with must succeed");

    // Dropping the prompt closes its end of the stderr pipe, so the read ends.
    drop(prompt);
    let mut shown = String::new();
    stderr_r.read_to_string(&mut shown).await.unwrap();
    (std::fs::read_to_string(target).unwrap(), shown)
}
