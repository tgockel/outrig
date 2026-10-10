//! A session's events, as values: what each subscriber is handed, in the order
//! the session published them.
//!
//! One stream rather than one per subject, because an agent's timeline is a
//! causal chain -- a model call produced this code, which printed this, which
//! sent this message -- and recovering that order by joining separate records
//! fails exactly when it is needed. Each event is numbered as it is published,
//! and the numbers are unique and gap-free, so [`Event::id`] is its place in
//! the session.
//!
//! A subscription is code its owner runs in its own process; nothing listens
//! for connections. It never slows the session: a subscription that falls
//! behind loses its oldest events, and is told how many before the next one it
//! receives ([`Received::Missed`]). Since the numbering has no gaps, every id a
//! subscription did not see is counted that way.
//!
//! The payloads are the ones `events.jsonl` records under `data`, field for
//! field: an event's [`Payload`] serializes to exactly that object, and
//! [`Event::kind`] is its `type` after `org.outrig.`. The [event log
//! reference](https://tgockel.github.io/outrig/reference/events.html) says what
//! each one means. Every type here is `#[non_exhaustive]`: a later release adds
//! fields and kinds without a break, so an embedder reads these and never
//! builds them.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::config::RoleAlternation;

/// How many events a subscription holds for its reader before it starts
/// losing the oldest, unless its owner chose otherwise.
pub const DEFAULT_CAPACITY: usize = 4096;

/// One thing the session did, or that was done to it, as it was published.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Event {
    /// Its place in the session's sequence, from 1.
    pub id: u64,
    /// When it was published.
    pub time: SystemTime,
    /// The agent it belongs to, or `None` for one about the session or the
    /// interpreter every agent shares.
    pub subject: Option<Subject>,
    pub payload: Payload,
}

impl Event {
    /// The event's type, without the `org.outrig.` prefix: `exec.submitted`,
    /// say.
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }
}

/// The agent an event belongs to, as the interpreter's protocol names it:
/// `agent/primary` for the one agent a session runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subject(Cow<'static, str>);

impl Subject {
    pub(crate) const PRIMARY: Subject = Subject(Cow::Borrowed(crate::events::PRIMARY_SUBJECT));

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub(crate) const fn new(id: u64) -> Self {
                Self(id)
            }

            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// One submission, for the life of the session's interpreter.
    ExecId
);
id_type!(
    /// One message on an agent's channel.
    MessageId
);
id_type!(
    /// One model call the loop decided to make, however many requests it took:
    /// a call that was retried or moved to another model keeps its id.
    CallId
);
id_type!(
    /// One request actually sent to a provider. A call that was retried or
    /// moved has an attempt for each request, under the one [`CallId`]. Unique
    /// within its session.
    AttemptId
);

/// Seconds, as the event log writes a duration.
fn seconds<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
}

