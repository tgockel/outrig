//! Integration tests for `resolve_agent` and `build_agent`. Covers
//! every failure mode listed in `plan/done/0012-llm-resolver.md`'s test
//! plan plus the two happy-path resolutions.

#[cfg(not(feature = "local-llm"))]
use std::path::Path;

use outrig::config::{Config, MistralrsDeviceSpec};
use outrig_cli::error::CliError;
#[cfg(not(feature = "local-llm"))]
use outrig_cli::llm::build_agent;
use outrig_cli::llm::{
    DEFAULT_TOOL_RESULT_MAX_BYTES, LlmResolveError, MAX_TOOL_CALLS, ResolvedProvider,
    resolve_agent, resolve_agent_with_device_override, resolve_agent_with_overrides,
};

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
style = "mistralrs"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[models.smart]
provider   = "openai"
identifier = "gpt-4o"

[models.claude]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"

{agents}
"#,
    )
}

fn local_mistralrs_cfg(device: Option<&str>) -> Config {
    let device = device
        .map(|value| format!("device     = {value:?}\n"))
        .unwrap_or_default();
    parse(&format!(
        r#"
default-model = "local"

[providers.local]
style = "mistralrs"

[models.local]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
{device}

[agents.smoke]
preamble = "hi"
"#,
    ))
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
    assert_eq!(r.tool_call_max, MAX_TOOL_CALLS);
    assert_eq!(r.tool_result_max_bytes, DEFAULT_TOOL_RESULT_MAX_BYTES);

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
fn model_override_replaces_agent_model() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MODEL_OVERRIDE_AGENT";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.review]
model    = "fast"
preamble = "be meticulous"
"#,
    ));

    let r = resolve_agent_with_overrides(&cfg, "review", Some("smart"), None).expect("resolves");
    assert_eq!(r.model_name, "smart");
    assert_eq!(r.model_identifier, "gpt-4o");
    assert_eq!(r.preamble, "be meticulous");

    unset_env(var);
}

#[test]
fn model_override_replaces_default_model() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MODEL_OVERRIDE_DEFAULT";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble = "code"
"#,
    ));

    let r = resolve_agent_with_overrides(&cfg, "coding", Some("smart"), None).expect("resolves");
    assert_eq!(r.model_name, "smart");
    assert_eq!(r.model_identifier, "gpt-4o");

    unset_env(var);
}

#[test]
fn model_override_supplies_model_without_default() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MODEL_OVERRIDE_NO_DEFAULT";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        "",
        r#"
[agents.coding]
preamble = "code"
"#,
    ));

    let r = resolve_agent_with_overrides(&cfg, "coding", Some("fast"), None).expect("resolves");
    assert_eq!(r.model_name, "fast");
    assert_eq!(r.model_identifier, "gpt-4o-mini");

    unset_env(var);
}

#[test]
fn unknown_model_override_errors() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_MODEL_OVERRIDE_UNKNOWN";
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble = "code"
"#,
    ));

    let err = resolve_agent_with_overrides(&cfg, "coding", Some("ghost"), None).unwrap_err();
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::UnknownModel { name }) if name == "ghost"
        ),
        "got: {err:?}",
    );
}

#[test]
fn tool_call_max_resolves_from_top_level_then_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_TOOL_MAX";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"
default-model = "fast"
tool-call-max = 100
"#,
        r#"
[agents.coding]
preamble = "code"

[agents.review]
preamble = "review"
tool-call-max = 300
"#,
    ));

    let coding = resolve_agent(&cfg, "coding").expect("coding resolves");
    assert_eq!(coding.tool_call_max, 100);

    let review = resolve_agent(&cfg, "review").expect("review resolves");
    assert_eq!(review.tool_call_max, 300);

    unset_env(var);
}

#[test]
fn tool_result_max_resolves_from_top_level_then_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_TOOL_RESULT_MAX";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"
default-model = "fast"
tool-result-max = 524288
"#,
        r#"
[agents.coding]
preamble = "code"

[agents.review]
preamble = "review"
tool-result-max = 1048576
"#,
    ));

    let coding = resolve_agent(&cfg, "coding").expect("coding resolves");
    assert_eq!(coding.tool_result_max_bytes, 524288);

    let review = resolve_agent(&cfg, "review").expect("review resolves");
    assert_eq!(review.tool_result_max_bytes, 1048576);

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
fn mistralrs_device_defaults_to_cpu() {
    let cfg = local_mistralrs_cfg(None);
    let r = resolve_agent(&cfg, "smoke").expect("resolves");
    let weights = r.model_weights.as_ref().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn mistralrs_cpu_device_resolves_to_weights() {
    let cfg = local_mistralrs_cfg(Some("cpu"));
    let r = resolve_agent(&cfg, "smoke").expect("resolves");
    let weights = r.model_weights.as_ref().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn mistralrs_invalid_device_errors_during_resolve() {
    let cfg = local_mistralrs_cfg(Some("cuda:"));
    let err = resolve_agent(&cfg, "smoke").unwrap_err();
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceInvalid {
                model,
                device,
            }) if model == "local" && device == "cuda:"
        ),
        "got: {err:?}",
    );
}

