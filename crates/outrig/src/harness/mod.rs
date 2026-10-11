//! The harness: an agent session an owner holds, drives, watches, and stops.
//!
//! The owner is a Rust program on the host -- `outrig run-new`, or an
//! application running agents for its own work. It gives a [`SessionBuilder`]
//! everything the session needs from outside -- the configuration, the agent
//! and model, the container, the secrets its model calls need, and its event
//! subscriptions -- and starts it in one call. The [`Session`] it gets back
//! owns the container, the Python interpreter in it, and the agent, and
//! everything the session started is stopped through it.
//!
//! - **Rounds.** [`Session::round`] drives one, and says how it ended and what
//!   it left running.
//! - **Events.** Each subscription receives the session's events in order
//!   ([`event`]); one that falls behind loses a counted gap and never slows the
//!   session. Its state -- starting, idle, round running, executing, closing,
//!   reported -- is among them, so an owner showing what the session is doing
//!   reads it there rather than parsing text.
//! - **Stopping.** [`Session::close_admission`] refuses new work at once, and
//!   [`Session::shutdown`] stops what is left and returns a
//!   [`ShutdownReport`]: whether everything is proven stopped, and how each
//!   execution running at the close ended.
//!
//! No type of the model-provider library this is built on appears here, so a
//! release of it cannot break this surface.

mod builder;
mod error;
pub mod event;
pub(crate) mod lifecycle;
mod report;
mod session;

use std::time::Duration;

pub use self::builder::{ContainerSpec, Progress, SessionBuilder, Step, container_spec};
pub use self::error::{Closed, ClosedBy, Failure, SessionError};
pub use self::report::{
    EventDelivery, ExecutionOutcome, ExecutionStatus, LogLoss, ShutdownReport, Stopped, Verdict,
};
pub use self::session::{Closer, RoundEnd, RoundOutcome, Session, StillRunning, Tasks};
pub use crate::agent::UserChannel;
pub use crate::config::{EnvSecrets, Secrets};

/// How long [`Session::shutdown`] lets an execution that is still running
/// finish, as `outrig run-new` uses it: long enough for one about to end to
/// report, short enough that leaving stays prompt.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);
