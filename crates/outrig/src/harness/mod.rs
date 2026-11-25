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
//! - **Services.** Rust objects the owner gives
//!   [`SessionBuilder::serve`], each describing its methods, which agent
//!   Python calls through an object generated from the description: each call
//!   checked against its method's schema, held to a deadline, cancellable,
//!   evented, and reported as a hosted request is.
//! - **Skills.** The instructions and Python each [`SkillSource`] given to
//!   [`SessionBuilder::skill_source`] holds, the first source holding a name
//!   serving it. Their metadata is read as the session starts, with no Python
//!   run, and the primary agent is told each by name and description; a
//!   skill's files reach the container only as its code imports or reads them.
//! - **Reading a root.** A [`RootHandle`] says what the root its owner constructed is doing --
//!   a round open, or why it is at rest -- and what each round said, read from the session's
//!   own record of its agents rather than from its events ([`RootHandle::status`]).
//! - **Reading the roster.** [`Session::roster`] lists the session's agents at one moment --
//!   each with what it is, what launched it, what it is doing and the requests made of it that
//!   have not ended -- from the same record, so it agrees with the [`ShutdownReport`] at the end.
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

// Here rather than crate-wide: the frozen 0.2 surface is undocumented.
#![warn(missing_docs)]

mod application;
mod approval;
pub(crate) mod bindings;
mod builder;
pub(crate) mod channels;
mod error;
pub mod event;
pub(crate) mod lifecycle;
pub(crate) mod mcp;
pub(crate) mod mcp_placement;
mod proxy;
mod questions;
pub use questions::{
    Answer, Choice, QUESTION_BYTES_MAX, QUESTION_CHANNEL, QUESTION_CHOICES_MAX, QUESTIONS_MAX,
    Question, QuestionBackend, QuestionContext, QuestionNotice, QuestionReference,
};
mod registry;
mod report;
mod root;
pub(crate) mod roster;
pub(crate) mod services;
mod session;
pub(crate) mod skills;
pub(crate) mod stop;

use std::time::Duration;

pub use self::application::{ApplicationIdentity, ApplicationSpec};
pub(crate) use self::proxy::connect_proxy_url;

pub use self::approval::{Approval, ApprovalRequest, ApprovalSubject, Approver};
pub use self::bindings::BindingSpec;
pub(crate) use self::builder::chosen_repo_root;
pub use self::builder::{ContainerSpec, Progress, SessionBuilder, Step, container_spec};
pub use self::channels::{
    BackendFuture, CHANNEL_BOUND_MAX, CancelledBy, Channel, ChannelBackend, ChannelDefinition,
    ChannelKind, ChannelOperation, Consumed, Contract, DEFAULT_CALL_TIMEOUT, DeliverError,
    Delivered, DeliveryId, Direction, Inbound, ItemKind, Outbound, Position, Query, SendAnswer,
    Settle, SettleError, Settled, Settlement, Wake, WithdrawAnswer, WithdrawEnd, Withdrawal,
};
pub use self::error::{Closed, ClosedBy, Failure, SessionError};
pub use self::mcp::{
    McpDeclaredIn, McpOutcomeKind, McpPauseHow, McpPlacement, McpPlacementKind, McpPolicy,
    McpRetryCause, McpServiceOutcome, McpServiceSpec, McpServiceStop, McpServices, McpStartupCause,
    McpStopHow, McpWriter, McpWrites, Quiesce, QuiesceOutcome, TrustAnchor,
};
pub use self::registry::{
    ChannelSnapshot, InboundItem, OutboundState, OutstandingRequest, Positions, ReceivedRequest,
    SentInRound, UncertainItem,
};
pub use self::report::{
    BindingStop, ChannelClose, ChannelRequestAtClose, EventDelivery, ExecutionOutcome,
    ExecutionStatus, HostedCall, LogLoss, McpUncertainCall, RequestOutcome, SendAtClose,
    ServiceCall, ShutdownReport, Stopped, UncertainAtClose, Verdict,
};
pub use self::root::{
    Argument, ConstructSpec, Facility, Problem, RequestChannel, RequestHandle, RootError,
    RootHandle,
};
pub use self::roster::{AgentKind, AgentRow, Outstanding, Parent, Roster, RosterState};
pub use self::services::{
    CallContext, MethodDescription, Service, ServiceDescription, ServiceError,
    ServiceErrorCategory, ServiceFuture, ServicePolicy,
};
pub use self::session::{Closer, RoundEnd, RoundOutcome, Session, StillRunning, Tasks};
pub use self::skills::{DirectorySource, SkillFile, SkillSource};
pub use self::stop::{AgentState, ModelRequests, RoundFacts, RoundSend, RoundStop, Status};
pub use crate::agent::UserChannel;
pub use crate::config::{EnvSecrets, Secrets};
pub use crate::owner::{HostProgram, LeftOpen, ProgramExit, RunningProgram, SessionEnd};
pub use crate::python::host::{OpenWait, Waits};

#[cfg(test)]
#[path = "bindings_tests.rs"]
mod bindings_tests;

#[cfg(test)]
#[path = "services_tests.rs"]
pub(crate) mod services_tests;

#[cfg(test)]
mod mcp_dispatch_tests;
#[cfg(test)]
mod mcp_http_tests;
#[cfg(test)]
mod mcp_presenter_tests;
#[cfg(test)]
mod mcp_tests;
#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

/// How long [`Session::shutdown`] lets an execution that is still running
/// finish, as `outrig run-new` uses it: long enough for one about to end to
/// report, short enough that leaving stays prompt.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);
