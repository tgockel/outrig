//! In-process LLM via the `mistralrs-core` crate.
//!
//! Wraps a loaded `mistralrs-core` engine in Rig's `CompletionModel` trait so
//! the same agent loop drives both OpenAI-compatible HTTP providers and
//! locally-loaded GGUF models. Tool-call passthrough is bounded by the
//! underlying GGUF model -- v0 makes no fidelity guarantee.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use indexmap::IndexMap;
use mistralrs_core::{
    AutoDeviceMapParams, DefaultSchedulerMethod, DeviceMapSetting, GGUFLoaderBuilder,
    GGUFSpecificConfig, MessageContent, MistralRs, MistralRsBuilder, ModelDType, NormalRequest,
    Request, RequestMessage, Response, SamplingParams, SchedulerConfig, TokenSource, Tool,
    ToolCallResponse, ToolType,
};
use mistralrs_core::{Function as MistralrsFunction, GLOBAL_HF_CACHE};
use rig::OneOrMany;
use rig::completion::message::{AssistantContent, Message, ToolCall, ToolFunction, UserContent};
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
};
use rig::streaming::StreamingCompletionResponse;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::info;

use crate::error::Result;
use crate::llm::LlmResolveError;

/// Thin handle around a loaded mistralrs engine. Required by Rig's
/// `CompletionModel::Client` associated type; outrig constructs models
/// directly in [`load`] without going through this client, but Rig's `make`
/// factory needs it to produce additional `MistralrsModel`s.
#[derive(Clone)]
pub struct MistralrsClient {
    engine: Arc<MistralRs>,
}

/// Rig `CompletionModel` impl backed by an in-process mistralrs engine.
/// Cloning is cheap (the engine is `Arc`-shared).
#[derive(Clone)]
pub struct MistralrsModel {
    engine: Arc<MistralRs>,
    model_identifier: String,
}

/// Round-trip-friendly wrapper for `mistralrs_core::ChatCompletionResponse`.
/// The trait bound on `CompletionModel::Response` requires
/// `Serialize + DeserializeOwned`; mistralrs's response type is `Serialize`
/// only, so we stash the JSON form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MistralrsRawResponse {
    pub raw: Value,
}

/// Load a mistralrs engine. Exactly one of `model_id` / `model_path` must
/// be set; `validate.rs` enforces this at config-load time so reaching the
/// "neither" or "both" branch here is an internal bug.
///
/// `cache_root` directs HF downloads via a process-global `OnceLock<Cache>`
/// in mistralrs-core. The first call wins; later calls with a different
/// root are silently ignored.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load(
    model_name: &str,
    model_id: Option<&str>,
    model_path: Option<&Path>,
    model_file: Option<&[String]>,
    revision: Option<&str>,
    context_length: Option<u32>,
    cache_root: &Path,
) -> Result<MistralrsModel> {
    let _ = GLOBAL_HF_CACHE.set(hf_hub::Cache::new(cache_root.to_path_buf()));

    let load_err = |source: anyhow::Error| LlmResolveError::MistralrsLoad {
        model: model_name.to_string(),
        source,
    };

    let (quantized_model_id, quantized_filenames, hf_revision, identifier) = match (
        model_id, model_path,
    ) {
        (Some(id), None) => {
            let files = model_file.filter(|s| !s.is_empty()).ok_or_else(|| {
                load_err(anyhow::anyhow!(
                    "internal: validate.rs should have required model-file when \
                     model-id is set"
                ))
            })?;
            info!(
                model = model_name,
                model_id = id,
                model_files = ?files,
                revision = revision.unwrap_or("main"),
                "downloading and loading GGUF model",
            );
            (
                id.to_string(),
                files.to_vec(),
                revision.map(str::to_string),
                id.to_string(),
            )
        }
        (None, Some(path)) => {
            let parent = path.parent().ok_or_else(|| {
                load_err(anyhow::anyhow!(
                    "model-path {:?} has no parent directory",
                    path.display()
                ))
            })?;
            let basename = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
                load_err(anyhow::anyhow!(
                    "model-path {:?} has no file name component",
                    path.display()
                ))
            })?;
            info!(
                model = model_name,
                model_path = %path.display(),
                "loading GGUF model from local path",
            );
            (
                parent.to_string_lossy().into_owned(),
                vec![basename.to_string()],
                None,
                basename.to_string(),
            )
        }
        (Some(_), Some(_)) | (None, None) => {
            return Err(load_err(anyhow::anyhow!(
                    "internal: validate.rs should have rejected this combination of model-id / model-path"
                ))
                .into());
        }
    };

    let started = Instant::now();
    let loader = GGUFLoaderBuilder::new(
        None,
        None,
        quantized_model_id,
        quantized_filenames,
        GGUFSpecificConfig::default(),
        false,
        None,
    )
    .build();
    let pipeline = loader
        .load_model_from_hf(
            hf_revision,
            TokenSource::CacheToken,
            &ModelDType::Auto,
            &candle_core::Device::Cpu,
            false,
            DeviceMapSetting::Auto(AutoDeviceMapParams::default_text()),
            None,
            None,
        )
        .map_err(load_err)?;

    info!(
        model = model_name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "model loaded",
    );

    let engine = MistralRsBuilder::new(pipeline, scheduler_config(), false, None)
        .build()
        .await;

    if let Some(requested) = context_length
        && let Ok(config) = engine.config(None)
        && let Some(max) = config.max_seq_len
        && (requested as usize) > max
    {
        return Err(LlmResolveError::MistralrsContextTooLong {
            model: model_name.to_string(),
            requested,
            max,
        }
        .into());
    }

    Ok(MistralrsModel {
        engine,
        model_identifier: identifier,
    })
}

fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::DefaultScheduler {
        method: DefaultSchedulerMethod::Fixed(
            // mistralrs's own CLI defaults to 5 here.
            std::num::NonZeroUsize::new(5).expect("5 is non-zero"),
        ),
    }
}

impl CompletionModel for MistralrsModel {
    type Response = MistralrsRawResponse;
    type StreamingResponse = ();
    type Client = MistralrsClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            engine: client.engine.clone(),
            model_identifier: model.into(),
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
        let (tx, mut rx) = mpsc::channel::<Response>(1);
        let normal = build_normal_request(&self.model_identifier, request, tx)?;
        let request_for_engine = Request::Normal(Box::new(normal));

        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || engine.send_request(request_for_engine))
            .await
            .map_err(|e| CompletionError::ProviderError(format!("mistralrs join error: {e}")))?
            .map_err(|e| CompletionError::ProviderError(format!("mistralrs send_request: {e}")))?;

        let response = rx.recv().await.ok_or_else(|| {
            CompletionError::ProviderError(
                "mistralrs engine closed the response channel without replying".into(),
            )
        })?;

        translate_response(response)
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> std::result::Result<StreamingCompletionResponse<()>, CompletionError> {
        Err(CompletionError::ProviderError(
            "streaming not supported by the mistralrs in-process backend".into(),
        ))
    }
}

fn build_normal_request(
    model_identifier: &str,
    req: CompletionRequest,
    response_tx: mpsc::Sender<Response>,
) -> std::result::Result<NormalRequest, CompletionError> {
    let messages = translate_messages(req.preamble.as_deref(), &req.chat_history);
    let tools = translate_tools(&req.tools)?;
    let tool_choice = translate_tool_choice(req.tool_choice.as_ref(), &tools);

    let mut sampling_params = SamplingParams::neutral();
    sampling_params.temperature = req.temperature;
    sampling_params.max_len = req.max_tokens.map(|t| t as usize);

    Ok(NormalRequest {
        messages: RequestMessage::Chat {
            messages,
            enable_thinking: None,
            reasoning_effort: None,
        },
        sampling_params,
        response: response_tx,
        return_logprobs: false,
        is_streaming: false,
        id: 0,
        constraint: mistralrs_core::Constraint::None,
        suffix: None,
        tools,
        tool_choice,
        logits_processors: None,
        return_raw_logits: false,
        web_search_options: None,
        model_id: Some(model_identifier.to_string()),
        truncate_sequence: false,
    })
}

