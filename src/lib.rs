//! outrig: run LLM agents with podman-isolated MCP servers.
//!
//! With default features (`internal` on), every subsystem is `pub` so the
//! `outrig` binary, the integration tests, and any in-tree consumer can
//! reach the lower-level primitives directly. Building with
//! `--no-default-features` strips that down to a curated library surface
//! centered on [`Outrig::launch`].

// With `internal` off, large swaths of the crate (session bookkeeping,
// repo path helpers, image-cache metadata) are not on any code path
// reachable from the curated surface. They still need to compile so the
// binary can use them when `internal` is on.
#![cfg_attr(not(feature = "internal"), allow(dead_code))]

use std::path::{Path, PathBuf};

// --- always public: the curated library surface ---
pub mod config;
pub mod error;
pub mod mcp_proxy;

mod outrig_;

pub use config::{CapabilityProfile, MountAccess, NetworkMode};
pub use mcp::{McpTool, McpToolResult};
pub use outrig_::{
    CapabilitySpec, LaunchSpec, MountSpec, NetworkSpec, Outrig, SecuritySpec, ToolHandle,
    WorkspaceSpec,
};

/// Load the project config rooted at `dir`. Walks up from `dir` looking
/// for `.agents/outrig/config.toml`, then merges in the optional `global`
/// config (repo precedence) and validates the result. Returns the merged
/// [`Config`] plus the resolved repo root.
///
/// [`Config`]: crate::config::Config
pub fn load_project(dir: &Path, global: Option<&Path>) -> error::Result<(config::Config, PathBuf)> {
    let repo_root = repo::find_repo_root_from(dir)?;
    let cfg = config::Config::load(&repo_root, global)?;
    Ok((cfg, repo_root))
}

// --- subsystems ---
//
// Modules below participate in two visibility tiers:
//
// 1. Always present in the crate (so the curated facade above and other
//    in-crate callers can use them).
// 2. Externally `pub` only when the `internal` feature is on. With
//    `internal` off, the module is just `mod foo;` so nothing outside the
//    curated surface above is visible to library users.

#[cfg(feature = "internal")]
pub mod container;
#[cfg(not(feature = "internal"))]
mod container;

#[cfg(feature = "internal")]
pub mod image;
#[cfg(not(feature = "internal"))]
mod image;

#[cfg(feature = "internal")]
pub mod mcp;
#[cfg(not(feature = "internal"))]
mod mcp;

#[cfg(feature = "internal")]
pub mod network;
#[cfg(not(feature = "internal"))]
mod network;

#[cfg(feature = "internal")]
pub mod process;
#[cfg(not(feature = "internal"))]
mod process;

#[cfg(feature = "internal")]
pub mod repo;
#[cfg(not(feature = "internal"))]
mod repo;

#[cfg(feature = "internal")]
pub mod session;
#[cfg(not(feature = "internal"))]
mod session;

#[cfg(feature = "internal")]
pub mod tool_name;
#[cfg(not(feature = "internal"))]
mod tool_name;

// Modules used only by the binary / `internal` callers. No curated-surface
// item depends on them, so they don't need to exist when `internal` is off.
#[cfg(feature = "internal")]
pub mod cli;
#[cfg(feature = "internal")]
pub mod hf;
#[cfg(feature = "internal")]
pub mod init;
#[cfg(feature = "internal")]
pub mod llm;
#[cfg(feature = "internal")]
pub mod mcp_self;
#[cfg(feature = "internal")]
pub mod repl;
#[cfg(feature = "internal")]
pub mod rig_tool;
