//! outrig: run LLM agents with podman-isolated MCP servers.
//!
//! This is the library crate. The companion `outrig-cli` crate produces
//! the `outrig` command-line tool and pulls in the LLM-side dependencies
//! (`rig-core`, `mistralrs-core`, `hf-hub`, etc.). Library consumers stay
//! free of that dep graph.
//!
//! The curated entry point is [`Outrig::launch`].

use std::path::{Path, PathBuf};

pub mod config;
pub mod container;
pub mod error;
pub mod image;
pub mod mcp;
pub mod mcp_proxy;
pub mod network;
mod outrig_;
pub mod process;
pub mod repo;
pub mod session;
pub mod tool_name;

pub use config::{
    CapabilityProfile, MountAccess, NetworkAction, NetworkEntry, NetworkMode, NetworkPolicy,
    NetworkPolicyBuilder,
};
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
