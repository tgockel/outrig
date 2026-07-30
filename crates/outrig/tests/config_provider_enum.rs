//! Schema tests for the `LlmProvider` tagged-enum (task 0013). Exercises
//! parse + validate paths the existing `config_schema` and `config_merge`
//! tests don't cover -- the mistralrs variant invariants, the typo error
//! message, and `model-cache-root` validation.

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use outrig::config::{ApiKeyRef, Config, ConfigValidationError, LlmProvider};
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

/// Every mistralrs weight field is rejected on an Anthropic model, and the
/// diagnostic names `anthropic` rather than whichever remote style happens to
/// share the rule.
#[test]
fn anthropic_model_rejects_every_mistralrs_field() {
    for (field, line) in [
        ("model-id", r#"model-id = "Qwen/Qwen2.5-7B-Instruct""#),
        ("model-path", r#"model-path = "/tmp/model.gguf""#),
        ("model-file", r#"model-file = "model.gguf""#),
        ("revision", r#"revision = "main""#),
        ("context-length", "context-length = 4096"),
        ("device", r#"device = "cpu""#),
    ] {
        let cfg = parse(&format!(
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
        ));
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::RemoteModelHasMistralrsField {
                    ref model, style, field: got, ..
                } if model == "sonnet" && style == "anthropic" && got == field
            ),
            "{field} should be rejected, got: {err:?}",
        );
    }
}

#[test]
fn mistralrs_with_model_id_and_file_parses_and_validates() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
"#,
    );
    cfg.validate(None).expect("validates");
    let serialized = toml::to_string(&cfg).expect("serializes");
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);
}

#[test]
fn mistralrs_device_forms_parse_validate_and_round_trip() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.cpu]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "cpu"

[models.cuda_default]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "cuda"

[models.cuda_indexed]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "cuda:1"

[models.metal]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "metal"
"#,
    );
    cfg.validate(None).expect("all documented forms validate");
    assert_eq!(cfg.models["cpu"].device.as_deref(), Some("cpu"));
    assert_eq!(cfg.models["cuda_default"].device.as_deref(), Some("cuda"));
    assert_eq!(cfg.models["cuda_indexed"].device.as_deref(), Some("cuda:1"));
    assert_eq!(cfg.models["metal"].device.as_deref(), Some("metal"));

    let serialized = toml::to_string(&cfg).expect("serializes");
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);
}

#[test]
fn mistralrs_invalid_device_fails_validate() {
    for device in ["gpu", "cuda:", "cuda:abc", "metal:0"] {
        let cfg = parse(&format!(
            r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "{device}"
"#,
        ));
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::MistralrsDeviceInvalid {
                    ref model,
                    device: ref got,
                } if model == "qwen" && got.as_str() == device
            ),
            "device {device:?} got: {err:?}",
        );
    }
}

#[test]
fn mistralrs_model_id_without_model_file_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider = "local"
model-id = "Qwen/Qwen2.5-7B-Instruct"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsModelIdMissingFile {
                ref model, ref model_id,
            } if model == "qwen" && model_id == "Qwen/Qwen2.5-7B-Instruct"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_missing_both_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider = "local"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsMissingModelSource { ref model }
                if model == "qwen"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_with_both_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-path = "/tmp/model.gguf"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsBothModelSources { ref model }
                if model == "qwen"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_extra_field_without_model_id_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider   = "local"
model-path = "/tmp/model.gguf"
model-file = "weights.gguf"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsExtraFieldRequiresModelId {
                ref model, field,
            } if model == "qwen" && field == "model-file"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_model_with_identifier_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.qwen]
provider   = "local"
identifier = "qwen-on-the-wire"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsModelHasRemoteField { ref model, field }
                if model == "qwen" && field == "identifier"
        ),
        "got: {err:?}",
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

#[test]
fn openai_model_with_weight_field_fails_validate() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
model-id   = "should-not-be-here"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::RemoteModelHasMistralrsField { ref model, style, field, .. }
                if model == "fast" && style == "openai" && field == "model-id"
        ),
        "got: {err:?}",
    );
}

#[test]
fn openai_model_with_device_fails_validate() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
device     = "cuda"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::RemoteModelHasMistralrsField { ref model, style, field, .. }
                if model == "fast" && style == "openai" && field == "device"
        ),
        "got: {err:?}",
    );
}

