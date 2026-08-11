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

    let r = resolve_agent(&cfg, Some("coding")).expect("resolves");
    assert_eq!(r.agent_name.as_deref(), Some("coding"));
    assert_eq!(r.model_name(), "fast");
    assert_eq!(r.model_identifier(), "gpt-4o-mini");
    assert_eq!(r.provider_name(), "openai");
    let ResolvedProvider::OpenAi {
        base_url, api_key, ..
    } = r.provider()
    else {
        panic!("expected OpenAi resolved-provider, got {:?}", r.provider());
    };
    assert_eq!(base_url, "https://api.openai.com/v1");
    assert_eq!(api_key, "test-key");
    assert_eq!(r.preamble.as_deref(), Some("you are a careful coder"));
    assert_eq!(r.temperature, Some(0.2));
    assert_eq!(r.max_tokens(), Some(4096));
    assert_eq!(r.tool_call_max, MAX_TOOL_CALLS);
    assert_eq!(r.tool_result_max_bytes, DEFAULT_TOOL_RESULT_MAX_BYTES);
    assert_eq!(
        r.subagent_width_max,
        outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX
    );

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

    let r = resolve_agent(&cfg, Some("review")).expect("resolves");
    assert_eq!(r.model_name(), "smart");
    assert_eq!(r.model_identifier(), "gpt-4o");

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

    let r =
        resolve_agent_with_overrides(&cfg, Some("review"), Some("smart"), None).expect("resolves");
    assert_eq!(r.model_name(), "smart");
    assert_eq!(r.model_identifier(), "gpt-4o");
    assert_eq!(r.preamble.as_deref(), Some("be meticulous"));

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

    let r =
        resolve_agent_with_overrides(&cfg, Some("coding"), Some("smart"), None).expect("resolves");
    assert_eq!(r.model_name(), "smart");
    assert_eq!(r.model_identifier(), "gpt-4o");

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

    let r =
        resolve_agent_with_overrides(&cfg, Some("coding"), Some("fast"), None).expect("resolves");
    assert_eq!(r.model_name(), "fast");
    assert_eq!(r.model_identifier(), "gpt-4o-mini");

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

    let err = resolve_agent_with_overrides(&cfg, Some("coding"), Some("ghost"), None).unwrap_err();
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

    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(coding.tool_call_max, 100);

    let review = resolve_agent(&cfg, Some("review")).expect("review resolves");
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

    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(coding.tool_result_max_bytes, 524288);

    let review = resolve_agent(&cfg, Some("review")).expect("review resolves");
    assert_eq!(review.tool_result_max_bytes, 1048576);

    unset_env(var);
}

#[test]
fn subagent_depth_max_resolves_from_default_then_top_level_then_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_SUBAGENT_DEPTH";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"
default-model = "fast"
subagent-depth-max = 4
"#,
        r#"
[agents.coding]
preamble = "code"

[agents.review]
preamble = "review"
subagent-depth-max = 2
"#,
    ));

    // Agent unset -> top-level; agent set -> agent wins.
    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(coding.subagent_depth_max, 4);
    let review = resolve_agent(&cfg, Some("review")).expect("review resolves");
    assert_eq!(review.subagent_depth_max, 2);

    unset_env(var);
}

#[test]
fn subagent_depth_max_absent_falls_back_to_default() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_SUBAGENT_DEPTH_DEFAULT";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"
default-model = "fast"
"#,
        r#"
[agents.coding]
preamble = "code"
"#,
    ));

    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(
        coding.subagent_depth_max,
        outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX
    );

    unset_env(var);
}

#[test]
fn subagent_width_max_resolves_from_default_then_top_level_then_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_SUBAGENT_WIDTH";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"
default-model = "fast"
subagent-width-max = 6
"#,
        r#"
[agents.coding]
preamble = "code"

[agents.review]
preamble = "review"
subagent-width-max = 3
"#,
    ));

    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(coding.subagent_width_max, 6);
    let review = resolve_agent(&cfg, Some("review")).expect("review resolves");
    assert_eq!(review.subagent_width_max, 3);

    unset_env(var);
}

