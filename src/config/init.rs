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
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::config::api_key::ApiKeyRef;
use crate::config::{LlmProvider, Model};
use crate::error::{OutrigError, Result};
use crate::init::prompt::{Field, PromptSource, TerminalPrompt};
use crate::repo;

/// Public entry: resolve the path, build a real `TerminalPrompt`, delegate to
/// `run_with`. `global_override` plumbs the top-level `--global-config` flag
/// into the same resolver `repo::global_config_path` uses elsewhere.
pub async fn run(force: bool, global_override: Option<&Path>) -> Result<()> {
    let path = repo::global_config_path(global_override);
    eprintln!("[outrig] writing global config to {}", path.display());
    let mut prompt = TerminalPrompt::from_real_io();
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

    let providers = prompt_providers(prompt).await?;
    let models = prompt_models(prompt, &providers).await?;
    let default_model = prompt_default_model(prompt, &models).await?;

    let toml_text = render(default_model.as_deref(), &providers, &models)?;
    write_atomic(path, &toml_text)?;
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
    description: "Name of an existing [providers.<name>] entry.",
    options: &[],
    doc_link: "doc/reference/config.md",
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
        let provider = match style {
            "openai" => prompt_openai_provider(prompt).await?,
            "mistralrs" => LlmProvider::Mistralrs,
            other => {
                return Err(OutrigError::Configuration(format!(
                    "unknown provider style: {other}"
                )));
            }
        };
        out.insert(name, provider);

        if !prompt.ask_bool(&ADD_PROVIDER_FIELD, false).await? {
            break;
        }
    }
    Ok(out)
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
    providers: &BTreeMap<String, LlmProvider>,
) -> Result<BTreeMap<String, Model>> {
    let mut out = BTreeMap::new();
    if !prompt.ask_bool(&DEFINE_MODEL_FIELD, true).await? {
        return Ok(out);
    }
    let first_provider = providers
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "openai".to_string());

    loop {
        let name = prompt.ask_string(&MODEL_NAME_FIELD, "fast").await?;
        let provider_name = loop {
            let answer = prompt
                .ask_string(&MODEL_PROVIDER_FIELD, &first_provider)
                .await?;
            if providers.contains_key(&answer) {
                break answer;
            }
            eprintln!(
                "[outrig] no provider named `{answer}`; defined: {}",
                providers.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        };
        let model = match providers.get(&provider_name).expect("validated above") {
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
    Ok(out)
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

async fn prompt_default_model(
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

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        OutrigError::Configuration(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
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
