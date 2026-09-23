//! Schema tests for the `LlmProvider` tagged-enum (task 0001-13). Exercises
//! parse + validate paths the existing `config_schema` and `config_merge`
//! tests don't cover -- the per-style field rules, the typo error message,
//! and the parse errors the removed in-process surface now produces.

use std::path::Path;

use outrig::config::{
    AnthropicOptions, ApiKeyRef, Config, ConfigValidationError, LlmProvider, OpenAiOptions,
};
use outrig::error::OutrigError;

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

fn expect_validation_err(cfg: &Config, repo_root: Option<&Path>) -> ConfigValidationError {
    match cfg.validate(repo_root) {
        Err(OutrigError::ConfigValidation(e)) => e,
        Err(other) => panic!("expected ConfigValidation, got: {other:?}"),
        Ok(()) => panic!("expected validation error, got Ok"),
    }
}

/// The native Anthropic style carries the same connection fields as `openai`
/// and survives a serialize/parse round trip, including the model-level
/// `max-tokens` its API needs.
#[test]
fn anthropic_provider_parses_validates_and_round_trips() {
    let cfg = parse(
        r#"
[providers.claude]
style                = "anthropic"
base-url             = "https://api.anthropic.com"
api-key              = "${ANTHROPIC_API_KEY}"
request-timeout-secs = 120
retry-budget-secs    = 300

[models.sonnet]
provider   = "claude"
identifier = "claude-sonnet-4-6"
max-tokens = 16384
"#,
    );
    cfg.validate(None).expect("validates");

    let LlmProvider::Anthropic {
        base_url,
        api_key,
        request_timeout_secs,
        retry_budget_secs,
        ..
    } = &cfg.providers["claude"]
    else {
        panic!("expected the Anthropic variant, got: {:?}", cfg.providers);
    };
    assert_eq!(base_url, "https://api.anthropic.com");
    assert_eq!(api_key.var_name(), "ANTHROPIC_API_KEY");
    assert_eq!(*request_timeout_secs, Some(120));
    assert_eq!(*retry_budget_secs, Some(300));
    assert_eq!(cfg.models["sonnet"].max_tokens, Some(16384));

    let serialized = toml::to_string(&cfg).expect("serializes");
    assert!(
        serialized.contains(r#"style = "anthropic""#),
        "style should round-trip as the documented tag, got: {serialized}"
    );
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);
}

/// `request-timeout-secs` and `retry-budget-secs` are the optional connection
/// fields, and unknown keys are rejected by the tagged enum rather than
/// silently ignored.
#[test]
fn anthropic_provider_field_rules() {
    let cfg = parse(
        r#"
[providers.claude]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"
"#,
    );
    let LlmProvider::Anthropic {
        request_timeout_secs,
        retry_budget_secs,
        ..
    } = &cfg.providers["claude"]
    else {
        panic!("expected the Anthropic variant");
    };
    assert_eq!(*request_timeout_secs, None);
    assert_eq!(*retry_budget_secs, None);

    for (missing, toml) in [
        (
            "base-url",
            r#"
[providers.claude]
style   = "anthropic"
api-key = "${ANTHROPIC_API_KEY}"
"#,
        ),
        (
            "api-key",
            r#"
[providers.claude]
style    = "anthropic"
base-url = "https://api.anthropic.com"
"#,
        ),
    ] {
        let err = Config::load_from_str(toml).expect_err("missing field should fail");
        assert!(
            err.to_string().contains(missing),
            "error should name the missing {missing}, got: {err}"
        );
    }

    let err = Config::load_from_str(
        r#"
[providers.claude]
style      = "anthropic"
base-url   = "https://api.anthropic.com"
api-key    = "${ANTHROPIC_API_KEY}"
max-tokens = 4096
"#,
    )
    .expect_err("unknown provider field should fail");
    assert!(
        err.to_string().contains("max-tokens"),
        "error should name the unknown field, got: {err}"
    );
}

#[test]
fn anthropic_model_missing_identifier_fails_validate() {
    let cfg = parse(
        r#"
[providers.claude]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"

[models.sonnet]
provider = "claude"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::RemoteModelMissingIdentifier { ref model, style, .. }
                if model == "sonnet" && style == "anthropic"
        ),
        "got: {err:?}",
    );
    assert!(
        err.to_string().contains("provider style=anthropic"),
        "message should name the style the user wrote, got: {err}"
    );
}

#[test]
fn openai_model_missing_identifier_fails_validate() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider = "openai"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::RemoteModelMissingIdentifier { ref model, style, .. }
                if model == "fast" && style == "openai"
        ),
        "got: {err:?}",
    );
}