#[test]
fn subagent_width_max_absent_falls_back_to_default() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_SUBAGENT_WIDTH_DEFAULT";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
preamble = "code"
"#,
    ));

    let coding = resolve_agent(&cfg, Some("coding")).expect("coding resolves");
    assert_eq!(
        coding.subagent_width_max,
        outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX
    );

    unset_env(var);
}

/// An agent that declares no `preamble` sends no system prompt.
#[test]
fn missing_preamble_sends_none() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_PREAMBLE";
    set_env(var, "k");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
"#,
    ));

    let r = resolve_agent(&cfg, Some("coding")).expect("resolves");
    assert_eq!(r.preamble, None, "an unset preamble must stay unset");

    unset_env(var);
}

/// The agentless session `outrig run` starts when neither `--agent` nor
/// `default-agent` names one. It resolves against an empty agent: no
/// preamble, no image hint, no sampling overrides, and every limit from the
/// top-level config.
#[test]
fn no_agent_resolves_against_the_defaults() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_NO_AGENT";
    set_env(var, "test-key");
    let cfg = parse(&cfg_with_key_var(var, r#"default-model = "fast""#, ""));

    let r = resolve_agent(&cfg, None).expect("resolves without an agent");
    assert_eq!(r.agent_name, None);
    assert_eq!(r.preamble, None);
    assert_eq!(r.image, None);
    assert_eq!(r.temperature, None);
    assert_eq!(r.max_tokens(), None);
    assert_eq!(r.model_name(), "fast");
    assert_eq!(r.model_identifier(), "gpt-4o-mini");
    assert_eq!(r.tool_call_max, MAX_TOOL_CALLS);
    assert_eq!(r.tool_result_max_bytes, DEFAULT_TOOL_RESULT_MAX_BYTES);

    unset_env(var);
}

/// Declaring agents does not make one *apply*: an agentless session ignores
/// the `[agents]` table entirely rather than picking an arbitrary entry.
#[test]
fn no_agent_ignores_declared_agents() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_NO_AGENT_IGNORES";
    set_env(var, "test-key");
    let cfg = parse(&cfg_with_key_var(
        var,
        r#"default-model = "fast""#,
        r#"
[agents.coding]
model    = "smart"
preamble = "you are a careful coder"
image    = "coding"
"#,
    ));

    let r = resolve_agent(&cfg, None).expect("resolves without an agent");
    assert_eq!(r.agent_name, None);
    assert_eq!(r.preamble, None);
    assert_eq!(r.image, None);
    assert_eq!(
        r.model_name(),
        "fast",
        "default-model, not the agent's model"
    );

    unset_env(var);
}

/// `--model` is the other way an agentless session names its model.
#[test]
fn no_agent_takes_the_model_override() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_NO_AGENT_MODEL";
    set_env(var, "test-key");
    let cfg = parse(&cfg_with_key_var(var, "", ""));

    let r = resolve_agent_with_overrides(&cfg, None, Some("smart"), None)
        .expect("resolves without an agent");
    assert_eq!(r.model_name(), "smart");
    assert_eq!(r.model_identifier(), "gpt-4o");

    unset_env(var);
}

/// A model is the one thing an agentless session cannot do without, and the
/// error has no agent name to quote -- so it names the two knobs instead.
#[test]
fn no_agent_and_no_model_errors() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_NO_AGENT_NO_MODEL";
    set_env(var, "test-key");
    let cfg = parse(&cfg_with_key_var(var, "", ""));

    let err = resolve_agent(&cfg, None).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(err, CliError::LlmResolve(LlmResolveError::MissingModel)),
        "got: {err:?}",
    );
    assert!(
        msg.contains("--model"),
        "error should name --model; got: {msg}"
    );
    assert!(
        msg.contains("default-model"),
        "error should name default-model; got: {msg}",
    );

    unset_env(var);
}

