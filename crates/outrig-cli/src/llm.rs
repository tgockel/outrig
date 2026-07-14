//! Resolve agent -> model -> provider; build Rig agent.

use std::cell::{Cell, Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "local-llm")]
use futures_util::StreamExt;
use rig::agent::{AgentHook, Flow, HookContext, StepEvent, StepEventKind};
#[cfg(feature = "local-llm")]
use rig::agent::{MultiTurnStreamItem, StreamingError};
use rig::completion::{CompletionModel, Message, Prompt};
#[cfg(feature = "local-llm")]
use rig::streaming::{StreamedAssistantContent, StreamingChat};
use thiserror::Error;
#[cfg(feature = "local-llm")]
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::Result;
use crate::rig_tool::McpToolAdapter;
use outrig::config::{Config, DEFAULT_TOOL_CALL_MAX, LlmProvider, MistralrsDeviceSpec};

/// Hard max on tool calls per turn. The per-turn [`OutrigPromptHook`] trips
/// this and surfaces a controllable message. rig's own `max_turns` is a
/// backstop set a little higher: as of rig 0.40 it counts *total* model calls
/// (initial completion + one continuation per tool call) rather than the looser
/// 0.39 budget, so callers pass `tool_call_max + 2` to preserve the previous
/// effective allowance and keep the hook -- not rig -- the limiter that fires
/// first.
pub const MAX_TOOL_CALLS: usize = DEFAULT_TOOL_CALL_MAX as usize;

/// Default byte ceiling applied to each individual MCP tool result before it
/// is handed to Rig and appended to model-visible chat history.
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
    pub image: Option<String>,
}

/// Walk `cfg.agents -> models -> providers` to resolve every knob the agent
/// loop needs. Bails with a descriptive error if a reference is dangling or
/// the api-key env var is unset.
///
/// Each lookup is re-checked here -- the function does not assume
/// `cfg.validate()` was called -- so errors carry the resolution context
/// (which agent, which model) regardless.
pub fn resolve_agent(cfg: &Config, agent_name: &str) -> Result<ResolvedAgent> {
    resolve_agent_with_overrides(cfg, agent_name, None, None)
}

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
    tools: Vec<McpToolAdapter>,
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
            } => run_turn_inner(agent, prompt, history, *tool_call_max).await,
            #[cfg(feature = "local-llm")]
            RigAgent::Mistralrs {
                agent,
                tool_call_max,
            } => run_turn_streaming_mistralrs(agent, prompt, history, *tool_call_max).await,
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
    registry: LlmRegistry,
    tools: RefCell<Vec<McpToolAdapter>>,
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
        tools: Vec<McpToolAdapter>,
        resolved: ResolvedAgent,
        cache_root: PathBuf,
        #[cfg(feature = "local-llm")] registry: LlmRegistry,
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
    pub fn extend_tools(&self, new: Vec<McpToolAdapter>) {
        self.tools.borrow_mut().extend(new);
        self.dirty.set(true);
    }

    /// Snapshot view of the current tool list. Do not hold across an await.
    pub fn tools(&self) -> Ref<'_, [McpToolAdapter]> {
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

async fn run_turn_inner<M: CompletionModel + 'static>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_max: usize,
) -> Result<String> {
    let hook = OutrigPromptHook::new(tool_call_max);
    let result = agent
        .prompt(prompt.to_string())
        .history(history.clone())
        .max_turns(tool_call_max + 2)
        .add_hook(hook)
        .extended_details()
        .await;

    match result {
        Ok(response) => {
            let messages = response
                .messages
                .expect("rig populates messages on extended_details");
            history.extend(messages);
            Ok(response.output)
        }
        Err(other) => handle_prompt_error(other, history),
    }
}

#[cfg(feature = "local-llm")]
async fn run_turn_streaming_mistralrs(
    agent: &rig::agent::Agent<crate::llm::mistralrs::MistralrsModel>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_max: usize,
) -> Result<String> {
    let mut stdout = tokio::io::stdout();
    run_turn_streaming_inner(agent, prompt, history, tool_call_max, &mut stdout).await
}

