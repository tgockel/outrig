//! Why a session could not start, or a round could not run.

use std::fmt;

use serde::Serialize;

use super::root::RootError;
use crate::agent::AgentError;

/// Why a session failed to start, or a round failed to run. Its text says
/// what to do about it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The session takes no new work: its owner closed admission, or its
    /// interpreter exited. A round asked for afterward, and every submission a
    /// running round makes, gets this.
    #[error(transparent)]
    Closed(#[from] Closed),
    /// The model, or a key it needs, could not be resolved. Nothing started.
    #[error(transparent)]
    Resolve(Failure),
    /// The session could not start. Nothing it started is left running.
    #[error(transparent)]
    Start(Failure),
    /// A round failed. The session goes on: the messages the round had not
    /// read are still waiting, and what it ran is kept.
    #[error(transparent)]
    Round(Failure),
    /// The session has no primary agent to run a round: it was started with
    /// [`SessionBuilder::no_primary`](super::SessionBuilder::no_primary).
    #[error("the session has no primary agent, so it runs no rounds")]
    NoPrimary,
    /// The session has a primary agent, whose owner is the user at its `user`
    /// channel, so its owner constructs no root: a root is constructed in a
    /// session started with
    /// [`SessionBuilder::no_primary`](super::SessionBuilder::no_primary).
    #[error("the session has a primary agent, so its owner constructs no root")]
    Primary,
    /// The root [`SessionBuilder::construct`](super::SessionBuilder::construct)
    /// asked for, `class`, was not launched as the session started: `error`
    /// says why, as its [`RootHandle::ready`](super::RootHandle::ready) would.
    /// Nothing the start launched is left running.
    #[error("the root `{class}` could not start: {error}")]
    Construct {
        /// The class, as the builder named it.
        class: String,
        /// Why: boxed, as the largest of these, since every `Err` this
        /// crate hands back is the size of its largest variant.
        error: Box<RootError>,
    },
}

/// What went wrong, rendered as text: what OutRig's own types and the model
/// provider's errors say, without either crossing this surface.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct Failure(pub(crate) AgentError);

impl Failure {
    /// Whether the round had already run Python when it failed. What it ran
    /// is kept in the conversation, so a message repeating an earlier one
    /// would ask for it again.
    pub fn ran_python(&self) -> bool {
        matches!(
            self.0,
            AgentError::PromptAfterWork(_) | AgentError::PromptAfterWorkTerminal(_)
        )
    }
}

/// The session was closed to new work, and by what.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the session is closed to new work: {by}")]
pub struct Closed {
    by: ClosedBy,
}

impl Closed {
    /// What closed the session's admission.
    pub fn by(&self) -> &ClosedBy {
        &self.by
    }
}

impl From<ClosedBy> for Closed {
    fn from(by: ClosedBy) -> Self {
        Self { by }
    }
}

/// What closed a session's admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "by", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClosedBy {
    /// Its owner: [`Session::close_admission`](super::Session::close_admission),
    /// or `shutdown`.
    Owner,
    /// The interpreter exited, which ends a session.
    #[non_exhaustive]
    InterpreterExited {
        /// How it ended, as its `interpreter.exited` event records it, or
        /// `its output closed` while that is not yet known.
        cause: String,
    },
}

impl fmt::Display for ClosedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClosedBy::Owner => f.write_str("its owner closed admission"),
            ClosedBy::InterpreterExited { cause } => {
                write!(f, "the Python interpreter exited ({cause})")
            }
        }
    }
}