/// What an event says. Each serializes to the `data` the event log records
/// for its kind.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Payload {
    // Model view: what a model call was presented, or produced.
    /// What every model call carries besides the conversation.
    #[non_exhaustive]
    ModelInstructions {
        model: String,
        preamble: String,
        tools: Vec<ToolDefinition>,
        max_tokens: Option<u32>,
    },
    /// A turn of the conversation, as it was committed. `messages` are rig's
    /// message JSON, whose form can change with a rig upgrade.
    #[non_exhaustive]
    TurnCommitted {
        turn: u64,
        round: u32,
        incomplete: bool,
        messages: Vec<Value>,
    },
    /// What a model call was sent of the conversation, and why.
    ModelCall(ModelCall),
    #[non_exhaustive]
    ExecSubmitted { execid: ExecId, source: String },

    // Execution diagnostics: the runtime's own state.
    #[non_exhaustive]
    SessionState { state: SessionState },
    /// What the session's shutdown found: its report, less how its events
    /// were delivered, which is final only once this has been published.
    #[non_exhaustive]
    SessionReport {
        closed_by: super::ClosedBy,
        stopped: super::Stopped,
        executions: Vec<super::ExecutionOutcome>,
        verdict: super::Verdict,
    },
    #[non_exhaustive]
    AgentStarted {
        model: String,
        python: String,
        container: String,
        tool_call_max: u64,
        tool_result_max: u64,
    },
    #[non_exhaustive]
    AgentStopped {},
    #[non_exhaustive]
    RoundStarted { round: u32 },
    /// A submission that was not run: `holder` held the interpreter, or the
    /// session was closed to new work.
    #[non_exhaustive]
    ExecRefused {
        execid: ExecId,
        holder: Option<ExecId>,
        reason: Refusal,
    },
    #[non_exhaustive]
    ExecCompleted {
        execid: ExecId,
        status: ExecStatus,
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        output: String,
        dropped: u64,
        error: Option<String>,
        background: Vec<Background>,
    },
    #[non_exhaustive]
    MemoryExhausted { execid: ExecId },
    #[non_exhaustive]
    ExecCancelSent { execid: ExecId },
    #[non_exhaustive]
    ExecInterruptSent { execid: ExecId, runaway: bool },
    #[non_exhaustive]
    ExecProbeFailed {
        execid: ExecId,
        verdict: ProbeVerdict,
    },
    #[non_exhaustive]
    ExecAbandoned { execid: ExecId, why: Abandoned },
    #[non_exhaustive]
    InventoryObserved {
        execid: ExecId,
        names: Vec<Held>,
        total: u64,
        more: u64,
    },
    #[non_exhaustive]
    ToolResultTruncated {
        execid: ExecId,
        size: u64,
        max: u64,
        kept: u64,
    },
    #[non_exhaustive]
    ContextPromoted { turns: Vec<u64> },
    #[non_exhaustive]
    ContextDemoted { turns: Vec<u64> },
    #[non_exhaustive]
    OutputUnattributed { text: String },
    #[non_exhaustive]
    InterpreterDiagnostic { text: String },
    /// The interpreter's output closed, for `cause`. `expected` when the
    /// session's shutdown stopped it; otherwise it died, which ends the
    /// session.
    #[non_exhaustive]
    InterpreterExited { cause: String, expected: bool },

    // Integration audit: what crossed to a provider, or across a channel.
    #[non_exhaustive]
    ModelRoundCompleted {
        round: u32,
        stopped: Option<String>,
        usage: Option<Usage>,
        calls: Vec<CallUsage>,
        attempts: Vec<AttemptId>,
        input_tokens_max: Option<u64>,
    },
    #[non_exhaustive]
    ModelRoundFailed {
        round: u32,
        error: String,
        calls: Vec<CallUsage>,
        usage: Option<Usage>,
        attempts: Vec<AttemptId>,
    },
    #[non_exhaustive]
    ModelRoundDropped {
        round: u32,
        calls: Vec<CallUsage>,
        usage: Option<Usage>,
        attempts: Vec<AttemptId>,
    },
    /// One request a provider was sent, and how it ended.
    ModelAttempt(ModelAttempt),
    #[non_exhaustive]
    ModelRetry {
        model: String,
        /// The try that failed, counted from 1 within its layer's retries.
        attempt: u32,
        #[serde(serialize_with = "seconds")]
        delay: Duration,
        error: String,
        call_id: CallId,
        attempt_id: AttemptId,
    },
    #[non_exhaustive]
    ModelFailover {
        from: String,
        to: String,
        error: String,
        call_id: CallId,
        /// The last request `from` was sent, or `None` when it was passed
        /// over without one.
        attempt_id: Option<AttemptId>,
    },
    /// A usage record that arrived after its attempt's event, accepted: it
    /// replaces that attempt's null usage, once.
    #[non_exhaustive]
    ModelUsageReplaced {
        call_id: CallId,
        attempt_id: AttemptId,
        usage: Usage,
    },
    /// A usage record that changed nothing.
    #[non_exhaustive]
    ModelUsageRefused {
        call_id: Option<CallId>,
        attempt_id: AttemptId,
        usage: Usage,
        reason: UsageRefusal,
    },
    #[non_exhaustive]
    MessageSent {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
        body: String,
    },
    #[non_exhaustive]
    MessageRefused {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
        reason: String,
    },
    #[non_exhaustive]
    MessageReceived {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
    },
}

