//! Schema tests for the `LlmProvider` tagged-enum (task 0001-13). Exercises
//! parse + validate paths the existing `config_schema` and `config_merge`
//! tests don't cover -- the per-style field rules, the typo error message,
//! what a load makes of the removed in-process surface, and a model's own
//! `style`.

use std::path::Path;

use outrig::config::{
    AnthropicOptions, ApiKeyRef, ApiStyle, ChatCompletionsOptions, Config, ConfigValidationError,
    ConfigWarningKind, LlmProvider, RoleAlternation,
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
/// `max-tokens` its API needs and the `context-window` `run-new` budgets to.
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
provider       = "claude"
identifier     = "claude-sonnet-4-6"
max-tokens     = 16384
context-window = 200000
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
    assert_eq!(cfg.models["sonnet"].context_window, Some(200_000));

    let serialized = toml::to_string(&cfg).expect("serializes");
    assert!(
        serialized.contains("context-window = 200000"),
        "the key round-trips in its documented spelling, got: {serialized}"
    );
    assert!(
        serialized.contains(r#"style = "anthropic""#),
        "style should round-trip as the documented tag, got: {serialized}"
    );
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);
}

/// `request-timeout-secs` and `retry-budget-secs` are the optional connection
/// fields, and an unknown key is refused by the tagged enum rather than
/// silently ignored -- which a config file's load turns into a warning naming
/// it.
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

    let with_max_tokens = r#"
[providers.claude]
style      = "anthropic"
base-url   = "https://api.anthropic.com"
api-key    = "${ANTHROPIC_API_KEY}"
max-tokens = 4096
"#;
    let err = toml::from_str::<Config>(with_max_tokens).expect_err("unknown provider field");
    assert!(
        err.to_string().contains("max-tokens"),
        "error should name the unknown field, got: {err}"
    );
    let cfg = parse(with_max_tokens);
    let keys: Vec<_> = cfg.warnings().iter().map(|w| w.key()).collect();
    assert_eq!(keys, ["providers.claude.max-tokens"]);
    assert!(matches!(
        cfg.providers["claude"],
        LlmProvider::Anthropic { .. }
    ));
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
        err.to_string().contains("(style=anthropic)"),
        "message should name the style the user wrote, got: {err}"
    );
}

#[test]
fn openai_model_missing_identifier_fails_validate() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "chat-completions"
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
                if model == "fast" && style == "chat-completions"
        ),
        "got: {err:?}",
    );
}

/// `style = "responses"` takes the keys `chat-completions` does, loads as its
/// own variant, and survives a serialize/parse round trip.
#[test]
fn responses_provider_parses_validates_and_round_trips() {
    let cfg = parse(
        r#"
[providers.openai-new]
style                = "responses"
base-url             = "https://api.openai.com/v1"
api-key              = "${OPENAI_API_KEY}"
request-timeout-secs = 120
retry-budget-secs    = 300
role-alternation     = "strict"

[models.fast]
provider   = "openai-new"
identifier = "gpt-5.4"
"#,
    );
    cfg.validate(None).expect("validates");
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());

    let LlmProvider::Responses {
        base_url,
        api_key,
        request_timeout_secs,
        retry_budget_secs,
        role_alternation,
        ..
    } = &cfg.providers["openai-new"]
    else {
        panic!("expected the Responses variant, got: {:?}", cfg.providers);
    };
    assert_eq!(base_url, "https://api.openai.com/v1");
    assert_eq!(api_key.var_name(), "OPENAI_API_KEY");
    assert_eq!(*request_timeout_secs, Some(120));
    assert_eq!(*retry_budget_secs, Some(300));
    assert_eq!(*role_alternation, Some(RoleAlternation::Strict));
    assert_eq!(cfg.providers["openai-new"].style(), "responses");

    let text = toml::to_string(&cfg).expect("serializes");
    assert!(text.contains(r#"style = "responses""#), "{text}");
    assert_eq!(parse(&text).providers, cfg.providers);
}

/// A model on a `responses` provider holds the rows to the same field rules
/// a `chat-completions` one is held to.
#[test]
fn responses_model_missing_identifier_fails_validate() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "responses"
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
                if model == "fast" && style == "responses"
        ),
        "got: {err:?}",
    );
}