fn translate_messages(
    preamble: Option<&str>,
    chat_history: &OneOrMany<Message>,
) -> Vec<IndexMap<String, MessageContent>> {
    let mut out: Vec<IndexMap<String, MessageContent>> = Vec::new();

    if let Some(p) = preamble.filter(|s| !s.is_empty()) {
        out.push(text_message("system", p));
    }

    for msg in chat_history.iter() {
        match msg {
            Message::System { content } => out.push(text_message("system", content)),
            Message::User { content } => translate_user(content, &mut out),
            Message::Assistant { content, .. } => translate_assistant(content, &mut out),
        }
    }

    out
}

fn text_message(role: &str, content: &str) -> IndexMap<String, MessageContent> {
    let mut map = IndexMap::new();
    map.insert("role".to_string(), MessageContent::Left(role.to_string()));
    map.insert(
        "content".to_string(),
        MessageContent::Left(content.to_string()),
    );
    map
}

fn translate_user(
    content: &OneOrMany<UserContent>,
    out: &mut Vec<IndexMap<String, MessageContent>>,
) {
    let mut text_parts: Vec<String> = Vec::new();
    for part in content.iter() {
        match part {
            UserContent::Text(t) => text_parts.push(t.text.clone()),
            UserContent::ToolResult(result) => {
                let body = tool_result_text(result);
                let mut map = IndexMap::new();
                map.insert("role".to_string(), MessageContent::Left("tool".to_string()));
                map.insert(
                    "tool_call_id".to_string(),
                    MessageContent::Left(result.id.clone()),
                );
                map.insert("content".to_string(), MessageContent::Left(body));
                out.push(map);
            }
            // v0 ignores non-text user content (images/audio/video/document) --
            // GGUF text models can't consume them anyway. The agent loop never
            // produces these for the in-process path in v0.
            UserContent::Image(_)
            | UserContent::Audio(_)
            | UserContent::Video(_)
            | UserContent::Document(_) => {}
        }
    }
    if !text_parts.is_empty() {
        out.push(text_message("user", &text_parts.join("\n")));
    }
}

fn tool_result_text(result: &rig::completion::message::ToolResult) -> String {
    let mut buf = String::new();
    for piece in result.content.iter() {
        match piece {
            rig::completion::message::ToolResultContent::Text(t) => {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(&t.text);
            }
            rig::completion::message::ToolResultContent::Image(_) => {
                // Skipped: GGUF text models can't consume images. Mirrors the
                // user-content handling above.
            }
        }
    }
    buf
}

fn translate_assistant(
    content: &OneOrMany<AssistantContent>,
    out: &mut Vec<IndexMap<String, MessageContent>>,
) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<IndexMap<String, Value>> = Vec::new();

    for part in content.iter() {
        match part {
            AssistantContent::Text(t) => text_parts.push(t.text.clone()),
            AssistantContent::Reasoning(_) | AssistantContent::Image(_) => {}
            AssistantContent::ToolCall(call) => {
                let mut function = IndexMap::new();
                function.insert(
                    "name".to_string(),
                    Value::String(call.function.name.clone()),
                );
                let arguments = serde_json::to_string(&call.function.arguments)
                    .unwrap_or_else(|_| "{}".to_string());
                function.insert("arguments".to_string(), Value::String(arguments));

                let mut entry = IndexMap::new();
                entry.insert("id".to_string(), Value::String(call.id.clone()));
                entry.insert("type".to_string(), Value::String("function".into()));
                entry.insert(
                    "function".to_string(),
                    serde_json::Value::Object(function.into_iter().collect()),
                );
                tool_calls.push(entry);
            }
        }
    }

    let mut map = IndexMap::new();
    map.insert(
        "role".to_string(),
        MessageContent::Left("assistant".to_string()),
    );
    map.insert(
        "content".to_string(),
        MessageContent::Left(text_parts.join("\n")),
    );
    if !tool_calls.is_empty() {
        map.insert("tool_calls".to_string(), MessageContent::Right(tool_calls));
    }
    out.push(map);
}