#[test]
fn mistralrs_device_defaults_to_cpu() {
    let cfg = local_mistralrs_cfg(None);
    let r = resolve_agent(&cfg, Some("smoke")).expect("resolves");
    let weights = r.model_weights().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn mistralrs_cpu_device_resolves_to_weights() {
    let cfg = local_mistralrs_cfg(Some("cpu"));
    let r = resolve_agent(&cfg, Some("smoke")).expect("resolves");
    let weights = r.model_weights().expect("mistralrs weights");
    assert_eq!(weights.device, MistralrsDeviceSpec::Cpu);
}

#[test]
fn mistralrs_invalid_device_errors_during_resolve() {
    let cfg = local_mistralrs_cfg(Some("cuda:"));
    let err = resolve_agent(&cfg, Some("smoke")).unwrap_err();
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
    let r = resolve_agent_with_device_override(&cfg, Some("smoke"), Some(MistralrsDeviceSpec::Cpu))
        .expect("resolves");
    let weights = r.model_weights().expect("mistralrs weights");
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
        Some("coding"),
        Some("claude"),
        Some(MistralrsDeviceSpec::Cpu),
    )
    .expect("resolves");
    assert_eq!(r.model_name(), "claude");
    let weights = r.model_weights().expect("mistralrs weights");
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

    let err =
        resolve_agent_with_device_override(&cfg, Some("coding"), Some(MistralrsDeviceSpec::Cpu))
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
    let err = resolve_agent(&cfg, Some("smoke")).unwrap_err();
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
    let err = resolve_agent(&cfg, Some("smoke")).unwrap_err();
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

    let err = resolve_agent(&cfg, Some("ghost")).unwrap_err();
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

    let err = resolve_agent(&cfg, Some("coding")).unwrap_err();
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

    let err = resolve_agent(&cfg, Some("coding")).unwrap_err();
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
    let resolved = resolve_agent(&cfg, Some("review")).expect("resolves");
    assert!(
        matches!(resolved.provider(), ResolvedProvider::Mistralrs),
        "expected Mistralrs resolved-provider, got {:?}",
        resolved.provider(),
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

/// Build a config naming an `anthropic` provider. `model_extra` lands inside
/// `[models.sonnet]`, `agents` at the bottom.
fn anthropic_cfg(env_name: &str, model_extra: &str, agents: &str) -> Config {
    parse(&format!(
        r#"
default-model = "sonnet"

[providers.claude]
style                = "anthropic"
base-url             = "https://api.anthropic.com"
api-key              = "${{{env_name}}}"
request-timeout-secs = 45

[models.sonnet]
provider   = "claude"
identifier = "claude-sonnet-4-6"
{model_extra}

{agents}
"#,
    ))
}

#[test]
fn anthropic_provider_resolves_connection_details() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ANTHROPIC";
    set_env(var, "sk-ant-test");
    let cfg = anthropic_cfg(
        var,
        "",
        r#"
[agents.coding]
preamble = "you are a careful coder"
"#,
    );

    let r = resolve_agent(&cfg, Some("coding")).expect("resolves");
    assert_eq!(r.model_identifier(), "claude-sonnet-4-6");
    assert_eq!(r.provider_name(), "claude");
    let ResolvedProvider::Anthropic {
        base_url,
        api_key,
        request_timeout_secs,
        retry_budget_secs,
    } = r.provider()
    else {
        panic!(
            "expected Anthropic resolved-provider, got {:?}",
            r.provider()
        );
    };
    assert_eq!(base_url, "https://api.anthropic.com");
    assert_eq!(api_key, "sk-ant-test");
    assert_eq!(*request_timeout_secs, Some(45));
    assert_eq!(
        *retry_budget_secs, None,
        "an unset budget resolves to None, and the client default applies",
    );
    assert!(
        r.model_weights().is_none(),
        "a remote model carries no weight spec"
    );

    unset_env(var);
}

/// The model's ceiling covers every agent pointed at it; an agent that sets
/// its own still wins.
#[test]
fn model_max_tokens_is_the_fallback_for_the_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ANTHROPIC_MAX_TOKENS";
    set_env(var, "k");

    let cfg = anthropic_cfg(
        var,
        "max-tokens = 16384",
        r#"
[agents.inherits]
preamble = "hi"

[agents.overrides]
preamble   = "hi"
max-tokens = 4096
"#,
    );
    assert_eq!(
        resolve_agent(&cfg, Some("inherits"))
            .expect("resolves")
            .max_tokens(),
        Some(16384),
    );
    assert_eq!(
        resolve_agent(&cfg, Some("overrides"))
            .expect("resolves")
            .max_tokens(),
        Some(4096),
    );

    // Neither set: nothing is imposed, and the provider decides (which for
    // an identifier rig does not recognize means the turn errors -- see
    // tests/anthropic_mock.rs).
    let cfg = anthropic_cfg(
        var,
        "",
        r#"
[agents.inherits]
preamble = "hi"
"#,
    );
    assert_eq!(
        resolve_agent(&cfg, Some("inherits"))
            .expect("resolves")
            .max_tokens(),
        None,
    );

    unset_env(var);
}

