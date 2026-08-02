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
    Prompt(rig::completion::PromptError),

    /// Rig refused to build the request because no output-token ceiling was
    /// set. Its own wording names `max_tokens`, which is the field on the wire
    /// and not a key any outrig config can carry -- the model and agent tables
    /// are `rename_all = "kebab-case"`, so `max_tokens` would be rejected as an
    /// unknown field. Say `max-tokens` instead, and say where it goes.
    ///
    /// `build_agent` supplies a fallback ceiling on the native Anthropic path,
    /// so this should not be reachable there. It stays for the paths that have
    /// no such fallback: streaming, a provider arm added later, or a rig
    /// upgrade that moves where the default comes from.
    #[error(
        "the model needs an output-token ceiling: set max-tokens under [models.<name>] \
         or [agents.<name>] in outrig.toml (the provider's API calls it `max_tokens`)"
    )]
    PromptMissingMaxTokens,
}

/// Rig's wording for the missing ceiling, matched to replace it. Pinned by the
/// tests below; a rig upgrade that rephrases it makes them fail rather than
/// silently letting the raw text back through.
const RIG_MISSING_MAX_TOKENS: &str = "`max_tokens` must be set for Anthropic";

impl From<rig::completion::PromptError> for CliError {
    fn from(e: rig::completion::PromptError) -> Self {
        use rig::completion::{CompletionError, PromptError};

        match &e {
            PromptError::CompletionError(CompletionError::RequestError(source))
                if source.to_string().contains(RIG_MISSING_MAX_TOKENS) =>
            {
                CliError::PromptMissingMaxTokens
            }
            _ => CliError::Prompt(e),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::{CompletionError, PromptError};

    fn request_error(message: &str) -> PromptError {
        PromptError::CompletionError(CompletionError::RequestError(message.into()))
    }

    /// The whole point of the rewrite: a user reading this must be able to go
    /// straight to their config, so it names the key they type and the tables
    /// it belongs in -- never rig's `max_tokens`, which no config accepts.
    #[test]
    fn a_missing_ceiling_names_the_config_key_not_the_wire_field() {
        let rendered = CliError::from(request_error(RIG_MISSING_MAX_TOKENS)).to_string();
        assert!(
            rendered.contains("max-tokens")
                && rendered.contains("[models.<name>]")
                && rendered.contains("[agents.<name>]"),
            "the message should point at the config, got: {rendered}",
        );
        assert!(
            !rendered.contains("agent prompt failed"),
            "the rewrite replaces the generic prefix rather than nesting under it, \
             got: {rendered}",
        );
    }

    /// Everything else keeps rig's text and the prefix that frames it. Without
    /// this the rewrite could quietly widen to swallow unrelated failures.
    #[test]
    fn any_other_prompt_failure_is_left_alone() {
        let rendered = CliError::from(request_error("something else entirely")).to_string();
        assert_eq!(
            rendered,
            "agent prompt failed: CompletionError: RequestError: something else entirely",
        );
    }
}