fn translate_tools(
    rig_tools: &[rig::completion::ToolDefinition],
) -> std::result::Result<Option<Vec<Tool>>, CompletionError> {
    if rig_tools.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(rig_tools.len());
    for tool in rig_tools {
        let parameters = match &tool.parameters {
            Value::Object(map) => Some(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<HashMap<_, _>>(),
            ),
            Value::Null => None,
            other => {
                return Err(CompletionError::ProviderError(format!(
                    "tool {:?} has non-object parameters {:?}; OpenAI-compatible \
                     tool definitions must be JSON objects",
                    tool.name, other
                )));
            }
        };
        out.push(Tool {
            tp: ToolType::Function,
            function: MistralrsFunction {
                description: Some(tool.description.clone()),
                name: tool.name.clone(),
                parameters,
            },
        });
    }
    Ok(Some(out))
}

fn translate_tool_choice(
    rig_choice: Option<&rig::completion::message::ToolChoice>,
    tools: &Option<Vec<Tool>>,
) -> Option<mistralrs_core::ToolChoice> {
    let choice = rig_choice?;
    match choice {
        rig::completion::message::ToolChoice::Auto => Some(mistralrs_core::ToolChoice::Auto),
        rig::completion::message::ToolChoice::None => Some(mistralrs_core::ToolChoice::None),
        // mistralrs has no "Required"; downgrade to Auto and rely on the model.
        rig::completion::message::ToolChoice::Required => Some(mistralrs_core::ToolChoice::Auto),
        rig::completion::message::ToolChoice::Specific { function_names } => {
            let name = function_names.first()?;
            let tool = tools.as_ref()?.iter().find(|t| t.function.name == *name)?;
            Some(mistralrs_core::ToolChoice::Tool(tool.clone()))
        }
    }
}

fn translate_response(
    response: Response,
) -> std::result::Result<CompletionResponse<MistralrsRawResponse>, CompletionError> {
    match response {
        Response::Done(chat) => {
            let raw_value = serde_json::to_value(&chat).map_err(CompletionError::JsonError)?;
            let usage = translate_usage(&chat.usage);
            let choice = translate_choice(&chat)?;
            Ok(CompletionResponse {
                choice,
                usage,
                raw_response: MistralrsRawResponse { raw: raw_value },
                message_id: Some(chat.id),
            })
        }
        Response::ModelError(msg, _partial) => Err(CompletionError::ProviderError(format!(
            "mistralrs model error: {msg}"
        ))),
        Response::InternalError(err) => Err(CompletionError::ProviderError(format!(
            "mistralrs internal error: {err}"
        ))),
        Response::ValidationError(err) => Err(CompletionError::ProviderError(format!(
            "mistralrs validation error: {err}"
        ))),
        Response::Chunk(_) | Response::CompletionChunk(_) => Err(CompletionError::ProviderError(
            "mistralrs returned a streaming chunk for a non-streaming request".into(),
        )),
        Response::CompletionDone(_) | Response::CompletionModelError(_, _) => Err(
            CompletionError::ProviderError("mistralrs returned a non-chat response".into()),
        ),
        Response::ImageGeneration(_)
        | Response::Speech { .. }
        | Response::Raw { .. }
        | Response::Embeddings { .. } => Err(CompletionError::ProviderError(
            "mistralrs returned an unexpected response variant for a chat request".into(),
        )),
    }
}

fn translate_choice(
    chat: &mistralrs_core::ChatCompletionResponse,
) -> std::result::Result<OneOrMany<AssistantContent>, CompletionError> {
    let first = chat.choices.first().ok_or_else(|| {
        CompletionError::ProviderError("mistralrs response had no choices".into())
    })?;
    let mut items: Vec<AssistantContent> = Vec::new();

    if let Some(text) = &first.message.content
        && !text.is_empty()
    {
        items.push(AssistantContent::Text(rig::completion::message::Text {
            text: text.clone(),
        }));
    }

    if let Some(calls) = &first.message.tool_calls {
        for call in calls {
            items.push(AssistantContent::ToolCall(translate_tool_call(call)));
        }
    }

    if items.is_empty() {
        items.push(AssistantContent::Text(rig::completion::message::Text {
            text: String::new(),
        }));
    }

    OneOrMany::many(items).map_err(|e| {
        CompletionError::ProviderError(format!("failed to assemble assistant content: {e}"))
    })
}

