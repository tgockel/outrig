//! Top-level error type.

use std::ffi::OsString;

use thiserror::Error;

use crate::config::api_key::ApiKeyError;
use crate::config::validate::ConfigValidationError;
use crate::llm::LlmResolveError;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

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

    #[error("mcp call_tool: arguments must be a JSON object or null, got {kind}")]
    McpArgsNotObject { kind: &'static str },

    #[error("{0}")]
    LlmResolve(#[from] LlmResolveError),

    #[error("agent prompt failed: {0}")]
    Prompt(#[from] rig::completion::PromptError),
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
