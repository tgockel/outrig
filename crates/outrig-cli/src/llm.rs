//! Resolve agent -> model -> provider; build Rig agent.

use std::cell::{Cell, Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "local-llm")]
use futures_util::StreamExt;
use rig::agent::{AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind};
#[cfg(feature = "local-llm")]
use rig::agent::{MultiTurnStreamItem, StreamingError};
use rig::completion::{CompletionModel, Message, Prompt};
#[cfg(feature = "local-llm")]
use rig::streaming::{StreamedAssistantContent, StreamingChat};
use thiserror::Error;
#[cfg(feature = "local-llm")]
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::{CliError, Result};
use crate::session_tool::{self, SessionTool};
use outrig::config::{Config, DEFAULT_TOOL_CALL_MAX, LlmProvider, MistralrsDeviceSpec};

/// Hard max on tool calls per turn. The per-turn [`OutrigPromptHook`] trips
/// this and surfaces a controllable message. rig's own `max_turns` is a
/// backstop set a little higher: as of rig 0.40 it counts *total* model calls
/// (initial completion + one continuation per tool call) rather than the looser
/// 0.39 budget, so callers pass `tool_call_max + 2` to preserve the previous
/// effective allowance and keep the hook -- not rig -- the limiter that fires
/// first.
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub const MAX_TOOL_CALLS: usize = DEFAULT_TOOL_CALL_MAX as usize;

/// Default byte ceiling applied to each individual MCP tool result before it
/// is handed to Rig and appended to model-visible chat history.
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub const DEFAULT_TOOL_RESULT_MAX_BYTES: usize =
    outrig::config::DEFAULT_TOOL_RESULT_MAX_BYTES as usize;

/// Default per-request HTTP timeout for remote providers when
/// `request-timeout-secs` is unset (see `doc/reference/config.md`). Generous
/// enough not to truncate long reasoning completions, and above typical proxy
/// timeouts so a client-side timeout never races a still-in-flight server
/// request.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

/// Output-token ceiling for a Claude identifier this build of rig has no
/// published ceiling for -- typically a model newer than the pinned rig, or a
/// proxy's own naming. Anthropic rejects a request that carries no ceiling at
/// all, so *something* has to fill the gap; see `build_agent`'s Anthropic arm
/// for when this one does.
///
/// High enough that a real reply is not clipped, and below every
/// current-generation ceiling. An identifier whose true limit is *lower* (the
/// 3.x families) draws a 400 from Anthropic naming that limit -- loud, and
/// fixable with one config line, which is the trade this number is chosen for.
pub const ANTHROPIC_FALLBACK_MAX_TOKENS: u64 = 32_768;

pub mod failover;
pub mod retry;

#[cfg(feature = "local-llm")]
pub mod mistralrs;
#[cfg(feature = "local-llm")]
pub mod registry;

#[cfg(feature = "local-llm")]
pub use registry::LlmRegistry;

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

    #[error("no model selected; pass --model <name> or set default-model in config")]
    MissingModel,

    #[error("model {name:?} is not defined under [models.<name>]")]
    UnknownModel { name: String },

    #[error("provider {name:?} is not defined under [providers.<name>]")]
    UnknownProvider { name: String },

    #[error(
        "provider {name:?} uses a provider style this build of outrig does \
         not know how to reach"
    )]
    UnsupportedProvider { name: String },

    #[error(
        "mistralrs provider {name:?} requested but this build of outrig \
         does not include the 'local-llm' feature; rebuild with \
         --features local-llm to enable"
    )]
    MistralrsFeatureDisabled { name: String },

    #[error(
        "mistralrs model {model:?} has invalid device {device:?}; \
         expected one of: cpu, cuda, cuda:N, metal"
    )]
    MistralrsDeviceInvalid { model: String, device: String },

    #[error(
        "mistralrs model {model:?} requested device {device:?} but this \
         build of outrig does not include the '{feature}' feature; rebuild \
         with --features {feature} to enable"
    )]
    MistralrsDeviceUnavailable {
        model: String,
        device: String,
        feature: &'static str,
    },

    #[error(
        "model {model:?} uses provider {provider:?}, which is not \
         style=mistralrs; --device only applies to mistralrs models"
    )]
    MistralrsDeviceOverrideUnsupported { model: String, provider: String },

    /// The multi-candidate counterpart of the variant above. Deliberately not
    /// that one: `--device` selects hardware for one in-process model, and an
    /// alias spanning several candidates has no single provider to name, so
    /// that message's "uses provider X" clause would be a lie.
    #[error(
        "--device selects hardware for one in-process model, but model \
         {model:?} is an alias over several candidates ({candidates}); pass \
         --model naming one of them directly"
    )]
    MistralrsDeviceOverrideAlias { model: String, candidates: String },

    /// A `[models.<name>]` row that is neither shape. Validation rejects it and
    /// `Model::source` panics on it, so this is reachable only through a
    /// `ModelSourceRef` variant added after this match was written.
    #[error("model {model:?} names neither a provider nor an alias")]
    ModelHasNoProvider { model: String },

    /// Every candidate of an alias was rejected before the session started.
    /// `tried` is pre-rendered one candidate per line: a list of three that
    /// fail for three different reasons is exactly the case a single-line
    /// error wastes an afternoon on.
    #[error("no usable model for alias {alias:?}; tried:\n{tried}")]
    NoUsableAliasCandidate { alias: String, tried: String },

    #[cfg(feature = "local-llm")]
    #[error(
        "mistralrs model {model:?}: requested context-length \
         {requested} exceeds the model's maximum of {max}"
    )]
    MistralrsContextTooLong {
        model: String,
        requested: u32,
        max: usize,
    },

    #[cfg(feature = "local-llm")]
    #[error("mistralrs model {model:?}: failed to load model: {source}")]
    MistralrsLoad {
        model: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("failed to build rig client: {0}")]
    RigClientBuild(String),
}

/// Runtime-shaped provider view -- mirrors the config `LlmProvider` enum, but
/// with the env-var-backed `ApiKeyRef` already resolved to a plain `String`
/// for the remote variants. Variants are kept in sync with `LlmProvider`'s.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedProvider {
    OpenAi {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
        retry_budget_secs: Option<u64>,
    },
    Anthropic {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
        retry_budget_secs: Option<u64>,
    },
    Mistralrs,
}

/// Weight-source spec for a mistralrs-backed model. Lifted off
/// `[models.<name>]` at resolve time. Only one of `model_id` / `model_path`
/// is meaningful in any given instance; validation enforces that, but
/// `mistralrs::load` is also defensive.
#[derive(Debug, Clone, PartialEq)]
pub struct MistralrsWeights {
    pub model_id: Option<String>,
    pub model_path: Option<PathBuf>,
    pub model_file: Option<Vec<String>>,
    pub revision: Option<String>,
    pub context_length: Option<u32>,
    pub device: MistralrsDeviceSpec,
}

/// One concrete `[models.<name>]` row a session may run against, fully
/// resolved.
///
/// These five fields used to sit directly on [`ResolvedAgent`], describing the
/// one row a session had picked. Under failover they travel *per candidate*: an
/// alias over three provider-equivalent rows resolves to three of these, and
/// the chain moves between them within one `completion()` call. Everything that
/// is a property of the *agent* rather than of the row it runs on -- the
/// preamble, the temperature, the tool limits -- stays on `ResolvedAgent`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCandidate {
    /// The *concrete* `[models.<name>]` row, which is also the `LlmRegistry`
    /// cache key -- so two names for one in-process model share one loaded
    /// engine. Never the alias's name, per candidate as much as per session.
    pub model_name: String,
    pub model_identifier: String,
    pub provider_name: String,
    pub provider: ResolvedProvider,
    /// `Some` for mistralrs-style models, `None` for remote ones. Carries
    /// the per-model weight spec that used to live on the provider config.
    pub model_weights: Option<MistralrsWeights>,
    /// This row's output-token ceiling: the agent's if it set one, this
    /// model's otherwise.
    ///
    /// Per candidate because the fallback half is, and because a chain that
    /// moved the identifier while keeping candidate one's ceiling would send a
    /// number the new endpoint never agreed to.
    pub max_tokens: Option<u32>,
}

/// Fully-resolved view of one agent: every knob the agent loop needs to
/// build a Rig client and run a turn.
///
/// For the remote provider variants, the api-key is resolved from the env at
/// construction time. The struct lives in the agent loop, not in session
/// metadata, so it should never get serialized.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAgent {
    /// `None` for a session that named no agent: `outrig run` with neither
    /// `--agent` nor `default-agent`. Every knob below then comes from the
    /// top-level config and the built-in defaults, exactly as it would for an
    /// `[agents.<name>]` block with no keys set.
    pub agent_name: Option<String>,
    /// The rows this session may run against, in preference order, and never
    /// empty.
    ///
    /// One element for a direct model name or a single-target alias, which is
    /// every pre-failover session and stays exactly that. More than one only
    /// for a multi-candidate alias, where the extras are what a mid-turn
    /// failure moves to. The first is what every reader that wants "the model"
    /// means, which is why [`primary`](Self::primary) exists rather than
    /// indexing at each site.
    pub candidates: Vec<ResolvedCandidate>,
    /// The name the caller asked for, when it was an alias standing for the
    /// candidates below; `None` when no alias was involved. Kept beside them
    /// rather than replacing them so attribution can show the hop without the
    /// alias reaching the registry.
    pub alias_name: Option<String>,
    /// `None` sends no system prompt at all. That is what an agent which omits
    /// `preamble` resolves to, and what every agentless session resolves to.
    pub preamble: Option<String>,
    pub temperature: Option<f32>,
    pub tool_call_max: usize,
    pub tool_result_max_bytes: usize,
    /// Maximum subagent nesting depth: the primary is the root at depth 1, and
    /// an agent at depth `D` may launch subagents while `D < subagent_depth_max`.
    pub subagent_depth_max: u32,
    /// Maximum number of live subagents this agent may launch at once.
    pub subagent_width_max: u32,
    pub image: Option<String>,
}

impl ResolvedAgent {
    /// The first candidate: the row this session runs against until something
    /// fails.
    ///
    /// Every reader that predates failover means this one, so it is a method
    /// rather than an index at each site -- and the `expect` states the
    /// invariant once. Resolution builds the list from a walk that either
    /// yields at least one candidate or returns an error, so an empty chain is
    /// a construction bug rather than a config the user can write.
    pub fn primary(&self) -> &ResolvedCandidate {
        self.candidates
            .first()
            .expect("a resolved agent always has at least one candidate")
    }

    /// The candidates after the first: what a mid-turn failure moves to, in
    /// order. Empty for every single-candidate session.
    ///
    /// For attribution. A chain means one session can span two models, so the
    /// banner names the fallbacks up front rather than leaving the first move
    /// to be the first the user hears of them.
    pub fn fallback_names(&self) -> Vec<&str> {
        self.candidates[1..]
            .iter()
            .map(|candidate| candidate.model_name.as_str())
            .collect()
    }

    /// The concrete row this session runs against. See [`primary`](Self::primary).
    pub fn model_name(&self) -> &str {
        &self.primary().model_name
    }

    /// The wire identifier the first candidate sends.
    pub fn model_identifier(&self) -> &str {
        &self.primary().model_identifier
    }

    /// The `[providers.<name>]` entry serving the first candidate.
    pub fn provider_name(&self) -> &str {
        &self.primary().provider_name
    }

    /// The first candidate's resolved provider.
    pub fn provider(&self) -> &ResolvedProvider {
        &self.primary().provider
    }

    /// The first candidate's weight spec, for in-process models.
    pub fn model_weights(&self) -> Option<&MistralrsWeights> {
        self.primary().model_weights.as_ref()
    }

    /// The first candidate's output-token ceiling.
    pub fn max_tokens(&self) -> Option<u32> {
        self.primary().max_tokens
    }