/// A 0.2.x config says `style = "openai"`. It loads as `chat-completions`, the
/// provider is not dropped and the models on it validate, and the warning
/// names the spelling to use now.
#[test]
fn the_deprecated_openai_style_loads_as_chat_completions_and_warns() {
    let cfg = parse(
        r#"
[providers.old]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "old"
identifier = "gpt-4o-mini"
"#,
    );
    cfg.validate(None).expect("the model on it validates");
    let LlmProvider::ChatCompletions { .. } = &cfg.providers["old"] else {
        panic!("expected chat-completions, got: {:?}", cfg.providers);
    };
    assert_eq!(cfg.providers["old"].style(), "chat-completions");
    let [warning] = cfg.warnings() else {
        panic!("one warning, got: {:?}", cfg.warnings());
    };
    assert_eq!(warning.key(), "providers.old.style");
    assert_eq!(warning.line(), 3);
    let ConfigWarningKind::Deprecated { replacement, .. } = warning.kind() else {
        panic!("a deprecation, got: {:?}", warning.kind());
    };
    assert_eq!(*replacement, "chat-completions");
    assert!(
        warning.to_string().contains(r#"use "chat-completions""#),
        "{warning}"
    );
}

/// Pin the unknown-style error so a teammate's typo (`open-ai` vs `openai`)
/// lands on a useful message rather than a cryptic serde dump. The contract is
/// "the message names what the user typed and every legal style" -- the
/// deprecated `openai` is read before the tag is, so it is not among them; we
/// don't pin the exact phrasing so a serde minor-version rephrasing won't
/// break the regression test.
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
        msg.contains("chat-completions") && msg.contains("responses") && msg.contains("anthropic"),
        "error should name at least one legal variant, got: {msg}",
    );
}

/// A `chat-completions` provider at `base-url` ending in `/v1`, which serves a
/// model of any style, and a model `sonnet` on it whose row carries `keys`.
fn gateway(keys: &str) -> String {
    format!(
        r#"
[providers.gateway]
style    = "chat-completions"
base-url = "https://gateway.example/v1"
api-key  = "${{GATEWAY_API_KEY}}"

[models.sonnet]
provider = "gateway"
{keys}
"#
    )
}

/// A provider's `style` is the default for its models, and a model's own
/// overrides it for that model alone: one endpoint that serves both Chat
/// Completions and Anthropic's Messages API is one provider. The key survives a
/// serialize/parse round trip in its documented spelling.
#[test]
fn a_models_style_overrides_its_providers_and_round_trips() {
    let cfg = parse(&format!(
        "{}\n[models.fast]\nprovider = \"gateway\"\nidentifier = \"gpt-4o-mini\"\n",
        gateway("identifier = \"claude-sonnet-4-6\"\nstyle = \"anthropic\"\nmax-tokens = 16384"),
    ));
    cfg.validate(None).expect("validates");
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());

    let provider = &cfg.providers["gateway"];
    assert_eq!(provider.style(), "chat-completions");
    assert_eq!(cfg.models["fast"].style, None);
    assert_eq!(
        cfg.models["fast"].effective_style(provider),
        ApiStyle::ChatCompletions
    );
    assert_eq!(cfg.models["sonnet"].style, Some(ApiStyle::Anthropic));
    assert_eq!(
        cfg.models["sonnet"].effective_style(provider),
        ApiStyle::Anthropic
    );

    let text = toml::to_string(&cfg).expect("serializes");
    assert!(text.contains(r#"style = "anthropic""#), "{text}");
    assert_eq!(parse(&text), cfg);
}

/// A model's `style` takes the values a provider's does, and a value it does
/// not take is refused with the message the same value gets on a provider.
/// `openai` is a provider's old spelling, which the loader reads there alone:
/// no 0.2.x config has a model `style`, so a model refuses it like any other.
#[test]
fn an_unknown_model_style_is_refused_as_an_unknown_provider_style_is() {
    let message = |toml: &str| match Config::load_from_str(toml) {
        Err(OutrigError::Config(err)) => err.message().to_string(),
        other => panic!("expected a parse error, got: {other:?}"),
    };
    let on_provider = message(
        r#"
[providers.gateway]
style    = "open-ai"
base-url = "https://gateway.example/v1"
api-key  = "${GATEWAY_API_KEY}"
"#,
    );
    assert_eq!(
        message(&gateway("identifier = \"m\"\nstyle = \"open-ai\"")),
        on_provider
    );
    assert_eq!(
        message(&gateway("identifier = \"m\"\nstyle = \"openai\"")),
        on_provider.replace("open-ai", "openai")
    );
}

/// The global config sets a model `style` it cannot read aside with a
/// warning, as it does any value it cannot use: the model stays, and is
/// called with its provider's style. Any other file refuses it (above).
#[test]
fn an_unknown_model_style_is_set_aside_only_from_the_global_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = dir.path().join("config.toml");
    let text = gateway("identifier = \"m\"\nstyle = \"gemini\"");
    std::fs::write(&global, text).expect("write global config");

    let cfg = Config::load_global(&global).expect("the global config loads");
    let keys: Vec<_> = cfg.warnings().iter().map(|w| w.key()).collect();
    assert_eq!(keys, ["models.sonnet.style"]);
    let sonnet = &cfg.models["sonnet"];
    assert_eq!(sonnet.style, None);
    assert_eq!(
        sonnet.effective_style(&cfg.providers["gateway"]),
        ApiStyle::ChatCompletions
    );
}

