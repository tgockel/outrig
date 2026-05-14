//! Resolve agent -> model -> provider; build Rig agent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "mistralrs")]
use futures_util::StreamExt;
use rig::agent::{HookAction, PromptHook, ToolCallHookAction};
#[cfg(feature = "mistralrs")]
use rig::agent::{MultiTurnStreamItem, StreamingError};
use rig::completion::{CompletionModel, Message, Prompt};
#[cfg(feature = "mistralrs")]
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use thiserror::Error;
#[cfg(feature = "mistralrs")]
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::config::{Config, DEFAULT_TOOL_CALL_CAP, LlmProvider};
use crate::error::{OutrigError, Result};
use crate::rig_tool::McpToolAdapter;

/// Hard cap on tool calls per turn. The hook below trips this; rig's own
/// `max_turns` is set to the same value as a defense in depth, so whichever
/// fires first surfaces a controllable message.
pub const MAX_TOOL_CALLS: usize = DEFAULT_TOOL_CALL_CAP as usize;

/// Default byte ceiling applied to each individual MCP tool result before it
/// is handed to Rig and appended to model-visible chat history.
pub const DEFAULT_TOOL_RESULT_CAP_BYTES: usize =
    crate::config::DEFAULT_TOOL_RESULT_CAP_BYTES as usize;

#[cfg(feature = "mistralrs")]
pub mod mistralrs;
#[cfg(feature = "mistralrs")]
pub mod registry;

#[cfg(feature = "mistralrs")]
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
         does not include the 'mistralrs' feature; rebuild with \
         --features mistralrs to enable"
    )]
    MistralrsFeatureDisabled { name: String },

    #[cfg(feature = "mistralrs")]
    #[error(
        "mistralrs model {model:?}: requested context-length \
         {requested} exceeds the model's maximum of {max}"
    )]
    MistralrsContextTooLong {
        model: String,
        requested: u32,
        max: usize,
    },

    #[cfg(feature = "mistralrs")]
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
    pub tool_call_cap: usize,
    pub tool_result_cap_bytes: usize,
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

    let (resolved_provider, model_weights, model_identifier) = match provider {
        LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
        } => {
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
            let weights = MistralrsWeights {
                model_id: model.model_id.clone(),
                model_path: model.model_path.clone(),
                model_file: model.model_file.clone(),
                revision: model.revision.clone(),
                context_length: model.context_length,
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
        tool_call_cap: agent
            .tool_call_cap
            .or(cfg.tool_call_cap)
            .unwrap_or(DEFAULT_TOOL_CALL_CAP) as usize,
        tool_result_cap_bytes: agent
            .tool_result_cap
            .or(cfg.tool_result_cap)
            .unwrap_or(crate::config::DEFAULT_TOOL_RESULT_CAP_BYTES)
            as usize,
        container: agent.container.clone(),
    })
}

