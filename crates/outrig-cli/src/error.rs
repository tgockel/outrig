//! Error type for the binary. Wraps [`outrig::error::OutrigError`] plus
//! the bin-only variants for LLM resolution and Rig prompt failures.

use thiserror::Error;

pub use outrig::error::OutrigError;

use crate::llm::LlmResolveError;

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Outrig(#[from] OutrigError),

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