impl Payload {
    /// The event's type, without the `org.outrig.` prefix.
    pub fn kind(&self) -> &'static str {
        match self {
            Payload::ModelInstructions { .. } => "model.instructions",
            Payload::TurnCommitted { .. } => "turn.committed",
            Payload::ModelCall(_) => "model.call",
            Payload::ExecSubmitted { .. } => "exec.submitted",
            Payload::SessionState { .. } => "session.state",
            Payload::SessionReport { .. } => "session.report",
            Payload::AgentStarted { .. } => "agent.started",
            Payload::AgentStopped {} => "agent.stopped",
            Payload::RoundStarted { .. } => "round.started",
            Payload::ExecRefused { .. } => "exec.refused",
            Payload::ExecCompleted { .. } => "exec.completed",
            Payload::MemoryExhausted { .. } => "memory.exhausted",
            Payload::ExecCancelSent { .. } => "exec.cancel.sent",
            Payload::ExecInterruptSent { .. } => "exec.interrupt.sent",
            Payload::ExecProbeFailed { .. } => "exec.probe.failed",
            Payload::ExecAbandoned { .. } => "exec.abandoned",
            Payload::InventoryObserved { .. } => "inventory.observed",
            Payload::ToolResultTruncated { .. } => "tool.result.truncated",
            Payload::ContextPromoted { .. } => "context.promoted",
            Payload::ContextDemoted { .. } => "context.demoted",
            Payload::OutputUnattributed { .. } => "output.unattributed",
            Payload::InterpreterDiagnostic { .. } => "interpreter.diagnostic",
            Payload::InterpreterExited { .. } => "interpreter.exited",
            Payload::ModelRoundCompleted { .. } => "model.round.completed",
            Payload::ModelRoundFailed { .. } => "model.round.failed",
            Payload::ModelRoundDropped { .. } => "model.round.dropped",
            Payload::ModelAttempt(_) => "model.attempt",
            Payload::ModelRetry { .. } => "model.retry",
            Payload::ModelFailover { .. } => "model.failover",
            Payload::ModelUsageReplaced { .. } => "model.usage.replaced",
            Payload::ModelUsageRefused { .. } => "model.usage.refused",
            Payload::MessageSent { .. } => "message.sent",
            Payload::MessageRefused { .. } => "message.refused",
            Payload::MessageReceived { .. } => "message.received",
        }
    }

    /// The agent the event belongs to, or `None` for one about the session or
    /// the interpreter every agent shares.
    pub(crate) fn subject(&self) -> Option<Subject> {
        match self {
            Self::SessionState { .. }
            | Self::SessionReport { .. }
            | Self::OutputUnattributed { .. }
            | Self::InterpreterDiagnostic { .. }
            | Self::InterpreterExited { .. } => None,
            _ => Some(Subject::PRIMARY),
        }
    }
}

/// A tool a model call carries, as the provider is told of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// One model call's manifest: what it was sent of the conversation, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelCall {
    /// This assembly's place among the session's, from 0. A call moved to
    /// another model is assembled again, under the same `call_id`.
    pub call: u64,
    pub call_id: CallId,
    pub round: u32,
    pub budget: CallBudget,
    pub estimate: u64,
    pub carried: Vec<Chosen>,
    pub evicted: Vec<Chosen>,
    pub withheld: Vec<Chosen>,
    /// The round's opening, as rig's message JSON, when it was the call's
    /// prompt.
    pub opening: Option<Value>,
    pub adjacent: Vec<Repeat>,
    pub left_out: Vec<LeftOut>,
}

/// What a call was held to, in tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallBudget {
    pub model: String,
    pub window: u32,
    pub window_assumed: bool,
    pub reserve: u32,
    pub overhead: u64,
    pub max_tokens: Option<u32>,
    pub role_alternation: RoleAlternation,
}

/// A turn a call carried or left out, and why it was chosen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Chosen {
    pub turn: u64,
    pub why: Why,
}

/// Why a turn was chosen for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Why {
    /// The turn the call answers.
    Latest,
    /// An earlier turn of the call's own round.
    Round,
    /// A turn the agent's code promoted.
    Promoted,
    /// One of the conversation's first rounds.
    First,
    /// One of the rounds just before this one.
    Recent,
}

/// Where one role follows itself in what a call was sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Repeat {
    /// The turn that begins with the repeat, or `None` for the round's
    /// opening.
    pub turn: Option<u64>,
    pub role: Role,
}

/// Whose side of the conversation a message is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    User,
    Assistant,
}

/// A part of a carried turn a call was not sent, by its place: the turn, the
/// message within it, and the part within that.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct LeftOut {
    pub turn: u64,
    pub message: u64,
    pub part: u64,
}

/// What the session is doing, as an owner would show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionState {
    /// The container and the interpreter are coming up.
    Starting,
    /// No round is running.
    Idle,
    /// A round is in progress, and its model is being called.
    RoundRunning,
    /// Within a round, a submission is running in the interpreter.
    Executing,
    /// From the close of admission until the shutdown report exists.
    Closing,
    /// The shutdown report exists. The last event of a session.
    Reported,
}

/// Why a submission was not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Refusal {
    /// Another execution held the interpreter: the event's `holder`.
    Held,
    /// The session was closed to new work.
    Closed,
}

