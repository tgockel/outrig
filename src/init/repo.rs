//! Repo-config phase of `outrig init`, plus the bootstrap fallback used
//! by `outrig container add` when run in an uninitialized repo.
//!
//! Two public entry points share one private writer:
//! - [`ensure`] is what `outrig init` calls: idempotent: write the config
//!   if missing, log + skip if present.
//! - [`resolve_or_bootstrap`] is what `outrig container add` calls before
//!   dispatching: walk up to find an existing `.agents/outrig/config.toml`,
//!   and on `NoRepoConfig` prompt the user to bootstrap one against `cwd`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Agent, Config, Workspace};
use crate::error::{OutrigError, Result};
use crate::init::prompt::{Field, PromptSource};
use crate::repo;

pub async fn ensure(repo_root: &Path, prompt: &mut impl PromptSource) -> Result<()> {
    let cfg_path = repo::repo_config_path(repo_root);
    if cfg_path.exists() {
        eprintln!(
            "[outrig] using existing repo config at {}",
            cfg_path.display()
        );
        return Ok(());
    }
    eprintln!(
        "[outrig] no repo config at {} -- let's create one.",
        cfg_path.display()
    );
    write_repo_config(repo_root, prompt).await
}

/// Resolve the repo root for `outrig container add`. Walks up via
/// [`repo::find_repo_root_from`]; on [`OutrigError::NoRepoConfig`] prompts
/// the user, and on yes bootstraps the repo config against `cwd` and
/// returns `cwd`. On no, re-raises `NoRepoConfig` so the exit code and
/// error string match the previous behavior for scripts that test the
/// unconfigured case.
pub async fn resolve_or_bootstrap(cwd: &Path, prompt: &mut impl PromptSource) -> Result<PathBuf> {
    match repo::find_repo_root_from(cwd) {
        Ok(root) => Ok(root),
        Err(OutrigError::NoRepoConfig) => {
            eprintln!(
                "[outrig] no .agents/outrig/config.toml found in {} or any parent.",
                cwd.display()
            );
            if !prompt.ask_bool(&CONFIGURE_NOW_FIELD, true).await? {
                eprintln!("[outrig] skipping; run `outrig init` later to set up.");
                return Err(OutrigError::NoRepoConfig);
            }
            write_repo_config(cwd, prompt).await?;
            Ok(cwd.to_path_buf())
        }
        Err(other) => Err(other),
    }
}

/// Walks the five repo-config prompts, builds a [`Config`], serializes
/// to TOML, and writes atomically via [`repo::write_atomic`] (which
/// `create_dir_all`s the parent before persisting).
async fn write_repo_config(repo_root: &Path, prompt: &mut impl PromptSource) -> Result<()> {
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
    let agent_name = prompt.ask_string(&AGENT_NAME_FIELD, "coding").await?;

    let model = if prompt.ask_bool(&OVERRIDE_MODEL_FIELD, false).await? {
        let name = prompt.ask_string(&MODEL_NAME_FIELD, "").await?;
        if name.is_empty() { None } else { Some(name) }
    } else {
        None
    };

    let preamble = prompt
        .ask_string(&PREAMBLE_FIELD, "You are a careful coding assistant.")
        .await?;

    let toml_text = render(agent_name, host_path, container_path, model, preamble)?;
    let cfg_path = repo::repo_config_path(repo_root);
    repo::write_atomic(&cfg_path, &toml_text)?;
    eprintln!("[outrig] wrote {}", cfg_path.display());
    Ok(())
}

fn render(
    agent_name: String,
    host_path: String,
    container_path: String,
    model: Option<String>,
    preamble: String,
) -> Result<String> {
    let mut agents = BTreeMap::new();
    agents.insert(
        agent_name.clone(),
        Agent {
            model,
            container: None,
            preamble: Some(preamble),
            temperature: None,
            max_tokens: None,
        },
    );
    let cfg = Config {
        default_container: Some(agent_name.clone()),
        default_agent: Some(agent_name),
        workspace: Workspace {
            host_path: PathBuf::from(host_path),
            container_path: PathBuf::from(container_path),
        },
        agents,
        ..Config::default()
    };
    toml::to_string_pretty(&cfg)
        .map_err(|e| OutrigError::Configuration(format!("rendering repo config: {e}")))
}

// ---- prompt fields --------------------------------------------------------

const CONFIGURE_NOW_FIELD: Field = Field {
    name: "Configure outrig in this directory now?",
    description: "Yes walks the same prompts as `outrig init` (workspace, default \
                  agent, preamble) and writes .agents/outrig/config.toml here, \
                  then continues with `container add`. No exits without changes.",
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
    name: "Default agent name",
    description: "Used as the [agents.<name>] key and as default-agent / \
                  default-container so subsequent `container add` calls \
                  scaffold a matching container-config.",
    options: &[],
    doc_link: "doc/reference/config.md",
};

const OVERRIDE_MODEL_FIELD: Field = Field {
    name: "Override default-model for this agent?",
    description: "Yes: pin a specific [models.<name>] for this agent. \
                  No: agent inherits the global default-model.",
    options: &[],
    doc_link: "doc/concepts/llm-providers.md",
};

const MODEL_NAME_FIELD: Field = Field {
    name: "Model name (must exist in global config)",
    description: "Name of an existing [models.<name>] from the global config.",
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
    &OVERRIDE_MODEL_FIELD,
    &MODEL_NAME_FIELD,
    &PREAMBLE_FIELD,
];
