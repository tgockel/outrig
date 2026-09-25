//! Resolve agent -> model -> provider.
//!
//! Copied from `outrig-cli`'s `llm.rs`, which keeps its own; the two do not
//! share code during this phase. What one round needs came across, and what it
//! does not was left behind:
//!
//! - An alias resolves to the first of its models this build can reach, as the
//!   CLI's did before failover. Resolving the whole chain is failover's, and
//!   arrives with it.
//! - The subagent limits, the image hint, and the retry budget have no reader
//!   here, and neither do the names the CLI's banner prints.
//! - There are no fallback arms for a provider style nobody taught the
//!   resolver. `LlmProvider` is `#[non_exhaustive]` for other crates, not this
//!   one, so here a new style fails to compile until both matches handle it.

use thiserror::Error;

use super::AgentError;
use crate::config::{
    Agent, Config, DEFAULT_TOOL_CALL_MAX, DEFAULT_TOOL_RESULT_MAX_BYTES, LlmProvider,
};
use crate::error::OutrigError;

/// Failures that surface while walking `agents -> models -> providers` or
/// constructing the Rig client.
#[derive(Debug, Error)]
pub(crate) enum LlmResolveError {
    #[error(
        "agent {name:?} is not defined; pass --agent <name> or set \
         default-agent in config. Known agents: {known}"
    )]
    UnknownAgent { name: String, known: String },

    #[error("agent {agent:?} omits 'model' and no default-model is set")]
    AgentMissingModel { agent: String },

    #[error("no model selected; pass --model <name> or set default-model in config")]
    MissingModel,

    #[error("model {name:?} is not defined under [models.<name>]")]
    UnknownModel { name: String },

    #[error("provider {name:?} is not defined under [providers.<name>]")]
    UnknownProvider { name: String },

    /// A `[models.<name>]` row that is neither shape. Validation rejects it and
    /// `Model::source` panics on it, so this is reachable only through a
    /// `Config` built or mutated in code.
    #[error("model {model:?} names neither a provider nor an alias")]
    ModelHasNoProvider { model: String },

    /// Every model of an alias was rejected before the session started.
    /// `tried` is pre-rendered one model per line: a list of three that fail
    /// for three different reasons is exactly the case a single-line error
    /// wastes an afternoon on.
    #[error("no usable model for alias {alias:?}; tried:\n{tried}")]
    NoUsableAliasCandidate { alias: String, tried: String },

    #[error("failed to build rig client: {0}")]
    RigClientBuild(String),
}

/// Runtime-shaped provider view -- mirrors the config `LlmProvider` enum, but
/// with the env-var-backed `ApiKeyRef` already resolved to a plain `String`.
#[derive(Debug)]
pub(crate) enum ResolvedProvider {
    OpenAi {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
    },
    Anthropic {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
    },
}

/// The concrete `[models.<name>]` row a session runs against, fully resolved.
#[derive(Debug)]
pub(crate) struct ResolvedCandidate {
    /// The *concrete* row: never an alias's name.
    pub(crate) model_name: String,
    pub(crate) model_identifier: String,
    pub(crate) provider_name: String,
    pub(crate) provider: ResolvedProvider,
    /// This row's output-token ceiling: the agent's if it set one, this
    /// model's otherwise.
    pub(crate) max_tokens: Option<u32>,
}

/// Fully-resolved view of one agent: every knob the agent loop needs to
/// build a Rig client and run a round.
///
/// The api-key is resolved from the env at construction time. The struct
/// lives in the agent loop, not in session metadata, so it should never get
/// serialized.
#[derive(Debug)]
pub(crate) struct ResolvedAgent {
    pub(crate) candidate: ResolvedCandidate,
    /// `None` sends no system prompt at all.
    pub(crate) preamble: Option<String>,
    pub(crate) temperature: Option<f32>,
    pub(crate) tool_call_max: usize,
    pub(crate) tool_result_max_bytes: usize,
}

