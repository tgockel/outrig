#![doc = include_str!("../README.md")]
//!
//! # Supported surface
//!
//! The facade above is one of two supported tiers. `config`, `container`,
//! `error`, `image`, `mcp_proxy`, and `network` are the other: a caller that
//! wants the pieces rather than a whole managed session drives them directly,
//! and downstream crates do. Both tiers are a SemVer commitment; every other
//! module is private, reaching this root only through the re-exports below.

// `network` and `nsfork` below are declared unconditionally and call `setns`
// and `CLONE_NEW*`, which libc declares only under `linux_like` -- so an Apple
// or Windows target fails at name resolution, deep in a module a caller never
// asked for, dozens of errors at a time. This says it once instead. It is not
// a statement about `outrig-enter`: `build.rs` selects that helper's triple by
// architecture, never by OS, because it runs inside the container.
//
// One gate covers both crates: `outrig` is a hard dependency of `outrig-cli`,
// so cargo stops here before the binary is touched. Deleting this is part of
// the acceptance criteria in `plan/next/macos-host-support.md`.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "outrig does not build for this platform. Its container plumbing calls \
     setns(2) and CLONE_NEW* unconditionally, through the `network` and \
     `nsfork` modules. On Windows use WSL2, which is an ordinary Linux build; \
     on macOS run inside podman machine's VM. Host-native support for either \
     is tracked in plan/next/{macos,windows}-host-support.md."
);

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
mod supervise;
mod tool_name;

pub use config::{
    CapabilityProfile, MountAccess, NetworkAction, NetworkEntry, NetworkMode, NetworkPolicy,
    NetworkPolicyBuilder, SidecarView, SidecarWorkspaceAccess,
};
// Doubled at the root the way `config`'s types are, because it is the one
// `container` type a root-level *signature* names -- `Outrig::exec_stdio` and
// `exec_capture` take it. `ContainerCreateOptions` is used from `outrig_` but
// appears in no root signature, so it stays a single path. This is a
// convenience, not a rule: `Container` itself is named by
// `McpClient::connect_via_podman_exec` and stays put.
pub use container::ExecOptions;
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