/// How an execution ended, as far as the host saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ExecStatus {
    Ok,
    Error,
    /// The interpreter refused it, disagreeing with the host about which
    /// execution held it.
    Refused,
    /// The interpreter exited first.
    Lost,
}

/// What a liveness check found an execution's event loop doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ProbeVerdict {
    Blocked,
    Spinning,
    Starved,
}

/// Why the host stopped waiting for an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Abandoned {
    /// The user interrupted it twice.
    User,
    /// It kept a CPU busy without letting its loop turn.
    Runaway,
}

/// A name the agent's namespace holds, and its value's type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Held {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

/// Output an earlier execution wrote after it reported, delivered with a
/// later one's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Background {
    pub id: ExecId,
    pub output: String,
    pub dropped: u64,
}

/// Tokens, as a provider reported them. Where a provider reported nothing,
/// the field that holds a `Usage` is `None`, never zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// A usage of `total_tokens` in all, for a test.
    #[cfg(test)]
    pub(crate) fn total(total_tokens: u64) -> Self {
        Self {
            total_tokens,
            ..Self::default()
        }
    }
}

/// A total is its parts added up.
impl std::iter::Sum for Usage {
    fn sum<I: Iterator<Item = Usage>>(parts: I) -> Usage {
        parts.fold(Usage::default(), |sum, part| Usage {
            input_tokens: sum.input_tokens + part.input_tokens,
            output_tokens: sum.output_tokens + part.output_tokens,
            total_tokens: sum.total_tokens + part.total_tokens,
            cached_input_tokens: sum.cached_input_tokens + part.cached_input_tokens,
            cache_creation_input_tokens: sum.cache_creation_input_tokens
                + part.cache_creation_input_tokens,
            reasoning_tokens: sum.reasoning_tokens + part.reasoning_tokens,
        })
    }
}

/// One of a round's model calls: its place in the round, the model that
/// answered it, the attempt that did, and what that attempt used.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallUsage {
    pub index: u64,
    pub model: String,
    pub call_id: CallId,
    pub attempt_id: AttemptId,
    pub usage: Option<Usage>,
}

/// One request a provider was sent: the call it served, the settings it went
/// with, and how it ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelAttempt {
    pub call_id: CallId,
    pub attempt_id: AttemptId,
    /// The `[models.<name>]` row it was sent for.
    pub model: String,
    /// The model the request named on the wire.
    pub identifier: String,
    /// The output-token ceiling the request carried, if any.
    pub max_tokens: Option<u32>,
    /// Why it failed, or `None` when it answered.
    pub error: Option<String>,
    /// What the provider reported it used, or `None` when it reported nothing.
    pub usage: Option<Usage>,
}

/// Why a usage record changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum UsageRefusal {
    /// The session has no attempt by that id.
    Unknown,
    /// The attempt's own event reported its usage.
    Reported,
    /// An earlier record already replaced the attempt's null usage.
    Replaced,
}

/// What a subscription hands its reader.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Received {
    Event(Arc<Event>),
    /// This many events, the oldest the subscription held, were dropped while
    /// it fell behind; the next event's id is that many past the last one
    /// received.
    Missed(u64),
}

/// Why [`Subscription::try_recv`] had nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum TryRecvError {
    #[error("no event is waiting")]
    Empty,
    #[error("the session's stream has ended")]
    Closed,
}

/// One reader's view of a session's events, made by
/// [`SessionBuilder::subscribe`](super::SessionBuilder::subscribe) before the
/// session starts, so it sees every event from the first.
pub struct Subscription {
    queue: Arc<crate::events::Queue>,
}

impl Subscription {
    pub(crate) fn new(queue: Arc<crate::events::Queue>) -> Self {
        Self { queue }
    }

    /// The next event, or the count of those missed before it; `None` once
    /// the session has ended its stream and every event held here was taken.
    /// Cancel-safe: dropping the future takes nothing.
    pub async fn recv(&mut self) -> Option<Received> {
        self.queue.recv().await
    }

    /// What [`Subscription::recv`] would return, without waiting.
    pub fn try_recv(&mut self) -> Result<Received, TryRecvError> {
        self.queue.next()
    }

    /// How many events this subscription has lost in all, whether or not its
    /// reader has been told yet.
    pub fn missed(&self) -> u64 {
        self.queue.missed()
    }

    /// How many events it holds before it loses the oldest.
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("capacity", &self.capacity())
            .field("missed", &self.missed())
            .finish()
    }
}