fn translate_tool_call(call: &ToolCallResponse) -> ToolCall {
    let arguments = serde_json::from_str::<Value>(&call.function.arguments)
        .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
    ToolCall::new(
        call.id.clone(),
        ToolFunction::new(call.function.name.clone(), arguments),
    )
}

fn translate_usage(usage: &mistralrs_core::Usage) -> Usage {
    Usage {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.completion_tokens as u64,
        total_tokens: usage.total_tokens as u64,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mistralrs_core::{Choice, ResponseMessage, ToolCallType};

    fn chat_done(content: Option<&str>, calls: Option<Vec<ToolCallResponse>>) -> Response {
        Response::Done(mistralrs_core::ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            choices: vec![Choice {
                finish_reason: "stop".into(),
                index: 0,
                message: ResponseMessage {
                    content: content.map(str::to_string),
                    role: "assistant".into(),
                    tool_calls: calls,
                    reasoning_content: None,
                },
                logprobs: None,
            }],
            created: 0,
            model: "test".into(),
            system_fingerprint: "local".into(),
            object: "chat.completion".into(),
            usage: mistralrs_core::Usage {
                completion_tokens: 1,
                prompt_tokens: 2,
                total_tokens: 3,
                avg_tok_per_sec: 0.0,
                avg_prompt_tok_per_sec: 0.0,
                avg_compl_tok_per_sec: 0.0,
                total_time_sec: 0.0,
                total_prompt_time_sec: 0.0,
                total_completion_time_sec: 0.0,
            },
        })
    }

    fn tool_call(name: &str, args: &str) -> ToolCallResponse {
        ToolCallResponse {
            index: 0,
            id: "call-1".into(),
            tp: ToolCallType::Function,
            function: mistralrs_core::CalledFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn malformed_tool_call_arguments_fall_back_to_string() {
        // mistralrs: arguments is a String. rig: arguments is a parsed Value.
        // If the model emits something that isn't valid JSON, the shim must
        // surface the raw payload as a string instead of erroring -- the agent
        // loop produces a clearer downstream error against the tool's schema
        // than the shim could.
        let response = chat_done(None, Some(vec![tool_call("foo", "not json")]));
        let translated = translate_response(response).expect("response translates");
        let mut iter = translated.choice.iter();
        let first = iter.next().expect("at least one choice");
        let AssistantContent::ToolCall(call) = first else {
            panic!("expected tool call, got {first:?}");
        };
        assert_eq!(call.function.name, "foo");
        assert_eq!(call.function.arguments, Value::String("not json".into()));
    }

    #[test]
    fn well_formed_tool_call_arguments_parse() {
        let response = chat_done(None, Some(vec![tool_call("foo", r#"{"a":1}"#)]));
        let translated = translate_response(response).expect("response translates");
        let mut iter = translated.choice.iter();
        let first = iter.next().expect("at least one choice");
        let AssistantContent::ToolCall(call) = first else {
            panic!("expected tool call, got {first:?}");
        };
        assert_eq!(call.function.arguments, serde_json::json!({ "a": 1 }));
    }

    #[test]
    fn empty_assistant_content_yields_empty_text() {
        let response = chat_done(None, None);
        let translated = translate_response(response).expect("response translates");
        let mut iter = translated.choice.iter();
        let first = iter.next().expect("at least one choice");
        let AssistantContent::Text(t) = first else {
            panic!("expected text, got {first:?}");
        };
        assert_eq!(t.text, "");
    }

    #[test]
    fn streaming_chunk_for_non_streaming_request_errors() {
        let chunk = mistralrs_core::ChatCompletionChunkResponse {
            id: "x".into(),
            choices: vec![],
            created: 0,
            model: "test".into(),
            system_fingerprint: "local".into(),
            object: "chat.completion.chunk".into(),
            usage: None,
        };
        let err = translate_response(Response::Chunk(chunk)).unwrap_err();
        assert!(
            matches!(err, CompletionError::ProviderError(ref msg) if msg.contains("streaming chunk")),
            "got: {err:?}",
        );
    }
}
