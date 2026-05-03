//! Resolve agent -> model -> provider; build Rig client.

use thiserror::Error;

use crate::config::Config;
use crate::error::Result;
use crate::rig_tool::McpToolAdapter;

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
        "provider style {style:?} is not yet supported in v0; \
         v0 wires \"openai\" only"
    )]
    UnsupportedProviderStyle { style: String },

    #[error("failed to build rig client: {0}")]
    RigClientBuild(String),
}

/// Fully-resolved view of one agent: every knob the agent loop needs to
/// build a Rig client and run a turn.
///
/// `api_key` is resolved from the env at construction time. The struct lives
/// in the agent loop, not in session metadata, so it should never get
/// serialized.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    pub agent_name: String,
    pub model_name: String,
    pub model_identifier: String,
    pub provider_name: String,
    pub provider_style: String,
    pub provider_base_url: String,
    pub api_key: String,
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

    let api_key = provider.api_key.resolve()?;

    Ok(ResolvedAgent {
        agent_name: agent_name.to_string(),
        model_name: model_name.to_string(),
        model_identifier: model.identifier.clone(),
        provider_name: model.provider.clone(),
        provider_style: provider.style.clone(),
        provider_base_url: provider.base_url.clone(),
        api_key,
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

/// Build a Rig provider client from a resolved agent. Only `style = "openai"`
/// is wired -- any other style errors out before touching rig.
pub fn build_rig_client(resolved: &ResolvedAgent) -> Result<RigClient> {
    if resolved.provider_style != "openai" {
        return Err(LlmResolveError::UnsupportedProviderStyle {
            style: resolved.provider_style.clone(),
        }
        .into());
    }
    RigClient::builder()
        .api_key(resolved.api_key.clone())
        .base_url(&resolved.provider_base_url)
        .build()
        .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()).into())
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
