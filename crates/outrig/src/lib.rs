#![doc = include_str!("../README.md")]
//!
//! # Supported surface
//!
//! The facade above is one of two supported tiers. `config`, `container`,
//! `error`, `image`, `mcp_proxy`, and `network` are the other: a caller that
//! wants the pieces rather than a whole managed session drives them directly,
//! and downstream crates do. Both tiers are a SemVer commitment; every other
//! module is private, reaching this root only through the re-exports below.

use std::path::{Path, PathBuf};

pub mod config;
pub mod container;
pub mod error;
pub mod image;
mod mcp;
pub mod mcp_proxy;
pub mod network;
mod nsfork;
mod outrig_;
mod process;
mod repo;
mod tool_name;

pub use config::{
    CapabilityProfile, MountAccess, NetworkAction, NetworkEntry, NetworkMode, NetworkPolicy,
    NetworkPolicyBuilder, SidecarView, SidecarWorkspaceAccess,
};
pub use mcp::{McpClient, McpTool, McpToolResult, resolve_mcp_env};
pub use outrig_::{
    CapabilitySpec, EmbeddedMcpPolicy, LaunchSpec, MountSpec, NetworkSpec, Outrig, SecuritySpec,
    SidecarServerSpec, SidecarSpec, ToolHandle, WorkspaceSpec,
};
pub use process::Transcript;
pub use tool_name::{RESERVED_SERVER, sanitize as sanitize_tool_name};

/// Base URL of the published book. User-facing messages must link here rather
/// than to a `doc/` path -- the repo tree isn't present in an installed
/// binary, so a relative path is a dead end for anyone who didn't clone.
pub const PUBLIC_DOC_BASE_URL: &str = "https://tgockel.github.io/outrig/";

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
