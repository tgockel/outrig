//! Error type for the binary. Wraps [`outrig::error::OutrigError`] plus
//! the bin-only variants for LLM resolution and Rig prompt failures.

use thiserror::Error;

pub use outrig::error::OutrigError;

use crate::cli::signals::EndSignal;
use crate::llm::LlmResolveError;

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Outrig(#[from] OutrigError),

    /// A monitored session container went away: the borrowed container in
    /// attach mode stopped, or the watcher saw the primary die externally.
    #[error("{0}")]
    SessionMonitorStopped(String),

    /// A signal ended the session before it finished on its own. Not a
    /// failure to report: the signal was announced as it landed, and the
    /// session is torn down with its record keeping [`EndSignal::exit_code`],
    /// which is also what the process exits with.
    #[error("interrupted by {0}")]
    Interrupted(EndSignal),

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

    /// A session's `session.json` exists but does not parse -- a hand edit, a
    /// corrupted file, or a record from an outrig whose schema renamed a
    /// required field. Not a configuration fault, so it is not an
    /// [`OutrigError::Configuration`]: nothing the user configured is wrong.
    #[error("session record {} is unreadable: {source}", path.display())]
    SessionRecord {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// Rig's agent failed the turn. Boxed, as `Session` is: the error carries
    /// a run's history, and inline it would dominate every `CliError`.
    #[error("agent prompt failed: {0}")]
    Prompt(Box<rig::completion::PromptError>),

    /// A failure from an `outrig` harness session, which renders its own
    /// errors. Boxed, as `McpServerInitialize` is: inline, it is as large as
    /// [`OutrigError`], and the two together would grow every `CliError`.
    #[error(transparent)]
    Session(Box<outrig::harness::SessionError>),

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
    /// one a command records for the session: the signal's for
    /// [`CliError::Interrupted`], the one [`CliError::with_status`] set, and 1
    /// for everything else.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Interrupted(sig) => sig.exit_code(),
            CliError::WithStatus { status, .. } => *status,
            _ => 1,
        }
    }
}

impl From<rig::completion::PromptError> for CliError {
    fn from(e: rig::completion::PromptError) -> Self {
        CliError::Prompt(Box::new(e))
    }
}

impl From<outrig::harness::SessionError> for CliError {
    fn from(e: outrig::harness::SessionError) -> Self {
        CliError::Session(Box::new(e))
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
    use rig::completion::PromptError;
    use rig::error::ProviderError;

    /// A prompt failure keeps rig's text, under the prefix that frames it.
    #[test]
    fn a_prompt_failure_keeps_rigs_text_under_the_prefix() {
        let failed = PromptError::Provider(ProviderError::request(std::io::Error::other(
            "something else entirely",
        )));
        let rendered = CliError::from(failed).to_string();
        assert!(
            rendered.starts_with("agent prompt failed: "),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("something else entirely"),
            "rig's own text must come through: {rendered}"
        );
    }

    /// The status rides along without changing what `app.rs` prints: the text
    /// is the wrapped error's own, not a prefix over it.
    #[test]
    fn a_status_changes_the_exit_and_not_the_text() {
        let plain = CliError::from(std::io::Error::other("stdout went away"));
        assert_eq!(plain.exit_code(), 1, "an error without a status exits 1");
        let text = plain.to_string();

        let carried = plain.with_status(3);
        assert_eq!(carried.exit_code(), 3);
        assert_eq!(carried.to_string(), text);
    }
}
