//! `outrig container add` -- interactive scaffolding of a new container-config.
//!
//! Walks the user through name, base image, toolchains, and MCP servers, then
//! writes `.agents/outrig/containers/<name>/Dockerfile` and appends matching
//! `[containers.<name>]` and `[containers.<name>.mcp]` blocks to the repo
//! `config.toml`. The TOML mutation goes through `toml_edit` so any
//! surrounding comments and formatting survive.
//!
//! `run` constructs real terminal I/O; `run_with` is the test seam that takes
//! an arbitrary `PromptSource`.

use std::path::Path;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use crate::container_setup::render::{self, BaseImage, McpServer, Toolchain};
use crate::error::{OutrigError, Result};
use crate::init::prompt::{self, Field, PromptSource};
use crate::init::repo as init_repo;
use crate::paths::{
    container_dir, container_dir_rel, global_config_path, repo_config_path, write_atomic,
};

/// CLI entry point. Resolves the repo root from `cwd` (walking up, with a
/// fallback prompt to bootstrap a fresh `.agents/outrig/config.toml` if
/// none is found) before running the interactive container-add flow. One
/// `PromptSource` is threaded through both halves so the user sees a
/// single conversation. `global_override` plumbs `--global-config` into
/// the bootstrap path so the model-section can list models from the
/// right global config.
pub async fn run(
    cwd: &Path,
    global_override: Option<&Path>,
    name: Option<String>,
    force: bool,
) -> Result<()> {
    let global_path = global_config_path(global_override);
    let mut prompt = prompt::auto();
    let mut hf = crate::hf::auto();
    let (repo_root, bootstrapped_name) =
        init_repo::resolve_or_bootstrap(cwd, &global_path, &mut prompt, &mut hf).await?;
    // CLI-provided name wins; otherwise reuse whatever the bootstrap
    // already asked for.
    let effective = name.or(bootstrapped_name);
    run_with(&repo_root, effective, force, &mut prompt).await
}

/// Drives the interactive flow against an arbitrary `PromptSource`.
///
/// Idempotency probe (Dockerfile path + existing `[containers.<name>]`
/// block) runs *before* any prompts so an accidental re-run doesn't burn
/// through the user's input before bailing.
pub async fn run_with(
    repo_root: &Path,
    name_arg: Option<String>,
    force: bool,
    prompt: &mut impl PromptSource,
) -> Result<()> {
    let cfg_path = repo_config_path(repo_root);

    let name = match name_arg {
        Some(n) => n,
        None => {
            let default = init_repo::default_container_name(repo_root);
            prompt.ask_string(&NAME_FIELD, &default).await?
        }
    };

    let dockerfile_path = container_dir(repo_root, &name).join("Dockerfile");
    let mut doc = load_doc(&cfg_path)?;

    if !force {
        if dockerfile_path.exists() {
            return Err(OutrigError::Configuration(format!(
                "{} already exists; pass --force to overwrite.",
                dockerfile_path.display()
            ))
            .into());
        }
        if container_block_exists(&doc, &name) {
            return Err(OutrigError::Configuration(format!(
                "[containers.{name}] already exists in {}; pass --force to overwrite.",
                cfg_path.display()
            ))
            .into());
        }
    }

    let base_idx = prompt.ask_select(&BASE_FIELD, 0).await?;
    let base = BaseImage::ALL[base_idx];

    let toolchain_indices = prompt.ask_multiselect(&TOOLCHAIN_FIELD, &[]).await?;
    let toolchains: Vec<Toolchain> = toolchain_indices
        .iter()
        .map(|&i| Toolchain::ALL[i])
        .collect();

    // Defaults to `fs` only -- the most-defensible v0 minimum.
    let default_mcps: Vec<usize> = vec![DEFAULT_MCP_INDEX];
    let mcp_indices = prompt.ask_multiselect(&MCP_FIELD, &default_mcps).await?;
    let mcps: Vec<McpServer> = mcp_indices.iter().map(|&i| McpServer::ALL[i]).collect();

    let dockerfile = render::render(base, &toolchains, &mcps);
    write_atomic(&dockerfile_path, &dockerfile)?;
    eprintln!(
        "[outrig] wrote {}",
        display_rel(&dockerfile_path, repo_root)
    );

    insert_container_block(&mut doc, &name, &mcps);
    write_atomic(&cfg_path, &doc.to_string())?;
    eprintln!(
        "[outrig] added [containers.{name}] block to {}",
        display_rel(&cfg_path, repo_root)
    );
    if mcps.is_empty() {
        eprintln!("[outrig] [containers.{name}.mcp] is empty");
    } else {
        let names: Vec<&str> = mcps.iter().map(|m| m.as_str()).collect();
        eprintln!(
            "[outrig] added [containers.{name}.mcp] entries: {}",
            names.join(", ")
        );
    }

    Ok(())
}

