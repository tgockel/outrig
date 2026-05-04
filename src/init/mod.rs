//! `outrig init` -- three-phase orchestrator for end-to-end setup.
//!
//! Each phase is independently idempotent:
//! 1. **Global config** -- defer to [`config::init::run_with`] when
//!    `~/.outrig/config.toml` is absent; log + skip when present.
//! 2. **Repo config** -- [`repo::ensure`] writes `.agents/outrig/config.toml`
//!    against `cwd` if missing. Also reused by `outrig container add` as a
//!    fallback when run in an uninitialized repo.
//! 3. **Container loop** -- offer to scaffold container-configs by
//!    delegating to [`container::add::run_with`] in a loop.
//!
//! One [`PromptSource`] is threaded through all three phases so scripted
//! tests can drive the entire flow with a single byte stream.

pub mod prompt;
pub mod repo;

use std::path::Path;

use crate::error::Result;
use crate::init::prompt::{Field, PromptSource};
use crate::{config, container, repo as repo_paths};

pub async fn run(force: bool, global_override: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut prompt = prompt::auto();
    run_with(force, global_override, &cwd, &mut prompt).await
}

/// Drives the three-phase flow against an arbitrary `PromptSource`.
/// `cwd` anchors the repo-config phase (no walk-up; init is meant for
/// initial setup of the directory you're standing in).
pub async fn run_with(
    force: bool,
    global_override: Option<&Path>,
    cwd: &Path,
    prompt: &mut impl PromptSource,
) -> Result<()> {
    // Phase 1: global config.
    let global_path = repo_paths::global_config_path(global_override);
    if global_path.exists() {
        eprintln!(
            "[outrig] using existing global config at {}",
            global_path.display()
        );
    } else {
        eprintln!(
            "[outrig] no global config found at {} -- let's create one.",
            global_path.display()
        );
        config::init::run_with(force, &global_path, prompt).await?;
        eprintln!("[outrig] wrote {}", global_path.display());
    }

    // Phase 2: repo config.
    repo::ensure(cwd, prompt).await?;

    // Phase 3: container loop. Always offered -- adding more containers
    // later is the expected workflow.
    let mut first = true;
    loop {
        let field = if first {
            &ADD_FIRST_CONTAINER_FIELD
        } else {
            &ADD_ANOTHER_CONTAINER_FIELD
        };
        let default = first;
        if !prompt.ask_bool(field, default).await? {
            break;
        }
        container::add::run_with(cwd, None, force, prompt).await?;
        first = false;
    }

    Ok(())
}

const ADD_FIRST_CONTAINER_FIELD: Field = Field {
    name: "Add a container-config now?",
    description: "Yes: walk through `outrig container add` to scaffold a \
                  Dockerfile and [containers.<name>] block.",
    options: &[],
    doc_link: "doc/usage/init.md",
};

const ADD_ANOTHER_CONTAINER_FIELD: Field = Field {
    name: "Add another container-config?",
    description: "Yes: scaffold one more container-config via \
                  `outrig container add`. No: finish init.",
    options: &[],
    doc_link: "doc/usage/init.md",
};

/// Slice of every `Field` declared in this module, for `prompt_doc_sync.rs`.
pub const DOC_SYNC_FIELDS: &[&Field] = &[&ADD_FIRST_CONTAINER_FIELD, &ADD_ANOTHER_CONTAINER_FIELD];
