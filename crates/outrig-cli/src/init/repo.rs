//! Repo-config phase of `outrig init`, plus the bootstrap fallback used
//! by `outrig image add` when run in an uninitialized repo.
//!
//! Two public entry points share one private writer:
//! - [`ensure`] is what `outrig init` calls: idempotent: write the config
//!   if missing, log + skip if present.
//! - [`resolve_or_bootstrap`] is what `outrig image add` calls before
//!   dispatching: walk up to find an existing `.agents/outrig/config.toml`,
//!   and on `NoRepoConfig` prompt the user to bootstrap one against `cwd`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use heck::ToKebabCase;

use crate::config_init;
use crate::error::{OutrigError, Result};
use crate::hf::HfTreeFetcher;
use crate::image_setup::add as image_add;
use crate::init::prompt::{Field, PromptSource};
use crate::paths::{find_repo_root_from, repo_config_path, write_atomic};
use outrig::config::{Agent, Config, LlmProvider, Model, Workspace};

/// Idempotent. Returns `Some(image_name)` when this call wrote the
/// repo config (the user named an image during the bootstrap), or
/// `None` when the file already existed and was left alone. Callers
/// thread the name into the subsequent `image_setup::add::run_with` so the
/// image-name prompt doesn't fire twice.
pub async fn ensure(
    repo_root: &Path,
    global_path: &Path,
    prompt: &mut impl PromptSource,
    hf: &mut impl HfTreeFetcher,
) -> Result<Option<String>> {
    let cfg_path = repo_config_path(repo_root);
    if cfg_path.exists() {
        eprintln!(
            "[outrig] using existing repo config at {}",
            cfg_path.display()
        );
        return Ok(None);
    }
    eprintln!(
        "[outrig] no repo config at {} -- let's create one.",
        cfg_path.display()
    );
    let name = write_repo_config(repo_root, global_path, prompt, hf).await?;
    Ok(Some(name))
}

/// Resolve the repo root for `outrig image add`. Walks up via
/// [`find_repo_root_from`]; on [`OutrigError::NoRepoConfig`] prompts
/// the user, and on yes bootstraps the repo config against `cwd` and
/// returns `cwd`. On no, re-raises `NoRepoConfig` so the exit code and
/// error string match the previous behavior for scripts that test the
/// unconfigured case.
/// Returns the resolved repo root paired with `Some(image_name)` when
/// this call ran the bootstrap (the user named an image) or `None`
/// when an existing config was found by walking up.
pub async fn resolve_or_bootstrap(
    cwd: &Path,
    global_path: &Path,
    prompt: &mut impl PromptSource,
    hf: &mut impl HfTreeFetcher,
) -> Result<(PathBuf, Option<String>)> {
    match find_repo_root_from(cwd) {
        Ok(root) => Ok((root, None)),
        Err(OutrigError::NoRepoConfig) => {
            eprintln!(
                "[outrig] no .agents/outrig/config.toml found in {} or any parent.",
                cwd.display()
            );
            if !prompt.ask_bool(&CONFIGURE_NOW_FIELD, true).await? {
                eprintln!("[outrig] skipping; run `outrig init` later to set up.");
                return Err(OutrigError::NoRepoConfig.into());
            }
            let name = write_repo_config(cwd, global_path, prompt, hf).await?;
            Ok((cwd.to_path_buf(), Some(name)))
        }
        Err(other) => Err(other.into()),
    }
}