    /// The model as it should be *shown*: `alias -> concrete` when an alias was
    /// involved, and the bare name otherwise.
    ///
    /// One function behind every surface that prints a model name -- the
    /// banner, the subagent launch trace, and the transcript header -- because
    /// static selection is otherwise invisible by construction. Picking a
    /// candidate from a list because of an environment variable is exactly the
    /// kind of decision that stays hidden until it is wrong.
    ///
    /// The arrow appears only when an alias was actually involved, so a direct
    /// model name prints exactly what it printed before aliases existed.
    pub(crate) fn model_display(&self) -> String {
        match &self.alias_name {
            Some(alias) => format!("{alias} -> {}", self.model_name()),
            None => self.model_name().to_string(),
        }
    }
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// `agent_name` is optional: `None` resolves the *agentless* session that
/// `outrig run` starts when neither `--agent` nor `default-agent` names one.
/// That case behaves as an `[agents.<name>]` block with no keys set -- no
/// preamble, no image hint, every limit from the top-level config -- so the
/// only thing it still needs from somewhere is a model.
///
/// Why a concrete model is not a candidate this build could pick.
///
/// Every arm that has a canonical error elsewhere *is* that error rather than a
/// second wording of it, so the same misconfiguration reads identically whether
/// the user named the model directly or reached it through an alias -- the
/// drift this predicate exists to prevent.
#[derive(Debug)]
pub(crate) enum Unselectable {
    /// No `[models.<name>]` row, or one naming no provider. Unreachable through
    /// either caller today, since both feed names that came out of
    /// `Config::model_candidates`; kept so the predicate stays total on a
    /// hand-built config.
    NotConcrete,
    /// Carries the resolver's own error for the three cases it also reports.
    AsResolved(LlmResolveError),
    /// The rendered `ApiKeyError` -- unset, or not valid UTF-8. Held as text
    /// because `ApiKeyRef::resolve` returns the library crate's `OutrigError`.
    ApiKey(String),
    /// `ApiKeyError` has no "set but empty" variant, so this one is ours.
    ApiKeyEmpty(String),
}

impl std::fmt::Display for Unselectable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConcrete => write!(f, "names neither a provider nor an alias"),
            Self::AsResolved(e) => write!(f, "{e}"),
            Self::ApiKey(message) => write!(f, "{message}"),
            Self::ApiKeyEmpty(var) => write!(f, "api-key env var {var} is set but empty"),
        }
    }
}

/// Whether this build could actually reach `model_name`, which must be a
/// concrete (provider-shape) row.
///
/// The one predicate behind two callers -- [`first_selectable`] below and
/// `subagent::usable_model_names`, which advertises a name when any of its
/// candidates passes. They were separate filters that agreed by coincidence.
///
/// The bar is "not a guaranteed failure" rather than "works". A wrong key, a
/// down endpoint, and a missing GGUF are all invisible from here and stay so --
/// no network I/O, no weight load, no client construction, so the synchronous
/// schema-building path can call it.
///
/// It predicts two later stages: the api-key resolution in
/// `resolve_agent_with_overrides` and the `local-llm` check in `build_agent`.
/// A precondition added to either without being added here goes stale silently
/// -- aliases would pick a candidate that then dies, and the subagent schema
/// would over-advertise.
///
/// One divergence is deliberate: an *empty* api-key variable counts as unset,
/// where `ApiKeyRef::resolve` accepts it (`std::env::var` returns `Ok("")` for
/// `FOO=`) and the request then fails at the endpoint with a provider-side auth
/// error. Making `resolve` reject empty values would move an existing error
/// path for direct model names, which is a separate decision -- so the two
/// differ on purpose, in the safe direction: under-advertise, never
/// over-advertise.
///
/// Reads `provider` directly rather than through `Model::source()`, which
/// panics on an unvalidated row. This must stay total.
pub(crate) fn selectability(
    cfg: &Config,
    model_name: &str,
) -> std::result::Result<(), Unselectable> {
    let model = cfg.models.get(model_name).ok_or(Unselectable::NotConcrete)?;
    let provider_name = model.provider.as_deref().ok_or(Unselectable::NotConcrete)?;
    let provider = cfg.providers.get(provider_name).ok_or_else(|| {
        Unselectable::AsResolved(LlmResolveError::UnknownProvider {
            name: provider_name.to_string(),
        })
    })?;
    match provider {
        LlmProvider::OpenAi { api_key, .. } | LlmProvider::Anthropic { api_key, .. } => {
            // Through `resolve` rather than `env::var` so "not set" and "not
            // valid UTF-8" read exactly as they will when the session resolves.
            match api_key.resolve() {
                Ok(value) if !value.is_empty() => Ok(()),
                Ok(_) => Err(Unselectable::ApiKeyEmpty(api_key.var_name().to_string())),
                Err(e) => Err(Unselectable::ApiKey(e.to_string())),
            }
        }
        // Two compile-time decisions, not runtime ones. `cfg!` keeps one body
        // compiling in every build, so they cannot drift apart.
        LlmProvider::Mistralrs => {
            if !cfg!(feature = "local-llm") {
                return Err(Unselectable::AsResolved(
                    LlmResolveError::MistralrsFeatureDisabled {
                        name: provider_name.to_string(),
                    },
                ));
            }
            // The *backend* is a second gate: a row asking for `cuda` in a
            // build without `--features cuda` is as unreachable as a mistralrs
            // row without `local-llm`, and resolution rejects it a moment
            // later. Selecting it anyway would strand a multi-candidate alias
            // on a model that cannot run while a hosted candidate sat behind
            // it. Delegated to the resolver's own parser so the device rules
            // are stated once.
            parse_mistralrs_device(model_name, model.device.as_deref())
                .map(|_| ())
                .map_err(Unselectable::AsResolved)
        }
        // `LlmProvider` is `#[non_exhaustive]`: a style this function has not
        // been taught about is not selectable.
        _ => Err(Unselectable::AsResolved(
            LlmResolveError::UnsupportedProvider {
                name: provider_name.to_string(),
            },
        )),
    }
}

/// The first candidate this build could reach, or every candidate paired with
/// why it was skipped.
///
/// Shared by the alias selector and `subagent::usable_model_names` so the
/// *traversal* is common, not just the per-candidate predicate: a future
/// selection rule cannot land in one and not the other.
///
/// Selection is deliberately blind to whether an endpoint is *up* -- building a
/// remote client does no I/O, so this answers "am I configured for this" and
/// not "is this working". Moving to another candidate when one fails mid-turn
/// is a separate mechanism.
pub(crate) fn first_selectable<'a>(
    cfg: &Config,
    candidates: &[&'a str],
) -> std::result::Result<&'a str, Vec<(&'a str, Unselectable)>> {
    // Infallible indexing: `selectable_candidates` returns `Ok` only for a
    // non-empty list.
    selectable_candidates(cfg, candidates).map(|kept| kept[0])
}

/// *Every* candidate this build could reach, in preference order, or every
/// candidate paired with why it was skipped.
///
/// The failover counterpart of [`first_selectable`], and the same traversal:
/// selection answers "am I configured for this", so a chain is built from the
/// candidates that pass it and failover decides among *those* which is working.
/// Dropping the unreachable ones here rather than at the first turn is what
/// keeps a chain from spending a move on a candidate whose api-key was never
/// set.
///
/// Returns `Err` only when nothing survives, carrying the same per-candidate
/// reasons a single selection would have reported.
pub(crate) fn selectable_candidates<'a>(
    cfg: &Config,
    candidates: &[&'a str],
) -> std::result::Result<Vec<&'a str>, Vec<(&'a str, Unselectable)>> {
    let mut kept = Vec::new();
    let mut tried = Vec::new();
    for candidate in candidates {
        match selectability(cfg, candidate) {
            Ok(()) => kept.push(*candidate),
            Err(reason) => tried.push((*candidate, reason)),
        }
    }
    if kept.is_empty() { Err(tried) } else { Ok(kept) }
}

/// [`selectable_candidates`] with the alias's name attached, rendering the
/// failure as one line per candidate.
///
/// Only reached when there is a genuine choice to make -- a single-target alias
/// resolves its one target directly, so this never turns a legible single-model
/// error into a list of one. The per-candidate shape is for the case worth
/// serving: three candidates failing for three different reasons, where one
/// line costs the user an afternoon.
fn select_candidates<'a>(cfg: &Config, alias: &str, candidates: &[&'a str]) -> Result<Vec<&'a str>> {
    selectable_candidates(cfg, candidates).map_err(|tried| {
        let rows: Vec<_> = tried
            .iter()
            .map(|(name, reason)| (*name, reason.to_string()))
            .collect();
        LlmResolveError::NoUsableAliasCandidate {
            alias: alias.to_string(),
            tried: render_candidate_reasons(&rows),
        }
        .into()
    })
}