/// Runtime-dispatched Rig agent. The OpenAi-backed and mistralrs-backed
/// `CompletionModel` impls produce concretely different `Agent<M>` types
/// (Rig's trait carries associated types, so a single concrete `RigAgent`
/// can't carry both). Callers (the agent loop) match on the variant.
pub enum RigAgent {
    OpenAi {
        agent: rig::agent::Agent<rig::providers::openai::CompletionModel>,
        tool_call_cap: usize,
    },
    #[cfg(feature = "mistralrs")]
    Mistralrs {
        agent: rig::agent::Agent<crate::llm::mistralrs::MistralrsModel>,
        tool_call_cap: usize,
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
    #[cfg(feature = "mistralrs")] registry: &LlmRegistry,
) -> Result<RigAgent> {
    #[cfg(not(feature = "mistralrs"))]
    let _ = cache_root;
    match &resolved.provider {
        ResolvedProvider::OpenAi {
            base_url, api_key, ..
        } => {
            use rig::client::CompletionClient;
            use rig::providers::openai::CompletionsClient;

            let client = CompletionsClient::builder()
                .api_key(api_key.clone())
                .base_url(base_url)
                .build()
                .map_err(|e| LlmResolveError::RigClientBuild(e.to_string()))?;
            let model = client.completion_model(&resolved.model_identifier);
            Ok(RigAgent::OpenAi {
                agent: finish_agent(model, resolved, tools),
                tool_call_cap: resolved.tool_call_cap,
            })
        }
        ResolvedProvider::Mistralrs => {
            #[cfg(not(feature = "mistralrs"))]
            {
                Err(LlmResolveError::MistralrsFeatureDisabled {
                    name: resolved.provider_name.clone(),
                }
                .into())
            }
            #[cfg(feature = "mistralrs")]
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
                let model = registry
                    .get_or_init(model_name, || async move {
                        crate::llm::mistralrs::load(
                            model_name,
                            model_id,
                            model_path,
                            model_file,
                            revision,
                            context_length,
                            cache_root,
                        )
                        .await
                    })
                    .await?;
                Ok(RigAgent::Mistralrs {
                    agent: finish_agent((*model).clone(), resolved, tools),
                    tool_call_cap: resolved.tool_call_cap,
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
    /// the resolved tool-call cap. If the hook terminates the loop, Rig
    /// returns the partial chat history it had accumulated; outrig splices in
    /// that new suffix so the user can send a follow-up prompt to continue.
    pub async fn run_turn(&self, prompt: &str, history: &mut Vec<Message>) -> Result<String> {
        match self {
            RigAgent::OpenAi {
                agent,
                tool_call_cap,
            } => run_turn_inner(agent, prompt, history, *tool_call_cap).await,
            #[cfg(feature = "mistralrs")]
            RigAgent::Mistralrs {
                agent,
                tool_call_cap,
            } => run_turn_streaming_mistralrs(agent, prompt, history, *tool_call_cap).await,
        }
    }
}

async fn run_turn_inner<M: CompletionModel + 'static>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_cap: usize,
) -> Result<String> {
    let hook = OutrigPromptHook::new(tool_call_cap);
    let result = agent
        .prompt(prompt.to_string())
        .with_history(history.clone())
        .max_turns(tool_call_cap)
        .with_hook(hook)
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

#[cfg(feature = "mistralrs")]
async fn run_turn_streaming_mistralrs(
    agent: &rig::agent::Agent<crate::llm::mistralrs::MistralrsModel>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_cap: usize,
) -> Result<String> {
    let mut stdout = tokio::io::stdout();
    run_turn_streaming_inner(agent, prompt, history, tool_call_cap, &mut stdout).await
}

#[cfg(feature = "mistralrs")]
async fn run_turn_streaming_inner<M, W>(
    agent: &rig::agent::Agent<M>,
    prompt: &str,
    history: &mut Vec<Message>,
    tool_call_cap: usize,
    stdout: &mut W,
) -> Result<String>
where
    M: CompletionModel + 'static,
    W: AsyncWrite + Unpin,
{
    let hook = OutrigPromptHook::new(tool_call_cap);
    let mut stream = agent
        .stream_prompt(prompt.to_string())
        .with_history(history.clone())
        .multi_turn(tool_call_cap)
        .with_hook(hook)
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
                final_history = response.history().map(|messages| messages.to_vec());
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

#[cfg(feature = "mistralrs")]
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
            Ok("(turn ended; tool-call cap reached)".to_string())
        }
        rig::completion::PromptError::MaxTurnsError {
            max_turns,
            chat_history,
            ..
        } => {
            eprintln!("[outrig] tool-call iteration cap ({max_turns}) reached; ending turn");
            eprintln!(
                "[outrig] partial history retained -- send another prompt \
                 (e.g. \"continue\") to keep going, or \"/reset\" to drop it."
            );
            extend_history_with_new_suffix(history, *chat_history);
            Ok("(turn ended; tool-call cap reached)".to_string())
        }
        other => Err(OutrigError::Prompt(other)),
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
/// loop after `cap` calls. Cloned by rig per request; shared atomics keep a
/// single turn's calls counting against the same cap.
#[derive(Clone)]
pub struct OutrigPromptHook {
    counter: Arc<AtomicUsize>,
    cap_reached: Arc<AtomicBool>,
    cap: usize,
}

impl OutrigPromptHook {
    pub fn new(cap: usize) -> Self {
        Self {
            counter: Arc::new(AtomicUsize::new(0)),
            cap_reached: Arc::new(AtomicBool::new(false)),
            cap,
        }
    }
}

impl<M: CompletionModel> PromptHook<M> for OutrigPromptHook {
    async fn on_completion_call(&self, _prompt: &Message, _history: &[Message]) -> HookAction {
        if self.cap_reached.load(Ordering::SeqCst) {
            return HookAction::terminate(format!(
                "tool-call iteration cap ({}) reached; ending turn",
                self.cap
            ));
        }
        HookAction::cont()
    }

    async fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        args: &str,
    ) -> ToolCallHookAction {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.cap {
            self.cap_reached.store(true, Ordering::SeqCst);
            return ToolCallHookAction::skip(format!(
                "[outrig] tool call not executed: per-turn tool-call cap ({}) \
                 was reached before this call could run. The user may continue \
                 with a fresh cap; repeat the tool call if still needed.",
                self.cap
            ));
        }
        eprintln!("[outrig] tool call: {tool_name}({args})");
        ToolCallHookAction::cont()
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
    #[cfg(feature = "mistralrs")]
    use rig::completion::{CompletionError, CompletionRequest, CompletionResponse, Usage};
    #[cfg(feature = "mistralrs")]
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

    #[cfg(feature = "mistralrs")]
    #[derive(Clone)]
    struct ScriptedStreamingModel {
        chunks: Arc<Vec<RawStreamingChoice<()>>>,
    }

    #[cfg(feature = "mistralrs")]
    impl ScriptedStreamingModel {
        fn new(chunks: Vec<RawStreamingChoice<()>>) -> Self {
            Self {
                chunks: Arc::new(chunks),
            }
        }
    }

    #[cfg(feature = "mistralrs")]
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

    #[cfg(feature = "mistralrs")]
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
