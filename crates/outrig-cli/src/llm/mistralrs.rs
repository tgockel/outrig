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

use hf_hub::api::sync::{Api, ApiBuilder, ApiError, ApiRepo};
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
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage, Usage,
};
use rig::streaming::{
    RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse, StreamingResult,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::info;

use crate::error::Result;
use crate::llm::{LlmResolveError, is_blank, retry};
use outrig::config::MistralrsDeviceSpec;

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
}

/// Round-trip-friendly wrapper for `mistralrs_core::ChatCompletionResponse`.
/// The trait bound on `CompletionModel::Response` requires
/// `Serialize + DeserializeOwned`; mistralrs's response type is `Serialize`
/// only, so we stash the JSON form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MistralrsRawResponse {
    pub raw: Value,
}

/// Final metadata surfaced by Rig's streaming interface. mistralrs sends
/// usage on the final chunk, so the stream can preserve token accounting even
/// though chunk response types themselves are serialize-only.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MistralrsStreamResponse {
    pub usage: Option<Usage>,
}

impl GetTokenUsage for MistralrsStreamResponse {
    fn token_usage(&self) -> Usage {
        self.usage.unwrap_or_default()
    }
}

/// Load a mistralrs engine. Exactly one of `model_id` / `model_path` must
/// be set; `validate.rs` enforces this at config-load time so reaching the
/// "neither" or "both" branch here is an internal bug.
///
/// A `model_id`'s files are downloaded into `cache_root` by [`fetch_snapshot`],
/// and mistralrs is then handed the directory they landed in, as it is for a
/// `model_path`. Given the repo id itself, mistralrs-core 0.8.1 downloads into
/// Hugging Face's own cache whatever cache it was configured with (#423).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load(
    model_name: &str,
    model_id: Option<&str>,
    model_path: Option<&Path>,
    model_file: Option<&[String]>,
    revision: Option<&str>,
    context_length: Option<u32>,
    device_spec: MistralrsDeviceSpec,
    cache_root: &Path,
) -> Result<MistralrsModel> {
    // mistralrs builds a hub client before it looks at the directory it was
    // handed, and with no cache set it uses `Cache::default()`, which panics
    // where no home directory resolves. Nothing is downloaded through it.
    let _ = GLOBAL_HF_CACHE.set(hf_hub::Cache::new(cache_root.to_path_buf()));

    let load_err = |source: anyhow::Error| LlmResolveError::MistralrsLoad {
        model: model_name.to_string(),
        source,
    };

    let (quantized_model_id, quantized_filenames, hf_revision) = match (model_id, model_path) {
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
            (id.to_string(), files.to_vec(), revision.map(str::to_string))
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
    let device = candle_device(model_name, device_spec)?;
    // `Some` for a `model-id`, whose files are fetched into it.
    let fetch_root = model_id.is_some().then(|| cache_root.to_path_buf());
    // The download and the load are synchronous and can take minutes. On the
    // blocking pool they leave the runtime free to hear a signal: the session
    // then ends around a load still running, and whatever it produces is
    // dropped with the task that nothing is waiting on anymore. The download
    // starts only after the device check above, so a device this build cannot
    // drive fails before gigabytes are fetched.
    let pipeline = tokio::task::spawn_blocking(move || {
        // A `model-id` is still a repo id here; mistralrs needs the directory
        // holding its files, as a `model-path` already gives it.
        let model_dir = match fetch_root {
            Some(root) => {
                let api = ApiBuilder::from_cache(hf_hub::Cache::new(root.clone()))
                    // `from_cache` reads a token file beside the cache; this
                    // replaces it, `None` included.
                    .with_token(hf_token())
                    .build()?;
                fetch_snapshot(
                    &api,
                    &quantized_model_id,
                    &quantized_filenames,
                    hf_revision.as_deref(),
                    &root,
                )?
            }
            None => quantized_model_id,
        };
        GGUFLoaderBuilder::new(
            None,
            None,
            model_dir,
            quantized_filenames,
            GGUFSpecificConfig::default(),
            false,
            None,
        )
        .build()
        .load_model_from_hf(
            None,
            TokenSource::None,
            &ModelDType::Auto,
            &device,
            false,
            DeviceMapSetting::Auto(AutoDeviceMapParams::default_text()),
            None,
            None,
        )
    })
    .await
    .map_err(|e| load_err(anyhow::anyhow!("model load task failed: {e}")))?
    .map_err(load_err)?;

    info!(
        model = model_name,
        device = %device_spec,
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

    Ok(MistralrsModel { engine })
}

/// What mistralrs reads from beside the weights for the stop tokens a GGUF's own
/// metadata does not name, such as Llama 3's `<|eom_id|>`.
const GENERATION_CONFIG: &str = "generation_config.json";

/// Fetch `files` of the Hugging Face repo `model_id` at `revision` (default
/// `main`) with `api`, whose cache is `cache_root`, and return the snapshot
/// directory that holds them all.
///
/// outrig fetches, and mistralrs-core 0.8.1 does not, because mistralrs
/// replaces whatever cache it is given with Hugging Face's own (`$HF_HUB_CACHE`,
/// else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`) whenever one resolves,
/// which is on any host with a home directory.
///
/// The named files, and the repo's [`GENERATION_CONFIG`] when it has one, which
/// lands in the same snapshot for mistralrs to find there. mistralrs also
/// fetched `chat_template.json`, `preprocessor_config.json` and
/// `processor_config.json`; a GGUF carries its own chat template, and the
/// `model-path` form never had them either.
///
/// Blocking, so `load` runs it on the blocking pool. Cache-first: when every
/// named file is cached under `revision` the hub is asked about nothing, so a
/// cached model loads with no network, and one that another tool cached without
/// the config stays without it. Otherwise the hub is asked which commit
/// `revision` is at and everything is fetched at that commit, so that a branch
/// advancing mid-load cannot split the files between two snapshots; `revision`
/// is then left pointing at it. The config comes before the weights, because
/// once they are cached the next load counts as cached: a failure after them
/// would leave the model without the config for good. `api` is a parameter so
/// that a test can run this against a hub and a cache of its own. `files` must
/// not be empty.
fn fetch_snapshot(
    api: &Api,
    model_id: &str,
    files: &[String],
    revision: Option<&str>,
    cache_root: &Path,
) -> anyhow::Result<String> {
    // hf-hub joins a file name onto the snapshot directory as given, so a root
    // or a `..` would write outside it.
    for file in files {
        anyhow::ensure!(
            !file.split('/').any(|part| matches!(part, "" | "." | "..")),
            "model-file {file:?} must be a relative path without empty, `.` or `..` components"
        );
    }
    let revision = revision.unwrap_or("main");
    let repo_at = |rev: &str| {
        hf_hub::Repo::with_revision(model_id.into(), hf_hub::RepoType::Model, rev.into())
    };
    let cache = hf_hub::Cache::new(cache_root.to_path_buf());
    let cached = cache.repo(repo_at(revision));
    let get = |repo: &ApiRepo, file: &str| {
        repo.get(file).map_err(|err| {
            let what = format!(
                "fetch {file:?} from Hugging Face repo {model_id:?} at revision {revision:?} \
                 into {cache_root:?}"
            );
            hub_error(what, err)
        })
    };

    if files.iter().any(|file| cached.get(file).is_none()) {
        let info = api.repo(repo_at(revision)).info().map_err(|err| {
            let what = format!(
                "look up the commit of revision {revision:?} of Hugging Face repo {model_id:?}"
            );
            hub_error(what, err)
        })?;
        let commit = info.sha;
        let record = |target: &hf_hub::CacheRepo| {
            target.create_ref(&commit).map_err(|err| {
                let what = format!(
                    "record the commit of revision {revision:?} of Hugging Face repo \
                     {model_id:?} in {cache_root:?}"
                );
                hub_error(what, err.into())
            })
        };
        // hf-hub finds a cached file through the ref of the revision it is asked
        // for, and a handle at the commit has none until it downloads something:
        // without one, a file cached by an interrupted load would be fetched
        // again.
        record(&cache.repo(repo_at(&commit)))?;
        let pinned = api.repo(repo_at(&commit));
        if info
            .siblings
            .iter()
            .any(|sibling| sibling.rfilename == GENERATION_CONFIG)
        {
            get(&pinned, GENERATION_CONFIG)?;
        }
        for file in files {
            get(&pinned, file)?;
        }
        record(&cached)?;
    }

    // `get` returns `<snapshot>/<file>`, and the file has one component per
    // `/`-separated part.
    let mut dir = cached.get(&files[0]).ok_or_else(|| {
        anyhow::anyhow!(
            "{:?} is not in {cache_root:?} after it was fetched\nhelp: another process may \
             have moved revision {revision:?} meanwhile; run again",
            files[0]
        )
    })?;
    for _ in files[0].split('/') {
        dir.pop();
    }
    // A lossy conversion would name a directory that does not exist, which
    // mistralrs reads as a repo id and downloads into the Hugging Face cache.
    dir.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("model-cache-root {cache_root:?} is not valid UTF-8"))
}

/// The error for a failed hub request or cache write: `what` is what failed,
/// after "failed to", and a `help:` line follows for the failures that have a
/// usual cause.
fn hub_error(what: String, err: ApiError) -> anyhow::Error {
    let text = err.to_string();
    let mut msg = format!("failed to {what}: {text}");
    if ["status code 401", "status code 403", "status code 404"]
        .iter()
        .any(|status| text.contains(status))
    {
        msg += "\nhelp: check model-id, model-file and revision; a private or gated \
                repo needs a token (set HF_TOKEN or run `huggingface-cli login`)";
    }
    if matches!(err, ApiError::IoError(_)) {
        msg += "\nhelp: model-cache-root must be writable and on a filesystem that \
                supports symbolic links";
    }
    anyhow::anyhow!(msg)
}

/// The Hugging Face token mistralrs's `TokenSource::CacheToken` used:
/// `HF_TOKEN`, then `HF_HUB_TOKEN`, then the file `huggingface-cli login`
/// writes. A blank value counts as unset; no token means an anonymous download.
fn hf_token() -> Option<String> {
    let usable = |raw: String| Some(raw.trim().to_string()).filter(|token| !token.is_empty());
    ["HF_TOKEN", "HF_HUB_TOKEN"]
        .into_iter()
        .find_map(|name| usable(std::env::var(name).ok()?))
        .or_else(|| usable(std::fs::read_to_string(mistralrs_core::hf_token_path()?).ok()?))
}

fn candle_device(model_name: &str, spec: MistralrsDeviceSpec) -> Result<candle_core::Device> {
    match spec {
        MistralrsDeviceSpec::Cpu => Ok(candle_core::Device::Cpu),
        MistralrsDeviceSpec::Cuda(ordinal) => {
            #[cfg(feature = "cuda")]
            {
                candle_core::Device::new_cuda(ordinal).map_err(|source| {
                    LlmResolveError::MistralrsLoad {
                        model: model_name.to_string(),
                        source: anyhow::anyhow!("cuda device {ordinal} init: {source}"),
                    }
                    .into()
                })
            }
            #[cfg(not(feature = "cuda"))]
            {
                let _ = ordinal;
                Err(LlmResolveError::MistralrsDeviceUnavailable {
                    model: model_name.to_string(),
                    device: spec.to_string(),
                    feature: "cuda",
                }
                .into())
            }
        }
        MistralrsDeviceSpec::Metal => {
            #[cfg(all(feature = "metal", target_os = "macos"))]
            {
                candle_core::Device::new_metal(0).map_err(|source| {
                    LlmResolveError::MistralrsLoad {
                        model: model_name.to_string(),
                        source: anyhow::anyhow!("metal device 0 init: {source}"),
                    }
                    .into()
                })
            }
            #[cfg(all(feature = "metal", not(target_os = "macos")))]
            {
                Err(LlmResolveError::MistralrsLoad {
                    model: model_name.to_string(),
                    source: anyhow::anyhow!("metal backend requires a macOS target"),
                }
                .into())
            }
            #[cfg(not(feature = "metal"))]
            {
                Err(LlmResolveError::MistralrsDeviceUnavailable {
                    model: model_name.to_string(),
                    device: spec.to_string(),
                    feature: "metal",
                }
                .into())
            }
        }
        // `MistralrsDeviceSpec` is `#[non_exhaustive]`; a device kind this
        // build has no candle backend for cannot be silently downgraded to
        // CPU, which would run the model far slower than asked without
        // saying so.
        _ => Err(LlmResolveError::MistralrsLoad {
            model: model_name.to_string(),
            source: anyhow::anyhow!("device {spec} is not supported by this build"),
        }
        .into()),
    }
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
    type StreamingResponse = MistralrsStreamResponse;
    type Client = MistralrsClient;

    /// The engine holds one model and a request reaches it without naming it
    /// (see [`build_normal_request`]), so the name has nothing to select.
    fn make(client: &Self::Client, _model: impl Into<String>) -> Self {
        Self {
            engine: client.engine.clone(),
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
        // Read off the request before it is consumed, as `RetryingModel` does
        // for a remote turn: a reply that showed nothing is reported with the
        // ceiling that was in force.
        let max_tokens = request.max_tokens;
        let (tx, mut rx) = mpsc::channel::<Response>(1);
        let normal = build_normal_request(request, tx, false)?;
        let request_for_engine = Request::Normal(Box::new(normal));

        dispatch_request(self.engine.clone(), request_for_engine).await?;

        let response = rx.recv().await.ok_or_else(|| {
            CompletionError::ProviderError(
                "mistralrs engine closed the response channel without replying".into(),
            )
        })?;

        let finish_reason = finish_reason_of(&response).map(str::to_string);
        let completion = translate_response(response)?;
        // The primary agent streams, so this is the path of a local row in a
        // model alias chain. Nothing wraps that row in `RetryingModel`, and the
        // chain erases `raw_response` past here, so this is the last place that
        // can name the finish reason and the ceiling that ended the turn.
        if !retry::showed_something(&completion.choice) {
            eprintln!(
                "{}",
                retry::textless_line(finish_reason.as_deref(), max_tokens)
            );
        }
        Ok(completion)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError>
    {
        // Read off the request before it is consumed; see `completion`.
        let max_tokens = request.max_tokens;
        let (tx, rx) = mpsc::channel::<Response>(1);
        let normal = build_normal_request(request, tx, true)?;
        let request_for_engine = Request::Normal(Box::new(normal));

        dispatch_request(self.engine.clone(), request_for_engine).await?;

        let stream = response_stream(rx, max_tokens);
        Ok(StreamingCompletionResponse::stream(stream))
    }
}

/// The stream rig reads for one request, fed by the engine's response channel.
///
/// Split out of [`MistralrsModel::stream`] so that everything past the engine
/// can be driven from a scripted channel, without loading any weights. Callers
/// wrap it in a [`StreamingCompletionResponse`]; it is returned unwrapped
/// because that wrapper drops every final response after the first, and a test
/// needs to see whether a second one was sent.
///
/// `max_tokens` is the ceiling the request carried, which the report on a reply
/// that showed nothing names.
fn response_stream(
    mut rx: mpsc::Receiver<Response>,
    max_tokens: Option<u64>,
) -> StreamingResult<MistralrsStreamResponse> {
    Box::pin(async_stream::try_stream! {
        let mut state = MistralrsStreamState::default();
        while let Some(response) = rx.recv().await {
            for item in state.translate(response)? {
                yield item;
            }
        }
        // mistralrs-core 0.8.1 drops a reply's last chunk, the one with the
        // finish reason and usage, when its unsent bytes end partway through a
        // multi-byte character, as a max-tokens stop inside an emoji does. Text
        // it was still holding back goes with it: the start of what might be a
        // reasoning tag, or everything since the reply began to look like a
        // tool call, which can be all of it. That is a reply cut short, not a
        // failed one, even when nothing arrived: end it as a finished reply
        // ends, with no usage, and warn that text may be missing.
        if !state.saw_final_response {
            eprintln!(
                "{}",
                crate::llm::labeled_line(
                    "warning: the local model's reply ended without a finish reason, \
                     so some or all of it may be missing. The likely cause is mistralrs \
                     dropping the last chunk of a reply that stops partway through a \
                     multi-byte character, such as an emoji cut off at max-tokens."
                )
            );
            yield RawStreamingChoice::FinalResponse(MistralrsStreamResponse { usage: None });
        }
        // The report a remote turn gets from `RetryingModel`, for a reply that
        // was all reasoning, such as one cut off at `max-tokens` inside its
        // `<think>` block. It is made here and not by `run_turn_streaming_inner`
        // because rig forwards a turn's final response to its caller only when
        // the turn streamed text, so nothing above the model ever sees this
        // turn's finish reason. Rig reads this stream to its end, so this runs
        // before the turn does.
        if let Some(line) = state.textless_report(max_tokens) {
            eprintln!("{line}");
        }
    })
}

async fn dispatch_request(
    engine: Arc<MistralRs>,
    request_for_engine: Request,
) -> std::result::Result<(), CompletionError> {
    tokio::task::spawn_blocking(move || engine.send_request(request_for_engine))
        .await
        .map_err(|e| CompletionError::ProviderError(format!("mistralrs join error: {e}")))?
        .map_err(|e| CompletionError::ProviderError(format!("mistralrs send_request: {e}")))
}

fn build_normal_request(
    req: CompletionRequest,
    response_tx: mpsc::Sender<Response>,
    is_streaming: bool,
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
        is_streaming,
        id: 0,
        constraint: mistralrs_core::Constraint::None,
        suffix: None,
        tools,
        tool_choice,
        logits_processors: None,
        return_raw_logits: false,
        web_search_options: None,
        // Named, a model must match the id mistralrs registered it under, which
        // it derives itself: for a `model-path` GGUF that is the file's
        // directory, not the file. The engine holds only the model `load`
        // built, so naming none routes every request to it.
        model_id: None,
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
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<IndexMap<String, Value>> = Vec::new();

    for part in content.iter() {
        match part {
            AssistantContent::Text(t) => text_parts.push(t.text.clone()),
            AssistantContent::Reasoning(r) => reasoning_parts.push(r.display_text()),
            AssistantContent::Image(_) => {}
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
    // Handed to the chat template as-is; whether it renders the key (Qwen3's
    // does, for a step after the last user message) is the template's call.
    let reasoning = reasoning_parts.join("\n");
    if !reasoning.trim().is_empty() {
        map.insert(
            "reasoning_content".to_string(),
            MessageContent::Left(reasoning),
        );
    }
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

fn translate_stream_response(
    response: Response,
) -> std::result::Result<Vec<RawStreamingChoice<MistralrsStreamResponse>>, CompletionError> {
    match response {
        Response::Chunk(chunk) => Ok(translate_stream_chunk(chunk)),
        Response::Done(chat) => translate_stream_done(chat),
        Response::ModelError(msg, _partial) => Err(CompletionError::ProviderError(format!(
            "mistralrs model error: {msg}"
        ))),
        Response::InternalError(err) => Err(CompletionError::ProviderError(format!(
            "mistralrs internal error: {err}"
        ))),
        Response::ValidationError(err) => Err(CompletionError::ProviderError(format!(
            "mistralrs validation error: {err}"
        ))),
        Response::CompletionChunk(_)
        | Response::CompletionDone(_)
        | Response::CompletionModelError(_, _) => Err(CompletionError::ProviderError(
            "mistralrs returned a non-chat response".into(),
        )),
        Response::ImageGeneration(_)
        | Response::Speech { .. }
        | Response::Raw { .. }
        | Response::Embeddings { .. } => Err(CompletionError::ProviderError(
            "mistralrs returned an unexpected response variant for a chat request".into(),
        )),
    }
}

#[derive(Debug, Default)]
struct MistralrsStreamState {
    saw_chunk: bool,
    saw_final_response: bool,
    /// Non-blank text or a tool call has streamed; reasoning does not count.
    showed_something: bool,
    /// The first finish reason mistralrs reported for the reply.
    finish_reason: Option<String>,
}

impl MistralrsStreamState {
    fn translate(
        &mut self,
        response: Response,
    ) -> std::result::Result<Vec<RawStreamingChoice<MistralrsStreamResponse>>, CompletionError>
    {
        if self.finish_reason.is_none() {
            self.finish_reason = finish_reason_of(&response).map(str::to_string);
        }
        let items = match response {
            Response::Chunk(chunk) => {
                self.saw_chunk = true;
                translate_stream_chunk(chunk)
            }
            Response::Done(chat) if self.saw_chunk => {
                if self.saw_final_response {
                    Vec::new()
                } else {
                    vec![RawStreamingChoice::FinalResponse(MistralrsStreamResponse {
                        usage: Some(translate_usage(&chat.usage)),
                    })]
                }
            }
            other => translate_stream_response(other)?,
        };

        self.showed_something |= items.iter().any(|item| match item {
            RawStreamingChoice::Message(text) => !is_blank(text),
            RawStreamingChoice::ToolCall(_) => true,
            _ => false,
        });
        if items
            .iter()
            .any(|item| matches!(item, RawStreamingChoice::FinalResponse(_)))
        {
            self.saw_final_response = true;
        }

        Ok(items)
    }

    /// The report for a reply that ended on a final response and showed the user
    /// nothing, in the words a remote turn's report uses.
    ///
    /// A stream that ended without one had its last chunk dropped, so it may be
    /// missing some of its reply; [`response_stream`] warns about that instead.
    fn textless_report(&self, max_tokens: Option<u64>) -> Option<String> {
        (self.saw_final_response && !self.showed_something)
            .then(|| retry::textless_line(self.finish_reason.as_deref(), max_tokens))
    }
}

/// Why mistralrs says a reply ended: the finish reason on a streamed reply's
/// last chunk, or on a whole reply.
fn finish_reason_of(response: &Response) -> Option<&str> {
    match response {
        Response::Chunk(chunk) => chunk.choices.first()?.finish_reason.as_deref(),
        Response::Done(chat) => Some(chat.choices.first()?.finish_reason.as_str()),
        _ => None,
    }
}

fn translate_stream_chunk(
    chunk: mistralrs_core::ChatCompletionChunkResponse,
) -> Vec<RawStreamingChoice<MistralrsStreamResponse>> {
    let mut out = Vec::new();
    if !chunk.id.is_empty() {
        out.push(RawStreamingChoice::MessageId(chunk.id.clone()));
    }

    let usage = chunk.usage.as_ref().map(translate_usage);
    let mut final_chunk = usage.is_some();

    for choice in chunk.choices {
        if choice.finish_reason.is_some() {
            final_chunk = true;
        }
        // Ahead of the text: a delta that straddles a closing `</think>`
        // carries both, and the reasoning is the part generated first.
        if let Some(reasoning) = choice.delta.reasoning_content
            && !reasoning.is_empty()
        {
            out.push(RawStreamingChoice::ReasoningDelta {
                id: None,
                reasoning,
            });
        }
        if let Some(text) = choice.delta.content
            && !text.is_empty()
        {
            out.push(RawStreamingChoice::Message(text));
        }
        if let Some(calls) = choice.delta.tool_calls {
            for call in calls {
                out.push(RawStreamingChoice::ToolCall(raw_streaming_tool_call(call)));
            }
        }
    }

    if final_chunk {
        out.push(RawStreamingChoice::FinalResponse(MistralrsStreamResponse {
            usage,
        }));
    }

    out
}

fn translate_stream_done(
    chat: mistralrs_core::ChatCompletionResponse,
) -> std::result::Result<Vec<RawStreamingChoice<MistralrsStreamResponse>>, CompletionError> {
    let usage = translate_usage(&chat.usage);
    let message_id = chat.id.clone();
    let choice = translate_choice(&chat)?;
    let mut out = vec![RawStreamingChoice::MessageId(message_id)];

    for item in choice.iter() {
        match item {
            AssistantContent::Text(text) if !text.text.is_empty() => {
                out.push(RawStreamingChoice::Message(text.text.clone()));
            }
            AssistantContent::Reasoning(reasoning) => {
                out.push(RawStreamingChoice::ReasoningDelta {
                    id: None,
                    reasoning: reasoning.display_text(),
                });
            }
            AssistantContent::ToolCall(call) => {
                out.push(RawStreamingChoice::ToolCall(RawStreamingToolCall::new(
                    call.id.clone(),
                    call.function.name.clone(),
                    call.function.arguments.clone(),
                )));
            }
            AssistantContent::Text(_) | AssistantContent::Image(_) => {}
        }
    }

    out.push(RawStreamingChoice::FinalResponse(MistralrsStreamResponse {
        usage: Some(usage),
    }));
    Ok(out)
}

fn raw_streaming_tool_call(call: ToolCallResponse) -> RawStreamingToolCall {
    RawStreamingToolCall::new(
        call.id,
        call.function.name,
        parse_tool_arguments(&call.function.arguments),
    )
}

fn translate_choice(
    chat: &mistralrs_core::ChatCompletionResponse,
) -> std::result::Result<OneOrMany<AssistantContent>, CompletionError> {
    let first = chat.choices.first().ok_or_else(|| {
        CompletionError::ProviderError("mistralrs response had no choices".into())
    })?;
    let mut items: Vec<AssistantContent> = Vec::new();

    // mistralrs *moves* a reasoning model's `<think>` block (or Harmony
    // analysis channel) out of `content` into this field, so it is the only
    // copy. Dropped, a turn that was all reasoning would reach the agent loop
    // as the empty-text sentinel below and be reported as no content at all.
    if let Some(reasoning) = &first.message.reasoning_content
        && !reasoning.is_empty()
    {
        items.push(AssistantContent::reasoning(reasoning));
    }

    if let Some(text) = &first.message.content
        && !text.is_empty()
    {
        items.push(AssistantContent::Text(rig::completion::message::Text::new(
            text.clone(),
        )));
    }

    if let Some(calls) = &first.message.tool_calls {
        for call in calls {
            items.push(AssistantContent::ToolCall(translate_tool_call(call)));
        }
    }

    if items.is_empty() {
        items.push(AssistantContent::Text(rig::completion::message::Text::new(
            String::new(),
        )));
    }

    OneOrMany::many(items).map_err(|e| {
        CompletionError::ProviderError(format!("failed to assemble assistant content: {e}"))
    })
}

fn translate_tool_call(call: &ToolCallResponse) -> ToolCall {
    ToolCall::new(
        call.id.clone(),
        ToolFunction::new(
            call.function.name.clone(),
            parse_tool_arguments(&call.function.arguments),
        ),
    )
}

fn parse_tool_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn translate_usage(usage: &mistralrs_core::Usage) -> Usage {
    Usage {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.completion_tokens as u64,
        total_tokens: usage.total_tokens as u64,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        tool_use_prompt_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use mistralrs_core::{Choice, ChunkChoice, Delta, ResponseMessage, ToolCallType};
    use rig::streaming::StreamedAssistantContent;
    use std::io::{BufRead, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use tempfile::TempDir;

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

    fn usage() -> mistralrs_core::Usage {
        mistralrs_core::Usage {
            completion_tokens: 1,
            prompt_tokens: 2,
            total_tokens: 3,
            avg_tok_per_sec: 0.0,
            avg_prompt_tok_per_sec: 0.0,
            avg_compl_tok_per_sec: 0.0,
            total_time_sec: 0.0,
            total_prompt_time_sec: 0.0,
            total_completion_time_sec: 0.0,
        }
    }

    fn stream_chunk(
        content: Option<&str>,
        calls: Option<Vec<ToolCallResponse>>,
        finish_reason: Option<&str>,
        usage: Option<mistralrs_core::Usage>,
    ) -> Response {
        Response::Chunk(mistralrs_core::ChatCompletionChunkResponse {
            id: "chunk-test".into(),
            choices: vec![ChunkChoice {
                finish_reason: finish_reason.map(str::to_string),
                index: 0,
                delta: Delta {
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
            object: "chat.completion.chunk".into(),
            usage,
        })
    }

    /// `response` with `reasoning` split out of its first choice, as mistralrs
    /// files a `<think>` block when the model's chat template declares one.
    fn with_reasoning(response: Response, reasoning: &str) -> Response {
        match response {
            Response::Done(mut chat) => {
                chat.choices[0].message.reasoning_content = Some(reasoning.to_string());
                Response::Done(chat)
            }
            Response::Chunk(mut chunk) => {
                chunk.choices[0].delta.reasoning_content = Some(reasoning.to_string());
                Response::Chunk(chunk)
            }
            _ => unreachable!("only chat responses carry reasoning"),
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

    /// A turn that was all reasoning -- typically one cut off at `max-tokens`
    /// inside its `<think>` block -- reaches rig as reasoning. Erased, it came
    /// through as the empty-text sentinel above, and the agent loop reported a
    /// model that had produced no content at all.
    #[test]
    fn reasoning_only_response_is_carried_as_reasoning() {
        let response = with_reasoning(chat_done(None, None), "weighing the options");
        let translated = translate_response(response).expect("response translates");
        let parts: Vec<_> = translated.choice.iter().collect();

        assert!(
            matches!(
                parts.as_slice(),
                [AssistantContent::Reasoning(r)] if r.display_text() == "weighing the options"
            ),
            "the reasoning alone, with no sentinel beside it: {parts:?}",
        );
    }

    /// The reasoning is moved out of `content`, not copied, so a turn with a
    /// reply loses it too unless it is carried alongside -- ahead of the text,
    /// the order the model generated them in.
    #[test]
    fn reasoning_precedes_the_text_it_was_split_from() {
        let response = with_reasoning(chat_done(Some("the answer"), None), "weighing it");
        let translated = translate_response(response).expect("response translates");
        let parts: Vec<_> = translated.choice.iter().collect();

        assert!(
            matches!(
                parts.as_slice(),
                [AssistantContent::Reasoning(r), AssistantContent::Text(t)]
                    if r.display_text() == "weighing it" && t.text == "the answer"
            ),
            "got: {parts:?}",
        );
    }

    fn replayed_step(parts: Vec<AssistantContent>) -> IndexMap<String, MessageContent> {
        let history = OneOrMany::many(vec![Message::Assistant {
            id: None,
            content: OneOrMany::many(parts).expect("non-empty"),
        }])
        .expect("non-empty");
        translate_messages(None, &history).remove(0)
    }

    fn tool_call_part() -> AssistantContent {
        AssistantContent::tool_call("call-1", "shell__exec", serde_json::json!({"cmd": "ls"}))
    }

    /// Qwen3's template renders `<think>` for a tool-loop step only from a
    /// string `reasoning_content` key; without it the model gets the tool
    /// result back minus the plan that asked for it.
    #[test]
    fn replayed_reasoning_is_carried_as_reasoning_content() {
        let map = replayed_step(vec![
            AssistantContent::reasoning("run cargo test first"),
            tool_call_part(),
        ]);

        assert!(matches!(
            map.get("reasoning_content"),
            Some(MessageContent::Left(r)) if r == "run cargo test first"
        ));
        assert!(map.contains_key("tool_calls"), "the call is still replayed");
    }

    #[test]
    fn replayed_step_without_reasoning_has_no_reasoning_content() {
        let map = replayed_step(vec![tool_call_part()]);
        assert!(!map.contains_key("reasoning_content"));
    }

    #[test]
    fn replayed_blank_reasoning_has_no_reasoning_content() {
        let map = replayed_step(vec![AssistantContent::reasoning(" \n"), tool_call_part()]);
        assert!(!map.contains_key("reasoning_content"));
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

    #[test]
    fn streaming_text_chunks_translate_in_order() {
        let first = translate_stream_response(stream_chunk(Some("hel"), None, None, None))
            .expect("first chunk translates");
        let second =
            translate_stream_response(stream_chunk(Some("lo"), None, Some("stop"), Some(usage())))
                .expect("second chunk translates");

        let RawStreamingChoice::Message(text) = &first[1] else {
            panic!("expected text message, got {:?}", first[1]);
        };
        assert_eq!(text, "hel");

        let RawStreamingChoice::Message(text) = &second[1] else {
            panic!("expected text message, got {:?}", second[1]);
        };
        assert_eq!(text, "lo");

        let RawStreamingChoice::FinalResponse(final_response) = &second[2] else {
            panic!("expected final response, got {:?}", second[2]);
        };
        assert_eq!(
            final_response.usage.expect("usage"),
            translate_usage(&usage())
        );
    }

    #[test]
    fn streaming_tool_call_chunk_translates_to_complete_tool_call() {
        let items = translate_stream_response(stream_chunk(
            None,
            Some(vec![tool_call("foo", r#"{"a":1}"#)]),
            Some("tool_calls"),
            Some(usage()),
        ))
        .expect("tool chunk translates");

        let RawStreamingChoice::ToolCall(call) = &items[1] else {
            panic!("expected tool call, got {:?}", items[1]);
        };
        assert_eq!(call.id, "call-1");
        assert_eq!(call.name, "foo");
        assert_eq!(call.arguments, serde_json::json!({ "a": 1 }));
    }

    /// The delta that closes a `</think>` block carries the last of the
    /// reasoning and the first of the reply, and they stream in that order.
    #[test]
    fn streaming_reasoning_delta_translates_ahead_of_text() {
        let items = translate_stream_response(with_reasoning(
            stream_chunk(Some("so: "), None, None, None),
            "done.",
        ))
        .expect("straddling chunk translates");

        assert!(
            matches!(
                &items[1..],
                [
                    RawStreamingChoice::ReasoningDelta { id: None, reasoning },
                    RawStreamingChoice::Message(text),
                ] if reasoning == "done." && text == "so: "
            ),
            "got: {items:?}",
        );
    }

    /// A `Done` with no chunk before it is translated whole, into the items a
    /// chunk with the same content would have produced -- reasoning included,
    /// which this arm used to discard outright.
    #[test]
    fn streaming_done_without_chunks_carries_reasoning() {
        let mut state = MistralrsStreamState::default();
        let items = state
            .translate(with_reasoning(chat_done(None, None), "weighing the options"))
            .expect("done translates");

        assert!(
            matches!(
                items.as_slice(),
                [
                    RawStreamingChoice::MessageId(_),
                    RawStreamingChoice::ReasoningDelta { reasoning, .. },
                    RawStreamingChoice::FinalResponse(_),
                ] if reasoning == "weighing the options"
            ),
            "got: {items:?}",
        );
    }

    #[test]
    fn streaming_done_after_final_chunk_does_not_duplicate_text() {
        let mut state = MistralrsStreamState::default();

        let first = state
            .translate(stream_chunk(Some("hel"), None, None, None))
            .expect("first chunk translates");
        let second = state
            .translate(stream_chunk(Some("lo"), None, Some("stop"), Some(usage())))
            .expect("final chunk translates");
        let done = state
            .translate(chat_done(Some("hello"), None))
            .expect("done translates");

        assert!(
            matches!(&first[1], RawStreamingChoice::Message(text) if text == "hel"),
            "got: {first:?}",
        );
        assert!(
            matches!(&second[1], RawStreamingChoice::Message(text) if text == "lo"),
            "got: {second:?}",
        );
        assert!(
            matches!(&second[2], RawStreamingChoice::FinalResponse(_)),
            "got: {second:?}",
        );
        assert!(done.is_empty(), "got: {done:?}");
    }

    #[test]
    fn streaming_done_after_unfinalized_chunk_supplies_usage_only() {
        let mut state = MistralrsStreamState::default();

        state
            .translate(stream_chunk(Some("hello"), None, None, None))
            .expect("chunk translates");
        let done = state
            .translate(chat_done(Some("hello"), None))
            .expect("done translates");

        assert_eq!(done.len(), 1);
        let RawStreamingChoice::FinalResponse(final_response) = &done[0] else {
            panic!("expected final response, got {done:?}");
        };
        assert_eq!(
            final_response.usage.expect("usage"),
            translate_usage(&usage())
        );
    }

    #[test]
    fn streaming_provider_errors_map_to_completion_errors() {
        let err = translate_stream_response(Response::InternalError(Box::new(
            std::io::Error::other("boom"),
        )))
        .unwrap_err();
        assert!(
            matches!(err, CompletionError::ProviderError(ref msg) if msg.contains("boom")),
            "got: {err:?}",
        );
    }

    /// Naming the model by its file failed every `model-path` request with
    /// `ModelNotFound` (#223); see the comment on `model_id` for why.
    #[test]
    fn a_request_names_no_model_so_it_reaches_the_loaded_one() {
        let request = ScriptedEngine(Vec::new).completion_request("hello").build();
        let (tx, _rx) = mpsc::channel(1);

        let normal = build_normal_request(request, tx, false).expect("request builds");

        assert_eq!(normal.model_id, None);
    }

    /// A response channel holding `script`, closed behind it, as the engine
    /// leaves one once it is done with a request.
    fn scripted_channel(script: Vec<Response>) -> mpsc::Receiver<Response> {
        let (tx, rx) = mpsc::channel(script.len().max(1));
        for response in script {
            assert!(
                tx.try_send(response).is_ok(),
                "the channel is sized to hold the whole script",
            );
        }
        rx
    }

    /// An engine that answers every request with the same script, read back
    /// through the [`response_stream`] a real engine's channel feeds.
    ///
    /// The script is a function because `Response` is not `Clone` and
    /// `stream` only borrows the engine.
    #[derive(Clone)]
    struct ScriptedEngine(fn() -> Vec<Response>);

    impl CompletionModel for ScriptedEngine {
        type Response = MistralrsRawResponse;
        type StreamingResponse = MistralrsStreamResponse;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self(Vec::new)
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
            Err(CompletionError::ProviderError(
                "the scripted engine only streams".into(),
            ))
        }

        async fn stream(
            &self,
            request: CompletionRequest,
        ) -> std::result::Result<
            StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            let rx = scripted_channel((self.0)());
            let stream = response_stream(rx, request.max_tokens);
            Ok(StreamingCompletionResponse::stream(stream))
        }
    }

    /// The local counterpart of `anthropic_mock.rs`'s
    /// `a_reasoning_only_turn_is_recovered_and_reported`, from the engine's
    /// response channel to the report the REPL prints.
    ///
    /// A reasoning model cut off at `max-tokens` inside its `<think>` block
    /// streams reasoning deltas and then a `length` finish, and nothing else.
    /// With the reasoning dropped in translation, that turn was reported as
    /// having produced no content at all -- to the user, and through
    /// `silent_reason` to a subagent's parent -- pointing away from the
    /// ceiling that actually cut it off.
    #[tokio::test]
    async fn a_reasoning_only_local_turn_is_recovered_and_reported() {
        let engine = ScriptedEngine(|| {
            vec![
                with_reasoning(stream_chunk(None, None, None, None), "weighing "),
                with_reasoning(
                    stream_chunk(None, None, Some("length"), Some(usage())),
                    "the options",
                ),
            ]
        });
        let agent = rig::agent::AgentBuilder::new(engine).build();
        let mut history = Vec::new();
        let mut stdout = Vec::new();

        let end = crate::llm::run_turn_streaming_to(
            &agent,
            "choose",
            &mut history,
            crate::llm::OutrigPromptHook::new(50),
            &mut stdout,
        )
        .await
        .expect("a reasoning-only turn ends the turn, it does not fail the session");

        assert!(
            stdout.is_empty(),
            "reasoning is not the reply, so none of it is streamed: {:?}",
            String::from_utf8_lossy(&stdout),
        );
        assert!(end.is_silent(), "no text and no stop reason: {end:?}");
        assert_eq!(
            end.recovered.as_deref(),
            Some("weighing the options"),
            "the streamed deltas are salvaged, as one block",
        );
        let report = end.silent_report();
        assert!(
            report.contains("hidden reasoning") && report.contains("Recovered reasoning follows"),
            "the report names which silence this was and carries the reasoning: {report}",
        );

        let Some(Message::Assistant { content, .. }) = history.last() else {
            panic!("the completed turn is retained in history: {history:#?}");
        };
        assert!(
            content.iter().any(|part| matches!(
                part,
                AssistantContent::Reasoning(r) if r.display_text() == "weighing the options"
            )),
            "with its reasoning: {content:?}",
        );
    }

    /// mistralrs-core 0.8.1 drops the chunk with the finish reason and usage
    /// when a reply stops partway through a multi-byte character. The stream
    /// still ends on a final response, with no usage, so rig records one as it
    /// would for a reply that finished.
    #[tokio::test]
    async fn a_stream_closed_before_its_final_chunk_ends_on_a_final_response() {
        let rx = scripted_channel(vec![stream_chunk(Some("Here is a crab: "), None, None, None)]);
        let mut stream = StreamingCompletionResponse::stream(response_stream(rx, None));

        let items: Vec<_> = stream.by_ref().collect().await;

        assert!(
            matches!(
                items.as_slice(),
                [
                    Ok(StreamedAssistantContent::Text(text)),
                    Ok(StreamedAssistantContent::Final(MistralrsStreamResponse { usage: None })),
                ] if text.text == "Here is a crab: "
            ),
            "got: {items:?}",
        );
        assert!(
            matches!(
                stream.response,
                Some(MistralrsStreamResponse { usage: None })
            ),
            "rig records the final response: {:?}",
            stream.response,
        );
    }

    /// A reply whose last chunk carried its finish reason ends on that chunk's
    /// final response alone, so the warning is not printed either. Read ahead
    /// of rig's wrapper, which would hide a second final response.
    #[tokio::test]
    async fn a_finished_stream_ends_on_its_own_final_response_alone() {
        let rx = scripted_channel(vec![stream_chunk(
            Some("done"),
            None,
            Some("stop"),
            Some(usage()),
        )]);

        let items: Vec<_> = response_stream(rx, None).collect().await;

        assert!(
            matches!(
                items.as_slice(),
                [
                    Ok(RawStreamingChoice::MessageId(_)),
                    Ok(RawStreamingChoice::Message(_)),
                    Ok(RawStreamingChoice::FinalResponse(MistralrsStreamResponse {
                        usage: Some(_)
                    })),
                ]
            ),
            "got: {items:?}",
        );
    }

    /// A channel that closes before any response is the same cut, with all of
    /// the reply held back. It used to fail the stream, which ended the whole
    /// `outrig run` session; the turn now ends with nothing to show, and is
    /// reported as silent like any other turn that produced no content.
    #[tokio::test]
    async fn a_local_turn_whose_channel_closes_empty_is_silent_not_fatal() {
        let agent = rig::agent::AgentBuilder::new(ScriptedEngine(Vec::new)).build();
        let mut history = Vec::new();
        let mut stdout = Vec::new();

        let end = crate::llm::run_turn_streaming_to(
            &agent,
            "draw a crab",
            &mut history,
            crate::llm::OutrigPromptHook::new(50),
            &mut stdout,
        )
        .await
        .expect("a reply that never arrived ends the turn, not the session");

        assert!(
            stdout.is_empty(),
            "nothing arrived, so nothing is streamed: {:?}",
            String::from_utf8_lossy(&stdout),
        );
        assert!(end.is_silent(), "no text and no stop reason: {end:?}");
    }

    /// `response`, a whole reply, finished for `reason` rather than `stop`.
    fn finishing(response: Response, reason: &str) -> Response {
        let Response::Done(mut chat) = response else {
            unreachable!("only a whole reply is built by `chat_done`");
        };
        chat.choices[0].finish_reason = reason.to_string();
        Response::Done(chat)
    }

    /// What `response_stream` prints once it has read `script`, if it prints
    /// anything for a reply that showed the user nothing.
    fn streamed_report(script: Vec<Response>, max_tokens: Option<u64>) -> Option<String> {
        let mut state = MistralrsStreamState::default();
        for response in script {
            state.translate(response).expect("the script translates");
        }
        state.textless_report(max_tokens)
    }

    /// A reasoning model cut off at `max-tokens` inside its `<think>` block
    /// streams reasoning and a `length` finish, and rig forwards nothing of that
    /// to the turn: this report is the only place either is named.
    #[test]
    fn a_stream_cut_off_inside_its_reasoning_names_the_finish_reason_and_the_ceiling() {
        let report = streamed_report(
            vec![
                with_reasoning(stream_chunk(None, None, None, None), "weighing "),
                with_reasoning(
                    stream_chunk(None, None, Some("length"), Some(usage())),
                    "the options",
                ),
            ],
            Some(2048),
        );

        assert_eq!(
            report,
            Some(retry::textless_line(Some("length"), Some(2048)))
        );
    }

    /// Text or a tool call on any chunk is something shown, however the chunk
    /// with the finish reason ends the reply.
    #[test]
    fn a_stream_that_showed_text_or_a_tool_call_is_not_reported() {
        let text_then_reasoning = vec![
            stream_chunk(Some("hel"), None, None, None),
            with_reasoning(
                stream_chunk(None, None, Some("stop"), Some(usage())),
                "done",
            ),
        ];
        let calls = vec![stream_chunk(
            None,
            Some(vec![tool_call("foo", "{}")]),
            Some("tool_calls"),
            Some(usage()),
        )];

        assert_eq!(streamed_report(text_then_reasoning, Some(2048)), None);
        assert_eq!(streamed_report(calls, Some(2048)), None);
    }

    #[test]
    fn a_stream_of_only_whitespace_is_reported() {
        let chunk = stream_chunk(Some(" \n"), None, Some("stop"), Some(usage()));

        assert_eq!(
            streamed_report(vec![chunk], None),
            Some(retry::textless_line(Some("stop"), None)),
        );
    }

    /// A reply with no finish reason is reported without one, not with a made-up
    /// one.
    #[test]
    fn a_stream_that_names_no_finish_reason_is_reported_without_one() {
        let chunk = stream_chunk(None, None, None, Some(usage()));

        assert_eq!(
            streamed_report(vec![chunk], None),
            Some(retry::textless_line(None, None)),
        );
    }

    /// Rig keeps only the first final response of a stream, and the report
    /// keeps only the first finish reason: a later response does not replace it.
    #[test]
    fn the_first_finish_reason_stands() {
        let script = vec![
            stream_chunk(None, None, Some("length"), Some(usage())),
            chat_done(None, None),
        ];

        assert_eq!(
            streamed_report(script, None),
            Some(retry::textless_line(Some("length"), None)),
        );
    }

    /// When mistralrs drops the last chunk the reply may be partly missing, so
    /// "no text" could be false. The warning for that case is the only report.
    #[test]
    fn a_stream_whose_last_chunk_was_dropped_is_not_reported_as_textless() {
        let chunk = with_reasoning(stream_chunk(None, None, None, None), "weighing");

        assert_eq!(streamed_report(vec![chunk], Some(2048)), None);
    }

    /// A whole reply sent to a streaming request carries its finish reason on
    /// the reply, not on a chunk.
    #[test]
    fn a_whole_reply_on_the_stream_path_names_its_finish_reason() {
        let done = finishing(with_reasoning(chat_done(None, None), "weighing"), "length");

        assert_eq!(
            streamed_report(vec![done], Some(2048)),
            Some(retry::textless_line(Some("length"), Some(2048))),
        );
    }

    /// `completion`, the path of a local row in a model alias chain, reads the
    /// finish reason from the whole reply before translating it away.
    #[test]
    fn the_finish_reason_is_read_off_a_chunk_and_off_a_whole_reply() {
        let chunk = |reason| stream_chunk(Some("hi"), None, reason, None);
        let internal_error = Response::InternalError(Box::new(std::io::Error::other("boom")));

        assert_eq!(finish_reason_of(&chunk(Some("length"))), Some("length"));
        assert_eq!(finish_reason_of(&chunk(None)), None);
        assert_eq!(
            finish_reason_of(&finishing(chat_done(Some("hi"), None), "length")),
            Some("length"),
        );
        assert_eq!(finish_reason_of(&internal_error), None);
    }

    /// What `completion` judges a whole reply by is what `translate_response`
    /// makes of it: reasoning is not shown, and an empty reply is the
    /// empty-text sentinel.
    #[test]
    fn a_whole_reply_shows_the_user_text_or_a_tool_call_and_not_reasoning() {
        let shown = |done| {
            let completion = translate_response(done).expect("response translates");
            retry::showed_something(&completion.choice)
        };

        assert!(shown(chat_done(Some("an answer"), None)));
        assert!(shown(chat_done(None, Some(vec![tool_call("foo", "{}")]))));
        assert!(!shown(chat_done(None, None)));
        assert!(!shown(chat_done(Some(" \n"), None)));
        assert!(!shown(with_reasoning(
            chat_done(None, None),
            "weighing the options"
        )));
    }

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// A client for the hub at `endpoint` whose cache is `root`, with no token
    /// and no progress bar.
    fn api_at(endpoint: &str, root: &Path) -> Api {
        ApiBuilder::from_cache(hf_hub::Cache::new(root.to_path_buf()))
            .with_endpoint(endpoint.to_string())
            .with_token(None)
            .with_progress(false)
            .build()
            .expect("a client")
    }

    /// A client for a cache that holds everything asked of it: its endpoint
    /// refuses connections, so a request fails at once.
    fn offline_api(root: &Path) -> Api {
        let refused = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let endpoint = format!("http://{}", refused.local_addr().expect("a bound address"));
        drop(refused);
        api_at(&endpoint, root)
    }

    /// What the fake hub is doing, which a test can change while it runs.
    #[cfg(unix)]
    struct HubState {
        /// The commit `main` is at: what `info` reports, and what a request for
        /// a file at `main` is stamped with.
        main: String,
        /// The commit `main` moves to once a response has told a client where
        /// `main` is.
        then: Option<String>,
        /// Files a request for which is answered with a 500.
        broken: Vec<String>,
    }

    /// The hub on a local port. It serves `files` of `org/repo`, with `main` at
    /// commit `abc123`: `info` for `main` lists the files and gives the commit,
    /// and a file asked for at `main` or at a commit is answered stamped with
    /// that commit. Anything else is a 404. It records the paths it is asked for.
    /// hf-hub asks for each file twice: for its first byte, the response to which
    /// carries the commit and the etag, and then for the file. Its client, ureq,
    /// sends a request to 127.0.0.1 through a proxy variable if one is set, so
    /// these tests need none.
    #[cfg(unix)]
    struct FakeHub {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        state: Arc<Mutex<HubState>>,
    }

    #[cfg(unix)]
    impl FakeHub {
        fn serving(files: &[(&str, &str)]) -> FakeHub {
            for var in [
                "ALL_PROXY",
                "all_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
            ] {
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} is set, and hf-hub would send its requests to the fake hub through it",
                );
            }
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
            let endpoint = format!("http://{}", listener.local_addr().expect("a bound address"));
            let served: HashMap<String, (String, String)> = files
                .iter()
                .enumerate()
                .map(|(i, (name, body))| (name.to_string(), (format!("hub-{i}"), body.to_string())))
                .collect();
            let listing = files
                .iter()
                .map(|(name, _)| format!(r#"{{"rfilename":"{name}"}}"#))
                .collect::<Vec<_>>()
                .join(",");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let state = Arc::new(Mutex::new(HubState {
                main: "abc123".to_string(),
                then: None,
                broken: Vec::new(),
            }));
            let (log, shared) = (Arc::clone(&requests), Arc::clone(&state));
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut head = std::io::BufReader::new(&stream)
                        .lines()
                        .map_while(|line| line.ok())
                        .take_while(|line| !line.is_empty());
                    let request_line = head.next().unwrap_or_default();
                    let path = request_line
                        .split(' ')
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let first_byte_only =
                        head.any(|line| line.eq_ignore_ascii_case("range: bytes=0-0"));
                    log.lock().expect("the request log").push(path.clone());

                    let mut hub = shared.lock().expect("the hub state");
                    let tells_main = path == "/api/models/org/repo/revision/main"
                        || (first_byte_only && path.starts_with("/org/repo/resolve/main/"));
                    let not_found = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\
                                     connection: close\r\n\r\n";
                    let reply = if path == "/api/models/org/repo/revision/main" {
                        let body = format!(r#"{{"sha":"{}","siblings":[{listing}]}}"#, hub.main);
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len(),
                        )
                    } else if let Some((revision, name)) = path
                        .strip_prefix("/org/repo/resolve/")
                        .and_then(|rest| rest.split_once('/'))
                    {
                        let commit = if revision == "main" {
                            hub.main.clone()
                        } else {
                            revision.to_string()
                        };
                        match served.get(name) {
                            None => not_found.to_string(),
                            Some(_) if hub.broken.iter().any(|broken| broken == name) => {
                                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\
                                 connection: close\r\n\r\n"
                                    .to_string()
                            }
                            Some((etag, body)) if first_byte_only => format!(
                                "HTTP/1.1 206 Partial Content\r\nx-repo-commit: {commit}\r\n\
                                 etag: \"{etag}\"\r\ncontent-range: bytes 0-0/{}\r\n\
                                 content-length: 1\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                &body[..1],
                            ),
                            Some((_, body)) => format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\
                                 connection: close\r\n\r\n{body}",
                                body.len(),
                            ),
                        }
                    } else {
                        not_found.to_string()
                    };
                    if tells_main && let Some(next) = hub.then.take() {
                        hub.main = next;
                    }
                    drop(hub);
                    let _ = (&stream).write_all(reply.as_bytes());
                }
            });
            FakeHub {
                endpoint,
                requests,
                state,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("the request log").clone()
        }

        /// `main` moves to `commit` once a response has told a client where it is.
        fn advance_main_after_first_answer(&self, commit: &str) {
            self.state.lock().expect("the hub state").then = Some(commit.to_string());
        }

        /// A request for `file` is answered with a 500.
        fn break_file(&self, file: &str) {
            self.state
                .lock()
                .expect("the hub state")
                .broken
                .push(file.to_string());
        }
    }

    /// What hf-hub leaves in `root` after downloading `names` from `org/repo` at
    /// `commit`, on `revision`: the ref -- with no trailing newline, which hf-hub
    /// does not trim -- and per file a blob, with the snapshot entry a symlink to
    /// it. An implementation that canonicalizes the path it is given returns the
    /// blob's, not the snapshot's.
    #[cfg(unix)]
    fn seeded_hub_cache(root: &Path, revision: &str, commit: &str, names: &[&str]) {
        let repo = root.join("models--org--repo");
        let ref_path = repo.join("refs").join(revision);
        std::fs::create_dir_all(ref_path.parent().expect("a parent")).expect("refs dir");
        std::fs::write(ref_path, commit).expect("ref");
        std::fs::create_dir_all(repo.join("blobs")).expect("blobs dir");
        for (i, name) in names.iter().enumerate() {
            let blob = repo.join("blobs").join(format!("etag-{i}"));
            std::fs::write(&blob, b"GGUF").expect("blob");
            let entry = repo.join("snapshots").join(commit).join(name);
            std::fs::create_dir_all(entry.parent().expect("a parent")).expect("snapshot dir");
            std::os::unix::fs::symlink(&blob, &entry).expect("snapshot entry");
        }
    }

    /// #423: a `model-id` download went to Hugging Face's cache whatever
    /// `model-cache-root` said. The loader is now handed the snapshot directory
    /// under `cache_root`, which hf-hub finds here with no network: it answers
    /// from a cache that already holds the file.
    #[cfg(unix)]
    #[test]
    fn a_model_id_resolves_to_a_snapshot_under_the_cache_root() {
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "abc123", &["m.gguf"]);

        let api = offline_api(root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("a cached file resolves");

        let snapshot = root.path().join("models--org--repo/snapshots/abc123");
        assert_eq!(Path::new(&dir), snapshot);
        assert!(
            snapshot.join("m.gguf").is_file(),
            "the loader opens it from here by name",
        );
    }

    /// The directory is the snapshot's, not that of the subdirectory a
    /// quantization kept in a directory of its own is in: the loader joins each
    /// `model-file` entry, subdirectory and all, onto the directory it is given.
    #[cfg(unix)]
    #[test]
    fn a_model_file_in_a_subdirectory_resolves_to_the_snapshot_not_the_subdirectory() {
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "abc123", &["Q4_K_M/m.gguf"]);

        let api = offline_api(root.path());

        let dir = fetch_snapshot(
            &api,
            "org/repo",
            &files(&["Q4_K_M/m.gguf"]),
            None,
            root.path(),
        )
        .expect("a cached file resolves");

        assert_eq!(
            Path::new(&dir),
            root.path().join("models--org--repo/snapshots/abc123"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_shards_of_a_split_model_share_one_snapshot_directory() {
        let root = TempDir::new().expect("tempdir");
        let shards = ["Q4/m-00001-of-00002.gguf", "Q4/m-00002-of-00002.gguf"];
        seeded_hub_cache(root.path(), "main", "abc123", &shards);

        let api = offline_api(root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&shards), None, root.path())
            .expect("cached shards resolve");

        assert_eq!(
            Path::new(&dir),
            root.path().join("models--org--repo/snapshots/abc123"),
        );
    }

    /// A pinned revision is looked up under its own ref, and one with a `/` in
    /// it (a pull request's) only nests that ref: the snapshot directory is the
    /// commit's either way.
    #[cfg(unix)]
    #[test]
    fn a_pinned_revision_resolves_through_its_own_ref() {
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "abc123", &["m.gguf"]);
        seeded_hub_cache(root.path(), "refs/pr/1", "def456", &["m.gguf"]);

        let api = offline_api(root.path());

        let dir = fetch_snapshot(
            &api,
            "org/repo",
            &files(&["m.gguf"]),
            Some("refs/pr/1"),
            root.path(),
        )
        .expect("a cached file resolves");

        assert_eq!(
            Path::new(&dir),
            root.path().join("models--org--repo/snapshots/def456"),
        );
    }

    /// hf-hub writes a file wherever its name points, so a name that does not
    /// stay inside the snapshot is refused before anything is looked up, or
    /// created.
    #[test]
    fn a_model_file_that_is_not_a_plain_relative_path_is_refused_first() {
        for bad in [
            "./m.gguf",
            "../m.gguf",
            "/m.gguf",
            "a/../m.gguf",
            "a//m.gguf",
            "a/",
            "",
        ] {
            let root = TempDir::new().expect("tempdir");
            let api = offline_api(root.path());

            let err =
                fetch_snapshot(&api, "org/repo", &files(&[bad]), None, root.path()).expect_err(bad);

            assert!(err.to_string().contains("relative path"), "{bad:?}: {err}");
            assert_eq!(
                std::fs::read_dir(root.path()).expect("readable").count(),
                0,
                "{bad:?}: nothing is created",
            );
        }
    }

    /// mistralrs reads a repo's `generation_config.json` from beside the weights
    /// for the stop tokens a GGUF's metadata does not name, such as Llama 3's
    /// `<|eom_id|>`, and it is handed the snapshot directory to look in. A
    /// download brings the file into the snapshot the weights are in.
    #[cfg(unix)]
    #[test]
    fn a_download_brings_generation_config_into_the_snapshot_beside_the_weights() {
        let config = r#"{"eos_token_id": [128001, 128008, 128009]}"#;
        let hub = FakeHub::serving(&[("m.gguf", "GGUF"), ("generation_config.json", config)]);
        let root = TempDir::new().expect("tempdir");
        let api = api_at(&hub.endpoint, root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("a download");

        let snapshot = root.path().join("models--org--repo/snapshots/abc123");
        assert_eq!(Path::new(&dir), snapshot);
        assert_eq!(
            std::fs::read_to_string(snapshot.join("m.gguf")).expect("the weights"),
            "GGUF",
        );
        assert_eq!(
            std::fs::read_to_string(snapshot.join("generation_config.json")).expect("the config"),
            config,
        );
    }

    /// Most GGUF repos have no `generation_config.json`. The hub's listing of the
    /// repo says so, so it is not asked for, and that is not a failure.
    #[cfg(unix)]
    #[test]
    fn a_repo_without_generation_config_loads_without_it() {
        let hub = FakeHub::serving(&[("m.gguf", "GGUF")]);
        let root = TempDir::new().expect("tempdir");
        let api = api_at(&hub.endpoint, root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("a repo without one is not an error");

        let snapshot = Path::new(&dir);
        assert!(snapshot.join("m.gguf").is_file());
        assert!(!snapshot.join("generation_config.json").exists());
        assert!(
            !hub.requests()
                .iter()
                .any(|path| path.ends_with("/generation_config.json")),
            "the listing says it is absent, so it is not asked for: {:?}",
            hub.requests(),
        );
    }

    /// A model whose named files are all cached loads with no network, so the
    /// hub is not asked for `generation_config.json` either, though this one
    /// serves it: the cache loads as it is, as one that a Hugging Face tool
    /// filled with the weights alone does.
    #[cfg(unix)]
    #[test]
    fn a_cached_model_makes_no_request_to_the_hub() {
        let hub = FakeHub::serving(&[("generation_config.json", "{}")]);
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "abc123", &["m.gguf"]);
        let api = api_at(&hub.endpoint, root.path());

        fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("a cached file resolves");

        assert_eq!(hub.requests(), Vec::<String>::new());
    }

    /// A branch that advances while a load is under way must not split it. The
    /// config at one commit and the weights at the next fail the load, and the
    /// retry, finding every weight cached, would load the model without the
    /// config for good. Everything comes from the commit `main` was at when the
    /// load began, and `main` is left pointing at it, so the next load is cached.
    #[cfg(unix)]
    #[test]
    fn a_branch_that_advances_mid_load_does_not_split_the_snapshot() {
        let hub = FakeHub::serving(&[("m.gguf", "GGUF"), ("generation_config.json", "{}")]);
        hub.advance_main_after_first_answer("def456");
        let root = TempDir::new().expect("tempdir");
        let api = api_at(&hub.endpoint, root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("one commit throughout");

        let snapshot = root.path().join("models--org--repo/snapshots/abc123");
        assert_eq!(Path::new(&dir), snapshot);
        assert!(snapshot.join("m.gguf").is_file());
        assert!(snapshot.join("generation_config.json").is_file());

        let asked = hub.requests().len();
        fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("cached by now");
        assert_eq!(
            hub.requests().len(),
            asked,
            "the next load makes no request"
        );
    }

    /// A `generation_config.json` cached at an older commit than the one the hub
    /// serves now, for another `model-file` of the repo, must not be taken for
    /// the one that belongs with the weights fetched next: they would be in
    /// different snapshots, and the load would fail, and the next one would find
    /// the weights cached and never fetch it.
    #[cfg(unix)]
    #[test]
    fn a_generation_config_cached_at_an_older_commit_is_fetched_again() {
        let hub = FakeHub::serving(&[("m.gguf", "GGUF"), ("generation_config.json", "{}")]);
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "old000", &["generation_config.json"]);
        let api = api_at(&hub.endpoint, root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect("both come from the commit the hub serves");

        let snapshot = root.path().join("models--org--repo/snapshots/abc123");
        assert_eq!(Path::new(&dir), snapshot);
        assert!(snapshot.join("m.gguf").is_file());
        assert!(snapshot.join("generation_config.json").is_file());
    }

    /// A hub that cannot answer fails the load at the first question, which
    /// commit `revision` is at, before anything is downloaded.
    #[test]
    fn a_hub_that_cannot_answer_fails_the_load_before_anything_is_fetched() {
        let root = TempDir::new().expect("tempdir");
        let api = offline_api(root.path());

        let err = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect_err("no hub to ask");

        let text = err.to_string();
        assert!(text.contains("look up the commit"), "asked first: {text}");
        assert_eq!(
            std::fs::read_dir(root.path()).expect("readable").count(),
            0,
            "nothing is cached",
        );
    }

    /// `generation_config.json` is fetched before the weights, so that a hub that
    /// fails it fails the load first: once the weights are cached the next load
    /// counts as cached, and would never fetch it.
    #[test]
    fn a_config_that_cannot_be_fetched_fails_the_load_before_the_weights_are_fetched() {
        let hub = FakeHub::serving(&[("m.gguf", "GGUF"), ("generation_config.json", "{}")]);
        hub.break_file("generation_config.json");
        let root = TempDir::new().expect("tempdir");
        let api = api_at(&hub.endpoint, root.path());

        let err = fetch_snapshot(&api, "org/repo", &files(&["m.gguf"]), None, root.path())
            .expect_err("the config cannot be fetched");

        let text = err.to_string();
        assert!(text.contains("generation_config.json"), "{text}");
        assert!(
            !hub.requests().iter().any(|path| path.ends_with("/m.gguf")),
            "the weights were not asked for: {:?}",
            hub.requests(),
        );
    }

    /// A download that was interrupted resumes: a shard already cached at the
    /// commit is not fetched again, though the commit's own ref is not there.
    #[cfg(unix)]
    #[test]
    fn an_interrupted_download_is_resumed_not_repeated() {
        let shards = ["m-00001.gguf", "m-00002.gguf"];
        let hub = FakeHub::serving(&[(shards[0], "GGUF"), (shards[1], "GGUF")]);
        let root = TempDir::new().expect("tempdir");
        seeded_hub_cache(root.path(), "main", "abc123", &shards[..1]);
        let api = api_at(&hub.endpoint, root.path());

        let dir = fetch_snapshot(&api, "org/repo", &files(&shards), None, root.path())
            .expect("the second shard is fetched");

        let snapshot = root.path().join("models--org--repo/snapshots/abc123");
        assert_eq!(Path::new(&dir), snapshot);
        assert!(snapshot.join(shards[1]).is_file());
        assert!(
            !hub.requests().iter().any(|path| path.ends_with(shards[0])),
            "the shard already cached is not fetched again: {:?}",
            hub.requests(),
        );
    }
}