#[cfg(feature = "local-llm")]
async fn run_turn_streaming_inner<M, W>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_max: usize,
    stdout: &mut W,
) -> Result<String>
where
    M: CompletionModel + 'static,
    W: AsyncWrite + Unpin,
{
    let hook = OutrigPromptHook::new(tool_call_max);
    let mut stream = agent
        .stream_chat(prompt.to_string(), history.clone())
        .max_turns(tool_call_max + 2)
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
                return handle_streaming_error(err, history);
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

    Ok(String::new())
}

#[cfg(feature = "local-llm")]
fn handle_streaming_error(err: StreamingError, history: &mut Vec<Message>) -> Result<String> {
    let prompt_error = match err {
        StreamingError::Completion(err) => rig::completion::PromptError::CompletionError(err),
        StreamingError::Prompt(err) => *err,
        StreamingError::Tool(err) => rig::completion::PromptError::ToolError(err),
    };
    handle_prompt_error(prompt_error, history)
}

fn handle_prompt_error(
    err: rig::completion::PromptError,
    history: &mut Vec<Message>,
) -> Result<String> {
    match err {
        rig::completion::PromptError::PromptCancelled {
            reason,
            chat_history,
        } => {
            eprintln!("[outrig] {reason}");
            eprintln!(
                "[outrig] partial history retained -- send another prompt \
                 (e.g. \"continue\") to keep going, or \"/reset\" to drop it."
            );
            extend_history_with_new_suffix(history, chat_history);
            Ok("(turn ended; tool-call max reached)".to_string())
        }
        rig::completion::PromptError::MaxTurnsError {
            max_turns,
            chat_history,
            ..
        } => {
            eprintln!("[outrig] tool-call iteration max ({max_turns}) reached; ending turn");
            eprintln!(
                "[outrig] partial history retained -- send another prompt \
                 (e.g. \"continue\") to keep going, or \"/reset\" to drop it."
            );
            extend_history_with_new_suffix(history, *chat_history);
            Ok("(turn ended; tool-call max reached)".to_string())
        }
        other => Err(other.into()),
    }
}

fn extend_history_with_new_suffix(history: &mut Vec<Message>, returned: Vec<Message>) {
    let existing_len = history.len();
    if returned.len() >= existing_len && returned[..existing_len] == history[..] {
        history.extend(returned.into_iter().skip(existing_len));
    } else {
        history.extend(returned);
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
}

impl OutrigPromptHook {
    pub fn new(max: usize) -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
            cap_reached: Arc::new(AtomicBool::new(false)),
            max,
        }
    }
}

impl<M: CompletionModel> AgentHook<M> for OutrigPromptHook {
    // The hook only acts on `CompletionCall` and `ToolCall`; narrowing
    // `observes` keeps it off the per-token `TextDelta`/`ToolCallDelta` stream
    // in the streaming path, where `on_event` below would just no-op anyway.
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall | StepEventKind::ToolCall
        )
    }

    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            StepEvent::CompletionCall { .. } => {
                if self.cap_reached.load(Ordering::SeqCst) {
                    return Flow::terminate(format!(
                        "tool-call iteration max ({}) reached; ending turn",
                        self.max
                    ));
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
                eprintln!("[outrig] tool call: {tool_name}({args})");
                Flow::cont()
            }
            _ => Flow::cont(),
        }
    }
}

fn finish_agent<M: rig::completion::CompletionModel + 'static>(
    model: M,
    resolved: &ResolvedAgent,
    tools: Vec<McpToolAdapter>,
) -> rig::agent::Agent<M> {
    use rig::agent::AgentBuilder;
    use rig::tool::ToolDyn;

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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "local-llm")]
    use rig::completion::{CompletionError, CompletionRequest, CompletionResponse, Usage};
    #[cfg(feature = "local-llm")]
    use rig::streaming::{RawStreamingChoice, StreamingCompletionResponse};

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

        let reply = run_turn_streaming_inner(&agent, "hi", &mut history, 50, &mut stdout)
            .await
            .expect("streaming turn succeeds");

        assert_eq!(reply, "");
        assert_eq!(
            String::from_utf8(stdout).expect("stdout utf-8"),
            "hello world\n"
        );
        assert_eq!(
            history,
            vec![Message::user("hi"), Message::assistant("hello world")],
        );
    }
}