/// `--device` is a mistralrs knob; asking for one on a remote model is a
/// mistake worth naming rather than ignoring.
#[test]
fn anthropic_model_rejects_device_override() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ANTHROPIC_DEVICE";
    set_env(var, "k");
    let cfg = anthropic_cfg(
        var,
        "",
        r#"
[agents.coding]
preamble = "hi"
"#,
    );

    let err =
        resolve_agent_with_device_override(&cfg, Some("coding"), Some(MistralrsDeviceSpec::Cpu))
            .expect_err("device override should be rejected");
    unset_env(var);
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceOverrideUnsupported {
                model,
                provider,
            }) if model == "sonnet" && provider == "claude"
        ),
        "got: {err:?}",
    );
}

#[test]
fn anthropic_unset_api_key_errors() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ANTHROPIC_UNSET";
    unset_env(var);
    let cfg = anthropic_cfg(
        var,
        "",
        r#"
[agents.coding]
preamble = "hi"
"#,
    );

    let err = resolve_agent(&cfg, Some("coding")).expect_err("unset key should fail");
    assert!(
        matches!(err, CliError::Outrig(outrig::error::OutrigError::ApiKey(_))),
        "got: {err:?}"
    );
    assert!(
        err.to_string().contains(var),
        "error should name the missing var; got: {err}"
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

    let err = resolve_agent(&cfg, Some("coding")).unwrap_err();
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

// ---------------------------------------------------------------------------
// Model aliases (task 0110): one name for a model, or for an ordered set of
// provider-equivalent rows. The config-layer half -- flattening order and the
// five validation rules -- lives in `outrig/tests/config_model_alias.rs`.
// ---------------------------------------------------------------------------

/// Three provider-equivalent rows on three providers, each keyed by its own
/// env var, plus a `smart` alias listing them in preference order.
fn three_vendor_alias_cfg(vars: [&str; 3]) -> Config {
    let [a, b, c] = vars;
    parse(&format!(
        r#"
default-model = "smart"

[providers.bedrock]
style    = "openai"
base-url = "https://bedrock.example.invalid/v1"
api-key  = "${{{a}}}"

[providers.anthropic]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${{{b}}}"

[providers.azure]
style    = "openai"
base-url = "https://azure.example.invalid/v1"
api-key  = "${{{c}}}"

[models.smart]
alias = ["opus-5-bedrock", "opus-5-anthropic", "opus-5-azure"]

[models.opus-5-bedrock]
provider   = "bedrock"
identifier = "anthropic.claude-opus-5-v1:0"

[models.opus-5-anthropic]
provider   = "anthropic"
identifier = "claude-opus-5"

[models.opus-5-azure]
provider   = "azure"
identifier = "claude-opus-5-azure"

[agents.coding]
preamble = "hi"
"#,
    ))
}

/// The headline case: one name for one model, reachable through every surface
/// that already takes a model name.
#[test]
fn single_target_alias_resolves_through_model_flag_default_and_agent() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_SINGLE";
    set_env(var, "sk-test");
    let alias = "\n[models.opus]\nalias = \"fast\"\n";

    // via --model
    let cfg = parse(&format!(
        "{}{alias}",
        cfg_with_key_var(var, "", "[agents.coding]\npreamble = \"hi\"")
    ));
    let r = resolve_agent_with_overrides(&cfg, Some("coding"), Some("opus"), None).unwrap();
    assert_eq!(r.model_name(), "fast", "the concrete row wins the name");
    assert_eq!(r.alias_name.as_deref(), Some("opus"));
    assert_eq!(r.model_identifier(), "gpt-4o-mini");

    // via default-model
    let cfg = parse(&format!(
        "{}{alias}",
        cfg_with_key_var(
            var,
            "default-model = \"opus\"",
            "[agents.coding]\npreamble = \"hi\""
        )
    ));
    let r = resolve_agent(&cfg, Some("coding")).unwrap();
    assert_eq!(r.model_name(), "fast");
    assert_eq!(r.alias_name.as_deref(), Some("opus"));

    // via [agents.<n>].model
    let cfg = parse(&format!(
        "{}{alias}",
        cfg_with_key_var(
            var,
            "",
            "[agents.coding]\npreamble = \"hi\"\nmodel = \"opus\""
        )
    ));
    let r = resolve_agent(&cfg, Some("coding")).unwrap();
    assert_eq!(r.model_name(), "fast");
    assert_eq!(r.alias_name.as_deref(), Some("opus"));
}

