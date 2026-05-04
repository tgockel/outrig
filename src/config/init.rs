//! `outrig config init` -- interactive writer for the global config.
//!
//! Walks the user through providers, models, and `default-model`, then writes
//! a parseable + validated TOML file to the resolved global-config path. The
//! file refuses to clobber an existing one without `--force`. Atomic writes go
//! through `tempfile::NamedTempFile::persist` so an interrupted prompt never
//! leaves a half-written config behind.
//!
//! `run` constructs real terminal I/O; `run_with` is the test seam that takes
//! an arbitrary `PromptSource` and target path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::api_key::ApiKeyRef;
use crate::config::{LlmProvider, Model};
use crate::error::{OutrigError, Result};
use crate::init::prompt::{self, Field, PromptSource};
use crate::repo;

/// Public entry: resolve the path, pick a `PromptSource` via
/// `prompt::auto()` (dialoguer on a TTY, line-based on piped stdin), and
/// delegate to `run_with`. `global_override` plumbs the top-level
/// `--global-config` flag into the same resolver `repo::global_config_path`
/// uses elsewhere.
pub async fn run(force: bool, global_override: Option<&Path>) -> Result<()> {
    let path = repo::global_config_path(global_override);
    eprintln!("[outrig] writing global config to {}", path.display());
    let mut prompt = prompt::auto();
    run_with(force, &path, &mut prompt).await?;
    eprintln!("[outrig] wrote {}", path.display());
    Ok(())
}

/// Drives the interactive flow against an arbitrary `PromptSource`. The flow
/// short-circuits on existing files when `force == false` so an accidental
/// re-run doesn't burn through prompts before bailing.
pub async fn run_with(force: bool, path: &Path, prompt: &mut impl PromptSource) -> Result<()> {
    if path.exists() && !force {
        return Err(OutrigError::Configuration(format!(
            "{} already exists; pass --force to overwrite.",
            path.display()
        )));
    }

    let mut providers = prompt_providers(prompt).await?;
    let models = prompt_models(prompt, &mut providers).await?;
    let default_model = prompt_default_model(prompt, &models).await?;

    let toml_text = render(default_model.as_deref(), &providers, &models)?;
    repo::write_atomic(path, &toml_text)?;
    Ok(())
}

// ---- prompt-flow helpers --------------------------------------------------

const STYLES: &[(&str, &str)] = &[
    (
        "openai",
        "OpenAI Chat Completions wire format. Works with OpenAI, OpenRouter, vLLM, Ollama.",
    ),
    (
        "mistralrs",
        "In-process LLM via the mistralrs crate. Loads a local or HuggingFace model.",
    ),
];

const STYLE_FIELD: Field = Field {
    name: "Pick a provider style",
    description: "Which wire format / runtime this provider speaks.",
    options: STYLES,
    doc_link: "doc/concepts/llm-providers.md",
};

const PROVIDER_NAME_FIELD: Field = Field {
    name: "Provider name",
    description: "Used as the [providers.<name>] key and referenced from models.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const BASE_URL_FIELD: Field = Field {
    name: "Base URL",
    description: "HTTPS endpoint for the OpenAI-compatible API.",
    options: &[],
    doc_link: "doc/concepts/llm-providers.md",
};

const API_KEY_ENV_FIELD: Field = Field {
    name: "API key environment variable",
    description: "Name of the env var that holds the API key. Stored as ${VAR}.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const ADD_PROVIDER_FIELD: Field = Field {
    name: "Add another provider?",
    description: "Whether to define one more [providers.<name>] entry.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const AUTO_DOWNLOAD_FIELD: Field = Field {
    name: "Use auto-download by model ID?",
    description: "Yes: pull weights from HuggingFace by repo ID. No: load a local GGUF file by path.",
    options: &[],
    doc_link: "doc/concepts/in-process-llm.md",
};

const MODEL_ID_FIELD: Field = Field {
    name: "HuggingFace model-id",
    description: "Repo identifier, e.g. microsoft/Phi-3-mini-4k-instruct-gguf.",
    options: &[],
    doc_link: "doc/concepts/in-process-llm.md",
};