/// Walks the three repo-config sections (image / model / agent),
/// builds a [`Config`], serializes to TOML, and writes atomically via
/// [`write_atomic`]. Section headers signal each transition so
/// the prompts don't bleed together.
async fn write_repo_config(
    repo_root: &Path,
    global_path: &Path,
    prompt: &mut impl PromptSource,
    hf: &mut impl HfTreeFetcher,
) -> Result<String> {
    eprintln!();
    eprintln!("Configuring models");
    let global = load_global_summary(global_path)?;
    let model_choices = ask_repo_models(prompt, &global, hf).await?;

    // Agent before image: the image section then flows directly
    // into `image_setup::add`'s base/toolchains/MCP prompts without an
    // agent-section interruption.
    eprintln!();
    eprintln!("Configuring your first agent");
    let agent_name = prompt
        .ask_string(&AGENT_NAME_FIELD, DEFAULT_AGENT_NAME)
        .await?;
    // If neither the global nor the repo sets a default-model, the agent
    // *must* pin its own to produce a config that validates. Otherwise
    // the agent inherits whichever default is set.
    let agent_model = ask_agent_model(prompt, &global, &model_choices).await?;
    let preamble = prompt
        .ask_string(&PREAMBLE_FIELD, "You are a careful coding assistant.")
        .await?;

    eprintln!();
    eprintln!("Configuring your first image");
    let default_name = default_image_name(repo_root);
    let image_name = prompt
        .ask_string(&image_add::NAME_FIELD, &default_name)
        .await?;
    let ws_default = Workspace::default();
    let host_path = prompt
        .ask_string(&HOST_PATH_FIELD, &ws_default.host_path.to_string_lossy())
        .await?;
    let container_path = prompt
        .ask_string(
            &CONTAINER_PATH_FIELD,
            &ws_default.container_path.to_string_lossy(),
        )
        .await?;

    let toml_text = render(
        agent_name,
        agent_model,
        image_name.clone(),
        host_path,
        container_path,
        model_choices,
        preamble,
    )?;
    let cfg_path = repo_config_path(repo_root);
    write_atomic(&cfg_path, &toml_text)?;
    eprintln!();
    eprintln!("[outrig] wrote {}", cfg_path.display());
    Ok(image_name)
}

/// Snapshot of the parts of the global config we surface in the model
/// section. Providers come along so the repo-model loop can validate
/// `provider = "<name>"` references against what's actually defined.
#[derive(Default)]
struct GlobalSummary {
    providers: BTreeMap<String, LlmProvider>,
    models: Vec<String>,
    default_model: Option<String>,
}

/// Best-effort load of the global config. A missing file yields an empty
/// summary (the user will be prompted to run `outrig config init`); parse
/// errors propagate so a corrupt config surfaces immediately.
fn load_global_summary(global_path: &Path) -> Result<GlobalSummary> {
    let text = match std::fs::read_to_string(global_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GlobalSummary::default());
        }
        Err(e) => return Err(e.into()),
    };
    let cfg = Config::load_from_str(&text)?;
    Ok(GlobalSummary {
        providers: cfg.providers,
        models: cfg.models.keys().cloned().collect(),
        default_model: cfg.default_model,
    })
}

/// Surface what the global config offers, then on yes walk the repo-model
/// definition loop (reusing `config::init`'s prompt sequence) plus the
/// default-model selection. Returns the new repo-local models and the
/// repo-level `default-model` to embed in the rendered config; an empty
/// map / `None` means "inherit everything from global".
async fn ask_repo_models(
    prompt: &mut impl PromptSource,
    summary: &GlobalSummary,
    hf: &mut impl HfTreeFetcher,
) -> Result<RepoModelChoices> {
    if summary.providers.is_empty() {
        eprintln!(
            "[outrig] no providers defined in your global config -- run \
             `outrig config init` first; without a provider this agent \
             can't reach an LLM."
        );
        return Ok(RepoModelChoices::default());
    }

    let listing = if summary.models.is_empty() {
        "(none)".to_string()
    } else {
        summary.models.join(", ")
    };
    match &summary.default_model {
        Some(d) => {
            eprintln!("[outrig] models available in your global config: {listing} (default: {d})")
        }
        None => eprintln!("[outrig] models available in your global config: {listing}"),
    }

    if prompt.ask_bool(&CONFIGURE_REPO_MODELS_FIELD, true).await? {
        let (models, new_providers) =
            config_init::prompt_models_loop(prompt, &summary.providers, hf).await?;
        let default = config_init::prompt_default_model(prompt, &models).await?;
        return Ok(RepoModelChoices {
            models,
            providers: new_providers,
            default_model: default,
        });
    }

    // User declined to define repo-specific models. Offer to pin one of
    // the global models as the repo's default-model anyway -- skipping
    // the prompt would leave a config with no `default-model` when the
    // global also has none, which fails validation.
    let default = ask_repo_default_model_from_global(prompt, summary).await?;
    Ok(RepoModelChoices {
        models: BTreeMap::new(),
        providers: BTreeMap::new(),
        default_model: default,
    })
}

