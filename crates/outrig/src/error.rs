//! Top-level error type.

use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

use crate::config::{ApiKeyError, ConfigValidationError, EnvValueError};
use crate::container::embedded::EmbeddedImageConfigError;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("configuration: {0}")]
    Configuration(String),

    #[error(
        "this outrig was built without the filesystem-view helper\n\
         help: install the musl target (`rustup target add x86_64-unknown-linux-musl`, or the \
         aarch64 equivalent) and rebuild"
    )]
    FilesystemHelperUnavailable,

    #[error(
        "no .agents/outrig/config.toml found in current directory or any parent\n\
         help: run `outrig init` to initialize"
    )]
    NoRepoConfig,

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Config(#[from] toml::de::Error),

    #[error(
        "{source}\n\
         help: key names containing `.` must be quoted, e.g. `[models.\"opus-4.7\"]` instead of `[models.opus-4.7]`"
    )]
    ConfigDottedKey {
        #[source]
        source: toml::de::Error,
    },

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

    #[error("mcp server initialize: {0}")]
    McpServerInitialize(#[source] Box<rmcp::service::ServerInitializeError>),

    #[error("mcp server {name:?} env key {key:?}: {source}")]
    McpEnvResolveFailed {
        name: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} build-arg {key:?}: {source}")]
    BuildArgResolveFailed {
        image: String,
        key: String,
        #[source]
        source: EnvValueError,
    },

    #[error("image {image:?} embedded config: {source}")]
    EmbeddedImageConfigParse {
        image: String,
        #[source]
        source: Box<EmbeddedImageConfigError>,
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
}

impl From<tempfile::PersistError> for OutrigError {
    fn from(e: tempfile::PersistError) -> Self {
        OutrigError::Io(e.error)
    }
}

impl From<rmcp::service::ServerInitializeError> for OutrigError {
    fn from(e: rmcp::service::ServerInitializeError) -> Self {
        OutrigError::McpServerInitialize(Box::new(e))
    }
}

/// Boxed payload for [`OutrigError::McpStartupFailed`]. Carried behind a `Box`
/// so the variant doesn't bloat the size of `OutrigError` (which is what
/// `clippy::result_large_err` watches).
#[derive(Debug, Error)]
#[error(
    "mcp server {name:?}{declaration} failed to start: {source}\n  \
     exit: {exit}\n  \
     command: {command}\n  \
     stderr ({stderr_path}):\n{stderr_tail}",
    declaration = format_mcp_declaration_source(declaration_source),
    stderr_path = stderr_path.display(),
)]
pub struct McpStartupFailure {
    pub name: String,
    pub declaration_source: Option<String>,
    pub command: String,
    pub exit: String,
    pub stderr_path: PathBuf,
    pub stderr_tail: String,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

pub type Result<T> = std::result::Result<T, OutrigError>;

fn format_mcp_declaration_source(source: &Option<String>) -> String {
    source
        .as_ref()
        .map(|source| format!(" from {source}"))
        .unwrap_or_default()
}

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