const REVISION_FIELD: Field = Field {
    name: "revision (blank for `main`)",
    description: "Git ref on the HuggingFace repo to pin. Defaults to `main`.",
    options: &[],
    doc_link: "doc/concepts/in-process-llm.md",
};

const MODEL_PATH_FIELD: Field = Field {
    name: "Local model-path",
    description: "Filesystem path to a GGUF file.",
    options: &[],
    doc_link: "doc/concepts/in-process-llm.md",
};

const CONTEXT_LENGTH_FIELD: Field = Field {
    name: "context-length (blank for the model's default)",
    description: "Override the model's default context window. Integer.",
    options: &[],
    doc_link: "doc/concepts/in-process-llm.md",
};

const DEFINE_MODEL_FIELD: Field = Field {
    name: "Define a model now?",
    description: "Whether to add a [models.<name>] entry to the new config.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const MODEL_NAME_FIELD: Field = Field {
    name: "Model name",
    description: "Used as the [models.<name>] key and referenced from agents.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const MODEL_IDENTIFIER_FIELD: Field = Field {
    name: "Model identifier",
    description: "Identifier passed to the provider API (e.g. gpt-4o-mini).",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const MODEL_PROVIDER_FIELD: Field = Field {
    name: "Provider for this model",
    description: "An LLM provider is a backend that hosts the model -- e.g. \
                  OpenAI, OpenRouter, vLLM, or a local mistralrs runtime. \
                  Each carries its own connection details (URL, API key, \
                  etc.). This can be the name of an existing \
                  [providers.<name>] entry or you can give a new name to \
                  create a new provider.",
    options: &[],
    doc_link: "doc/concepts/llm-providers.md",
};

const ADD_NEW_PROVIDER_FIELD: Field = Field {
    name: "Add this provider now?",
    description: "Yes: walk through the provider style + connection prompts \
                  to define a new [providers.<name>] entry under the name \
                  you just typed. No: re-enter the provider name.",
    options: &[],
    doc_link: "doc/concepts/llm-providers.md",
};

const ADD_MODEL_FIELD: Field = Field {
    name: "Add another model?",
    description: "Whether to define one more [models.<name>] entry.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const USE_DEFAULT_FIELD: Field = Field {
    name: "Use this model as default-model?",
    description: "Sets the top-level `default-model` so agents without an explicit model use it.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const DEFAULT_MODEL_FIELD: Field = Field {
    name: "Default model name",
    description: "Name of an existing model to set as `default-model`. Blank for none.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

/// Slice of every `Field` declared in this module, for `prompt_doc_sync.rs`.
pub const DOC_SYNC_FIELDS: &[&Field] = &[
    &STYLE_FIELD,
    &PROVIDER_NAME_FIELD,
    &BASE_URL_FIELD,
    &API_KEY_ENV_FIELD,
    &ADD_PROVIDER_FIELD,
    &AUTO_DOWNLOAD_FIELD,
    &MODEL_ID_FIELD,
    &REVISION_FIELD,
    &MODEL_PATH_FIELD,
    &CONTEXT_LENGTH_FIELD,
    &DEFINE_MODEL_FIELD,
    &MODEL_NAME_FIELD,
    &MODEL_IDENTIFIER_FIELD,
    &MODEL_PROVIDER_FIELD,
    &ADD_NEW_PROVIDER_FIELD,
    &ADD_MODEL_FIELD,
    &USE_DEFAULT_FIELD,
    &DEFAULT_MODEL_FIELD,
];

async fn prompt_providers(prompt: &mut impl PromptSource) -> Result<BTreeMap<String, LlmProvider>> {
    let mut out = BTreeMap::new();
    loop {
        let style_idx = prompt.ask_select(&STYLE_FIELD, 0).await?;
        let style = STYLES[style_idx].0;
        let name = prompt.ask_string(&PROVIDER_NAME_FIELD, style).await?;
        let provider = prompt_provider_body(prompt, style).await?;
        out.insert(name, provider);

        if !prompt.ask_bool(&ADD_PROVIDER_FIELD, false).await? {
            break;
        }
    }
    Ok(out)
}

/// Walks just the style-specific prompts for a single provider whose name
/// is already known. Used inline by `prompt_models_loop` when the user
/// references a provider that doesn't exist yet -- we already have the
/// name (what they typed at the model's provider prompt) and only need to
/// ask the style + connection details.
pub(crate) async fn prompt_new_provider_for_name(
    prompt: &mut impl PromptSource,
) -> Result<LlmProvider> {
    let style_idx = prompt.ask_select(&STYLE_FIELD, 0).await?;
    let style = STYLES[style_idx].0;
    prompt_provider_body(prompt, style).await
}

async fn prompt_provider_body(prompt: &mut impl PromptSource, style: &str) -> Result<LlmProvider> {
    match style {
        "openai" => prompt_openai_provider(prompt).await,
        "mistralrs" => Ok(LlmProvider::Mistralrs),
        other => Err(OutrigError::Configuration(format!(
            "unknown provider style: {other}"
        ))),
    }
}

async fn prompt_openai_provider(prompt: &mut impl PromptSource) -> Result<LlmProvider> {
    let base_url = prompt
        .ask_string(&BASE_URL_FIELD, "https://api.openai.com/v1")
        .await?;
    // We capture the env-var name and render it as `${VAR}` -- `ApiKeyRef` only
    // accepts that form, so feeding a bare name would be rejected at parse time.
    let env_name = prompt
        .ask_string(&API_KEY_ENV_FIELD, "OPENAI_API_KEY")
        .await?;
    let api_key = ApiKeyRef::parse(&format!("${{{env_name}}}"))?;
    Ok(LlmProvider::OpenAi {
        base_url,
        api_key,
        request_timeout_secs: None,
    })
}

async fn prompt_models(
    prompt: &mut impl PromptSource,
    providers: &mut BTreeMap<String, LlmProvider>,
) -> Result<BTreeMap<String, Model>> {
    if !prompt.ask_bool(&DEFINE_MODEL_FIELD, true).await? {
        return Ok(BTreeMap::new());
    }
    let (models, new_providers) = prompt_models_loop(prompt, providers).await?;
    providers.extend(new_providers);
    Ok(models)
}

/// The model-add loop without the outer `Define a model now?` gate.
/// Returns `(models, new_providers)` -- providers added inline (when the
/// user references one that doesn't exist yet) come back to the caller so
/// init::repo can write them to the repo config without mutating the
/// global providers it was passed.
pub(crate) async fn prompt_models_loop(
    prompt: &mut impl PromptSource,
    existing_providers: &BTreeMap<String, LlmProvider>,
) -> Result<(BTreeMap<String, Model>, BTreeMap<String, LlmProvider>)> {
    let mut out = BTreeMap::new();
    let mut new_providers: BTreeMap<String, LlmProvider> = BTreeMap::new();

    loop {
        let name = prompt.ask_string(&MODEL_NAME_FIELD, "fast").await?;

        // Print providers defined so far (existing + any added inline) so
        // the user has the list at hand for the next prompt.
        let provider_names: Vec<&str> = existing_providers
            .keys()
            .chain(new_providers.keys())
            .map(String::as_str)
            .collect();
        if !provider_names.is_empty() {
            eprintln!("[outrig] providers defined: {}", provider_names.join(", "));
        }

        let suggestion = provider_names
            .first()
            .copied()
            .unwrap_or("openai")
            .to_string();
        let provider_name = loop {
            let answer = prompt
                .ask_string(&MODEL_PROVIDER_FIELD, &suggestion)
                .await?;
            if existing_providers.contains_key(&answer) || new_providers.contains_key(&answer) {
                break answer;
            }
            eprintln!("[outrig] no provider named `{answer}` yet.");
            if prompt.ask_bool(&ADD_NEW_PROVIDER_FIELD, true).await? {
                let provider = prompt_new_provider_for_name(prompt).await?;
                new_providers.insert(answer.clone(), provider);
                break answer;
            }
        };
        let provider = existing_providers
            .get(&provider_name)
            .or_else(|| new_providers.get(&provider_name))
            .expect("validated above");
        let model = match provider {
            LlmProvider::OpenAi { .. } => {
                let identifier = prompt
                    .ask_string(&MODEL_IDENTIFIER_FIELD, "gpt-4o-mini")
                    .await?;
                Model {
                    provider: provider_name,
                    identifier: Some(identifier),
                    model_id: None,
                    model_path: None,
                    model_file: None,
                    revision: None,
                    context_length: None,
                }
            }
            LlmProvider::Mistralrs => prompt_mistralrs_model(prompt, provider_name).await?,
        };
        out.insert(name, model);
        if !prompt.ask_bool(&ADD_MODEL_FIELD, false).await? {
            break;
        }
    }
    Ok((out, new_providers))
}

async fn prompt_mistralrs_model(
    prompt: &mut impl PromptSource,
    provider_name: String,
) -> Result<Model> {
    let auto_download = prompt.ask_bool(&AUTO_DOWNLOAD_FIELD, true).await?;
    let (model_id, model_path, revision) = if auto_download {
        let id = ask_required(prompt, &MODEL_ID_FIELD).await?;
        let rev = blank_to_none(prompt.ask_string(&REVISION_FIELD, "").await?);
        (Some(id), None, rev)
    } else {
        let path = ask_required(prompt, &MODEL_PATH_FIELD).await?;
        (None, Some(PathBuf::from(path)), None)
    };
    let context_length = blank_to_none(prompt.ask_string(&CONTEXT_LENGTH_FIELD, "").await?)
        .map(|s| {
            s.parse::<u32>().map_err(|_| {
                OutrigError::Configuration(format!(
                    "context-length must be a non-negative integer; got `{s}`"
                ))
            })
        })
        .transpose()?;
    Ok(Model {
        provider: provider_name,
        identifier: None,
        model_id,
        model_path,
        // model-file is intentionally not prompted: the typical single-file
        // GGUF repo case doesn't need it; multi-file repos hand-edit the TOML.
        model_file: None,
        revision,
        context_length,
    })
}

pub(crate) async fn prompt_default_model(
    prompt: &mut impl PromptSource,
    models: &BTreeMap<String, Model>,
) -> Result<Option<String>> {
    match models.len() {
        0 => Ok(None),
        1 => {
            let only = models.keys().next().expect("len==1");
            if prompt.ask_bool(&USE_DEFAULT_FIELD, true).await? {
                Ok(Some(only.clone()))
            } else {
                Ok(None)
            }
        }
        _ => loop {
            // BTreeMap iteration order is alphabetical, which is fine as a
            // suggestion -- the user picks freely from the validated set.
            let suggestion = models.keys().next().expect("len>1");
            let answer = prompt
                .ask_string(&DEFAULT_MODEL_FIELD, suggestion.as_str())
                .await?;
            if answer.is_empty() {
                return Ok(None);
            }
            if models.contains_key(&answer) {
                return Ok(Some(answer));
            }
            eprintln!(
                "[outrig] no model named `{answer}`; defined: {}",
                models.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        },
    }
}

// ---- rendering + atomic write --------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct GlobalOut<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    providers: &'a BTreeMap<String, LlmProvider>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    models: &'a BTreeMap<String, Model>,
}

fn render(
    default_model: Option<&str>,
    providers: &BTreeMap<String, LlmProvider>,
    models: &BTreeMap<String, Model>,
) -> Result<String> {
    let view = GlobalOut {
        default_model,
        providers,
        models,
    };
    toml::to_string_pretty(&view)
        .map_err(|e| OutrigError::Configuration(format!("rendering global config: {e}")))
}

fn blank_to_none(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// `ask_string` wrapper that re-prompts on empty input. Used for fields
/// where empty is not a meaningful answer (e.g. a HuggingFace model id).
async fn ask_required(prompt: &mut impl PromptSource, field: &Field) -> Result<String> {
    loop {
        let answer = prompt.ask_string(field, "").await?;
        if !answer.is_empty() {
            return Ok(answer);
        }
        eprintln!("[outrig] this field requires a value");
    }
}
