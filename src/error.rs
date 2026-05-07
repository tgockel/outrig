//! Top-level error type.

use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

use crate::config::api_key::ApiKeyError;
use crate::config::env_value::EnvValueError;
use crate::config::validate::ConfigValidationError;
#[cfg(feature = "internal")]
use crate::llm::LlmResolveError;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("configuration: {0}")]
    Configuration(String),

    #[error("no .agents/outrig/config.toml found in current directory or any parent")]
    NoRepoConfig,

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Config(#[from] toml::de::Error),

    #[error("{0}")]
    ApiKey(#[from] ApiKeyError),

    #[error("{0}")]
    ConfigValidation(#[from] ConfigValidationError),

    #[error("{}", format_process(program, argv, *exit_code, stderr_tail))]
    Process {
        program: &'static str,
        argv: Vec<OsString>,
        exit_code: Option<i32>,
        stderr_tail: String,
    },

    #[error("could not allocate {kind} name during container bootstrap after retries")]
    BootstrapExhausted { kind: &'static str },

    #[error("mcp service: {0}")]
    McpService(#[from] rmcp::service::ServiceError),

    #[error("mcp server {name:?} env key {key:?}: {source}")]
    McpEnvResolveFailed {
        name: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("{0}")]
    McpStartupFailed(Box<McpStartupFailure>),

    #[error("mcp server {name:?} tools/list failed: {source}")]
    McpToolsListFailed {
        name: String,
        #[source]
        source: Box<rmcp::service::ServiceError>,
    },

    #[error("mcp call_tool: arguments must be a JSON object or null, got {kind}")]
    McpArgsNotObject { kind: &'static str },

    // Only the binary's `outrig run` / `cli::session_setup` path resolves
    // an LLM provider, and that path is gated under `internal`. Library
    // callers drive their own LLM loop; they never hit this variant. So
    // the enum stays one variant smaller in the curated build.
    #[cfg(feature = "internal")]
    #[error("{0}")]
    LlmResolve(#[from] LlmResolveError),

    #[error("agent prompt failed: {0}")]
    Prompt(#[from] rig::completion::PromptError),
}

impl From<tempfile::PersistError> for OutrigError {
    fn from(e: tempfile::PersistError) -> Self {
        OutrigError::Io(e.error)
    }
}

/// Boxed payload for [`OutrigError::McpStartupFailed`]. Carried behind a `Box`
/// so the variant doesn't bloat the size of `OutrigError` (which is what
/// `clippy::result_large_err` watches).
#[derive(Debug, Error)]
#[error(
    "mcp server {name:?} failed to start: {source}\n  \
     exit: {exit}\n  \
     command: {command}\n  \
     stderr ({stderr_path}):\n{stderr_tail}",
    stderr_path = stderr_path.display(),
)]
pub struct McpStartupFailure {
    pub name: String,
    pub command: String,
    pub exit: String,
    pub stderr_path: PathBuf,
    pub stderr_tail: String,
    #[source]
    pub source: std::io::Error,
}

pub type Result<T> = std::result::Result<T, OutrigError>;

fn format_process(
    program: &str,
    argv: &[OsString],
    exit_code: Option<i32>,
    stderr_tail: &str,
) -> String {
    let exit = match exit_code {
        Some(c) => format!("code {c}"),
        None => "signal".to_string(),
    };
    format!(
        "process `{program}` exited with {exit}\nargv: {argv:?}\n\
         --- stderr (tail) ---\n{stderr_tail}"
    )
}