/// The `identifier` rule is the one of the style a model is called with, and
/// its error names that style: the model's own, not its provider's.
#[test]
fn a_model_missing_identifier_names_its_own_style() {
    let cfg = parse(&gateway("style = \"anthropic\""));
    let err = expect_validation_err(&cfg, None);
    assert!(
        matches!(
            err,
            ConfigValidationError::RemoteModelMissingIdentifier { ref model, style, .. }
                if model == "sonnet" && style == "anthropic"
        ),
        "got: {err:?}",
    );
    assert!(err.to_string().contains("(style=anthropic)"), "{err}");
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
        msg.contains("chat-completions") && msg.contains("responses") && msg.contains("anthropic"),
        "error should name at least one legal style, got: {msg}",
    );
}

/// The six weight keys and the top-level `model-cache-root` went with the
/// style. A config written for 0.2 still loads, each one set aside with a
/// warning naming it, as any key this build does not know is -- and a strict
/// parse still refuses it.
#[test]
fn removed_local_llm_keys_are_set_aside_with_a_warning() {
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
        let cfg = Config::load_from_str(&toml).expect("a 0.2 config loads");
        let warnings: Vec<_> = cfg.warnings().iter().map(|w| (w.key(), w.kind())).collect();
        let expected = format!("models.sonnet.{key}");
        assert_eq!(
            warnings,
            [(expected.as_str(), &ConfigWarningKind::UnknownKey)],
            "{key}"
        );
        let err = toml::from_str::<Config>(&toml).expect_err("a strict parse refuses it");
        assert!(err.to_string().contains(key), "{key}: {err}");
    }

    let cfg = Config::load_from_str("model-cache-root = \"/var/cache/outrig/models\"\n")
        .expect("a 0.2 config loads");
    let keys: Vec<_> = cfg.warnings().iter().map(|w| w.key()).collect();
    assert_eq!(keys, ["model-cache-root"]);
}

/// `style = "mistralrs"` names a style this build does not have. The global
/// config sets such a provider aside with a warning, as it does any value it
/// cannot use, so a machine's config shared with 0.2 keeps loading; a model
/// naming the provider then has none. Any other file fails to load.
#[test]
fn a_mistralrs_provider_is_set_aside_only_from_the_global_config() {
    let text = "[providers.local]\nstyle = \"mistralrs\"\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let global = dir.path().join("config.toml");
    std::fs::write(&global, text).expect("write global config");

    let cfg = Config::load_global(&global).expect("the global config loads");
    assert!(cfg.providers.is_empty(), "{:?}", cfg.providers);
    assert_eq!(
        cfg.warnings().first().map(|w| w.key()),
        Some("providers.local"),
        "{:?}",
        cfg.warnings()
    );

    let err = Config::load_from_str(text).expect_err("a repo file's provider fails");
    assert!(matches!(err, OutrigError::Config(_)), "{err:?}");
    assert!(err.to_string().contains("mistralrs"), "{err}");
}