/// The point of the indirection: repointing one line moves every agent naming
/// it, with no other edit.
#[test]
fn repointing_an_alias_moves_every_agent_naming_it() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_REPOINT";
    set_env(var, "sk-test");
    let agents = "[agents.one]\npreamble = \"a\"\nmodel = \"opus\"\n\n\
                  [agents.two]\npreamble = \"b\"\nmodel = \"opus\"";
    let mut cfg = parse(&format!(
        "{}\n[models.opus]\nalias = \"fast\"\n",
        cfg_with_key_var(var, "", agents)
    ));

    for agent in ["one", "two"] {
        assert_eq!(
            resolve_agent(&cfg, Some(agent)).unwrap().model_name(),
            "fast"
        );
    }

    // The single edit.
    cfg.models
        .insert("opus".to_string(), outrig::config::Model::alias(["smart"]));

    for agent in ["one", "two"] {
        let r = resolve_agent(&cfg, Some(agent)).unwrap();
        assert_eq!(r.model_name(), "smart");
        assert_eq!(r.model_identifier(), "gpt-4o");
        assert_eq!(r.alias_name.as_deref(), Some("opus"));
    }
}

/// The credential case the static half exists for: one committed config, and
/// each machine picks the row it is actually credentialed for.
#[test]
fn alias_selects_the_first_candidate_whose_key_is_set() {
    let vars = [
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_SEL_A",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_SEL_B",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_SEL_C",
    ];
    let cfg = three_vendor_alias_cfg(vars);

    // Only the second vendor's key is set, so the second candidate wins even
    // though it is not first in the list.
    unset_env(vars[0]);
    set_env(vars[1], "sk-test");
    unset_env(vars[2]);

    let r = resolve_agent(&cfg, Some("coding")).unwrap();
    assert_eq!(r.model_name(), "opus-5-anthropic");
    assert_eq!(r.alias_name.as_deref(), Some("smart"));
    assert_eq!(r.model_identifier(), "claude-opus-5");
    assert!(matches!(r.provider(), ResolvedProvider::Anthropic { .. }));

    // With the first also set, preference order decides.
    set_env(vars[0], "sk-test");
    let r = resolve_agent(&cfg, Some("coding")).unwrap();
    assert_eq!(r.model_name(), "opus-5-bedrock");
}

/// An empty variable is not a set one: `std::env::var` returns `Ok("")` for
/// `FOO=`, which would otherwise be selected and then fail at the endpoint
/// with a provider-side auth error.
#[test]
fn an_empty_api_key_variable_does_not_select_a_candidate() {
    let vars = [
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_EMPTY_A",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_EMPTY_B",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_EMPTY_C",
    ];
    let cfg = three_vendor_alias_cfg(vars);
    set_env(vars[0], "");
    set_env(vars[1], "sk-test");
    unset_env(vars[2]);

    let r = resolve_agent(&cfg, Some("coding")).unwrap();
    assert_eq!(r.model_name(), "opus-5-anthropic");
}

