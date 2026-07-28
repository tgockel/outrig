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

use crate::error::Result;
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

/// Default per-request HTTP timeout for OpenAi-style providers when
/// `request-timeout-secs` is unset (see `doc/reference/config.md`). Generous
/// enough not to truncate long reasoning completions, and above typical proxy
/// timeouts so a client-side timeout never races a still-in-flight server
/// request.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

pub mod retry;

#[cfg(feature = "local-llm")]
pub mod mistralrs;
#[cfg(feature = "local-llm")]
pub mod registry;

#[cfg(feature = "local-llm")]
pub use registry::LlmRegistry;

/// Default preamble used when an agent leaves the field unset. Deliberately
/// generic; agents that need anything specific spell it out themselves.
const DEFAULT_PREAMBLE: &str =
    "You are a careful assistant whose tools run inside a sandboxed container.";

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
/// for the OpenAi variant. Variants are kept in sync with `LlmProvider`'s.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedProvider {
    OpenAi {
        base_url: String,
        api_key: String,
        request_timeout_secs: Option<u64>,
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
    /// `Some` for mistralrs-style models, `None` for openai-style. Carries
    /// the per-model weight spec that used to live on the provider config.
    pub model_weights: Option<MistralrsWeights>,
    pub preamble: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub tool_call_max: usize,
    pub tool_result_max_bytes: usize,
    /// Maximum subagent nesting depth: the primary is the root at depth 1, and
    /// an agent at depth `D` may launch subagents while `D < subagent_depth_max`.
    pub subagent_depth_max: u32,
    pub image: Option<String>,
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// Each lookup is re-checked here -- the function does not assume
/// `cfg.validate()` was called -- so errors carry the resolution context
/// (which agent, which model) regardless.
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub fn resolve_agent(cfg: &Config, agent_name: &str) -> Result<ResolvedAgent> {
    resolve_agent_with_overrides(cfg, agent_name, None, None)
}

#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub fn resolve_agent_with_device_override(
    cfg: &Config,
    agent_name: &str,
    device_override: Option<MistralrsDeviceSpec>,
) -> Result<ResolvedAgent> {
    resolve_agent_with_overrides(cfg, agent_name, None, device_override)
}