/// The constructor options carry both remote-only settings in one place, each
/// `with_*` reaches its own field without disturbing the other, and `new`
/// leaves both unset. Nothing in the config path constructs a provider this way
/// -- serde populates the fields directly -- so this is the only thing pinning
/// the published surface.
#[test]
fn remote_provider_options_populate_remote_variants() {
    let key = ApiKeyRef::parse("${OPENAI_API_KEY}").expect("api-key ref parses");
    let openai = LlmProvider::chat_completions(
        "https://api.openai.com/v1",
        key,
        ChatCompletionsOptions::new()
            .with_request_timeout_secs(90)
            .with_retry_budget_secs(120),
    );
    assert!(
        matches!(
            openai,
            LlmProvider::ChatCompletions {
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
    assert_eq!(
        ChatCompletionsOptions::new(),
        ChatCompletionsOptions::default()
    );
    assert_eq!(AnthropicOptions::new(), AnthropicOptions::default());
}

/// `role-alternation` is optional on both styles and takes its two values; a
/// row that leaves it out is unset rather than relaxed, so the setting
/// round-trips only where it was written. A value that is neither names what
/// was written and a legal value. The builders set it the same way.
#[test]
fn role_alternation_parses_on_both_styles() {
    let cfg = parse(
        r#"
[providers.gateway]
style            = "chat-completions"
base-url         = "https://gateway.example/v1"
api-key          = "${GATEWAY_KEY}"
role-alternation = "strict"

[providers.claude]
style            = "anthropic"
base-url         = "https://api.anthropic.com"
api-key          = "${ANTHROPIC_API_KEY}"
role-alternation = "relaxed"

[providers.openai]
style    = "chat-completions"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"
"#,
    );
    cfg.validate(None).expect("validates");
    let alternation = |provider: &LlmProvider| match provider {
        LlmProvider::ChatCompletions {
            role_alternation, ..
        }
        | LlmProvider::Anthropic {
            role_alternation, ..
        } => *role_alternation,
        other => panic!("unexpected style: {other:?}"),
    };
    assert_eq!(
        alternation(&cfg.providers["gateway"]),
        Some(RoleAlternation::Strict)
    );
    assert_eq!(
        alternation(&cfg.providers["claude"]),
        Some(RoleAlternation::Relaxed)
    );
    assert_eq!(alternation(&cfg.providers["openai"]), None);

    let serialized = toml::to_string(&cfg).expect("serializes");
    assert_eq!(
        serialized.matches("role-alternation").count(),
        2,
        "the key round-trips where it was set and nowhere else, got: {serialized}"
    );
    assert!(
        serialized.contains(r#"role-alternation = "strict""#),
        "the value round-trips in its documented spelling, got: {serialized}"
    );
    let again = Config::load_from_str(&serialized).expect("reserialized parses");
    assert_eq!(cfg, again);

    let err = Config::load_from_str(
        r#"
[providers.gateway]
style            = "chat-completions"
base-url         = "https://gateway.example/v1"
api-key          = "${GATEWAY_KEY}"
role-alternation = "loose"
"#,
    )
    .expect_err("a value that is neither fails");
    let msg = err.to_string();
    assert!(
        msg.contains("loose") && (msg.contains("strict") || msg.contains("relaxed")),
        "error should quote the value and name a legal one, got: {msg}"
    );

    let key = ApiKeyRef::parse("${GATEWAY_KEY}").expect("api-key ref parses");
    let built = LlmProvider::chat_completions(
        "https://gateway.example/v1",
        key,
        ChatCompletionsOptions::new().with_role_alternation(RoleAlternation::Strict),
    );
    assert_eq!(alternation(&built), Some(RoleAlternation::Strict));
    let key = ApiKeyRef::parse("${ANTHROPIC_API_KEY}").expect("api-key ref parses");
    let built = LlmProvider::anthropic(
        "https://api.anthropic.com",
        key,
        AnthropicOptions::new().with_role_alternation(RoleAlternation::Strict),
    );
    assert_eq!(alternation(&built), Some(RoleAlternation::Strict));
    assert_eq!(
        alternation(&LlmProvider::anthropic(
            "https://api.anthropic.com",
            ApiKeyRef::parse("${ANTHROPIC_API_KEY}").expect("api-key ref parses"),
            AnthropicOptions::new(),
        )),
        None
    );
}