/// A list of three failing for three different reasons is exactly the case a
/// single-line error wastes an afternoon on.
///
/// The third candidate is an in-process model, so what this config *means*
/// depends on the build: without `local-llm` every candidate is unreachable and
/// the alias is exhausted, and with it the third one is the answer. Both halves
/// are asserted, because the exhaustion message is only interesting if the
/// selector really would have taken a reachable candidate.
#[test]
fn alias_with_no_selectable_candidate_names_every_reason() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_NONE";
    unset_env(var);
    let cfg = parse(&format!(
        r#"
default-model = "smart"

[providers.bedrock]
style    = "openai"
base-url = "https://bedrock.example.invalid/v1"
api-key  = "${{{var}}}"

[providers.local]
style = "mistralrs"

[models.smart]
alias = ["opus-5-bedrock", "opus-5-orphan", "opus-5-local"]

[models.opus-5-bedrock]
provider   = "bedrock"
identifier = "anthropic.claude-opus-5-v1:0"

[models.opus-5-orphan]
provider   = "nowhere"
identifier = "claude-opus-5"

[models.opus-5-local]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"

[agents.coding]
preamble = "hi"
"#,
    ));

    let resolved = resolve_agent(&cfg, Some("coding"));

    #[cfg(feature = "local-llm")]
    {
        // The first two are unreachable, so selection walks past them to the
        // in-process candidate this build *can* run.
        let r = resolved.expect("the mistralrs candidate is reachable in this build");
        assert_eq!(r.model_name(), "opus-5-local");
        assert_eq!(r.alias_name.as_deref(), Some("smart"));
    }

    #[cfg(not(feature = "local-llm"))]
    {
        let err = resolved.unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(
                err,
                CliError::LlmResolve(LlmResolveError::NoUsableAliasCandidate { .. })
            ),
            "got: {err:?}"
        );
        assert!(msg.contains("no usable model for alias \"smart\""), "{msg}");
        // Every candidate, each with its own distinct reason.
        assert!(
            msg.contains("opus-5-bedrock")
                && msg.contains(&format!("api-key env var {var} is not set")),
            "{msg}"
        );
        assert!(
            msg.contains("opus-5-orphan")
                && msg.contains("provider \"nowhere\" is not defined under [providers.<name>]"),
            "{msg}"
        );
        // The reasons are the resolver's own errors, not a second wording of
        // them, so a candidate reads the same here as it would if named
        // directly -- including the remedy.
        assert!(
            msg.contains("opus-5-local") && msg.contains("rebuild with --features local-llm"),
            "{msg}"
        );
    }
}

/// A configured device this build has no backend for makes a candidate as
/// unreachable as a missing feature: resolution rejects it a moment later with
/// `MistralrsDeviceUnavailable`. Selection has to agree, or a multi-candidate
/// alias strands itself on a model that cannot run while a hosted candidate
/// sits behind it unused.
#[cfg(all(feature = "local-llm", not(feature = "cuda")))]
#[test]
fn alias_skips_a_candidate_whose_device_backend_is_missing() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_DEVICE_FALLBACK";
    set_env(var, "sk-test");
    let cfg = parse(&format!(
        r#"
default-model = "smart"

[providers.hosted]
style    = "openai"
base-url = "https://hosted.example.invalid/v1"
api-key  = "${{{var}}}"

[providers.local]
style = "mistralrs"

[models.smart]
alias = ["gpu-only", "hosted-fallback"]

[models.gpu-only]
provider   = "local"
model-id   = "Qwen/Qwen2.5-7B-Instruct"
model-file = "qwen2.5-7b-instruct-q4_k_m.gguf"
device     = "cuda"

[models.hosted-fallback]
provider   = "hosted"
identifier = "gpt-4o"

[agents.coding]
preamble = "hi"
"#,
    ));

    let r = resolve_agent(&cfg, Some("coding")).expect("falls through to the hosted candidate");
    assert_eq!(r.model_name(), "hosted-fallback");
    assert_eq!(r.alias_name.as_deref(), Some("smart"));
}