/// Pin the unknown-style error so a teammate's typo (`mistral-rs` vs
/// `mistralrs`) lands on a useful message rather than a cryptic serde dump.
/// The contract is "the message names what the user typed and at least one
/// legal variant"; we don't pin the exact phrasing so a serde minor-version
/// rephrasing won't break the regression test.
#[test]
fn unknown_style_typo_useful_error() {
    let toml = r#"
[providers.local]
style    = "mistral-rs"
base-url = "https://localhost:1234/v1"
api-key  = "${KEY}"
"#;
    let err = Config::load_from_str(toml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mistral-rs"),
        "error should quote the offending value, got: {msg}",
    );
    assert!(
        msg.contains("openai") || msg.contains("mistralrs"),
        "error should name at least one legal variant, got: {msg}",
    );
}

#[test]
fn model_cache_root_relative_fails_validate() {
    let cfg = parse(
        r#"
model-cache-root = "models"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::ModelCacheRootNotAbsolute { ref path }
                if path.as_os_str() == "models"
        ),
        "got: {err:?}",
    );
}

#[test]
fn model_cache_root_absolute_validates() {
    let cfg = parse(
        r#"
model-cache-root = "/var/cache/outrig/models"
"#,
    );
    cfg.validate(None).expect("absolute path is fine");
}

/// Mirrors how dockerfile/context paths are checked against `repo_root`:
/// `model-path` is allowed to be relative; the existence check is anchored
/// at the repo root.
#[test]
fn mistralrs_relative_model_path_resolves_against_repo_root() {
    let tmp = tempdir().unwrap();
    let model_dir = tmp.path().join("models");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join("local.gguf"), b"\0").unwrap();

    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.local]
provider   = "local"
model-path = "models/local.gguf"
"#,
    );
    cfg.validate(Some(tmp.path()))
        .expect("relative model-path under repo_root resolves");
}

#[test]
fn mistralrs_relative_model_path_missing_under_repo_root_errors() {
    let tmp = tempdir().unwrap();
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"

[models.local]
provider   = "local"
model-path = "models/missing.gguf"
"#,
    );
    let err = match cfg.validate(Some(tmp.path())) {
        Err(OutrigError::ConfigValidation(e)) => e,
        other => panic!("expected validation error, got: {other:?}"),
    };
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsModelPathMissing { ref model, .. }
                if model == "local"
        ),
        "got: {err:?}",
    );
}

/// `with_retry_budget_secs` is the additive counterpart to the positional
/// constructors, so it has to hold for every variant -- including the
/// in-process one, where it is a documented no-op. Nothing in the config path
/// calls it (serde populates the field directly), so this is the only thing
/// pinning the public API's behavior.
#[test]
fn with_retry_budget_secs_sets_remote_variants_and_skips_mistralrs() {
    let key = ApiKeyRef::parse("${OPENAI_API_KEY}").expect("api-key ref parses");
    let openai = LlmProvider::openai("https://api.openai.com/v1", key, Some(90))
        .with_retry_budget_secs(Some(120));
    let LlmProvider::OpenAi {
        request_timeout_secs,
        retry_budget_secs,
        ..
    } = &openai
    else {
        panic!("expected the OpenAi variant, got: {openai:?}");
    };
    assert_eq!(*retry_budget_secs, Some(120));
    assert_eq!(
        *request_timeout_secs,
        Some(90),
        "the builder must not disturb the fields the constructor set",
    );

    let key = ApiKeyRef::parse("${ANTHROPIC_API_KEY}").expect("api-key ref parses");
    let anthropic =
        LlmProvider::anthropic("https://api.anthropic.com", key, None).with_retry_budget_secs(None);
    assert!(
        matches!(
            anthropic,
            LlmProvider::Anthropic {
                retry_budget_secs: None,
                ..
            }
        ),
        "got: {anthropic:?}",
    );

    // No HTTP layer, so nothing to retry and nowhere to record it. The TOML
    // path cannot express this at all -- `deny_unknown_fields` on the tagged
    // enum rejects `retry-budget-secs` under `style = "mistralrs"`.
    assert_eq!(
        LlmProvider::Mistralrs.with_retry_budget_secs(Some(300)),
        LlmProvider::Mistralrs,
    );
}