/// One line per candidate: the name, padded so the reasons align, and why that
/// candidate is not the one serving this turn.
///
/// Shared by the two halves of the same message. The static half reports it
/// when no candidate is *selectable* at resolve time; the chain reports it when
/// every selectable candidate has *failed* mid-turn. Same list, same question
/// from the user's side -- "what did you try, and what went wrong with each" --
/// at two different moments, so one renderer rather than two that drift on
/// alignment or separator with nothing failing.
pub(crate) fn render_candidate_reasons(rows: &[(&str, String)]) -> String {
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(name, reason)| format!("  {name:width$} -- {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// `agent_name` is optional: `None` resolves the *agentless* session that
/// `outrig run` starts when neither `--agent` nor `default-agent` names one.
/// That case behaves as an `[agents.<name>]` block with no keys set -- no
/// preamble, no image hint, every limit from the top-level config -- so the
/// only thing it still needs from somewhere is a model.
///
/// Each lookup is re-checked here -- the function does not assume
/// `cfg.validate()` was called -- so errors carry the resolution context
/// (which agent, which model) regardless.
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub fn resolve_agent(cfg: &Config, agent_name: Option<&str>) -> Result<ResolvedAgent> {
    resolve_agent_with_overrides(cfg, agent_name, None, None)
}

#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub fn resolve_agent_with_device_override(
    cfg: &Config,
    agent_name: Option<&str>,
    device_override: Option<MistralrsDeviceSpec>,
) -> Result<ResolvedAgent> {
    resolve_agent_with_overrides(cfg, agent_name, None, device_override)
}

pub fn resolve_agent_with_overrides(
    cfg: &Config,
    agent_name: Option<&str>,
    model_override: Option<&str>,
    device_override: Option<MistralrsDeviceSpec>,
) -> Result<ResolvedAgent> {
    // The agentless session resolves against an empty agent rather than a
    // parallel code path, so every fallback below is written once and cannot
    // drift between the two cases.
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
            empty = outrig::config::Agent::default();
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
    let mut alias_name = None;

    // A model names either a provider that serves it or other models it stands
    // for. Branching on the shape rather than on how many candidates an alias
    // happens to flatten to is what keeps the no-alias path *textually* the
    // code it has always been, rather than merely arguably equivalent to it.
    //
    // Read from the raw field rather than through `Model::source()`, which
    // panics on a row that is both shapes or neither. This function promises
    // above that it does not assume `cfg.validate()` ran, and a hand-built or
    // mutated `Config` can reach it through the library API -- a promise of a
    // `Result` has to be kept with a `Result`.
    let concrete: Vec<&str> = if root.alias.is_some() {
        let candidates = cfg
            .model_candidates(model_name)
            .map_err(|e| CliError::Outrig(e.into()))?;

        // `--device` selects hardware for one in-process model, and an alias
        // may span several styles. Picking a device for whichever candidate
        // happened to win is a silent surprise on a multi-GPU host. A
        // single-target alias names exactly one model, so it is free to take
        // the flag.
        if candidates.len() > 1 && device_override.is_some() {
            return Err(LlmResolveError::MistralrsDeviceOverrideAlias {
                model: model_name.to_string(),
                candidates: candidates.join(", "),
            }
            .into());
        }

        let chain = match candidates.as_slice() {
            // One candidate is renaming, not choosing. Resolve it exactly as if
            // the user had typed it, rather than filtering it: that keeps every
            // existing error with its own text *and its own remedy* -- an unset
            // api key names the variable, and a mistralrs model in a default
            // build still reports `MistralrsFeatureDisabled`, the one that says
            // "rebuild with --features local-llm".
            [only] => vec![*only],
            // Every selectable candidate, not just the first: the extras are
            // the chain a mid-turn failure moves along. Selection still drops
            // the ones this build could never reach, so failover only ever
            // chooses among candidates that were configured.
            _ => select_candidates(cfg, model_name, &candidates)?,
        };

        alias_name = Some(model_name);
        chain
    } else {
        vec![model_name]
    };

    // From here down each row resolves exactly as the single row did before
    // aliases existed -- the loop is the only new thing.
    let mut resolved = Vec::with_capacity(concrete.len());
    for name in concrete {
        resolved.push(resolve_candidate(cfg, agent, name, device_override)?);
    }

    Ok(ResolvedAgent {
        agent_name: agent_name.map(str::to_string),
        candidates: resolved,
        alias_name: alias_name.map(str::to_string),
        preamble: agent.preamble.clone(),
        temperature: agent.temperature,
        tool_call_max: agent
            .tool_call_max
            .or(cfg.tool_call_max)
            .unwrap_or(DEFAULT_TOOL_CALL_MAX) as usize,
        tool_result_max_bytes: agent
            .tool_result_max
            .or(cfg.tool_result_max)
            .unwrap_or(outrig::config::DEFAULT_TOOL_RESULT_MAX_BYTES)
            as usize,
        subagent_depth_max: agent
            .subagent_depth_max
            .or(cfg.subagent_depth_max)
            .unwrap_or(outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX),
        subagent_width_max: agent
            .subagent_width_max
            .or(cfg.subagent_width_max)
            .unwrap_or(outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX),
        image: agent.image.clone(),
    })
}

/// Resolve one concrete `[models.<name>]` row against the provider serving it.
///
/// Lifted out of [`resolve_agent_with_overrides`] whole when failover made the
/// resolution happen `N` times instead of once. Every error it raises names the
/// row it was resolving, so a chain's candidate reports the same text it would
/// have if the user had named it directly -- the property the static half's
/// `Unselectable` also exists to preserve.
fn resolve_candidate(
    cfg: &Config,
    agent: &outrig::config::Agent,
    model_name: &str,
    device_override: Option<MistralrsDeviceSpec>,
) -> Result<ResolvedCandidate> {
    let model = cfg
        .models
        .get(model_name)
        .ok_or_else(|| LlmResolveError::UnknownModel {
            name: model_name.to_string(),
        })?;

    let Some(provider_name) = model.provider.as_deref() else {
        // A row that is neither shape. Validation rejects it on every load
        // path, so this is reachable only through a `Config` built or mutated
        // in code -- which is exactly the case the check above exists for. Say
        // what is wrong rather than looking up the empty string and blaming a
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

    // `--device` selects hardware for an in-process model, so it is a
    // mistralrs-only knob. Checking it once here rather than per remote arm
    // means a remote style added later cannot forget to reject it.
    if device_override.is_some() && !matches!(provider, LlmProvider::Mistralrs) {
        return Err(LlmResolveError::MistralrsDeviceOverrideUnsupported {
            model: model_name.to_string(),
            provider: provider_name.to_string(),
        }
        .into());
    }
    // Every remote style names its model the same way: the configured
    // identifier, falling back to the model's own name.
    let remote_identifier = || {
        model
            .identifier
            .clone()
            .unwrap_or_else(|| model_name.to_string())
    };

    let (resolved_provider, model_weights, model_identifier) = match provider {
        LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
            retry_budget_secs,
            ..
        } => (
            ResolvedProvider::OpenAi {
                base_url: base_url.clone(),
                api_key: api_key.resolve()?,
                request_timeout_secs: *request_timeout_secs,
                retry_budget_secs: retry_budget_secs.or(cfg.retry_budget_secs),
            },
            None,
            remote_identifier(),
        ),
        LlmProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
            retry_budget_secs,
            ..
        } => (
            ResolvedProvider::Anthropic {
                base_url: base_url.clone(),
                api_key: api_key.resolve()?,
                request_timeout_secs: *request_timeout_secs,
                retry_budget_secs: retry_budget_secs.or(cfg.retry_budget_secs),
            },
            None,
            remote_identifier(),
        ),
        LlmProvider::Mistralrs => {
            let device = match device_override {
                Some(device) => validate_mistralrs_device(model_name, device)?,
                None => parse_mistralrs_device(model_name, model.device.as_deref())?,
            };
            let weights = MistralrsWeights {
                model_id: model.model_id.clone(),
                model_path: model.model_path.clone(),
                model_file: model.model_file.clone(),
                revision: model.revision.clone(),
                context_length: model.context_length,
                device,
            };
            // For display: prefer the HF model-id, fall back to the GGUF
            // basename, then the model name. mistralrs's own `load()`
            // derives the same kind of identifier internally; this is for
            // banner / error messaging only.
            let identifier = weights
                .model_id
                .clone()
                .or_else(|| {
                    weights
                        .model_path
                        .as_deref()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| model_name.to_string());
            (ResolvedProvider::Mistralrs, Some(weights), identifier)
        }
        // `LlmProvider` is `#[non_exhaustive]` and lives in another crate, so
        // this match can never be exhaustive: a new style that forgets its arm
        // above lands here instead of failing to compile. There is no generic
        // way to reach a style this build has no client for, so say so rather
        // than guess at one.
        _ => {
            return Err(LlmResolveError::UnsupportedProvider {
                name: provider_name.to_string(),
            }
            .into());
        }
    };

    Ok(ResolvedCandidate {
        // The *concrete* row, never the alias. `LlmRegistry` is keyed on this
        // (`llm/registry.rs`), so an alias name reaching it loads the same
        // multi-gigabyte GGUF twice in one process. The name the caller asked
        // for rides in `ResolvedAgent::alias_name` instead.
        model_name: model_name.to_string(),
        model_identifier,
        provider_name: provider_name.to_string(),
        provider: resolved_provider,
        model_weights,
        // The agent's ceiling wins; the model's is the fallback. A model that
        // carries one covers every agent pointed at it, which is what an
        // Anthropic identifier rig does not recognize needs -- it has no
        // provider-side default and errors without a ceiling from somewhere.
        //
        // Per candidate because the fallback half is: two rows in one chain can
        // publish different ceilings, and the one that travels must be the one
        // belonging to the row actually being called.
        max_tokens: agent.max_tokens.or(model.max_tokens),
    })
}

fn parse_mistralrs_device(
    model_name: &str,
    device: Option<&str>,
) -> std::result::Result<MistralrsDeviceSpec, LlmResolveError> {
    let spec = match device {
        Some(value) => value
            .parse()
            .map_err(|_| LlmResolveError::MistralrsDeviceInvalid {
                model: model_name.to_string(),
                device: value.to_string(),
            })?,
        None => MistralrsDeviceSpec::Cpu,
    };
    if !cfg!(feature = "local-llm") {
        return Ok(spec);
    }

    validate_mistralrs_device(model_name, spec)
}

fn validate_mistralrs_device(
    model_name: &str,
    spec: MistralrsDeviceSpec,
) -> std::result::Result<MistralrsDeviceSpec, LlmResolveError> {
    if !cfg!(feature = "local-llm") {
        return Ok(spec);
    }

    match spec {
        MistralrsDeviceSpec::Cuda(_) if !cfg!(feature = "cuda") => {
            Err(LlmResolveError::MistralrsDeviceUnavailable {
                model: model_name.to_string(),
                device: spec.to_string(),
                feature: "cuda",
            })
        }
        MistralrsDeviceSpec::Metal if !cfg!(feature = "metal") => {
            Err(LlmResolveError::MistralrsDeviceUnavailable {
                model: model_name.to_string(),
                device: spec.to_string(),
                feature: "metal",
            })
        }
        _ => Ok(spec),
    }
}

/// Runtime-dispatched Rig agent. The OpenAi-backed, Anthropic-backed, and
/// mistralrs-backed `CompletionModel` impls produce concretely different
/// `Agent<M>` types (Rig's trait carries associated response and client types,
/// so a single concrete `RigAgent` can't carry them all). Callers (the agent
/// loop) match on the variant.
pub enum RigAgent {
    OpenAi {
        agent: rig::agent::Agent<
            retry::RetryingModel<
                rig::providers::openai::CompletionModel<retry::RetryingHttpClient>,
            >,
        >,
        tool_call_max: usize,
    },
    Anthropic {
        agent: rig::agent::Agent<
            retry::RetryingModel<
                rig::providers::anthropic::completion::CompletionModel<retry::RetryingHttpClient>,
            >,
        >,
        tool_call_max: usize,
    },
    #[cfg(feature = "local-llm")]
    Mistralrs {
        agent: rig::agent::Agent<crate::llm::mistralrs::MistralrsModel>,
        tool_call_max: usize,
    },
    /// A multi-candidate alias: one agent over a chain that moves between
    /// provider-equivalent rows when one fails mid-turn.
    ///
    /// A fourth variant rather than a wrapper around the three above, for the
    /// reason the enum exists at all -- the chain is its own concrete
    /// `CompletionModel`, and a chain spanning an OpenAI row and an Anthropic
    /// one is neither of those variants. A single-candidate alias never lands
    /// here; it builds the variant its provider always built.
    Failover {
        agent: rig::agent::Agent<failover::FailoverModel>,
        tool_call_max: usize,
    },
}

/// Build a Rig `Agent` ready to receive a turn. Preamble, sampling params,
/// and the dynamic-tool list come from `resolved` plus the caller-supplied
/// MCP-backed adapters. The `cache_root` argument is the directory into
/// which the mistralrs HF-download path stages model files; it's ignored
/// for remote providers.
///
/// The function is async because the mistralrs arm has to load (and on
/// first use, download) a multi-gigabyte model. The remote arms do no I/O:
/// they build an HTTP client and hand it to Rig.
/// The retry policy every remote provider gets, from the provider's
/// `retry-budget-secs` or [`DEFAULT_RETRY_BUDGET_SECS`]. Both retry layers take
/// the same one, so `retry-budget-secs = 0` switches off both.
///
/// `retry-budget-secs` is not the whole policy: the default carries a second,
/// much shorter bound for a call that never reaches the endpoint, which config
/// cannot reach. It never widens this one -- the loop applies whichever is
/// smaller -- so `0` still means no retries anywhere. See
/// [`RetryPolicy::connect_budget`].
///
/// [`RetryPolicy::connect_budget`]: retry::RetryPolicy::connect_budget
///
/// [`DEFAULT_RETRY_BUDGET_SECS`]: outrig::config::DEFAULT_RETRY_BUDGET_SECS
fn retry_policy(retry_budget_secs: Option<u64>) -> retry::RetryPolicy {
    retry::RetryPolicy {
        budget: std::time::Duration::from_secs(
            retry_budget_secs.unwrap_or(outrig::config::DEFAULT_RETRY_BUDGET_SECS),
        ),
        ..retry::RetryPolicy::default()
    }
}

/// The HTTP client every remote provider gets: one per-request timeout, from
/// the provider's `request-timeout-secs` or [`DEFAULT_REQUEST_TIMEOUT_SECS`],
/// wrapped in the transient-retry loop `policy` bounds. Shared so neither
/// default can drift between the styles.
fn remote_http_client(
    request_timeout_secs: Option<u64>,
    policy: retry::RetryPolicy,
) -> Result<retry::RetryingHttpClient> {
    let timeout =
        std::time::Duration::from_secs(request_timeout_secs.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));
    // Two ceilings, on two different things. `timeout` bounds the whole
    // request, and is high because a non-streaming completion answers only
    // when the model has finished. `connect_timeout` bounds getting connected
    // at all, which no completion is waiting on -- without it, a host that
    // silently drops packets would hold an attempt open for the full request
    // timeout, and the retry loop's much shorter budget for an endpoint that
    // never answered could not stop it (see [`retry::CONNECT_TIMEOUT`]).
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(retry::CONNECT_TIMEOUT);

    // Unit tests point providers at loopback fixtures. reqwest picks up
    // `HTTP_PROXY` / `ALL_PROXY` automatically and has no loopback exemption of
    // its own, so on a machine behind a proxy every fixture request would be
    // sent to that proxy instead of the fixture -- leaking the request and
    // hanging the test rather than failing it usefully. Real runs keep
    // automatic proxy detection, which is how a user reaches a hosted provider
    // from behind one.
    #[cfg(test)]
    let builder = builder.no_proxy();

    let inner = builder
        .build()
        .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()))?;
    Ok(retry::RetryingHttpClient::new(inner, policy))
}

