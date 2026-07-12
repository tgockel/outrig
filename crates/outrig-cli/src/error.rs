//! Error type for the binary. Wraps [`outrig::error::OutrigError`] plus
//! the bin-only variants for LLM resolution and Rig prompt failures.

use thiserror::Error;

pub use outrig::error::OutrigError;

use crate::llm::LlmResolveError;

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Outrig(#[from] OutrigError),

    /// A monitored session container went away: the borrowed container in
    /// attach mode stopped, or the watcher saw the primary die externally.
    /// The type doubles as a control signal -- after teardown the process
    /// must `std::process::exit` instead of returning, because the blocking
    /// stdin read (MCP stdio transport, REPL) never completes while the peer
    /// holds the pipe open. See `cli::watcher::exit_if_monitor_stopped`.
    #[error("{0}")]
    SessionMonitorStopped(String),

    #[error("{0}")]
    LlmResolve(#[from] LlmResolveError),

    #[error("agent prompt failed: {0}")]
    Prompt(#[from] rig::completion::PromptError),
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Outrig(OutrigError::Io(e))
    }
}

impl From<rmcp::service::ServerInitializeError> for CliError {
    fn from(e: rmcp::service::ServerInitializeError) -> Self {
        CliError::Outrig(OutrigError::from(e))
    }
}

pub type Result<T> = std::result::Result<T, CliError>;