/// Offered after the user declined to define repo-specific models. The
/// gate's default flips with whether a global default already exists --
/// if it does, inheriting is the natural choice (default N); if not,
/// picking is needed to avoid a broken config (default Y).
async fn ask_repo_default_model_from_global(
    prompt: &mut impl PromptSource,
    summary: &GlobalSummary,
) -> Result<Option<String>> {
    if summary.models.is_empty() {
        return Ok(None);
    }
    let prompt_default = summary.default_model.is_none();
    if !prompt
        .ask_bool(&REPO_DEFAULT_MODEL_FIELD, prompt_default)
        .await?
    {
        return Ok(None);
    }
    Ok(Some(pick_global_model(prompt, summary).await?))
}

/// Forces the agent to pin its own `model` when there's no default-model
/// set anywhere -- otherwise the merged config fails the
/// `agent omits 'model' and no top-level 'default-model' is set`
/// validation rule. When a default is set somewhere, returns `None` so
/// the agent inherits.
async fn ask_agent_model(
    prompt: &mut impl PromptSource,
    summary: &GlobalSummary,
    repo: &RepoModelChoices,
) -> Result<Option<String>> {
    if repo.default_model.is_some() || summary.default_model.is_some() {
        return Ok(None);
    }
    // Models the agent could reference: repo-defined first, then global.
    let mut available: Vec<&str> = repo.models.keys().map(String::as_str).collect();
    available.extend(summary.models.iter().map(String::as_str));
    if available.is_empty() {
        eprintln!(
            "[outrig] no models defined anywhere -- the agent will be \
             written without a model and the config won't run until \
             you add one."
        );
        return Ok(None);
    }
    eprintln!(
        "[outrig] no default-model is set globally or in this repo; \
         this agent needs an explicit model."
    );
    eprintln!("[outrig] models available: {}", available.join(", "));
    let suggestion = available[0].to_string();
    loop {
        let answer = prompt.ask_string(&AGENT_MODEL_FIELD, &suggestion).await?;
        if available.iter().any(|m| *m == answer) {
            return Ok(Some(answer));
        }
        eprintln!(
            "[outrig] no model named `{answer}`; available: {}",
            available.join(", ")
        );
    }
}

/// Prompts for a global model name, validating against `summary.models`.
/// Used by both the post-N "set repo default-model?" path and any other
/// caller that needs the user to pick from already-configured models.
async fn pick_global_model(
    prompt: &mut impl PromptSource,
    summary: &GlobalSummary,
) -> Result<String> {
    let listing = summary.models.join(", ");
    let suggestion = summary
        .default_model
        .as_deref()
        .unwrap_or(&summary.models[0])
        .to_string();
    loop {
        let answer = prompt.ask_string(&PICK_MODEL_FIELD, &suggestion).await?;
        if summary.models.iter().any(|m| m == &answer) {
            return Ok(answer);
        }
        eprintln!("[outrig] no model named `{answer}`; available: {listing}");
    }
}

/// Outcome of the model section: any new repo-local providers / models
/// the user defined, plus the chosen repo `default-model` if any.
#[derive(Default)]
struct RepoModelChoices {
    providers: BTreeMap<String, LlmProvider>,
    models: BTreeMap<String, Model>,
    default_model: Option<String>,
}

/// Default agent name used in the bootstrap prompt. A simple constant --
/// agents are named by role, not by repo, so the same default works
/// regardless of where you run from.
pub(crate) const DEFAULT_AGENT_NAME: &str = "coder";

/// Suggest `<repo-folder-kebab>-standard` as the default image-config
/// name, so the image (and `default-image`) carries the repo's
/// identity by default. Falls back to plain `"standard"` when the path
/// has no usable last component. Shared with `image_setup::add` so its
/// name prompt suggests the same value as `default-image` written
/// here.
pub(crate) fn default_image_name(repo_root: &Path) -> String {
    let folder = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_kebab_case())
        .filter(|s| !s.is_empty());
    match folder {
        Some(name) => format!("{name}-standard"),
        None => "standard".to_string(),
    }
}