pub async fn build_agent(
    resolved: &ResolvedAgent,
    tools: Vec<SessionTool>,
    cache_root: &Path,
    #[cfg(feature = "local-llm")] registry: &Arc<LlmRegistry>,
) -> Result<RigAgent> {
    #[cfg(not(feature = "local-llm"))]
    let _ = cache_root;

    // A chain of one is not a special case: it takes the single-candidate path
    // below and produces exactly the `RigAgent` it always did, wrapper and all.
    // That is what keeps every no-alias session byte-for-byte what it was --
    // the same variant, the same retry stack, the same error text -- rather
    // than merely equivalent to it.
    if resolved.candidates.len() == 1 {
        return build_single(resolved, resolved.primary(), tools, cache_root,
            #[cfg(feature = "local-llm")]
            registry,
        )
        .await;
    }

    // One policy for the whole chain, so its `chain_deadline` is the same
    // handle in every candidate's HTTP client and model wrapper. Arming it once
    // per `completion()` call is what bounds the chain's worst case at one
    // budget rather than one per candidate.
    //
    // The budget itself comes from the first candidate's provider. A chain
    // spanning providers that disagree on `retry-budget-secs` has no single
    // right answer, and the head of a preference order is the defensible one --
    // it is the endpoint the user said to use.
    let policy = retry_policy(chain_retry_budget_secs(&resolved.candidates));

    let mut candidates: Vec<Box<dyn failover::Candidate>> =
        Vec::with_capacity(resolved.candidates.len());
    for candidate in &resolved.candidates {
        candidates.push(
            build_candidate(candidate, &policy, cache_root,
                #[cfg(feature = "local-llm")]
                registry,
            )
            .await?,
        );
    }

    Ok(RigAgent::Failover {
        agent: finish_agent(
            failover::FailoverModel::new(candidates, policy),
            resolved,
            // The chain rewrites `max_tokens` per candidate inside the call, so
            // the ceiling baked into the agent here is only the starting one.
            resolved.max_tokens(),
            tools,
        ),
        tool_call_max: resolved.tool_call_max,
    })
}

/// The `retry-budget-secs` governing a whole chain. See [`build_agent`].
///
/// The first *remote* candidate's, rather than the first candidate's.
/// `[providers.<name>] style = "mistralrs"` has no such key at all -- an
/// in-process model does no HTTP and so has nothing to retry -- so a local head
/// is skipped. Reading it anyway meant a local-first alias fell through to the
/// compiled 600 seconds and handed that to a remote fallback, ignoring a
/// `retry-budget-secs = 0` the user had set and the docs call the one "no
/// retries" knob.
///
/// The two `None`s are *not* the same and the nesting is what keeps them apart.
/// A local row contributes no `Option` and is skipped; a remote row whose
/// `retry_budget_secs` is `None` contributes `Some(None)` and **stops** the
/// search, because that `None` is an answer -- "the compiled default" -- and
/// not an absence. Flattening one level of it is deliberate: collapsing them
/// instead would let a later candidate's explicit override govern a preferred
/// remote candidate that had simply inherited the default.
///
/// So the rule the head-of-preference-order argument actually supports holds:
/// the earliest candidate that *can* answer decides. A chain of only local rows
/// yields `None`, harmlessly, because nothing in it retries over HTTP.
fn chain_retry_budget_secs(candidates: &[ResolvedCandidate]) -> Option<u64> {
    candidates
        .iter()
        .find_map(|candidate| match &candidate.provider {
            ResolvedProvider::OpenAi {
                retry_budget_secs, ..
            }
            | ResolvedProvider::Anthropic {
                retry_budget_secs, ..
            } => Some(*retry_budget_secs),
            ResolvedProvider::Mistralrs => None,
        })
        .flatten()
}

/// Build the one-candidate agent: the pre-failover path, unchanged.
async fn build_single(
    resolved: &ResolvedAgent,
    candidate: &ResolvedCandidate,
    tools: Vec<SessionTool>,
    cache_root: &Path,
    #[cfg(feature = "local-llm")] registry: &Arc<LlmRegistry>,
) -> Result<RigAgent> {
    #[cfg(not(feature = "local-llm"))]
    let _ = cache_root;
    match &candidate.provider {
        ResolvedProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
            retry_budget_secs,
        } => {
            use rig::client::CompletionClient;

            let policy = retry_policy(*retry_budget_secs);
            let http = remote_http_client(*request_timeout_secs, policy.clone())?;
            let client = openai_client(base_url, api_key, http)?;
            let model = client.completion_model(&candidate.model_identifier);
            Ok(RigAgent::OpenAi {
                agent: finish_agent(
                    retry::RetryingModel::new(model, policy),
                    resolved,
                    candidate.max_tokens,
                    tools,
                ),
                tool_call_max: resolved.tool_call_max,
            })
        }
        ResolvedProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
            retry_budget_secs,
        } => {
            let policy = retry_policy(*retry_budget_secs);
            let http = remote_http_client(*request_timeout_secs, policy.clone())?;
            let client = anthropic_client(base_url, api_key, http)?;
            let (model, max_tokens) = anthropic_model(&client, candidate);
            Ok(RigAgent::Anthropic {
                agent: finish_agent(
                    retry::RetryingModel::new(model, policy),
                    resolved,
                    max_tokens,
                    tools,
                ),
                tool_call_max: resolved.tool_call_max,
            })
        }
        ResolvedProvider::Mistralrs => {
            #[cfg(not(feature = "local-llm"))]
            {
                Err(LlmResolveError::MistralrsFeatureDisabled {
                    name: candidate.provider_name.clone(),
                }
                .into())
            }
            #[cfg(feature = "local-llm")]
            {
                let model = mistralrs_model(candidate, cache_root, registry).await?;
                Ok(RigAgent::Mistralrs {
                    agent: finish_agent((*model).clone(), resolved, candidate.max_tokens, tools),
                    tool_call_max: resolved.tool_call_max,
                })
            }
        }
    }
}

/// Build one link of a chain: the same client and the same retry stack
/// `build_single` builds, behind the object-safe shim a chain holds.
///
/// Every candidate takes the *chain's* policy rather than its own provider's,
/// so they share one deadline. The rest -- the client, the ceiling precedence,
/// the Anthropic cap -- is per candidate and identical to the single path,
/// which is what makes a move land on a correctly-built model rather than a
/// simplified one.
async fn build_candidate(
    candidate: &ResolvedCandidate,
    policy: &retry::RetryPolicy,
    cache_root: &Path,
    #[cfg(feature = "local-llm")] registry: &Arc<LlmRegistry>,
) -> Result<Box<dyn failover::Candidate>> {
    #[cfg(not(feature = "local-llm"))]
    let _ = cache_root;
    match &candidate.provider {
        ResolvedProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } => {
            use rig::client::CompletionClient;

            let http = remote_http_client(*request_timeout_secs, policy.clone())?;
            let client = openai_client(base_url, api_key, http)?;
            let model = client.completion_model(&candidate.model_identifier);
            Ok(Box::new(failover::ModelCandidate::new(
                retry::RetryingModel::new(model, policy.clone()),
                &candidate.model_name,
                &candidate.model_identifier,
                candidate.max_tokens,
            )))
        }
        ResolvedProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } => {
            let http = remote_http_client(*request_timeout_secs, policy.clone())?;
            let client = anthropic_client(base_url, api_key, http)?;
            let (model, max_tokens) = anthropic_model(&client, candidate);
            Ok(Box::new(failover::ModelCandidate::new(
                retry::RetryingModel::new(model, policy.clone()),
                &candidate.model_name,
                &candidate.model_identifier,
                max_tokens,
            )))
        }
        ResolvedProvider::Mistralrs => {
            #[cfg(not(feature = "local-llm"))]
            {
                Err(LlmResolveError::MistralrsFeatureDisabled {
                    name: candidate.provider_name.clone(),
                }
                .into())
            }
            #[cfg(feature = "local-llm")]
            {
                // Deliberately *not* loaded here -- see `LazyLocalCandidate`.
                Ok(Box::new(LazyLocalCandidate {
                    registry: Arc::clone(registry),
                    candidate: candidate.clone(),
                    cache_root: cache_root.to_path_buf(),
                }))
            }
        }
    }
}

/// A chain candidate whose weights load the first time the chain reaches it.
///
/// Every other candidate is built eagerly, and design fork §6 is right that
/// this costs nothing: constructing a remote client does no I/O, so building
/// three of them is free and a move pays no build cost mid-turn. A mistralrs
/// row breaks that premise twice over. Its "construction" is a multi-gigabyte
/// load, and unlike a remote client it can *fail* -- missing weights, a corrupt
/// file, no room on the device.
///
/// Eagerly, both land on the wrong session. A fallback whose weights are
/// missing takes down a session whose hosted primary is perfectly healthy,
/// which inverts the reason for naming a fallback at all: the chain exists to
/// make an outage survivable, and it would instead make a *second* model a new
/// way to fail to start. So the load happens when the candidate is actually
/// reached, and a failure there is that candidate's failure, which the chain
/// reports and moves past like any other.
///
/// The first-use wait is announced by the load path itself, which is the
/// mitigation `plan/done/0101-subagent-model-selection.md`'s decision 7 already
/// built for exactly this surprise.
#[cfg(feature = "local-llm")]
struct LazyLocalCandidate {
    registry: Arc<LlmRegistry>,
    candidate: ResolvedCandidate,
    cache_root: PathBuf,
}

#[cfg(feature = "local-llm")]
impl failover::Candidate for LazyLocalCandidate {
    fn model_name(&self) -> &str {
        &self.candidate.model_name
    }

    fn model_identifier(&self) -> &str {
        &self.candidate.model_identifier
    }

    fn max_tokens(&self) -> Option<u64> {
        self.candidate.max_tokens.map(u64::from)
    }

    /// Answered without loading, which is what makes the laziness possible at
    /// all: `FailoverModel::new` ANDs this across the chain at build time.
    ///
    /// Sound because it is a property of the *type*, not of the loaded weights:
    /// `MistralrsModel` does not override rig's `false` default. That is an
    /// assumption rather than something the compiler holds, so it is stated
    /// here -- if the in-process model ever does compose native structured
    /// output with tools, this hardcoded answer becomes wrong for every chain
    /// naming a local row, and the conservative direction is the safe one to be
    /// wrong in (a chain that says `false` loses guaranteed structured output;
    /// one that wrongly says `true` promises what a candidate cannot honor).
    fn composes_native_output_with_tools(&self) -> bool {
        false
    }

    fn completion(
        &self,
        request: rig::completion::CompletionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<
                        rig::completion::CompletionResponse<()>,
                        rig::completion::CompletionError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            // The wait this laziness moved here. Deferring the load is only
            // defensible if the surprise moves with it: reaching this candidate
            // can mean minutes of downloading or mapping weights, in the middle
            // of a turn, and the move line above says only that the chain moved
            // on -- which reads as "and the next one is answering now". The
            // wording is the subagent path's, because it is the same wait.
            if !self.registry.is_loaded(&self.candidate.model_name) {
                eprintln!(
                    "[outrig] loading in-process model {} (first use; this may take \
                     several minutes)",
                    self.candidate.model_name,
                );
            }
            let model = mistralrs_model(&self.candidate, &self.cache_root, &self.registry)
                .await
                // A load failure is this candidate's failure, not the session's.
                // Terminal rather than transient: a missing GGUF is still missing
                // on a resend, so a chain of only these ends the process instead
                // of advising a retry that cannot help.
                .map_err(|e| rig::completion::CompletionError::ProviderError(e.to_string()))?;
            let response =
                rig::completion::CompletionModel::completion(&*model, request).await?;
            Ok(rig::completion::CompletionResponse {
                choice: response.choice,
                usage: response.usage,
                raw_response: (),
                message_id: response.message_id,
            })
        })
    }
}

/// Rig's OpenAI-compatible client for one candidate.
///
/// Shared by the single path and the chain for the same reason its Anthropic
/// sibling is: a candidate built by a simplified copy of this is how a chain
/// would quietly stop talking to the endpoint the same way a lone model does.
fn openai_client(
    base_url: &str,
    api_key: &str,
    http: retry::RetryingHttpClient,
) -> Result<rig::providers::openai::CompletionsClient<retry::RetryingHttpClient>> {
    use rig::providers::openai::CompletionsClient;

    CompletionsClient::builder()
        .api_key(api_key.to_string())
        .base_url(base_url)
        .http_client(http)
        .build()
        .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()).into())
}