#[test]
fn mistralrs_device_override_replaces_model_device() {
    let cfg = local_mistralrs_cfg(Some("cuda"));
    let r = resolve_agent_with_device_override(&cfg, "smoke", Some(MistralrsDeviceSpec::Cpu))
        .expect("resolves");
    let weights = r.model_weights.as_ref().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn device_override_applies_to_model_override() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_DEVICE_WITH_MODEL_OVERRIDE";
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
model    = "fast"
preamble = "hi"
"#,
    ));

    let r = resolve_agent_with_overrides(
        &cfg,
        "coding",
        Some("claude"),
        Some(MistralrsDeviceSpec::Cpu),
    )
    .expect("resolves");
    assert_eq!(r.model_name, "claude");
    let weights = r.model_weights.as_ref().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn device_override_rejects_openai_models() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_DEVICE_OVERRIDE_OPENAI";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble = "hi"
"#,
    ));

    let err = resolve_agent_with_device_override(&cfg, "coding", Some(MistralrsDeviceSpec::Cpu))
        .unwrap_err();
    unset_env(var);
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceOverrideUnsupported {
                model,
                provider,
            }) if model == "fast" && provider == "openai"
        ),
        "got: {err:?}",
    );
    assert!(
        err.to_string()
            .contains("--device only applies to mistralrs models"),
        "got: {err}",
    );
}

#[cfg(all(feature = "local-llm", not(feature = "cuda")))]
#[test]
fn mistralrs_cuda_device_feature_off_explains_clearly() {
    let cfg = local_mistralrs_cfg(Some("cuda:2"));
    let err = resolve_agent(&cfg, "smoke").unwrap_err();
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceUnavailable {
                model,
                device,
                feature,
            }) if model == "local" && device == "cuda:2" && *feature == "cuda"
        ),
        "got: {err:?}",
    );
    assert_eq!(
        err.to_string(),
        "mistralrs model \"local\" requested device \"cuda:2\" but this \
         build of outrig does not include the 'cuda' feature; \
         rebuild with --features cuda to enable",
    );
}

#[cfg(all(feature = "local-llm", not(feature = "metal")))]
#[test]
fn mistralrs_metal_device_feature_off_explains_clearly() {
    let cfg = local_mistralrs_cfg(Some("metal"));
    let err = resolve_agent(&cfg, "smoke").unwrap_err();
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceUnavailable {
                model,
                device,
                feature,
            }) if model == "local" && device == "metal" && *feature == "metal"
        ),
        "got: {err:?}",
    );
    assert_eq!(
        err.to_string(),
        "mistralrs model \"local\" requested device \"metal\" but this \
         build of outrig does not include the 'metal' feature; \
         rebuild with --features metal to enable",
    );
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
            CliError::LlmResolve(LlmResolveError::UnknownAgent { .. })
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
            CliError::LlmResolve(LlmResolveError::UnknownModel { name }) if name == "ghost"
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
            CliError::LlmResolve(LlmResolveError::AgentMissingModel { agent }) if agent == "coding"
        ),
        "got: {err:?}",
    );
}

/// Feature-off build: building an agent for a `mistralrs` provider fails
/// with a message that names both the provider and the missing feature
/// flag, so the fix ("rebuild with --features local-llm") is one shot.
/// Pinned verbatim because `doc/concepts/llm-providers.md` promises this
/// wording.
#[cfg(not(feature = "local-llm"))]
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
        matches!(resolved.provider, ResolvedProvider::Mistralrs),
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
            CliError::LlmResolve(LlmResolveError::MistralrsFeatureDisabled { name }) if name == "local"
        ),
        "got: {err:?}",
    );
    assert_eq!(
        err.to_string(),
        "mistralrs provider \"local\" requested but this build of outrig \
         does not include the 'local-llm' feature; rebuild with \
         --features local-llm to enable",
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
    assert!(
        matches!(err, CliError::Outrig(outrig::error::OutrigError::ApiKey(_))),
        "got: {err:?}"
    );
    assert!(
        msg.contains(var),
        "error should name the missing var; got: {msg}"
    );
}