pub fn resolve_agent_with_overrides(
    cfg: &Config,
    agent_name: &str,
    model_override: Option<&str>,
    device_override: Option<MistralrsDeviceSpec>,
) -> Result<ResolvedAgent> {
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

    let model_name = model_override
        .or(agent.model.as_deref())
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

    let (resolved_provider, model_weights, model_identifier) = match provider {
        LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } => {
            if device_override.is_some() {
                return Err(LlmResolveError::MistralrsDeviceOverrideUnsupported {
                    model: model_name.to_string(),
                    provider: model.provider.clone(),
                }
                .into());
            }
            let identifier = model
                .identifier
                .clone()
                .unwrap_or_else(|| model_name.to_string());
            (
                ResolvedProvider::OpenAi {
                    base_url: base_url.clone(),
                    api_key: api_key.resolve()?,
                    request_timeout_secs: *request_timeout_secs,
                },
                None,
                identifier,
            )
        }
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
        // `LlmProvider` is `#[non_exhaustive]`. There is no generic way to
        // reach a provider style this build has no client for, so say so
        // rather than guess at one.
        _ => {
            return Err(LlmResolveError::UnsupportedProvider {
                name: model.provider.clone(),
            }
            .into());
        }
    };

    Ok(ResolvedAgent {
        agent_name: agent_name.to_string(),
        model_name: model_name.to_string(),
        model_identifier,
        provider_name: model.provider.clone(),
        provider: resolved_provider,
        model_weights,
        preamble: agent
            .preamble
            .clone()
            .unwrap_or_else(|| DEFAULT_PREAMBLE.to_string()),
        temperature: agent.temperature,
        max_tokens: agent.max_tokens,
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
        image: agent.image.clone(),
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

/// Runtime-dispatched Rig agent. The OpenAi-backed and mistralrs-backed
/// `CompletionModel` impls produce concretely different `Agent<M>` types
/// (Rig's trait carries associated types, so a single concrete `RigAgent`
/// can't carry both). Callers (the agent loop) match on the variant.
pub enum RigAgent {
    OpenAi {
        agent: rig::agent::Agent<retry::RetryingModel<rig::providers::openai::CompletionModel>>,
        tool_call_max: usize,
    },
    #[cfg(feature = "local-llm")]
    Mistralrs {
        agent: rig::agent::Agent<crate::llm::mistralrs::MistralrsModel>,
        tool_call_max: usize,
    },
}

/// Build a Rig `Agent` ready to receive a turn. Preamble, sampling params,
/// and the dynamic-tool list come from `resolved` plus the caller-supplied
/// MCP-backed adapters. The `cache_root` argument is the directory into
/// which the mistralrs HF-download path stages model files; it's ignored
/// for OpenAi providers.
///
/// The function is async because the mistralrs arm has to load (and on
/// first use, download) a multi-gigabyte model. The OpenAi arm is sync-in-
/// async, free.
pub async fn build_agent(
    resolved: &ResolvedAgent,
    tools: Vec<SessionTool>,
    cache_root: &Path,
    #[cfg(feature = "local-llm")] registry: &LlmRegistry,
) -> Result<RigAgent> {
    #[cfg(not(feature = "local-llm"))]
    let _ = cache_root;
    match &resolved.provider {
        ResolvedProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
        } => {
            use rig::client::CompletionClient;
            use rig::providers::openai::CompletionsClient;

            let timeout = std::time::Duration::from_secs(
                request_timeout_secs.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
            );
            let http = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()))?;

            let client = CompletionsClient::builder()
                .api_key(api_key.clone())
                .base_url(base_url)
                .http_client(http)
                .build()
                .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()))?;
            let model =
                retry::RetryingModel::new(client.completion_model(&resolved.model_identifier));
            Ok(RigAgent::OpenAi {
                agent: finish_agent(model, resolved, tools),
                tool_call_max: resolved.tool_call_max,
            })
        }
        ResolvedProvider::Mistralrs => {
            #[cfg(not(feature = "local-llm"))]
            {
                Err(LlmResolveError::MistralrsFeatureDisabled {
                    name: resolved.provider_name.clone(),
                }
                .into())
            }
            #[cfg(feature = "local-llm")]
            {
                let weights = resolved.model_weights.as_ref().ok_or_else(|| {
                    LlmResolveError::MistralrsLoad {
                        model: resolved.model_name.clone(),
                        source: anyhow::anyhow!(
                            "internal: resolved mistralrs agent has no model_weights"
                        ),
                    }
                })?;
                let model_name = resolved.model_name.as_str();
                let model_id = weights.model_id.as_deref();
                let model_path = weights.model_path.as_deref();
                let model_file = weights.model_file.as_deref();
                let revision = weights.revision.as_deref();
                let context_length = weights.context_length;
                let device = weights.device;
                let model = registry
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
                    .await?;
                Ok(RigAgent::Mistralrs {
                    agent: finish_agent((*model).clone(), resolved, tools),
                    tool_call_max: resolved.tool_call_max,
                })
            }
        }
    }
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
    /// [`TurnEnd::stopped_early`] is the cause the round driver passes on to
    /// the parent when nothing was published.
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
/// (cheap -- the OpenAI arm is an HTTP client; mistralrs model loads are
/// registry-cached).
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
    /// Set when the loop was cut short rather than finishing on its own --
    /// the tool-call budget, or a hook that stopped it. Carries the reason.
    pub stopped_early: Option<String>,
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
                stopped_early: observer.stop_reason(),
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
        stopped_early: observer.stop_reason(),
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
fn handle_prompt_error(
    err: rig::completion::PromptError,
    history: &mut Vec<Message>,
    hook: &OutrigPromptHook,
) -> Result<TurnEnd> {
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
        stopped_early: Some(reason),
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

fn finish_agent<M: rig::completion::CompletionModel + 'static>(
    model: M,
    resolved: &ResolvedAgent,
    tools: Vec<SessionTool>,
) -> rig::agent::Agent<M> {
    use rig::agent::AgentBuilder;

    let mut builder = AgentBuilder::new(model).preamble(&resolved.preamble);
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(temperature as f64);
    }
    if let Some(max_tokens) = resolved.max_tokens {
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
            end.stopped_early.as_deref(),
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

        let reason = end.stopped_early.expect("the budget is a reason");
        assert!(reason.contains("(52)"), "got: {reason}");
    }

    /// Both turn paths read `stopped_early` straight off the hook, so a hook
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
            end.stopped_early, None,
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
}