/// Why this build could not reach `model_name`, a concrete (provider-shape)
/// row, or `Ok` if it could.
///
/// The bar is "not a guaranteed failure" rather than "works": no network I/O
/// and no client construction. It predicts one later stage, the api-key
/// resolution in [`resolve_candidate`], and every reason is worded as that
/// stage would word it. It deliberately treats an *empty* api-key variable as
/// unset where that resolution accepts it -- the safe direction for choosing
/// among an alias's models.
///
/// Reads `provider` directly rather than through `Model::source()`, which
/// panics on an unvalidated row. This must stay total.
fn selectability(cfg: &Config, model_name: &str) -> Result<(), String> {
    let not_concrete = || "names neither a provider nor an alias".to_string();
    let model = cfg.models.get(model_name).ok_or_else(not_concrete)?;
    let provider_name = model.provider.as_deref().ok_or_else(not_concrete)?;
    let provider = cfg.providers.get(provider_name).ok_or_else(|| {
        LlmResolveError::UnknownProvider {
            name: provider_name.to_string(),
        }
        .to_string()
    })?;
    let (LlmProvider::OpenAi { api_key, .. } | LlmProvider::Anthropic { api_key, .. }) = provider;
    // Through `resolve` rather than `env::var` so "not set" and "not valid
    // UTF-8" read exactly as they will when the session resolves.
    match api_key.resolve() {
        Ok(value) if !value.is_empty() => Ok(()),
        Ok(_) => Err(format!(
            "api-key env var {} is set but empty",
            api_key.var_name()
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The first of `alias`'s `models` this build could reach, or an error listing
/// every one and why it was skipped.
///
/// Selection is blind to whether an endpoint is *up*: it answers "am I
/// configured for this", not "is this working".
fn first_selectable<'a>(
    cfg: &Config,
    alias: &str,
    models: &[&'a str],
) -> Result<&'a str, AgentError> {
    let mut tried = Vec::new();
    for model in models {
        match selectability(cfg, model) {
            Ok(()) => return Ok(model),
            Err(reason) => tried.push((*model, reason)),
        }
    }
    let width = tried.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let tried = tried
        .iter()
        .map(|(name, reason)| format!("  {name:width$} -- {reason}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(LlmResolveError::NoUsableAliasCandidate {
        alias: alias.to_string(),
        tried,
    }
    .into())
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// `agent_name` is optional: `None` resolves the *agentless* session, which
/// behaves as an `[agents.<name>]` block with no keys set. `model_override`
/// wins over the agent's model and `default-model`.
///
/// Each lookup is re-checked here -- the function does not assume
/// `cfg.validate()` was called -- so errors carry the resolution context
/// (which agent, which model) regardless.
pub(crate) fn resolve_agent(
    cfg: &Config,
    agent_name: Option<&str>,
    model_override: Option<&str>,
) -> Result<ResolvedAgent, AgentError> {
    // The agentless session resolves against an empty agent rather than a
    // parallel code path, so every fallback below is written once.
    let empty;
    let agent = match agent_name {
        Some(name) => cfg.agents.get(name).ok_or_else(|| {
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
                name: name.to_string(),
                known,
            }
        })?,
        None => {
            empty = Agent::default();
            &empty
        }
    };

    let model_name = model_override
        .or(agent.model.as_deref())
        .or(cfg.default_model.as_deref())
        .ok_or_else(|| match agent_name {
            Some(agent) => LlmResolveError::AgentMissingModel {
                agent: agent.to_string(),
            },
            None => LlmResolveError::MissingModel,
        })?;

    let root = cfg
        .models
        .get(model_name)
        .ok_or_else(|| LlmResolveError::UnknownModel {
            name: model_name.to_string(),
        })?;

    // A model names either a provider that serves it or other models it stands
    // for. Read from the raw field rather than through `Model::source()`,
    // which panics on a row that is both shapes or neither.
    let concrete = if root.alias.is_some() {
        let models = cfg
            .model_candidates(model_name)
            .map_err(OutrigError::from)?;
        match models.as_slice() {
            // One model is renaming, not choosing. Resolve it exactly as if the
            // user had typed it, so every error keeps its own text and remedy.
            [only] => only,
            _ => {
                let chosen = first_selectable(cfg, model_name, &models)?;
                tracing::warn!(
                    "{model_name} names {} models, and this loop has no failover yet: it runs \
                     against {chosen} alone",
                    models.len(),
                );
                chosen
            }
        }
    } else {
        model_name
    };

    Ok(ResolvedAgent {
        candidate: resolve_candidate(cfg, agent, concrete)?,
        preamble: agent.preamble.clone(),
        temperature: agent.temperature,
        tool_call_max: agent
            .tool_call_max
            .or(cfg.tool_call_max)
            .unwrap_or(DEFAULT_TOOL_CALL_MAX) as usize,
        tool_result_max_bytes: agent
            .tool_result_max
            .or(cfg.tool_result_max)
            .unwrap_or(DEFAULT_TOOL_RESULT_MAX_BYTES) as usize,
    })
}

/// Resolve one concrete `[models.<name>]` row against the provider serving it.
///
/// Every error it raises names the row it was resolving, so an alias's model
/// reports the same text it would have if the user had named it directly.
fn resolve_candidate(
    cfg: &Config,
    agent: &Agent,
    model_name: &str,
) -> Result<ResolvedCandidate, AgentError> {
    let model = cfg
        .models
        .get(model_name)
        .ok_or_else(|| LlmResolveError::UnknownModel {
            name: model_name.to_string(),
        })?;

    let Some(provider_name) = model.provider.as_deref() else {
        // A row that is neither shape. Say what is wrong rather than blaming a
        // provider named "".
        return Err(LlmResolveError::ModelHasNoProvider {
            model: model_name.to_string(),
        }
        .into());
    };

    let provider =
        cfg.providers
            .get(provider_name)
            .ok_or_else(|| LlmResolveError::UnknownProvider {
                name: provider_name.to_string(),
            })?;

    let resolved_provider = match provider {
        LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } => ResolvedProvider::OpenAi {
            base_url: base_url.clone(),
            api_key: api_key.resolve()?,
            request_timeout_secs: *request_timeout_secs,
        },
        LlmProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } => ResolvedProvider::Anthropic {
            base_url: base_url.clone(),
            api_key: api_key.resolve()?,
            request_timeout_secs: *request_timeout_secs,
        },
    };

    Ok(ResolvedCandidate {
        model_name: model_name.to_string(),
        // Every style names its model the same way: the configured identifier,
        // falling back to the model's own name.
        model_identifier: model
            .identifier
            .clone()
            .unwrap_or_else(|| model_name.to_string()),
        provider_name: provider_name.to_string(),
        provider: resolved_provider,
        // The agent's ceiling wins; the model's is the fallback.
        max_tokens: agent.max_tokens.or(model.max_tokens),
    })
}