/// Rig's Anthropic client for one candidate.
///
/// Rig's client owns the protocol: `x-api-key`, the `anthropic-version` header,
/// `POST {base-url}/v1/messages`, and the native content blocks. It also
/// normalizes a trailing `/v1` or `/messages` off the configured base URL.
fn anthropic_client(
    base_url: &str,
    api_key: &str,
    http: retry::RetryingHttpClient,
) -> Result<rig::providers::anthropic::Client<retry::RetryingHttpClient>> {
    use rig::providers::anthropic;

    anthropic::Client::builder()
        .api_key(api_key.to_string())
        .base_url(base_url)
        .http_client(http)
        .build()
        .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()).into())
}

/// One candidate's Anthropic model, and the output-token ceiling that reaches
/// the wire with it.
///
/// Shared by the single path and the chain so the three-tier precedence is
/// stated once. Under a chain it runs per candidate, which is what makes the
/// ceiling travel with the identifier instead of staying candidate one's.
fn anthropic_model(
    client: &rig::providers::anthropic::Client<retry::RetryingHttpClient>,
    candidate: &ResolvedCandidate,
) -> (
    rig::providers::anthropic::completion::CompletionModel<retry::RetryingHttpClient>,
    Option<u32>,
) {
    use rig::client::CompletionClient;

    // `completion_model`, never `CompletionModel::with_model`: the two
    // disagree about a model identifier rig does not recognize. This one
    // leaves the default unset, which is the signal the fallback below keys
    // off. `with_model` would have already capped every reply at 2048 tokens,
    // silently and without outrig ever seeing that it happened.
    // `tests/anthropic_mock.rs` pins the difference.
    let mut model = client.completion_model(&candidate.model_identifier);
    // Precedence, highest first: the agent's or model's `max-tokens` (already
    // merged into the candidate by `resolve_candidate`), then rig's published
    // ceiling for an identifier it recognizes, then ours. Anthropic rejects a
    // request carrying no ceiling at all, so the last tier has to exist;
    // filling it *silently* is the failure mode the warning exists to prevent.
    if candidate.max_tokens.is_none() && model.default_max_tokens.is_none() {
        warn_fallback_ceiling(candidate);
        model.default_max_tokens = Some(ANTHROPIC_FALLBACK_MAX_TOKENS);
    }
    // Tier 2 is also a cap, not only a default. A configured ceiling above what
    // this identifier can serve is a request the API refuses outright, so the
    // whole turn fails rather than being cut short -- lowering it is the only
    // outcome that runs at all, and the number is not invented, it is what the
    // model publishes. Only reachable for an identifier rig recognizes; for one
    // it does not there is no ceiling to compare against, and outrig guesses
    // none.
    let max_tokens = match (candidate.max_tokens, model.default_max_tokens) {
        (Some(want), Some(ceiling)) => Some(u64::from(want).min(ceiling) as u32),
        _ => candidate.max_tokens,
    };
    (model, max_tokens)
}

/// One candidate's in-process model, from the registry that keys them by
/// concrete name -- so two candidates naming one model share a loaded engine.
#[cfg(feature = "local-llm")]
async fn mistralrs_model(
    candidate: &ResolvedCandidate,
    cache_root: &Path,
    registry: &LlmRegistry,
) -> Result<std::sync::Arc<crate::llm::mistralrs::MistralrsModel>> {
    let weights =
        candidate
            .model_weights
            .as_ref()
            .ok_or_else(|| LlmResolveError::MistralrsLoad {
                model: candidate.model_name.clone(),
                source: anyhow::anyhow!("internal: resolved mistralrs agent has no model_weights"),
            })?;
    let model_name = candidate.model_name.as_str();
    let model_id = weights.model_id.as_deref();
    let model_path = weights.model_path.as_deref();
    let model_file = weights.model_file.as_deref();
    let revision = weights.revision.as_deref();
    let context_length = weights.context_length;
    let device = weights.device;
    registry
        .get_or_init(model_name, || async move {
            crate::llm::mistralrs::load(
                model_name,
                model_id,
                model_path,
                model_file,
                revision,
                context_length,
                device,
                cache_root,
            )
            .await
        })
        .await
}

/// Say, once per model, that outrig picked an output-token ceiling nobody asked
/// for.
///
/// The whole hazard of a fallback ceiling is that a reply cut off at it looks
/// like a bad model rather than a config gap, so the operator hears about it
/// before the first turn. Not once per *build*: `build_agent` runs again on
/// `/sidecar add` and once more per subagent launch, and a fan-out of subagents
/// repeating an identical line would bury the traces around it.
///
/// Keyed per concrete model rather than by a process-wide `Once`, which is what
/// failover asks of it: a chain builds several candidates, and a `Once` would
/// spend the warning on the first and stay silent about a second candidate with
/// a different published ceiling -- exactly the case where a reply cut short
/// reads as the model's doing. The name is the concrete row, so two agents on
/// one model still warn once between them.
fn warn_fallback_ceiling(candidate: &ResolvedCandidate) {
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    // The guard drops with the condition's temporary, so the warning below is
    // printed unlocked.
    if !WARNED
        .lock()
        .expect("fallback ceiling warnings")
        .insert(candidate.model_name.clone())
    {
        return;
    }
    eprintln!(
        "[outrig] {} has no published output-token ceiling in this build, so turns \
         are capped at {ANTHROPIC_FALLBACK_MAX_TOKENS}. Set [models.{}].max-tokens \
         (or the agent's) to choose your own.",
        candidate.model_identifier, candidate.model_name,
    );
}

impl RigAgent {
    /// Run one user turn: prompt the model, drive the model->tool->model loop,
    /// extend `history` with everything emitted (user prompt + tool turns +
    /// final assistant reply), and return the assistant's text reply.
    ///
    /// The per-turn [`OutrigPromptHook`] prints `[outrig] tool call: ...` to
    /// stderr for every tool invocation and terminates the loop after
    /// the resolved tool-call max. If the hook terminates the loop, Rig
    /// returns the partial chat history it had accumulated; outrig splices in
    /// that new suffix so the user can send a follow-up prompt to continue.
    pub async fn run_turn(&self, prompt: &str, history: &mut Vec<Message>) -> Result<String> {
        match self {
            RigAgent::OpenAi {
                agent,
                tool_call_max,
            } => run_turn_inner(
                agent,
                prompt,
                history,
                OutrigPromptHook::new(*tool_call_max),
            )
            .await
            .map(|end| end.reply),
            RigAgent::Anthropic {
                agent,
                tool_call_max,
            } => run_turn_inner(
                agent,
                prompt,
                history,
                OutrigPromptHook::new(*tool_call_max),
            )
            .await
            .map(|end| end.reply),
            RigAgent::Failover {
                agent,
                tool_call_max,
            } => run_turn_inner(
                agent,
                prompt,
                history,
                OutrigPromptHook::new(*tool_call_max),
            )
            .await
            .map(|end| end.reply),
            #[cfg(feature = "local-llm")]
            RigAgent::Mistralrs {
                agent,
                tool_call_max,
            } => {
                let mut stdout = tokio::io::stdout();
                run_turn_streaming_to(
                    agent,
                    prompt,
                    history,
                    OutrigPromptHook::new(*tool_call_max),
                    &mut stdout,
                )
                .await
            }
        }
    }

    /// Run one round for a subagent: nothing reaches stdout, traces carry
    /// `label`, and each model call picks up whatever the parent has queued
    /// through `injections`.
    ///
    /// Subagent outcomes come from `outrig__set_result`, not from this return
    /// value -- the text is for the transcript log, and
    /// [`TurnEnd::stopped`] is the cause the round driver passes on to the
    /// parent when nothing was published.
    pub async fn run_turn_captured(
        &self,
        prompt: &str,
        history: &mut Vec<Message>,
        label: &str,
        injections: InjectionSource,
    ) -> Result<TurnEnd> {
        match self {
            RigAgent::OpenAi {
                agent,
                tool_call_max,
            } => {
                let hook = OutrigPromptHook::for_subagent(*tool_call_max, label, injections);
                run_turn_inner(agent, prompt, history, hook).await
            }
            RigAgent::Anthropic {
                agent,
                tool_call_max,
            } => {
                let hook = OutrigPromptHook::for_subagent(*tool_call_max, label, injections);
                run_turn_inner(agent, prompt, history, hook).await
            }
            RigAgent::Failover {
                agent,
                tool_call_max,
            } => {
                let hook = OutrigPromptHook::for_subagent(*tool_call_max, label, injections);
                run_turn_inner(agent, prompt, history, hook).await
            }
            #[cfg(feature = "local-llm")]
            RigAgent::Mistralrs {
                agent,
                tool_call_max,
            } => {
                let hook = OutrigPromptHook::for_subagent(*tool_call_max, label, injections);
                let mut sink = Vec::new();
                run_turn_streaming_inner(agent, prompt, history, hook, &mut sink).await
            }
        }
    }
}

/// A [`RigAgent`] plus the recipe to rebuild it when `/sidecar add` grows
/// the tool list mid-session. The rig agent's toolset is frozen at build
/// time, so [`RebuildingAgent::extend_tools`] only marks the agent stale;
/// the next [`RebuildingAgent::run_turn`] rebuilds over the full list
/// (a remote arm builds a fresh HTTP client, so the first turn after a
/// rebuild re-handshakes rather than reusing the pooled connection; mistralrs
/// model loads are registry-cached).
///
/// Interior mutability (`RefCell`/`Cell`) because the wrapper is shared by
/// `&` between the REPL's prompt path and its `/sidecar` command; the
/// binary's runtime is current-thread and callbacks run sequentially, so
/// borrows never overlap and none is held across an await.
pub struct RebuildingAgent {
    resolved: ResolvedAgent,
    cache_root: PathBuf,
    #[cfg(feature = "local-llm")]
    registry: Arc<LlmRegistry>,
    tools: RefCell<Vec<SessionTool>>,
    dirty: Cell<bool>,
    // The agent rides in an inner `Rc` so a turn clones it out and never
    // holds the `RefCell` borrow across the run_turn await (a rebuild in a
    // later turn just swaps the Rc).
    agent: RefCell<Rc<RigAgent>>,
}

impl RebuildingAgent {
    /// Wrap an already-built agent. The first build stays with the caller
    /// so its progress reporting (and any model download) happens there.
    pub fn new(
        agent: RigAgent,
        tools: Vec<SessionTool>,
        resolved: ResolvedAgent,
        cache_root: PathBuf,
        #[cfg(feature = "local-llm")] registry: Arc<LlmRegistry>,
    ) -> Self {
        Self {
            resolved,
            cache_root,
            #[cfg(feature = "local-llm")]
            registry,
            tools: RefCell::new(tools),
            dirty: Cell::new(false),
            agent: RefCell::new(Rc::new(agent)),
        }
    }

    /// Append tool adapters and mark the agent stale; the next
    /// [`RebuildingAgent::run_turn`] rebuilds over the extended list.
    pub fn extend_tools(&self, new: Vec<SessionTool>) {
        self.tools.borrow_mut().extend(new);
        self.dirty.set(true);
    }

    /// Snapshot view of the current tool list. Do not hold across an await.
    pub fn tools(&self) -> Ref<'_, [SessionTool]> {
        Ref::map(self.tools.borrow(), Vec::as_slice)
    }

    /// The per-result byte ceiling the agent was resolved with; adapters
    /// built for tools added mid-session use the same limit.
    pub fn tool_result_max_bytes(&self) -> usize {
        self.resolved.tool_result_max_bytes
    }

    /// Rebuild the agent if the tool list grew since the last turn, then
    /// delegate to [`RigAgent::run_turn`].
    pub async fn run_turn(&self, prompt: &str, history: &mut Vec<Message>) -> Result<String> {
        if self.dirty.get() {
            let tools_snapshot = self.tools.borrow().clone();
            let rebuilt = build_agent(
                &self.resolved,
                tools_snapshot,
                &self.cache_root,
                #[cfg(feature = "local-llm")]
                &self.registry,
            )
            .await?;
            *self.agent.borrow_mut() = Rc::new(rebuilt);
            self.dirty.set(false);
        }
        let turn_agent = self.agent.borrow().clone();
        turn_agent.run_turn(prompt, history).await
    }
}

