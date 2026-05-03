//! Resolve agent -> model -> provider; build Rig client.

use std::path::PathBuf;

use thiserror::Error;

use crate::config::{Config, LlmProvider};
use crate::error::Result;
use crate::rig_tool::McpToolAdapter;

#[cfg(feature = "mistralrs")]
mod mistralrs;

/// Default preamble used when an agent leaves the field unset. Deliberately
/// generic; agents that need anything specific spell it out themselves.
const DEFAULT_PREAMBLE: &str =
    "You are a careful assistant operating inside a sandboxed container.";

/// Failures that surface while walking `agents -> models -> providers` or
/// constructing the Rig client. Wrapped into [`crate::error::OutrigError`]
/// at the top level via `#[from]`.
#[derive(Debug, Error)]
pub enum LlmResolveError {
    #[error(
        "agent {name:?} is not defined; pass --agent <name> or set \
         default-agent in config. Known agents: {known}"
    )]
    UnknownAgent { name: String, known: String },

    #[error("agent {agent:?} omits 'model' and no default-model is set")]
    AgentMissingModel { agent: String },

    #[error("model {name:?} is not defined under [models.<name>]")]
    UnknownModel { name: String },

    #[error("provider {name:?} is not defined under [providers.<name>]")]
    UnknownProvider { name: String },

    #[error(
        "mistralrs provider {name:?} requested but this build of outrig \
         does not include the 'mistralrs' feature; rebuild with \
         --features mistralrs to enable"
    )]
    MistralrsFeatureDisabled { name: String },

    #[cfg(feature = "mistralrs")]
    #[error("provider style 'mistralrs' has no runtime in this build")]
    MistralrsRuntimeUnavailable,

    #[error("failed to build rig client: {0}")]
    RigClientBuild(String),
}

/// Runtime-shaped provider view -- mirrors the config `LlmProvider` enum, but
/// with the env-var-backed `ApiKeyRef` already resolved to a plain `String`
/// for the OpenAi variant. Variants are kept in sync with `LlmProvider`'s.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedProvider {
    OpenAi {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
    },
    Mistralrs {
        model_id: Option<String>,
        model_path: Option<PathBuf>,
        model_file: Option<String>,
        revision: Option<String>,
        context_length: Option<u32>,
    },
}

/// Fully-resolved view of one agent: every knob the agent loop needs to
/// build a Rig client and run a turn.
///
/// For the `OpenAi` provider variant, the api-key is resolved from the env at
/// construction time. The struct lives in the agent loop, not in session
/// metadata, so it should never get serialized.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    pub agent_name: String,
    pub model_name: String,
    pub model_identifier: String,
    pub provider_name: String,
    pub provider: ResolvedProvider,
    pub preamble: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub container: Option<String>,
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// Each lookup is re-checked here -- the function does not assume
/// `cfg.validate()` was called -- so errors carry the resolution context
/// (which agent, which model) regardless.
pub fn resolve_agent(cfg: &Config, agent_name: &str) -> Result<ResolvedAgent> {
    let agent = cfg.agents.get(agent_name).ok_or_else(|| {
        let known = if cfg.agents.is_empty() {
            "(none)".to_string()
        } else {
            cfg.agents
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        LlmResolveError::UnknownAgent {
            name: agent_name.to_string(),
            known,
        }
    })?;

    let model_name = agent
        .model
        .as_deref()
        .or(cfg.default_model.as_deref())
        .ok_or_else(|| LlmResolveError::AgentMissingModel {
            agent: agent_name.to_string(),
        })?;

    let model = cfg
        .models
        .get(model_name)
        .ok_or_else(|| LlmResolveError::UnknownModel {
            name: model_name.to_string(),
        })?;

    let provider =
        cfg.providers
            .get(&model.provider)
            .ok_or_else(|| LlmResolveError::UnknownProvider {
                name: model.provider.clone(),
            })?;

    let resolved_provider = match provider {
        LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
        } => ResolvedProvider::OpenAi {
            base_url: base_url.clone(),
            api_key: api_key.resolve()?,
            request_timeout_secs: *request_timeout_secs,
        },
        LlmProvider::Mistralrs {
            model_id,
            model_path,
            model_file,
            revision,
            context_length,
        } => ResolvedProvider::Mistralrs {
            model_id: model_id.clone(),
            model_path: model_path.clone(),
            model_file: model_file.clone(),
            revision: revision.clone(),
            context_length: *context_length,
        },
    };

    Ok(ResolvedAgent {
        agent_name: agent_name.to_string(),
        model_name: model_name.to_string(),
        model_identifier: model.identifier.clone(),
        provider_name: model.provider.clone(),
        provider: resolved_provider,
        preamble: agent
            .preamble
            .clone()
            .unwrap_or_else(|| DEFAULT_PREAMBLE.to_string()),
        temperature: agent.temperature,
        max_tokens: agent.max_tokens,
        container: agent.container.clone(),
    })
}

pub type RigClient = rig::providers::openai::CompletionsClient;
pub type RigCompletionModel = rig::providers::openai::CompletionModel;
pub type RigAgent = rig::agent::Agent<RigCompletionModel>;

/// Build a Rig provider client from a resolved agent. Only the `OpenAi`
/// variant is wired in v0; `Mistralrs` returns a feature-flag-aware error
/// when the `mistralrs` feature is off, and a placeholder error when it is
/// on -- task 0015 replaces the placeholder with the real shim.
pub fn build_rig_client(resolved: &ResolvedAgent) -> Result<RigClient> {
    match &resolved.provider {
        ResolvedProvider::OpenAi {
            base_url, api_key, ..
        } => RigClient::builder()
            .api_key(api_key.clone())
            .base_url(base_url)
            .build()
            .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()).into()),
        ResolvedProvider::Mistralrs { .. } => {
            #[cfg(not(feature = "mistralrs"))]
            return Err(LlmResolveError::MistralrsFeatureDisabled {
                name: resolved.provider_name.clone(),
            }
            .into());

            #[cfg(feature = "mistralrs")]
            return Err(LlmResolveError::MistralrsRuntimeUnavailable.into());
        }
    }
}

/// Build a Rig `Agent` ready to receive a turn. Preamble, sampling params,
/// and the dynamic-tool list come from `resolved` plus the caller-supplied
/// MCP-backed adapters.
pub fn build_agent(
    resolved: &ResolvedAgent,
    client: &RigClient,
    tools: Vec<McpToolAdapter>,
) -> RigAgent {
    use rig::agent::AgentBuilder;
    use rig::client::CompletionClient;
    use rig::tool::ToolDyn;

    let model = client.completion_model(&resolved.model_identifier);
    let mut builder = AgentBuilder::new(model).preamble(&resolved.preamble);
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(temperature as f64);
    }
    if let Some(max_tokens) = resolved.max_tokens {
        builder = builder.max_tokens(max_tokens as u64);
    }
    let boxed: Vec<Box<dyn ToolDyn>> = tools
        .into_iter()
        .map(|t| Box::new(t) as Box<dyn ToolDyn>)
        .collect();
    builder.tools(boxed).build()
}
