//! Schema tests for the `LlmProvider` tagged-enum (task 0013). Exercises
//! parse + validate paths the existing `config_schema` and `config_merge`
//! tests don't cover -- the mistralrs variant invariants, the typo error
//! message, and `model-cache-root` validation.

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use outrig::config::{Config, ConfigValidationError};
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

#[test]
fn mistralrs_with_model_id_parses_and_validates() {
    let cfg = parse(
        r#"
[providers.local]
style    = "mistralrs"
model-id = "Qwen/Qwen2.5-7B-Instruct"
"#,
    );
    cfg.validate(None).expect("validates");
    let serialized = toml::to_string(&cfg).expect("serializes");
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);
}

#[test]
fn mistralrs_missing_both_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style = "mistralrs"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsMissingModelSource { ref provider }
                if provider == "local"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_with_both_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style      = "mistralrs"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-path = "/tmp/model.gguf"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsBothModelSources { ref provider }
                if provider == "local"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_extra_field_without_model_id_fails_validate() {
    let cfg = parse(
        r#"
[providers.local]
style      = "mistralrs"
model-path = "/tmp/model.gguf"
model-file = "weights.gguf"
"#,
    );
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::MistralrsExtraFieldRequiresModelId {
                ref provider, field,
            } if provider == "local" && field == "model-file"
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
style      = "mistralrs"
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
style      = "mistralrs"
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
            ConfigValidationError::MistralrsModelPathMissing { ref provider, .. }
                if provider == "local"
        ),
        "got: {err:?}",
    );
}
