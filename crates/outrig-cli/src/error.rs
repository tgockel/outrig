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

    /// The MCP `initialize` handshake failed while *serving* -- `outrig mcp`
    /// and `outrig mcp self` are the only two callers, and both are in this
    /// crate.
    ///
    /// It lives here rather than on `OutrigError` because the library never
    /// serves: a consumer driving `mcp_proxy::ProxyServer` calls the SDK's
    /// `serve_server` itself and handles the SDK's error itself. `CliError` is
    /// not published -- `outrig-cli` exports only `run()` -- so keeping the
    /// SDK's type is free here and would not have been there. Boxed because
    /// the variant otherwise dominates `CliError`'s size, which is what
    /// `clippy::result_large_err` watches.
    #[error("mcp server initialize: {0}")]
    McpServerInitialize(#[source] Box<rmcp::service::ServerInitializeError>),

    #[error("agent prompt failed: {0}")]
    Prompt(rig::completion::PromptError),

    /// A failure from an `outrig` harness session, which renders its own
    /// errors.
    #[error(transparent)]
    Session(#[from] outrig::harness::SessionError),

    /// Rig refused to build the request because no output-token ceiling was
    /// set. Its own wording names `max_tokens`, which is the field on the wire
    /// and not a key any outrig config can carry -- the model and agent tables
    /// are `rename_all = "kebab-case"`, so `max_tokens` would be rejected as an
    /// unknown field. Say `max-tokens` instead, and say where it goes.
    ///
    /// `build_agent` supplies a fallback ceiling on the native Anthropic path,
    /// so this should not be reachable there. It stays for the paths that have
    /// no such fallback: a provider arm added later, or a rig upgrade that
    /// moves where the default comes from.
    #[error(
        "the model needs an output-token ceiling: set max-tokens under [models.<name>] \
         or [agents.<name>] in outrig.toml (the provider's API calls it `max_tokens`)"
    )]
    PromptMissingMaxTokens,

    /// An error the process exits with `status` for, not the 1 every other
    /// error gets. It prints as the error it wraps, so `error: ...` reads as
    /// it would have; only the status differs. The field is not named
    /// `source`, so a walk of the error chain meets the wrapped error's text
    /// once, not twice. A `match` on a variant does not see through it. See
    /// [`CliError::with_status`].
    #[error("{inner}")]
    WithStatus { status: i32, inner: Box<CliError> },
}

impl CliError {
    /// This error, with the process to exit with `status` for it instead of 1.
    pub fn with_status(self, status: i32) -> Self {
        CliError::WithStatus {
            status,
            inner: Box::new(self),
        }
    }

    /// The status the process exits with after printing this error, and the
    /// one a command records for the session: 1, unless
    /// [`CliError::with_status`] said otherwise.
    pub fn exit_status(&self) -> i32 {
        match self {
            CliError::WithStatus { status, .. } => *status,
            _ => 1,
        }
    }
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
        CliError::McpServerInitialize(Box::new(e))
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

    /// The status rides along without changing what `app.rs` prints: the text
    /// is the wrapped error's own, not a prefix over it.
    #[test]
    fn a_status_changes_the_exit_and_not_the_text() {
        let plain = CliError::from(std::io::Error::other("stdout went away"));
        assert_eq!(plain.exit_status(), 1, "an error without a status exits 1");
        let text = plain.to_string();

        let carried = plain.with_status(3);
        assert_eq!(carried.exit_status(), 3);
        assert_eq!(carried.to_string(), text);
    }
}