/// How a turn ended, for callers that need more than the text.
///
/// The agent loop has always known the difference between "the model finished"
/// and "we cut it off", but [`handle_prompt_error`] used to flatten both into
/// the same string. A subagent round needs the distinction: a round stopped
/// part-way published nothing *because* it was stopped, and telling its parent
/// only that it "stopped without calling outrig__set_result" names the symptom
/// and drops the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEnd {
    /// The assistant's closing text.
    pub reply: String,
    /// Why the loop was cut short, when it was. `None` means the model
    /// finished on its own.
    pub stopped: Option<TurnStop>,
}

/// Why a turn stopped short of the model finishing.
///
/// Both variants are recoverable and the REPL treats them alike -- end the
/// turn, keep the session. They part ways at a subagent round, which is why
/// this is an enum rather than a reason string plus a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStop {
    /// The tool-call budget ran out, or a hook stopped the loop. Whatever the
    /// turn managed is spliced into the history, so it can be continued.
    Interrupted(String),
    /// The LLM endpoint stayed transiently broken -- rate-limited, or
    /// unreachable -- for as long as the retry budget allowed. Nothing was
    /// spliced, so the prompt itself is what wants resending. A subagent round
    /// publishes this as a *failed* round rather than a quiet stop: an
    /// unreachable endpoint is infrastructure for the parent to act on, not a
    /// report the model declined to write.
    EndpointFailed(String),
}

impl TurnStop {
    /// The reason, for display.
    ///
    /// Exercised only by this module's own tests today -- callers so far have
    /// matched the variant directly to get an owned `String` -- but it stays a
    /// real inherent method (not a test helper) because it is the read-only
    /// counterpart callers that only need the text, not the variant, should
    /// reach for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reason(&self) -> &str {
        match self {
            Self::Interrupted(reason) | Self::EndpointFailed(reason) => reason,
        }
    }
}

async fn run_turn_inner<M: CompletionModel + 'static>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    hook: OutrigPromptHook,
) -> Result<TurnEnd> {
    let max_turns = hook.max + 2;
    // Cloned rather than moved: the clone shares the hook's atomics, so it can
    // still be asked afterwards whether the stop was OutRig's doing.
    let observer = hook.clone();
    let result = agent
        .prompt(prompt.to_string())
        .history(history.clone())
        .max_turns(max_turns)
        .add_hook(hook)
        .extended_details()
        .await;

    match result {
        Ok(response) => {
            let messages = response
                .messages
                .expect("rig populates messages on extended_details");
            history.extend(messages);
            Ok(TurnEnd {
                reply: response.output,
                // A hook stop normally surfaces as an error, but reading the
                // reason back unconditionally means a stop can never be lost to
                // a path that ends the run cleanly instead.
                stopped: observer.stop_reason().map(TurnStop::Interrupted),
            })
        }
        Err(other) => handle_prompt_error(other, history, &observer),
    }
}

/// The primary agent's streaming path. Returns the empty string on purpose:
/// the reply already reached `sink` chunk by chunk while decoding, so handing
/// it back would make the REPL print it a second time. Discarding here -- and
/// not inside [`run_turn_streaming_inner`] -- is what lets subagents reuse the
/// same loop and actually receive the text.
///
/// `sink` is a parameter rather than a captured `tokio::io::stdout()` purely so
/// that suppression is testable: it is a one-line behavior the REPL's
/// print-if-non-empty depends on, and a refactor could otherwise drop it
/// silently.
#[cfg(feature = "local-llm")]
async fn run_turn_streaming_to<M, W>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    hook: OutrigPromptHook,
    sink: &mut W,
) -> Result<String>
where
    M: CompletionModel + 'static,
    W: AsyncWrite + Unpin,
{
    run_turn_streaming_inner(agent, prompt, history, hook, sink).await?;
    Ok(String::new())
}

#[cfg(feature = "local-llm")]
fn handle_streaming_error(
    err: StreamingError,
    history: &mut Vec<Message>,
    hook: &OutrigPromptHook,
) -> Result<TurnEnd> {
    let prompt_error = match err {
        StreamingError::Completion(err) => rig::completion::PromptError::CompletionError(err),
        StreamingError::Prompt(err) => *err,
        StreamingError::Tool(err) => rig::completion::PromptError::ToolError(err),
    };
    handle_prompt_error(prompt_error, history, hook)
}

#[cfg(feature = "local-llm")]
/// Drives the streaming loop, writing decoded text to `stdout` and returning
/// it. Callers choose the sink: the primary agent passes real stdout, a
/// subagent passes a buffer, since only the primary's reply may reach stdout.
async fn run_turn_streaming_inner<M, W>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    hook: OutrigPromptHook,
    stdout: &mut W,
) -> Result<TurnEnd>
where
    M: CompletionModel + 'static,
    W: AsyncWrite + Unpin,
{
    let max_turns = hook.max + 2;
    // See `run_turn_inner`: the clone shares the hook's atomics so the stop
    // reason survives the move into rig.
    let observer = hook.clone();
    let mut stream = agent
        .stream_chat(prompt.to_string(), history.clone())
        .max_turns(max_turns)
        .add_hook(hook)
        .await;

    let mut streamed_reply = String::new();
    let mut final_history: Option<Vec<Message>> = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                stdout.write_all(text.text.as_bytes()).await?;
                stdout.flush().await?;
                streamed_reply.push_str(&text.text);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                stdout.flush().await?;
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_history = response.messages().map(|messages| messages.to_vec());
            }
            Ok(_) => {}
            Err(err) => {
                return handle_streaming_error(err, history, &observer);
            }
        }
    }

    if let Some(messages) = final_history {
        extend_history_with_new_suffix(history, messages);
    }

    if !streamed_reply.is_empty() && !streamed_reply.ends_with('\n') {
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    // Not just `finished`: a stream that ends without yielding an error can
    // still have been cut short by the hook, and this is the only place that
    // would notice.
    Ok(TurnEnd {
        reply: streamed_reply,
        stopped: observer.stop_reason().map(TurnStop::Interrupted),
    })
}

/// Turn a loop-ending error into a [`TurnEnd`], or pass it on.
///
/// `hook` is the turn's own hook, consulted to tell OutRig's deliberate stops
/// from a cancellation rig raised for itself. Both arrive as
/// `PromptError::PromptCancelled` and are indistinguishable by shape, so
/// without this an internal fault -- a driver protocol violation, say -- would
/// be reported to a subagent's parent as an ordinary "stopped before
/// reporting", which reads like the model's doing and hides a bug.
///
/// The remaining two recoverable cases are both the endpoint's doing: one that
/// stayed transiently broken -- rate-limited, or unreachable -- for the whole
/// retry budget, and one that answered with a body rig could not turn into a
/// completion. Either used to end the process: the error reached `repl.rs`'s
/// `res?` and unwound past the REPL loop, tearing down the containers and
/// dropping the conversation. They end the *turn* instead, so the user can wait
/// out the window and send the prompt again in the same session.
/// End the *turn* -- not the session -- because the endpoint failed.
///
/// The shared tail of [`handle_prompt_error`]'s three endpoint arms: a chain
/// that exhausted every candidate, one endpoint that stayed transiently broken
/// for its whole budget, and a response rig could not use. They differ in
/// `reason`, and the chain additionally prints its per-candidate report through
/// `tried`; everything after that is common, because all three leave the
/// history untouched.
///
/// The advice is deliberately not the "continue" advice the truncation paths
/// give: nothing was appended, so there is no partial turn to continue -- the
/// prompt itself is what wants resending. No budget is named, because with
/// `retry-budget-secs = 0` there was none; the retry progress lines above name
/// it whenever there was one.
fn endpoint_failed(reason: String, tried: Option<&str>) -> Result<TurnEnd> {
    eprintln!("[outrig] {reason}; ending turn");
    if let Some(tried) = tried {
        eprintln!("[outrig] tried:\n{tried}");
    }
    eprintln!(
        "[outrig] history unchanged -- send the prompt again to retry, \
         or \"/quit\" to stop."
    );
    Ok(TurnEnd {
        // The model never spoke, so nothing belongs on stdout. `repl.rs`'s
        // `if !reply.is_empty()` guard handles it.
        reply: String::new(),
        stopped: Some(TurnStop::EndpointFailed(reason)),
    })
}

fn handle_prompt_error(
    err: rig::completion::PromptError,
    history: &mut Vec<Message>,
    hook: &OutrigPromptHook,
) -> Result<TurnEnd> {
    // A whole chain gave up, which is a different claim from either of the two
    // below: `exhausted_transient_label`'s wording is about *one* endpoint that
    // stayed broken for its budget, and under a chain that sentence would be
    // false. The three do not depend on this order to stay apart -- they are
    // disjoint by `CompletionError` variant, and what keeps them so is a
    // property of `FailoverModel` rather than of the sequence here: a chain
    // aggregates its candidates into a `ProviderError` instead of letting any
    // one candidate's `HttpError` escape. The single-candidate paths below
    // therefore keep the exact text they have always had.
    // A whole chain gave up. Whether that ends the turn or the process is the
    // question a single candidate's failure already answers, asked of all of
    // them: if even one failed for a reason a resend could fix -- a rate limit,
    // an unusable body -- then waiting and resending is worth advising. If
    // every one was terminal, a resend is futile and this has to end the
    // process exactly as the same failure on a lone candidate does. A revoked
    // key on all three vendors is not an outage to wait out.
    if let Some(chain) = failover::chain_exhausted_label(&err) {
        let recoverable = chain.recoverable;
        let tried = chain.tried.to_string();
        if !recoverable {
            // The message already carries every candidate and its reason, so
            // propagating it names them all without re-rendering.
            return Err(err.into());
        }
        return endpoint_failed("every model candidate failed".to_string(), Some(&tried));
    }

    // Handled ahead of the match because it shares almost nothing with the
    // other two: no history to splice, no reply to print, and different advice.
    if let Some(label) = retry::exhausted_transient_label(&err) {
        // `PromptError::CompletionError` carries no `chat_history`, so a turn
        // that died on a *later* model call loses the tool calls it already
        // ran. Filed as
        // `plan/next/partial-turn-history-on-failed-model-call.md`.
        return endpoint_failed(
            format!("LLM endpoint failed and did not recover ({label})"),
            None,
        );
    }

    // A response rig could not use, still unusable after `RetryingModel` spent
    // its attempts. Handled the same way and for the same reasons: nothing was
    // appended, and the prompt is what wants resending.
    if let Some(detail) = retry::unusable_response_label(&err) {
        return endpoint_failed(
            format!("the model returned a response outrig could not use ({detail})"),
            None,
        );
    }

    let (reason, chat_history) = match err {
        rig::completion::PromptError::PromptCancelled {
            reason,
            chat_history,
        } => match hook.stop_reason() {
            Some(ours) => (ours, chat_history),
            // Nothing in OutRig asked for this, so it is rig's own. Propagating
            // keeps it an error: the round driver publishes it as a failed
            // round, which names it as a fault rather than a tidy stop.
            None => {
                return Err(rig::completion::PromptError::PromptCancelled {
                    reason,
                    chat_history,
                }
                .into());
            }
        },
        rig::completion::PromptError::MaxTurnsError {
            max_turns,
            chat_history,
            ..
        } => (
            format!("tool-call iteration max ({max_turns}) reached"),
            *chat_history,
        ),
        other => return Err(other.into()),
    };
    eprintln!("[outrig] {reason}; ending turn");
    eprintln!(
        "[outrig] partial history retained -- send another prompt \
         (e.g. \"continue\") to keep going, or \"/reset\" to drop it."
    );
    extend_history_with_new_suffix(history, chat_history);
    Ok(TurnEnd {
        reply: format!("(turn ended: {reason})"),
        stopped: Some(TurnStop::Interrupted(reason)),
    })
}

fn extend_history_with_new_suffix(history: &mut Vec<Message>, returned: Vec<Message>) {
    let existing_len = history.len();
    if returned.len() >= existing_len && returned[..existing_len] == history[..] {
        history.extend(returned.into_iter().skip(existing_len));
    } else {
        history.extend(returned);
    }
}

/// Supplies messages to splice into a turn's history at its next model call.
///
/// A closure rather than a concrete type so this module stays unaware of the
/// subagent registry: `subagent` hands one in that reads its queued steers.
/// Called on *every* model call in the turn, because rig's `RequestPatch` is
/// per-turn and non-sticky -- a steer applied once would vanish from the next
/// call.
pub type InjectionSource = Arc<dyn Fn() -> Vec<Message> + Send + Sync>;

