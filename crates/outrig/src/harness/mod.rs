//! The harness: an agent session an owner holds, drives, watches, and stops.
//!
//! The owner is a Rust program on the host -- `outrig run-new`, or an
//! application running agents for its own work. It gives a [`SessionBuilder`]
//! everything the session needs from outside -- the configuration, the agent
//! and model, the container, the secrets its model calls need, and its event
//! subscriptions -- and starts it in one call. The [`Session`] it gets back
//! owns the container, the Python interpreter in it, and the agent, and
//! everything the session started is stopped through it. A session started
//! with [`SessionBuilder::no_primary`] has no agent of its own and owns the
//! rest just the same.
//!
//! - **Rounds.** [`Session::round`] drives one, and says how it ended and what
//!   it left running. A session with no primary agent runs none.
//! - **Events.** Each subscription receives the session's events in order
//!   ([`event`]); one that falls behind loses a counted gap and never slows the
//!   session. Its state -- starting, idle, round running, executing, closing,
//!   reported -- is among them, so an owner showing what the session is doing
//!   reads it there rather than parsing text.
//! - **Bindings.** Objects a process on the host holds for the agent, from the
//!   configuration's `[bindings]` and [`SessionBuilder::bind`]. One a
//!   repository's config declared starts only once its [`Approver`] approves
//!   that exact declaration; each starts before the interpreter and stops with
//!   the session.
//! - **Skills.** The instructions and Python each [`SkillSource`] given to
//!   [`SessionBuilder::skill_source`] holds, the first source holding a name
//!   serving it. Their metadata is read as the session starts, with no Python
//!   run, and the primary agent is told each by name and description; a
//!   skill's files reach the container only as its code imports or reads them.
//! - **Stopping.** [`Session::close_admission`] refuses new work at once, and
//!   [`Session::shutdown`] stops what is left and returns a
//!   [`ShutdownReport`]: whether everything is proven stopped, and how each
//!   execution running at the close ended.
//!
//! - **A host program.** [`HostProgram`] runs a trusted Python program on the
//!   host as `outrig run-new <script.py>` does: its own `__main__`, argv, streams
//!   and exit status, with `outrig.config()` a view of the configuration
//!   holding no secret. Its `outrig.Session` enters a session with no primary
//!   agent, which this process starts and owns for it; whatever the program
//!   leaves open when it ends, this process stops, and reports in
//!   [`ProgramExit`].
//!
//! No type of the model-provider library this is built on appears here, so a
//! release of it cannot break this surface.

mod approval;
pub(crate) mod bindings;
mod builder;
mod error;
pub mod event;
pub(crate) mod lifecycle;
mod report;
mod session;
pub(crate) mod skills;

use std::time::Duration;

pub use self::approval::{Approval, ApprovalRequest, Approver};
pub use self::bindings::BindingSpec;
pub(crate) use self::builder::chosen_repo_root;
pub use self::builder::{ContainerSpec, Progress, SessionBuilder, Step, container_spec};
pub use self::error::{Closed, ClosedBy, Failure, SessionError};
pub use self::report::{
    BindingStop, EventDelivery, ExecutionOutcome, ExecutionStatus, HostedCall, LogLoss,
    RequestOutcome, ShutdownReport, Stopped, Verdict,
};
pub use self::session::{Closer, RoundEnd, RoundOutcome, Session, StillRunning, Tasks};
pub use self::skills::{DirectorySource, SkillFile, SkillSource};
pub use crate::agent::UserChannel;
pub use crate::config::{EnvSecrets, Secrets};
pub use crate::owner::{HostProgram, LeftOpen, ProgramExit, RunningProgram, SessionEnd};

#[cfg(test)]
#[path = "bindings_tests.rs"]
mod bindings_tests;

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

/// How long [`Session::shutdown`] lets an execution that is still running
/// finish, as `outrig run-new` uses it: long enough for one about to end to
/// report, short enough that leaving stays prompt.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);