fn render(
    agent_name: String,
    agent_model: Option<String>,
    image_name: String,
    host_path: String,
    container_path: String,
    model_choices: RepoModelChoices,
    preamble: String,
) -> Result<String> {
    let mut agents = BTreeMap::new();
    agents.insert(
        agent_name.clone(),
        Agent {
            model: agent_model,
            image: None,
            preamble: Some(preamble),
            temperature: None,
            max_tokens: None,
            tool_call_max: None,
            tool_result_max: None,
        },
    );
    let cfg = Config {
        default_image: Some(image_name),
        default_agent: Some(agent_name),
        default_model: model_choices.default_model,
        tool_call_max: None,
        tool_result_max: None,
        workspace: Workspace {
            host_path: PathBuf::from(host_path),
            container_path: PathBuf::from(container_path),
            mounts: Vec::new(),
        },
        providers: model_choices.providers,
        models: model_choices.models,
        agents,
        ..Config::default()
    };
    toml::to_string_pretty(&cfg)
        .map_err(|e| OutrigError::Configuration(format!("rendering repo config: {e}")).into())
}

// ---- prompt fields --------------------------------------------------------

const CONFIGURE_NOW_FIELD: Field = Field {
    name: "Configure outrig in this directory now?",
    description: "Yes walks the same prompts as `outrig init` (workspace, model, \
                  agent) and writes .agents/outrig/config.toml here, then \
                  continues with `image add`. No exits without changes.",
    options: &[],
    doc_link: "doc/usage/init.md",
};

const HOST_PATH_FIELD: Field = Field {
    name: "Workspace host-path",
    description: "Path on the host that gets bind-mounted into the container. \
                  Resolved relative to the repo root.",
    options: &[],
    doc_link: "doc/concepts/workspace.md",
};

const CONTAINER_PATH_FIELD: Field = Field {
    name: "Workspace container-path",
    description: "Path inside the container where the host workspace is mounted.",
    options: &[],
    doc_link: "doc/concepts/workspace.md",
};

const AGENT_NAME_FIELD: Field = Field {
    name: "Agent name",
    description: "Names the agent you're creating now. Becomes the \
                  [agents.<name>] key and is also set as default-agent.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const CONFIGURE_REPO_MODELS_FIELD: Field = Field {
    name: "Would you like to configure LLM models specific to this repo?",
    description: "Yes: define one or more [models.<name>] entries in the repo \
                  config (using the global providers) and optionally set one \
                  as the repo default-model. No: inherit the global models \
                  and default-model.",
    options: &[],
    doc_link: "doc/concepts/llm-providers.md",
};

const REPO_DEFAULT_MODEL_FIELD: Field = Field {
    name: "Set a default-model for this repo?",
    description: "Yes: pin one of the global models as the repo's \
                  `default-model`. No: inherit the global default-model \
                  if one is set, or fall back to per-agent model selection.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const PICK_MODEL_FIELD: Field = Field {
    name: "Default model",
    description: "Pick one of the models from your global config to set as \
                  the repo-level default-model.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const AGENT_MODEL_FIELD: Field = Field {
    name: "Model for this agent",
    description: "No default-model is configured globally or at the repo \
                  level, so this agent needs an explicit `model`. Pick one \
                  of the models available in scope.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const PREAMBLE_FIELD: Field = Field {
    name: "Preamble (one line, edit later)",
    description: "System prompt prepended to every conversation with this agent.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

/// Slice of every `Field` declared in this module, for `prompt_doc_sync.rs`.
pub const DOC_SYNC_FIELDS: &[&Field] = &[
    &CONFIGURE_NOW_FIELD,
    &HOST_PATH_FIELD,
    &CONTAINER_PATH_FIELD,
    &AGENT_NAME_FIELD,
    &CONFIGURE_REPO_MODELS_FIELD,
    &REPO_DEFAULT_MODEL_FIELD,
    &PICK_MODEL_FIELD,
    &AGENT_MODEL_FIELD,
    &PREAMBLE_FIELD,
];