/// Consecutive identical failures before the model is told that repeating the
/// call is pointless.
const REPEAT_NUDGE_AT: u32 = 2;

/// Consecutive identical failures before the round is ended outright.
///
/// A nudge the model ignores twice is a nudge it is not reading. Past that the
/// remaining tool-call budget buys nothing, and ending the round hands the
/// parent its (failed) outcome now rather than after the budget drains.
const REPEAT_TERMINATE_AT: u32 = 4;

/// What to do about a tool call that keeps failing the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatVerdict {
    /// Nothing unusual -- first failure, a different call, or a success.
    Allow,
    /// Append a note to the model-visible result saying repetition will not
    /// help.
    Nudge,
    /// End the round.
    Stop,
}

/// Counts consecutive identical failing tool calls within one round.
///
/// A model that cannot act on a tool error tends to re-emit the identical call
/// rather than try something else, and every repeat costs a model call. Rig
/// hands back tool failures as ordinary results, so nothing else in the loop
/// notices; this does, and is deliberately keyed on the arguments too -- the
/// same tool called with *different* arguments is a model making progress, not
/// a model stuck.
///
/// Nothing here is specific to `outrig__set_result` -- any tool can be looped
/// on. It is withheld from the primary agent because a human is sitting in
/// front of that one and can interrupt it, while a subagent loops unattended
/// inside a parent's tool call. Enforced by construction: `for_subagent` is the
/// only constructor that sets the tracker.
#[derive(Debug, Default)]
pub struct RepeatTracker {
    /// The `(tool, args)` of the last failing call, absent once anything
    /// succeeds.
    last_failure: Option<(String, String)>,
    consecutive: u32,
}

impl RepeatTracker {
    /// Fold in one tool result and say what should happen.
    pub fn observe(&mut self, tool_name: &str, args: &str, failed: bool) -> RepeatVerdict {
        if !failed {
            self.last_failure = None;
            self.consecutive = 0;
            return RepeatVerdict::Allow;
        }
        let call = (tool_name.to_string(), args.to_string());
        if self.last_failure.as_ref() == Some(&call) {
            self.consecutive += 1;
        } else {
            self.last_failure = Some(call);
            self.consecutive = 1;
        }
        match self.consecutive {
            n if n >= REPEAT_TERMINATE_AT => RepeatVerdict::Stop,
            n if n >= REPEAT_NUDGE_AT => RepeatVerdict::Nudge,
            _ => RepeatVerdict::Allow,
        }
    }
}

/// Per-request hook that traces every tool call to stderr and stops the agent
/// loop after `max` calls. Cloned by rig per request; shared atomics keep a
/// single turn's calls counting against the same max.
#[derive(Clone)]
pub struct OutrigPromptHook {
    counter: Arc<AtomicUsize>,
    cap_reached: Arc<AtomicBool>,
    max: usize,
    /// Prefixes trace lines so concurrent subagents are tellable apart. The
    /// primary agent leaves it unset and its traces keep their original shape.
    label: Option<Arc<str>>,
    injections: Option<InjectionSource>,
    /// Present for subagents only, which is what keeps the breaker off the
    /// primary agent's loop. See [`RepeatTracker`].
    repeats: Option<Arc<std::sync::Mutex<RepeatTracker>>>,
    /// Why this hook asked the loop to stop, once it has.
    ///
    /// Recorded here rather than recovered from the cancellation rig reports,
    /// because rig raises `PromptCancelled` for its own internal faults too --
    /// a driver protocol violation looks exactly like a deliberate stop from
    /// the outside. Only a reason this hook put here is one OutRig chose, so
    /// anything else can be surfaced as the fault it is instead of being
    /// dressed up as an orderly end.
    stop_reason: Arc<std::sync::Mutex<Option<String>>>,
}

impl OutrigPromptHook {
    pub fn new(max: usize) -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
            cap_reached: Arc::new(AtomicBool::new(false)),
            max,
            label: None,
            injections: None,
            repeats: None,
            stop_reason: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// The reason this hook stopped the loop, if it did.
    fn stop_reason(&self) -> Option<String> {
        self.stop_reason
            .lock()
            .expect("stop-reason mutex poisoned")
            .clone()
    }

    /// Record the reason and hand it to rig, so the same string reaches the
    /// log, the caller, and -- for a subagent -- its parent.
    fn stop(&self, reason: String) -> Flow {
        *self.stop_reason.lock().expect("stop-reason mutex poisoned") = Some(reason.clone());
        Flow::terminate(reason)
    }

    /// The subagent form: traces carry the subagent's name, each model call
    /// picks up whatever the parent has queued, and a tool call that keeps
    /// failing identically stops the round.
    pub fn for_subagent(max: usize, label: &str, injections: InjectionSource) -> Self {
        Self {
            label: Some(Arc::from(label)),
            injections: Some(injections),
            repeats: Some(Arc::new(std::sync::Mutex::new(RepeatTracker::default()))),
            ..Self::new(max)
        }
    }

    fn trace_prefix(&self) -> String {
        match &self.label {
            Some(label) => format!("  [{label}] "),
            None => String::new(),
        }
    }
}

impl<M: CompletionModel> AgentHook<M> for OutrigPromptHook {
    // The hook only acts on `CompletionCall` and `ToolCall`; narrowing
    // `observes` keeps it off the per-token `TextDelta`/`ToolCallDelta` stream
    // in the streaming path, where `on_event` below would just no-op anyway.
    fn observes(&self, kind: StepEventKind) -> bool {
        match kind {
            StepEventKind::CompletionCall | StepEventKind::ToolCall => true,
            // Only subagents run the repeat breaker, so the primary agent stays
            // off the per-result path entirely.
            StepEventKind::ToolResult => self.repeats.is_some(),
            _ => false,
        }
    }

    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            StepEvent::CompletionCall { history, .. } => {
                if self.cap_reached.load(Ordering::SeqCst) {
                    // Bare reason, no "ending turn": `handle_prompt_error`
                    // prints it and a subagent's parent is shown it, so a
                    // suffix here reads twice in the log and lands in the
                    // parent's error as trailing noise.
                    return self.stop(format!("tool-call iteration max ({}) reached", self.max));
                }
                // Steers are re-applied on every model call, not just the one
                // after they arrive: the patch is per-turn and non-sticky.
                if let Some(source) = &self.injections {
                    let steers = source();
                    if !steers.is_empty() {
                        let mut patched = history.to_vec();
                        patched.extend(steers);
                        return Flow::patch_request(RequestPatch::new().history(patched));
                    }
                }
                Flow::cont()
            }
            StepEvent::ToolCall {
                tool_name, args, ..
            } => {
                let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n > self.max {
                    self.cap_reached.store(true, Ordering::SeqCst);
                    return Flow::skip(format!(
                        "[outrig] tool call not executed: per-turn tool-call max ({}) \
                         was reached before this call could run. The user may continue \
                         with a fresh max; repeat the tool call if still needed.",
                        self.max
                    ));
                }
                eprintln!(
                    "[outrig] {}tool call: {tool_name}({args})",
                    self.trace_prefix()
                );
                Flow::cont()
            }
            StepEvent::ToolResult {
                tool_name,
                args,
                result,
                outcome,
                ..
            } => {
                let Some(repeats) = &self.repeats else {
                    return Flow::cont();
                };
                let verdict = repeats
                    .lock()
                    .expect("repeat-tracker mutex poisoned")
                    .observe(tool_name, args, outcome.is_error());
                match verdict {
                    RepeatVerdict::Allow => Flow::cont(),
                    // Rewrite rather than skip: the tool already ran, and the
                    // model needs to keep seeing *why* it failed alongside the
                    // note that repeating it will not change that.
                    RepeatVerdict::Nudge => Flow::rewrite_result(format!(
                        "{result}\n\n[outrig] this is the same call to {tool_name} with the \
                         same arguments as the previous one, and it failed the same way. \
                         Repeating it will not change the outcome -- change the arguments \
                         or do something else.",
                    )),
                    // No trace of its own: `handle_prompt_error` prints every
                    // terminate reason, and the round driver passes this one to
                    // the parent as the cause it stopped.
                    RepeatVerdict::Stop => self.stop(format!(
                        "{tool_name} failed {REPEAT_TERMINATE_AT} times in a row with \
                         identical arguments"
                    )),
                }
            }
            _ => Flow::cont(),
        }
    }
}