/// Pin the unknown-style error so a teammate's typo (`open-ai` vs `openai`)
/// lands on a useful message rather than a cryptic serde dump. `open-ai` is the
/// realistic one: it is what the kebab-case rule would have produced, which is
/// why the variant carries an explicit rename. The contract is "the message
/// names what the user typed and at least one legal variant"; we don't pin the
/// exact phrasing so a serde minor-version rephrasing won't break the
/// regression test.
#[test]
fn unknown_style_typo_useful_error() {
    let toml = r#"
[providers.local]
style    = "open-ai"
base-url = "https://localhost:1234/v1"
api-key  = "${KEY}"
"#;
    let err = Config::load_from_str(toml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("open-ai"),
        "error should quote the offending value, got: {msg}",
    );
    assert!(
        msg.contains("openai") || msg.contains("anthropic"),
        "error should name at least one legal variant, got: {msg}",
    );
}

/// `style = "mistralrs"` was removed rather than kept parsing: the tagged enum
/// refuses the tag at parse time, before validation, on every load path. Same
/// contract as the typo above -- quote the value, name a legal style.
#[test]
fn the_removed_mistralrs_style_is_a_parse_error() {
    let err = Config::load_from_str("[providers.local]\nstyle = \"mistralrs\"\n").unwrap_err();
    assert!(
        matches!(err, OutrigError::Config(_)),
        "a parse error, not a validation error: {err:?}",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("mistralrs"),
        "error should quote the offending value, got: {msg}",
    );
    assert!(
        msg.contains("openai") || msg.contains("anthropic"),
        "error should name at least one legal style, got: {msg}",
    );
}

/// The six weight keys and the top-level `model-cache-root` went with the
/// style. Each refuses to parse rather than loading with the key dropped.
#[test]
fn removed_local_llm_keys_are_parse_errors() {
    for line in [
        r#"model-id = "Qwen/Qwen2.5-7B-Instruct-GGUF""#,
        r#"model-path = "/var/cache/outrig/models/model.gguf""#,
        r#"model-file = "model.gguf""#,
        r#"revision = "main""#,
        "context-length = 4096",
        r#"device = "cpu""#,
    ] {
        let key = line.split_once(' ').expect("key = value").0;
        let toml = format!(
            r#"
[providers.claude]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${{ANTHROPIC_API_KEY}}"

[models.sonnet]
provider   = "claude"
identifier = "claude-sonnet-4-6"
{line}
"#
        );
        let err = Config::load_from_str(&toml).unwrap_err();
        assert!(
            matches!(err, OutrigError::Config(_)),
            "{key}: a parse error, not a validation error: {err:?}",
        );
        assert!(
            err.to_string().contains(key),
            "{key}: error should name the key, got: {err}",
        );
    }

    let err =
        Config::load_from_str("model-cache-root = \"/var/cache/outrig/models\"\n").unwrap_err();
    assert!(
        matches!(err, OutrigError::Config(_)),
        "a parse error, not a validation error: {err:?}",
    );
    assert!(
        err.to_string().contains("model-cache-root"),
        "error should name the key, got: {err}",
    );
}

/// The constructor options carry both remote-only settings in one place, each
/// `with_*` reaches its own field without disturbing the other, and `new`
/// leaves both unset. Nothing in the config path constructs a provider this way
/// -- serde populates the fields directly -- so this is the only thing pinning
/// the published surface.
#[test]
fn remote_provider_options_populate_remote_variants() {
    let key = ApiKeyRef::parse("${OPENAI_API_KEY}").expect("api-key ref parses");
    let openai = LlmProvider::openai(
        "https://api.openai.com/v1",
        key,
        OpenAiOptions::new()
            .with_request_timeout_secs(90)
            .with_retry_budget_secs(120),
    );
    assert!(
        matches!(
            openai,
            LlmProvider::OpenAi {
                request_timeout_secs: Some(90),
                retry_budget_secs: Some(120),
                ..
            }
        ),
        "got: {openai:?}",
    );

    let key = ApiKeyRef::parse("${ANTHROPIC_API_KEY}").expect("api-key ref parses");
    let anthropic = LlmProvider::anthropic(
        "https://api.anthropic.com",
        key,
        AnthropicOptions::new().with_retry_budget_secs(0),
    );
    assert!(
        matches!(
            anthropic,
            LlmProvider::Anthropic {
                request_timeout_secs: None,
                retry_budget_secs: Some(0),
                ..
            }
        ),
        "one setter must leave the other field alone, and `0` is retries off \
         rather than an absent value; got: {anthropic:?}",
    );

    // `new` is the no-override spelling, and the one the docs and the migration
    // note name -- so it has to keep agreeing with the derived `Default` that
    // four other call sites reach through. The fields themselves are covered
    // above: the Anthropic provider was built from a bare `new` plus one setter
    // and came out with `request_timeout_secs: None`.
    assert_eq!(OpenAiOptions::new(), OpenAiOptions::default());
    assert_eq!(AnthropicOptions::new(), AnthropicOptions::default());
}