// ---- prompt fields --------------------------------------------------------

pub(crate) const NAME_FIELD: Field = Field {
    name: "Container name",
    description: "Used as the [containers.<name>] key. Must match `^[a-zA-Z][a-zA-Z0-9_-]*$`.",
    options: &[],
    doc_link: "doc/usage/container.md",
};

const BASES: &[(&str, &str)] = &[
    (
        BaseImage::DebianBookwormSlim.as_str(),
        BaseImage::DebianBookwormSlim.description(),
    ),
    (
        BaseImage::Ubuntu24_04.as_str(),
        BaseImage::Ubuntu24_04.description(),
    ),
    (
        BaseImage::AlpineLatest.as_str(),
        BaseImage::AlpineLatest.description(),
    ),
    (
        BaseImage::Node20BookwormSlim.as_str(),
        BaseImage::Node20BookwormSlim.description(),
    ),
    (
        BaseImage::Python3_12Slim.as_str(),
        BaseImage::Python3_12Slim.description(),
    ),
];

const BASE_FIELD: Field = Field {
    name: "Base image",
    description: "The Dockerfile's `FROM` line. Pick one of the curated starting points.",
    options: BASES,
    doc_link: "doc/usage/container.md",
};

const TOOLCHAINS: &[(&str, &str)] = &[
    (
        "rust",
        "rustup + stable toolchain (cargo, rustfmt, clippy).",
    ),
    ("node", "Node 20 LTS via the base image's package manager."),
    ("python", "CPython 3 with pip and venv."),
    ("go", "Go 1.22."),
    ("none", "Just the base image -- nothing extra installed."),
];

const TOOLCHAIN_FIELD: Field = Field {
    name: "Language toolchains",
    description: "Pick zero or more language toolchains to install in the image. \
                  The Dockerfile template adds the corresponding install steps; \
                  you can edit the file afterwards.",
    options: TOOLCHAINS,
    doc_link: "doc/usage/container.md",
};

const MCPS: &[(&str, &str)] = &[
    (McpServer::Fs.as_str(), McpServer::Fs.description()),
    (McpServer::Git.as_str(), McpServer::Git.description()),
];

const MCP_FIELD: Field = Field {
    name: "MCP servers",
    description: "Pick zero or more MCP servers to install in the image. The \
                  Dockerfile installs each server's package and the matching \
                  [containers.<name>.mcp] entry is appended to config.toml.",
    options: MCPS,
    doc_link: "doc/concepts/mcp-servers.md",
};

/// Slice of every `Field` declared in this module, for `prompt_doc_sync.rs`.
pub const DOC_SYNC_FIELDS: &[&Field] = &[&NAME_FIELD, &BASE_FIELD, &TOOLCHAIN_FIELD, &MCP_FIELD];

/// Index into `McpServer::ALL` of the default selection (the `fs`
/// filesystem server). A `const` rather than a runtime `position()` lookup
/// since `ALL`'s order is itself a deliberate stable contract.
const DEFAULT_MCP_INDEX: usize = 0;
const _: () = assert!(matches!(McpServer::ALL[DEFAULT_MCP_INDEX], McpServer::Fs));

// ---- helpers --------------------------------------------------------------

fn display_rel<'a>(path: &'a Path, root: &Path) -> std::path::Display<'a> {
    path.strip_prefix(root).unwrap_or(path).display()
}

fn load_doc(cfg_path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(cfg_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    text.parse::<DocumentMut>().map_err(|e| {
        OutrigError::Configuration(format!("parsing {}: {e}", cfg_path.display())).into()
    })
}

fn container_block_exists(doc: &DocumentMut, name: &str) -> bool {
    doc.get("containers")
        .and_then(|c| c.as_table_like())
        .is_some_and(|t| t.contains_key(name))
}

fn insert_container_block(doc: &mut DocumentMut, name: &str, mcps: &[McpServer]) {
    let rel = container_dir_rel(name);
    let dockerfile = rel.join("Dockerfile").to_string_lossy().into_owned();
    let context = rel.to_string_lossy().into_owned();

    let mut entry = Table::new();
    entry.insert("dockerfile", Item::Value(Value::from(dockerfile)));
    entry.insert("context", Item::Value(Value::from(context)));

    let mut mcp = Table::new();
    for server in mcps {
        mcp.insert(server.as_str(), mcp_value(*server));
    }
    entry.insert("mcp", Item::Table(mcp));

    let containers = doc
        .entry("containers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .expect("containers must be a table");
    containers.insert(name, Item::Table(entry));
}

fn mcp_value(server: McpServer) -> Item {
    let mut cmd = Array::new();
    for arg in server.command_args() {
        cmd.push(*arg);
    }
    let mut full = InlineTable::new();
    full.insert("command", Value::Array(cmd));
    Item::Value(Value::InlineTable(full))
}
