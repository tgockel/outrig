//! Integration tests for `resolve_agent` and `build_agent`. Covers
//! every failure mode listed in `plan/done/0012-llm-resolver.md`'s test
//! plan plus the two happy-path resolutions.

#[cfg(not(feature = "mistralrs"))]
use std::path::Path;

use outrig::config::Config;
use outrig::error::OutrigError;
#[cfg(not(feature = "mistralrs"))]
use outrig::llm::build_agent;
use outrig::llm::{LlmResolveError, ResolvedProvider, resolve_agent};

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

// SAFETY: edition 2024 marks env::set_var unsafe due to multi-thread races.
// Each test uses a unique env-var name so concurrent test execution can't
// race on the same key.
fn set_env(var: &str, val: &str) {
    unsafe { std::env::set_var(var, val) }
}

fn unset_env(var: &str) {
    unsafe { std::env::remove_var(var) }
}

/// Build a config string with a stock provider/model registry. `top` lands
/// above the table sections (where scalar keys like `default-model` must
/// live in TOML); `agents` lands at the bottom.
fn cfg_with_key_var(env_name: &str, top: &str, agents: &str) -> String {
    format!(
        r#"
{top}

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${{{env_name}}}"

[providers.local]
style    = "mistralrs"
model-id = "Qwen/Qwen2.5-7B-Instruct"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"

[models.claude]
provider   = "local"
identifier = "Qwen/Qwen2.5-7B-Instruct"

{agents}
"#,
    )
}

#[test]
fn agent_inherits_default_model() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_INHERIT";
    set_env(var, "test-key");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble    = "you are a careful coder"
temperature = 0.2
max-tokens  = 4096
"#,
    ));

    let r = resolve_agent(&cfg, "coding").expect("resolves");
    assert_eq!(r.agent_name, "coding");
    assert_eq!(r.model_name, "fast");
    assert_eq!(r.model_identifier, "gpt-4o-mini");
    assert_eq!(r.provider_name, "openai");
    let ResolvedProvider::OpenAi {
        base_url, api_key, ..
    } = &r.provider
    else {
        panic!("expected OpenAi resolved-provider, got {:?}", r.provider);
    };
    assert_eq!(base_url, "https://api.openai.com/v1");
    assert_eq!(api_key, "test-key");
    assert_eq!(r.preamble, "you are a careful coder");
    assert_eq!(r.temperature, Some(0.2));
    assert_eq!(r.max_tokens, Some(4096));

    unset_env(var);
}

#[test]
fn agent_explicit_model_overrides_default() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_OVERRIDE";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.review]
model    = "smart"
preamble = "be meticulous"
"#,
    ));

    let r = resolve_agent(&cfg, "review").expect("resolves");
    assert_eq!(r.model_name, "smart");
    assert_eq!(r.model_identifier, "gpt-4o");

    unset_env(var);
}

#[test]
fn missing_preamble_falls_back_to_default() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_PREAMBLE";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
"#,
    ));

    let r = resolve_agent(&cfg, "coding").expect("resolves");
    assert!(
        r.preamble.contains("sandboxed container"),
        "default preamble should mention the sandbox; got: {}",
        r.preamble,
    );

    unset_env(var);
}

#[test]
fn missing_agent_error_names_agent_flags() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MISSING_AGENT";
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble = "hi"
"#,
    ));

    let err = resolve_agent(&cfg, "ghost").unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(
            err,
            OutrigError::LlmResolve(LlmResolveError::UnknownAgent { .. })
        ),
        "got: {err:?}",
    );
    assert!(
        msg.contains("--agent"),
        "error should name --agent; got: {msg}"
    );
    assert!(
        msg.contains("default-agent"),
        "error should name default-agent; got: {msg}",
    );
    assert!(
        msg.contains("ghost"),
        "error should quote the bad name; got: {msg}"
    );
}

#[test]
fn missing_model_errors() {
    // Construct a Config that bypasses validation: agent points at a model
    // name that has no [models.<name>] entry. Config::load_from_str does no
    // cross-reference checking on its own.
    let cfg = parse(
        r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OUTRIG_TEST_UNUSED_MISSING_MODEL}"

[agents.coding]
model    = "ghost"
preamble = "hi"
"#,
    );

    let err = resolve_agent(&cfg, "coding").unwrap_err();
    assert!(
        matches!(
            &err,
            OutrigError::LlmResolve(LlmResolveError::UnknownModel { name }) if name == "ghost"
        ),
        "got: {err:?}",
    );
}

#[test]
fn agent_without_model_or_default_errors() {
    let cfg = parse(
        r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OUTRIG_TEST_UNUSED_NO_MODEL}"

[agents.coding]
preamble = "hi"
"#,
    );

    let err = resolve_agent(&cfg, "coding").unwrap_err();
    assert!(
        matches!(
            &err,
            OutrigError::LlmResolve(LlmResolveError::AgentMissingModel { agent }) if agent == "coding"
        ),
        "got: {err:?}",
    );
}

/// Feature-off build: building an agent for a `mistralrs` provider fails
/// with a message that names both the provider and the missing feature
/// flag, so the fix ("rebuild with --features mistralrs") is one shot.
/// Pinned verbatim because `doc/concepts/llm-providers.md` promises this
/// wording.
#[cfg(not(feature = "mistralrs"))]
#[tokio::test]
async fn mistralrs_provider_feature_off_explains_clearly() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MISTRALRS";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "claude""#,
        r#"
[agents.review]
preamble = "hi"
"#,
    ));
    let resolved = resolve_agent(&cfg, "review").expect("resolves");
    assert!(
        matches!(resolved.provider, ResolvedProvider::Mistralrs { .. }),
        "expected Mistralrs resolved-provider, got {:?}",
        resolved.provider,
    );
    let result = build_agent(&resolved, vec![], Path::new("/tmp/outrig-test-cache")).await;
    unset_env(var);
    let err = match result {
        Ok(_) => panic!("expected build_agent to error on feature-off mistralrs"),
        Err(e) => e,
    };
    assert!(
        matches!(
            &err,
            OutrigError::LlmResolve(LlmResolveError::MistralrsFeatureDisabled { name }) if name == "local"
        ),
        "got: {err:?}",
    );
    assert_eq!(
        err.to_string(),
        "mistralrs provider \"local\" requested but this build of outrig \
         does not include the 'mistralrs' feature; rebuild with \
         --features mistralrs to enable",
    );
}

#[test]
fn unset_api_key_errors() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_UNSET_API_KEY";
    unset_env(var);
    let cfg = parse(&format!(
        r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${{{var}}}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
preamble = "hi"
"#,
    ));

    let err = resolve_agent(&cfg, "coding").unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, OutrigError::ApiKey(_)), "got: {err:?}");
    assert!(
        msg.contains(var),
        "error should name the missing var; got: {msg}"
    );
}