/// `max_tokens` is passed rather than read off `resolved` because the ceiling
/// that travels is not always the one that was configured: the Anthropic arm
/// caps it at what the identifier publishes. Every other field is the resolved
/// agent's as-is.
fn finish_agent<M: rig::completion::CompletionModel + 'static>(
    model: M,
    resolved: &ResolvedAgent,
    max_tokens: Option<u32>,
    tools: Vec<SessionTool>,
) -> rig::agent::Agent<M> {
    use rig::agent::AgentBuilder;

    let mut builder = AgentBuilder::new(model);
    // Skipped rather than passed as "": a builder that never saw a preamble
    // sends no system prompt, which is what an unset `preamble` now means.
    if let Some(preamble) = &resolved.preamble {
        builder = builder.preamble(preamble);
    }
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(temperature as f64);
    }
    if let Some(max_tokens) = max_tokens {
        builder = builder.max_tokens(max_tokens as u64);
    }
    builder.tools(session_tool::boxed(&tools)).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "local-llm")]
    use rig::completion::{CompletionError, CompletionRequest, CompletionResponse, Usage};
    #[cfg(feature = "local-llm")]
    use rig::streaming::{RawStreamingChoice, StreamingCompletionResponse};

    /// A candidate on `provider`, with only the fields these tests read.
    fn candidate(name: &str, provider: ResolvedProvider) -> ResolvedCandidate {
        ResolvedCandidate {
            model_name: name.to_string(),
            model_identifier: name.to_string(),
            provider_name: name.to_string(),
            provider,
            model_weights: None,
            max_tokens: None,
        }
    }

    fn remote(retry_budget_secs: Option<u64>) -> ResolvedProvider {
        ResolvedProvider::OpenAi {
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "k".to_string(),
            request_timeout_secs: None,
            retry_budget_secs,
        }
    }

    /// A chain takes its budget from the first candidate that *has* one.
    ///
    /// The head of a preference order decides, but an in-process row has no
    /// `retry-budget-secs` to give -- it does no HTTP -- and that is not the
    /// same as asking for the default. Reading only the head sent a
    /// local-first alias to the compiled 600 seconds and handed that to a
    /// remote fallback, quietly overriding the `0` the user set and the docs
    /// call the one "no retries" knob.
    #[test]
    fn a_chain_budget_skips_candidates_that_have_none() {
        // The case that regressed: local head, remote fallback carrying the
        // effective top-level `retry-budget-secs = 0`.
        assert_eq!(
            chain_retry_budget_secs(&[
                candidate("local", ResolvedProvider::Mistralrs),
                candidate("hosted", remote(Some(0))),
            ]),
            Some(0),
            "a local head must not discard the fallback's configured budget",
        );

        // The head still wins whenever it has an opinion of its own.
        assert_eq!(
            chain_retry_budget_secs(&[
                candidate("hosted-a", remote(Some(30))),
                candidate("hosted-b", remote(Some(600))),
            ]),
            Some(30),
        );

        // ...and "inherit the compiled default" *is* an opinion. A remote head
        // with no configured budget must not be skipped in favor of a later
        // candidate's override: that would let a fallback's `0` disable retries
        // for the vendor the user actually preferred.
        assert_eq!(
            chain_retry_budget_secs(&[
                candidate("hosted-a", remote(None)),
                candidate("hosted-b", remote(Some(0))),
            ]),
            None,
            "the head answered `use the default`; a later override does not overrule it",
        );

        // The local skip and that rule composing: skip the row that cannot
        // answer, then stop at the first that can, default or not.
        assert_eq!(
            chain_retry_budget_secs(&[
                candidate("local", ResolvedProvider::Mistralrs),
                candidate("hosted-a", remote(None)),
                candidate("hosted-b", remote(Some(0))),
            ]),
            None,
        );

        // An all-local chain has nothing to say, which is harmless: nothing in
        // it retries over HTTP.
        assert_eq!(
            chain_retry_budget_secs(&[
                candidate("local-a", ResolvedProvider::Mistralrs),
                candidate("local-b", ResolvedProvider::Mistralrs),
            ]),
            None,
        );
    }

    /// Feed one failing call repeatedly and collect the verdict each time.
    fn repeat_verdicts(times: usize) -> Vec<RepeatVerdict> {
        let mut tracker = RepeatTracker::default();
        (0..times)
            .map(|_| tracker.observe("outrig__set_result", r#"{"status":"result"}"#, true))
            .collect()
    }

    /// The shape this breaker exists for: one call, failing identically, over
    /// and over. The first failure is ordinary, the second earns a nudge, and
    /// the fourth ends the round rather than spending the rest of the
    /// tool-call budget on it.
    #[test]
    fn an_identical_failure_is_nudged_then_stopped() {
        assert_eq!(
            repeat_verdicts(4),
            vec![
                RepeatVerdict::Allow,
                RepeatVerdict::Nudge,
                RepeatVerdict::Nudge,
                RepeatVerdict::Stop,
            ]
        );
    }

    /// The same tool with *different* arguments is a model trying something
    /// else, which is exactly the behavior the nudge asks for -- counting it
    /// toward the limit would punish taking the hint.
    #[test]
    fn changing_the_arguments_restarts_the_count() {
        let mut tracker = RepeatTracker::default();
        assert_eq!(
            tracker.observe("fs__read_file", r#"{"path":"a"}"#, true),
            RepeatVerdict::Allow
        );
        assert_eq!(
            tracker.observe("fs__read_file", r#"{"path":"a"}"#, true),
            RepeatVerdict::Nudge
        );
        assert_eq!(
            tracker.observe("fs__read_file", r#"{"path":"b"}"#, true),
            RepeatVerdict::Allow,
            "different arguments are progress, not a loop"
        );
    }

    /// Only *consecutive* failures count. A tool that works in between means
    /// the subagent is getting somewhere.
    #[test]
    fn a_success_clears_the_count() {
        let mut tracker = RepeatTracker::default();
        for _ in 0..3 {
            tracker.observe("shell__exec", r#"{"cmd":"build"}"#, true);
        }
        assert_eq!(
            tracker.observe("shell__exec", r#"{"cmd":"build"}"#, false),
            RepeatVerdict::Allow
        );
        assert_eq!(
            tracker.observe("shell__exec", r#"{"cmd":"build"}"#, true),
            RepeatVerdict::Allow,
            "the count restarts after a success, so this is a first failure"
        );
    }

    /// The breaker is subagent-only, and `observes` is what enforces it: the
    /// primary agent never even sees the events it keys off.
    #[test]
    fn only_subagent_hooks_watch_tool_results() {
        let injections: InjectionSource = Arc::new(Vec::new);
        let subagent = OutrigPromptHook::for_subagent(10, "audit", injections);
        let primary = OutrigPromptHook::new(10);

        assert!(
            AgentHook::<rig::providers::openai::CompletionModel>::observes(
                &subagent,
                StepEventKind::ToolResult
            )
        );
        assert!(
            !AgentHook::<rig::providers::openai::CompletionModel>::observes(
                &primary,
                StepEventKind::ToolResult
            ),
            "the primary agent keeps its existing behavior"
        );
    }

    /// A cut-short turn has to carry *why* out of the loop. Flattening both
    /// exits into one canned string is what left a subagent's parent reading
    /// "stopped without calling outrig__set_result" when the real cause was the
    /// budget running out.
    /// A hook that stopped the loop records why, and that reason is what the
    /// caller -- and a subagent's parent -- is told.
    #[test]
    fn a_cut_short_turn_reports_the_reason_it_stopped() {
        let hook = OutrigPromptHook::new(50);
        hook.stop("outrig__set_result failed 4 times in a row".to_string());

        let mut history = Vec::new();
        let end = handle_prompt_error(
            rig::completion::PromptError::PromptCancelled {
                reason: "outrig__set_result failed 4 times in a row".to_string(),
                chat_history: Vec::new(),
            },
            &mut history,
            &hook,
        )
        .expect("a cut-short turn is not an error to the caller");

        assert_eq!(
            end.stopped.as_ref().map(TurnStop::reason),
            Some("outrig__set_result failed 4 times in a row")
        );
        assert!(end.reply.contains("failed 4 times"), "got: {}", end.reply);
    }

    /// Rig raises `PromptCancelled` for its own internal faults, which look
    /// identical to a deliberate stop. Passing one through would report a bug
    /// to a subagent's parent as an ordinary "stopped before reporting" -- the
    /// model's fault, apparently. It has to stay an error.
    #[test]
    fn a_cancellation_outrig_did_not_ask_for_stays_an_error() {
        let hook = OutrigPromptHook::new(50);
        let mut history = Vec::new();

        let err = handle_prompt_error(
            rig::completion::PromptError::PromptCancelled {
                reason: "agent run driver protocol violation".to_string(),
                chat_history: Vec::new(),
            },
            &mut history,
            &hook,
        )
        .expect_err("an internal fault must not be downgraded to a tidy stop");

        assert!(
            err.to_string().contains("protocol violation"),
            "the fault must reach the caller intact, got: {err}"
        );
    }

    /// The turn budget is the other way a round ends early, and it reaches the
    /// parent down the same path. Rig raises it, not the hook, so it is
    /// recognized by its variant rather than by a recorded reason.
    #[test]
    fn exhausting_the_turn_budget_names_the_budget() {
        let hook = OutrigPromptHook::new(50);
        let mut history = Vec::new();
        let end = handle_prompt_error(
            rig::completion::PromptError::MaxTurnsError {
                max_turns: 52,
                chat_history: Box::new(Vec::new()),
                prompt: Box::new(Message::user("go")),
            },
            &mut history,
            &hook,
        )
        .expect("exhaustion is not an error to the caller");

        let stopped = end.stopped.expect("the budget is a reason");
        assert!(
            matches!(stopped, TurnStop::Interrupted(_)),
            "a spent tool-call budget is a continuable stop, got: {stopped:?}",
        );
        assert!(stopped.reason().contains("(52)"), "got: {stopped:?}");
    }

    /// Both turn paths read the stop cause straight off the hook, so a hook
    /// that stopped nothing is what makes an ordinary turn report no cause.
    /// Were it to report one, every normal round would hand its parent a
    /// spurious reason.
    #[test]
    fn a_hook_that_stopped_nothing_reports_no_reason() {
        assert_eq!(OutrigPromptHook::new(50).stop_reason(), None);
    }

    /// The reason has to survive the clone: rig takes ownership of the hook, so
    /// only the copy left behind can be asked afterwards what happened.
    #[test]
    fn a_stop_reason_is_visible_through_a_clone() {
        let hook = OutrigPromptHook::new(50);
        let observer = hook.clone();
        hook.stop("tool-call iteration max (50) reached".to_string());

        assert_eq!(
            observer.stop_reason().as_deref(),
            Some("tool-call iteration max (50) reached")
        );
    }

    #[test]
    fn cancelled_history_retains_only_new_suffix_when_full_history_returned() {
        let original = vec![Message::user("first"), Message::assistant("done")];
        let mut history = original.clone();
        let mut returned = original;
        returned.push(Message::user("second"));
        returned.push(Message::assistant("partial"));

        extend_history_with_new_suffix(&mut history, returned);

        assert_eq!(
            history,
            vec![
                Message::user("first"),
                Message::assistant("done"),
                Message::user("second"),
                Message::assistant("partial"),
            ],
        );
    }

    #[test]
    fn cancelled_history_appends_when_returned_history_is_only_partial() {
        let mut history = vec![Message::user("first")];
        let returned = vec![Message::assistant("partial")];

        extend_history_with_new_suffix(&mut history, returned);

        assert_eq!(
            history,
            vec![Message::user("first"), Message::assistant("partial")],
        );
    }

    #[cfg(feature = "local-llm")]
    #[derive(Clone)]
    struct ScriptedStreamingModel {
        chunks: Arc<Vec<RawStreamingChoice<()>>>,
    }

    #[cfg(feature = "local-llm")]
    impl ScriptedStreamingModel {
        fn new(chunks: Vec<RawStreamingChoice<()>>) -> Self {
            Self {
                chunks: Arc::new(chunks),
            }
        }
    }

    #[cfg(feature = "local-llm")]
    impl CompletionModel for ScriptedStreamingModel {
        type Response = ();
        type StreamingResponse = ();
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self::new(Vec::new())
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
            Ok(CompletionResponse {
                choice: rig::OneOrMany::one(rig::completion::AssistantContent::text("")),
                usage: Usage::new(),
                raw_response: (),
                message_id: None,
            })
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<
            StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            let chunks = self.chunks.clone();
            let stream = async_stream::try_stream! {
                for chunk in chunks.iter().cloned() {
                    yield chunk;
                }
            };
            Ok(StreamingCompletionResponse::stream(Box::pin(stream)))
        }
    }

    #[cfg(feature = "local-llm")]
    #[tokio::test]
    async fn streaming_turn_writes_chunks_once_and_retains_history() {
        let model = ScriptedStreamingModel::new(vec![
            RawStreamingChoice::Message("hello ".to_string()),
            RawStreamingChoice::Message("world".to_string()),
        ]);
        let agent = rig::agent::AgentBuilder::new(model).build();
        let mut history = Vec::new();
        let mut stdout = Vec::new();

        let end = run_turn_streaming_inner(
            &agent,
            "hi",
            &mut history,
            OutrigPromptHook::new(50),
            &mut stdout,
        )
        .await
        .expect("streaming turn succeeds");

        // The inner loop hands back what it decoded so a subagent can capture
        // it; suppressing the REPL's reprint is `run_turn_streaming_to`'s job,
        // pinned by `primary_streaming_path_suppresses_the_reprint` below.
        assert_eq!(end.reply, "hello world");
        assert_eq!(
            end.stopped, None,
            "a turn the model finished must not look cut short"
        );
        assert_eq!(
            String::from_utf8(stdout).expect("stdout utf-8"),
            "hello world\n"
        );
        assert_eq!(
            history,
            vec![Message::user("hi"), Message::assistant("hello world")],
        );
    }

    /// The REPL prints `on_prompt`'s return value when it is non-empty
    /// (`repl.rs`), so the primary streaming path must return empty or the
    /// reply appears twice: once streamed while decoding, once reprinted.
    #[cfg(feature = "local-llm")]
    #[tokio::test]
    async fn primary_streaming_path_suppresses_the_reprint() {
        let model = ScriptedStreamingModel::new(vec![
            RawStreamingChoice::Message("hello ".to_string()),
            RawStreamingChoice::Message("world".to_string()),
        ]);
        let agent = rig::agent::AgentBuilder::new(model).build();
        let mut history = Vec::new();
        let mut sink = Vec::new();

        let reply = run_turn_streaming_to(
            &agent,
            "hi",
            &mut history,
            OutrigPromptHook::new(50),
            &mut sink,
        )
        .await
        .expect("streaming turn succeeds");

        assert_eq!(reply, "", "a non-empty return would double-print the reply");
        assert_eq!(
            String::from_utf8(sink).expect("sink utf-8"),
            "hello world\n",
            "the reply should still have been streamed exactly once"
        );
    }

    /// A model name resolved through an alias shows the hop it took. Static
    /// selection is otherwise invisible: picking a candidate because of an
    /// environment variable is exactly the kind of decision that stays hidden
    /// until it is wrong, so every surface that prints a model prints this.
    #[test]
    fn model_display_shows_the_alias_hop() {
        let resolved = ResolvedAgent {
            alias_name: Some("opus".to_string()),
            ..test_resolved_for_display()
        };
        assert_eq!(resolved.model_display(), "opus -> opus-5");
    }

    /// No alias, no arrow -- so the banner, the launch trace and the transcript
    /// header all read exactly as they did before aliases existed.
    #[test]
    fn model_display_is_the_bare_name_for_a_direct_model() {
        assert_eq!(test_resolved_for_display().model_display(), "opus-5");
    }

    /// The display helper only reads the first candidate's name and
    /// `alias_name`, so this borrows the session fixture rather than spelling a
    /// fifth full `ResolvedAgent` literal.
    fn test_resolved_for_display() -> ResolvedAgent {
        let base = crate::subagent::fixtures::test_resolved(1);
        ResolvedAgent {
            candidates: vec![ResolvedCandidate {
                model_name: "opus-5".to_string(),
                ..base.primary().clone()
            }],
            ..base
        }
    }
}