/// `LlmRegistry` is keyed by the resolved model name, so an alias and its
/// target must produce the *same* key -- otherwise two names for one GGUF load
/// the same multi-gigabyte weights twice in one process.
#[test]
fn an_alias_and_its_target_resolve_to_the_same_registry_key() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_REGISTRY";
    set_env(var, "sk-test");
    let cfg = parse(&format!(
        "{}\n[models.opus]\nalias = \"fast\"\n",
        cfg_with_key_var(var, "", "[agents.coding]\npreamble = \"hi\"")
    ));

    let via_alias = resolve_agent_with_overrides(&cfg, Some("coding"), Some("opus"), None).unwrap();
    let direct = resolve_agent_with_overrides(&cfg, Some("coding"), Some("fast"), None).unwrap();

    assert_eq!(
        via_alias.model_name(),
        direct.model_name(),
        "the registry key must not depend on which name was typed"
    );
    // ... while attribution still distinguishes them. The rendering itself is
    // `model_display`, unit-tested in `llm.rs`.
    assert_eq!(via_alias.alias_name.as_deref(), Some("opus"));
    assert_eq!(direct.alias_name, None);
}

/// `--device` selects hardware for one in-process model, so an alias that
/// could land on any of several is refused rather than silently picking one.
#[test]
fn device_override_is_refused_for_a_multi_candidate_alias() {
    let vars = [
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_DEV_A",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_DEV_B",
        "OUTRIG_TEST_LLM_RESOLVE_ALIAS_DEV_C",
    ];
    let cfg = three_vendor_alias_cfg(vars);
    for var in vars {
        set_env(var, "sk-test");
    }

    let err =
        resolve_agent_with_device_override(&cfg, Some("coding"), Some(MistralrsDeviceSpec::Cpu))
            .unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(
            err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceOverrideAlias { .. })
        ),
        "got: {err:?}"
    );
    assert!(
        msg.contains("opus-5-bedrock, opus-5-anthropic, opus-5-azure"),
        "{msg}"
    );
}

/// A single-target alias names exactly one model, so there is no ambiguity and
/// the flag applies just as it would to the target's own name.
#[test]
fn device_override_applies_through_a_single_target_mistralrs_alias() {
    let mut cfg = local_mistralrs_cfg(None);
    cfg.models.insert(
        "onprem".to_string(),
        outrig::config::Model::alias(["local"]),
    );
    cfg.default_model = Some("onprem".to_string());

    let r = resolve_agent_with_device_override(&cfg, Some("smoke"), Some(MistralrsDeviceSpec::Cpu))
        .unwrap();
    assert_eq!(r.model_name(), "local");
    assert_eq!(r.alias_name.as_deref(), Some("onprem"));
    assert_eq!(
        r.model_weights().expect("mistralrs weights").device,
        MistralrsDeviceSpec::Cpu
    );
}

/// A single-target alias onto a remote model still refuses `--device`, and
/// does so through the same message a direct name gets: there is one provider
/// to name, and it is not mistralrs.
#[test]
fn device_override_is_still_refused_through_a_single_target_remote_alias() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_DEV_REMOTE";
    set_env(var, "sk-test");
    let cfg = parse(&format!(
        "{}\n[models.opus]\nalias = \"fast\"\n",
        cfg_with_key_var(var, "", "[agents.coding]\npreamble = \"hi\"")
    ));

    let err = resolve_agent_with_overrides(
        &cfg,
        Some("coding"),
        Some("opus"),
        Some(MistralrsDeviceSpec::Cpu),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            CliError::LlmResolve(LlmResolveError::MistralrsDeviceOverrideUnsupported { .. })
        ),
        "got: {err:?}"
    );
}

/// A single-target alias is renaming, not choosing, so it must not be filtered
/// by selectability -- an unset key has to keep naming the variable, which is
/// the actionable part, rather than collapsing into a list of one.
#[test]
fn a_single_target_alias_keeps_its_targets_own_error() {
    let var = "OUTRIG_TEST_LLM_RESOLVE_ALIAS_ONE_ERR";
    unset_env(var);
    let cfg = parse(&format!(
        "{}\n[models.opus]\nalias = \"fast\"\n",
        cfg_with_key_var(var, "", "[agents.coding]\npreamble = \"hi\"")
    ));

    let err = resolve_agent_with_overrides(&cfg, Some("coding"), Some("opus"), None).unwrap_err();
    assert!(
        matches!(err, CliError::Outrig(outrig::error::OutrigError::ApiKey(_))),
        "expected the target's own api-key error, got: {err:?}"
    );
    assert!(err.to_string().contains(var), "got: {err}");
}

/// The same rule seen from the other side: a single-target alias onto an
/// in-process model in a build without `local-llm` still reports the feature,
/// because that message carries the remedy (a rebuild).
#[cfg(not(feature = "local-llm"))]
#[tokio::test]
async fn a_single_target_local_alias_still_reports_the_missing_feature() {
    let mut cfg = local_mistralrs_cfg(None);
    cfg.models.insert(
        "onprem".to_string(),
        outrig::config::Model::alias(["local"]),
    );
    cfg.default_model = Some("onprem".to_string());

    let resolved = resolve_agent(&cfg, Some("smoke")).expect("resolution succeeds");
    assert_eq!(resolved.model_name(), "local");

    let err = match build_agent(&resolved, vec![], Path::new("/tmp/outrig-test-cache")).await {
        Ok(_) => panic!("expected build_agent to error on feature-off mistralrs"),
        Err(e) => e,
    };
    assert!(
        matches!(
            &err,
            CliError::LlmResolve(LlmResolveError::MistralrsFeatureDisabled { name }) if name == "local"
        ),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("local-llm"), "got: {err}");
}

/// The walk must be total on a `Config` that never went through `validate` --
/// built by hand in a test, or by a library embedder -- rather than recursing
/// until the stack runs out.
#[test]
fn hand_built_cycle_fails_resolution_without_hanging() {
    let mut cfg = Config::default();
    cfg.default_model = Some("a".to_string());
    cfg.models
        .insert("a".to_string(), outrig::config::Model::alias(["b"]));
    cfg.models
        .insert("b".to_string(), outrig::config::Model::alias(["a"]));

    let err = resolve_agent(&cfg, None).unwrap_err();
    assert!(
        err.to_string().contains("model alias cycle: a -> b -> a"),
        "got: {err}"
    );
}

/// An alias naming a name that is not in the table is a config error, not a
/// silent empty candidate list.
#[test]
fn hand_built_dangling_alias_target_fails_resolution() {
    let mut cfg = Config::default();
    cfg.default_model = Some("a".to_string());
    cfg.models
        .insert("a".to_string(), outrig::config::Model::alias(["ghost"]));

    let err = resolve_agent(&cfg, None).unwrap_err();
    assert!(
        err.to_string().contains("alias target \"ghost\""),
        "got: {err}"
    );
}

/// `resolve_agent_with_overrides` promises a `Result` and documents that it does
/// not assume `cfg.validate()` ran, so a row that is neither shape has to come
/// back as an error. `Model::source()` panics on exactly that input, which is
/// why the resolver classifies from the raw fields instead of calling it.
#[test]
fn a_shapeless_model_fails_resolution_rather_than_panicking() {
    let mut cfg = Config::default();
    cfg.default_model = Some("broken".to_string());
    // Neither `provider` nor `alias`: unreachable through `Config::load`, which
    // validates, but reachable for a config built in code.
    let mut broken = outrig::config::Model::new("p");
    broken.provider = None;
    cfg.models.insert("broken".to_string(), broken);

    let err = resolve_agent(&cfg, None).unwrap_err();
    assert!(
        err.to_string()
            .contains("names neither a provider nor an alias"),
        "got: {err}"
    );
}
