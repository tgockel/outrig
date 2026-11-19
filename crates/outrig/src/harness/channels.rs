//! External channels: channels whose far end is a backend the session's owner
//! supplies, rather than the user or another agent.
//!
//! The owner declares each on [`SessionBuilder::channel`](super::SessionBuilder::channel) with a
//! [`ChannelDefinition`] -- its name, what crosses it each way, how many items it holds unread,
//! what an arrival does -- and the [`ChannelBackend`] at its far end: the owner's own messaging,
//! which routes, stores and identifies what crosses. One agent holds the channel, as
//! `runtime.channels[name]`: the primary agent, when the session has one the harness built, or the
//! agent whose facility dict names it ([`Facility::channel`](super::Facility::channel)).
//!
//! OutRig owns the local queue -- a window over the backend's durable one, which the owner paces
//! by pushing into it with [`Channel::deliver`] -- the announcement to the model by name and
//! count, the wake of a `runtime.wait`, the delivery ids, and the consumed position: an item is
//! reported to [`ChannelBackend::consumed`] once its holder's code receives it, and never when it
//! is merely queued. What the holder sends goes to [`ChannelBackend::send`], each item under an id
//! OutRig gives it, and its `send` returns once the backend has taken it, or raises saying why
//! not -- not taken, so a retry is safe; or not known, so a retry may repeat it.
//!
//! The backend is reached only from here: it is no binding, no global, nothing in the
//! interpreter, and no agent can name it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::event::{
    AgentRequestId, ItemOutcome, MessageId, Payload, RequestEnd, Resolved, Subject,
};
use super::lifecycle::Lifecycle;
use super::registry::{
    ChannelSnapshot, InboundItem, OutboundState, OutstandingRequest, Positions, ReceivedRequest,
    SentInRound, UncertainItem,
};
use super::report::{ChannelClose, ChannelRequestAtClose, SendAtClose, UncertainAtClose};
use super::skills::Catalog;
use super::stop::{ROUNDS_KEPT, RoundSend};
use crate::agent::orientation::ExternalText;
use crate::events::Events;
use crate::python::host::{
    Counts, Interpreter, InterpreterError, MESSAGE_MAX, PostRefusal, Posted,
};
use crate::python::hosted::ToInterpreter;

/// What a [`ChannelBackend`]'s methods return: a future of their answer, which
/// OutRig gives up on past the channel's [`ChannelDefinition::call_timeout`].
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The far end of an external channel: the session owner's own messaging.
///
/// OutRig calls it for every item the holder sends and every item the holder
/// receives, and never the other way: what arrives for the holder, the owner
/// pushes through [`Channel::deliver`]. Each call runs on the session's tokio
/// runtime, at most eight at once per channel whatever their kind, and is
/// dropped -- its future never polled again -- once it has run past the
/// channel's [`ChannelDefinition::call_timeout`]: a send dropped so is
/// [`ItemOutcome::Uncertain`].
pub trait ChannelBackend: Send + Sync + 'static {
    /// The holder sent `item`: take it durably and say so, with a reference
    /// of the backend's own if it has one; or say it was not taken, so
    /// sending it again is safe; or that it is not known whether it was.
    fn send(&self, item: Outbound) -> BackendFuture<'_, SendAnswer>;

    /// The holder's code received the item `notice` names. Called once per
    /// item, in the order they were received, and again after an error until
    /// it returns `Ok`, while the session lasts: treat it as idempotent per
    /// item.
    fn consumed(&self, notice: Consumed) -> BackendFuture<'_, io::Result<()>>;

    /// A request on a request channel ended as `notice` says, or an answer to
    /// one that had ended was refused, which changed nothing. Called once for
    /// each end of an inbound request -- the one path by which its requester
    /// learns the outcome -- and for each end of an outbound request the
    /// backend did not give itself: a reply that did not fit, or the channel's
    /// close, told once the request's [`send`](Self::send) has answered, and
    /// only if it took the request. In order with the channel's
    /// [`consumed`](Self::consumed) notices, and again after an error until it
    /// returns `Ok`, while the session lasts: treat it as idempotent per
    /// request. Acknowledge a notice of a request with no record here as well
    /// -- one whose send answered uncertain and was never kept: an error asks
    /// for the notice again, and holds back every later one on the channel
    /// until the session's drain ends.
    fn settled(&self, notice: Settled) -> BackendFuture<'_, io::Result<()>>;

    /// The holder's side ended outbound request `withdrawal` names -- its code
    /// cancelled it, or its holder was released or the session closed -- and
    /// asks the backend to withdraw it: answer [`WithdrawAnswer::Withdrawn`],
    /// or the reply or failure already recorded, which the request then
    /// settles with instead; a request with no record here is answered
    /// withdrawn. Called at most once per request, and only once the request's
    /// [`send`](Self::send) has answered that it took the request, or may
    /// have: a holder released, or a session closed, after its code cancelled
    /// the request asks nothing more. What withdrawing does at the request's
    /// peer is the backend's. A call that does not answer within the
    /// channel's [`ChannelDefinition::call_timeout`] leaves the request
    /// `uncertain`, and is not made again.
    fn withdrawn(&self, withdrawal: Withdrawal) -> BackendFuture<'_, io::Result<WithdrawAnswer>>;

    /// What became of the item `query` names, whose send answered uncertain or
    /// ran past its bound: answer from the backend's durable record --
    /// [`SendAnswer::Accepted`], with the reference it took the item under,
    /// [`SendAnswer::NotAccepted`], or [`SendAnswer::Uncertain`] when it cannot
    /// tell either.
    ///
    /// OutRig asks once as a send answers uncertain, before the holder's code
    /// is told; again each time that code asks with `resolve`; and once in the
    /// session's drain for a message still uncertain at the close. It never
    /// sends the item again: an uncertain item is resolved by its identity. A
    /// call that does not answer within the channel's
    /// [`ChannelDefinition::call_timeout`] leaves it uncertain.
    fn resolve_send(&self, query: Query) -> BackendFuture<'_, SendAnswer>;

    /// What became of the withdrawal of the outbound request `query` names,
    /// which [`withdrawn`](Self::withdrawn) did not answer in time: answer from
    /// the backend's durable record as `withdrawn` would have --
    /// [`WithdrawAnswer::Withdrawn`], or the reply or failure it recorded --
    /// which the request then settles with. An error, or no answer within the
    /// channel's [`ChannelDefinition::call_timeout`], leaves it uncertain.
    /// Asked only when the holder's code asks with `resolve`.
    fn resolve_withdrawal(&self, query: Query) -> BackendFuture<'_, io::Result<WithdrawAnswer>>;

    /// The channel's positions, as `positions` gives them: the last item its
    /// holder's code consumed, the last whose consumption the backend
    /// acknowledged, and those waiting unread, in order. Handed over at a cut,
    /// so the backend knows which items to deliver again, by item id, once the
    /// session resumes -- those unread, and those consumed and never
    /// acknowledged -- and which it may discard.
    fn positions(&self, positions: Positions) -> BackendFuture<'_, io::Result<()>>;
}

/// What an external channel is, fixed for the session: given with its backend
/// to [`SessionBuilder::channel`](super::SessionBuilder::channel).
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDefinition {
    name: String,
    kind: ChannelKind,
    receives: Contract,
    sends: Contract,
    bound: usize,
    wake: Wake,
    description: String,
    call_timeout: Duration,
    initial: Vec<Inbound>,
    reference: Option<String>,
}

/// The most items a channel holds unread, and its default bound.
pub const CHANNEL_BOUND_MAX: usize = 256;

/// How long a backend's method has to answer, unless the definition says.
/// This is a channel default, independent of the skill-source call bounds.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How many of one channel's calls its backend is given at once: its sends,
/// notices and withdrawals together.
const CALLS_MAX: usize = 8;

/// How long the interpreter has to say whether it queued an item: it answers
/// on its reader thread, checking the body against the channel's type.
const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a consumed notice the backend refused waits before it is sent
/// again, at first and at most.
const NOTICE_RETRY_FIRST: Duration = Duration::from_millis(100);
const NOTICE_RETRY_MAX: Duration = Duration::from_secs(5);

/// How the event log names an external channel's far end.
const BACKEND_END: &str = "backend";

/// Why a call of a backend has no answer once the session's drain has ended.
const DRAINED: &str = "the session's drain ended before the backend answered";

impl ChannelDefinition {
    /// The name the channel's holder reaches it by.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// A channel named `name` -- the holder's `runtime.channels` key -- that
    /// carries text both ways, holds up to [`CHANNEL_BOUND_MAX`] items unread,
    /// and wakes its holder.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: ChannelKind::Plain,
            receives: Contract::Text,
            sends: Contract::Text,
            bound: CHANNEL_BOUND_MAX,
            wake: Wake::Wake,
            description: String::new(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            initial: Vec::new(),
            reference: None,
        }
    }

    /// What the channel carries: messages, by default, or requests one way.
    /// A request channel's contracts are the holder's still: an inbound one
    /// receives requests and sends their replies, and an outbound one sends
    /// requests and receives their replies.
    #[must_use]
    pub fn kind(mut self, kind: ChannelKind) -> Self {
        self.kind = kind;
        self
    }

    /// What may arrive for the holder.
    #[must_use]
    pub fn receives(mut self, contract: Contract) -> Self {
        self.receives = contract;
        self
    }

    /// What the holder may send.
    #[must_use]
    pub fn sends(mut self, contract: Contract) -> Self {
        self.sends = contract;
        self
    }

    /// How many items the channel holds unread, 1 to [`CHANNEL_BOUND_MAX`]: a
    /// push past it is refused [`DeliverError::Full`]. On an inbound request
    /// channel, how many requests it holds unsettled -- unread, or received and
    /// unanswered; on an outbound one, how many its holder has outstanding, past
    /// which its `request` raises `outrig.AgentLimitReached`.
    #[must_use]
    pub fn bound(mut self, bound: usize) -> Self {
        self.bound = bound;
        self
    }

    /// What an arrival does. On an inbound request channel it decides only
    /// whether an arrival ends a `runtime.wait`: every request is announced,
    /// and opens a round for a holder at rest. Nothing arrives on an outbound
    /// one.
    #[must_use]
    pub fn wake(mut self, wake: Wake) -> Self {
        self.wake = wake;
        self
    }

    /// What the channel is for, which the holder's instructions carry.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// How long each call of this channel's backend may run before OutRig gives
    /// up on it, [`DEFAULT_CALL_TIMEOUT`] by default. The wait for one of the
    /// channel's eight concurrent calls is not counted. Sends, consumed and
    /// settled notices, and withdrawals all use this timeout.
    ///
    /// Choose a bound that allows the backend's durable writes to finish: a
    /// timed-out send is uncertain, not safely retryable. Timing out a notice
    /// retries it; timing out a withdrawal leaves the request uncertain. The
    /// session's close drain may end a call sooner. Other channels keep their
    /// own bounds even when they share this backend; skill-source bounds are
    /// independent.
    #[must_use]
    pub fn call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// An item the channel holds from the start, before any agent holds it:
    /// pushed, in the order given, as the session starts and before any agent
    /// is built, as [`Channel::deliver`] pushes one -- so the agent built to
    /// hold the channel finds it waiting as its first round opens, and
    /// receives it as it does every later item. One the channel refuses fails
    /// the start.
    #[must_use]
    pub fn deliver(mut self, item: Inbound) -> Self {
        self.initial.push(item);
        self
    }

    /// The backend's own reference for the channel, which OutRig never reads
    /// and gives back in the channel's [`ChannelSnapshot`].
    #[must_use]
    pub fn reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    /// Why the definition cannot be declared, if it cannot: its bound, or a
    /// typed contract that names no `module:Name`, or one in a session with no
    /// application code to name it in.
    pub(crate) fn problem(&self, application: bool) -> Option<String> {
        if self.bound == 0 || self.bound > CHANNEL_BOUND_MAX {
            return Some(format!(
                "its bound is {}, and must be 1 to {CHANNEL_BOUND_MAX}",
                self.bound
            ));
        }
        for contract in [&self.receives, &self.sends] {
            if matches!(self.kind, ChannelKind::Request(_)) && *contract == Contract::Nothing {
                return Some(
                    "it carries requests, so it carries something each way: a request and its \
                     reply"
                        .to_string(),
                );
            }
            if let Contract::Typed(path) = contract {
                let (module, name) = path.split_once(':').unwrap_or_default();
                if module.is_empty() || name.is_empty() {
                    return Some(format!("its type {path:?} is not given as module:Name"));
                }
                if !application {
                    return Some(format!(
                        "its type {path} names application code, and the session has none"
                    ));
                }
            }
        }
        None
    }

    /// What the interpreter is told of it.
    fn entry(&self) -> Value {
        json!({
            "name": self.name,
            "kind": self.kind.to_string(),
            "direction": self.kind.direction().map(|direction| direction.to_string()),
            "receives": self.receives.line(),
            "sends": self.sends.line(),
            "bound": self.bound,
            "wake": self.wake.to_string(),
            "description": self.description,
        })
    }
}

/// What kind of channel a definition declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChannelKind {
    /// Messages, each owing nothing, either way.
    #[default]
    Plain,
    /// Requests, each owing one reply, going one way: each ends once, and the
    /// backend's record and what the agent saw agree on how.
    Request(Direction),
}

impl ChannelKind {
    /// Which way a request channel's requests go.
    pub fn direction(self) -> Option<Direction> {
        match self {
            Self::Plain => None,
            Self::Request(direction) => Some(direction),
        }
    }
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain => "plain",
            Self::Request(_) => "request",
        })
    }
}

/// Which way a request channel's requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Direction {
    /// The backend requests and the holder replies: the owner pushes each
    /// request with [`Inbound::request`], the holder's code takes it with
    /// `receive()` and answers it with `reply` or `fail`, and
    /// [`ChannelBackend::settled`] hears how it ended.
    Inbound,
    /// The holder requests and the backend replies: the holder's code asks
    /// with `request(body)`, which hands the request to
    /// [`ChannelBackend::send`] and returns a handle, and the owner settles
    /// it with [`Channel::settle`].
    Outbound,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        })
    }
}

/// What may cross a channel one way.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Contract {
    /// A string.
    Text,
    /// Any JSON value: a body the backend owns the shape of, which the agent's
    /// code reads as the `dict`, `list` or scalar it decodes to.
    Json,
    /// A type in the session's application code, named `module:Name`, which
    /// the agent's code reads as an instance of it: a JSON value the type's
    /// schema describes, checked as it arrives and built as it is received.
    Typed(String),
    /// Nothing: the channel carries nothing this way.
    Nothing,
}

impl Contract {
    fn line(&self) -> Value {
        match self {
            Self::Text => json!("text"),
            Self::Json => json!("json"),
            Self::Nothing => json!("nothing"),
            Self::Typed(path) => json!({"typed": path}),
        }
    }
}

impl fmt::Display for Contract {
    /// As the holder's instructions and the event log say it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text => f.write_str("str"),
            Self::Json => f.write_str("JSON"),
            Self::Typed(path) => f.write_str(path),
            Self::Nothing => f.write_str("nothing"),
        }
    }
}

/// What an arrival on a channel does for its holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wake {
    /// It ends a `runtime.wait` with `MessageAvailable`, is announced, and
    /// opens a round for a holder at rest: what the `user` channel does.
    #[default]
    Wake,
    /// It is announced, and opens a round for a holder at rest, but ends no
    /// wait.
    Announce,
    /// Nothing: the holder's code reads the channel when it chooses.
    Silent,
}

impl fmt::Display for Wake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wake => "wake",
            Self::Announce => "announce",
            Self::Silent => "silent",
        })
    }
}

/// An item the session's owner pushes on a channel, for its holder.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Inbound {
    /// The id the backend's store gives it, by which OutRig tells a repeat.
    pub item: String,
    /// Who sent it, as the backend identifies them: what the holder's code
    /// reads as `delivery.sender`.
    pub sender: String,
    /// What it is.
    pub kind: ItemKind,
    /// The item itself, as the channel's contract has it.
    pub body: Value,
    /// An id it answers or follows, if any: what the holder's code reads as
    /// `delivery.correlation`.
    pub correlation: Option<String>,
    /// The backend's own reference for it, which OutRig never reads and gives
    /// back with the item's [`Consumed`] or [`Settled`] notice.
    pub reference: Option<String>,
    /// A skill of the session's it asks the holder to run, as a `/name` on the
    /// user channel does: what the holder's code reads as `delivery.skill`.
    pub skill: Option<String>,
}

impl Inbound {
    /// A message, `item` by the backend's id, from `sender`.
    pub fn message(item: impl Into<String>, sender: impl Into<String>, body: Value) -> Self {
        Self {
            item: item.into(),
            sender: sender.into(),
            kind: ItemKind::Message,
            body,
            correlation: None,
            reference: None,
            skill: None,
        }
    }

    /// A request, `item` by the backend's id, from `sender`, for an inbound
    /// request channel's holder to answer.
    pub fn request(item: impl Into<String>, sender: impl Into<String>, body: Value) -> Self {
        Self::message(item, sender, body).kind(ItemKind::Request)
    }

    /// The item as `kind`.
    #[must_use]
    pub fn kind(mut self, kind: ItemKind) -> Self {
        self.kind = kind;
        self
    }

    /// The item, answering or following `correlation`.
    #[must_use]
    pub fn correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation = Some(correlation.into());
        self
    }

    /// The item, with the backend's own `reference` for it.
    #[must_use]
    pub fn reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    /// The item, asking its holder to run the session's skill `skill`.
    #[must_use]
    pub fn skill(mut self, skill: impl Into<String>) -> Self {
        self.skill = Some(skill.into());
        self
    }
}

/// What an item on a channel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ItemKind {
    /// A message, which owes no reply.
    #[default]
    Message,
    /// A request, which owes one: only an inbound request channel takes it.
    Request,
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Message => "message",
            Self::Request => "request",
        })
    }
}

/// What [`Channel::deliver`] did with an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivered {
    /// It is queued for the holder, under `delivery`: what its code reads as
    /// `delivery.id`.
    Queued {
        /// The id OutRig delivers it under.
        delivery: DeliveryId,
    },
    /// The channel had taken an item of this id already, under `delivery`, and
    /// it is now at `position`: nothing more was queued.
    Repeat {
        /// The id OutRig delivered it under.
        delivery: DeliveryId,
        /// Where it is.
        position: Position,
    },
}

impl Delivered {
    /// The id the item was delivered under, the first time or this one.
    pub fn delivery(&self) -> DeliveryId {
        match self {
            Self::Queued { delivery } | Self::Repeat { delivery, .. } => *delivery,
        }
    }
}

/// The id OutRig delivered an item under: what the holder's code reads as
/// `delivery.id`, and every event about the item names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeliveryId {
    /// A message's, which its `message.*` events carry.
    Message(MessageId),
    /// A request's: the session's id for it, which its `agent.request.*`
    /// events, its [`Settled`] notice and [`Channel::cancel`] name.
    Request(AgentRequestId),
}

impl fmt::Display for DeliveryId {
    /// The number, as the event log writes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(id) => id.fmt(f),
            Self::Request(id) => id.fmt(f),
        }
    }
}

impl DeliveryId {
    /// A message's id, if it is one.
    pub fn message(self) -> Option<MessageId> {
        match self {
            Self::Message(id) => Some(id),
            Self::Request(_) => None,
        }
    }

    /// A request's id, if it is one.
    pub fn request(self) -> Option<AgentRequestId> {
        match self {
            Self::Request(id) => Some(id),
            Self::Message(_) => None,
        }
    }
}

/// Where an item a channel took is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Position {
    /// In the window, unread.
    Unread,
    /// Received by the holder's code; for a message, the backend has not yet
    /// acknowledged [`ChannelBackend::consumed`].
    Consumed,
    /// A request that has ended, the backend not yet having acknowledged
    /// [`ChannelBackend::settled`].
    Settled,
    /// Received, and acknowledged; for a request, ended, and its end
    /// acknowledged.
    Acknowledged,
}

/// Why [`Channel::deliver`] queued nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeliverError {
    /// The window holds its bound unread: deliver it later, once
    /// [`Channel::room`] resolves.
    #[error("the channel's window is full")]
    Full,
    /// The item is not what the channel's contract takes: each problem, with
    /// where in the body it is.
    #[error("the item is not what the channel takes: {}", .0.join("; "))]
    Contract(Vec<String>),
    /// The session declares no such channel.
    #[error("the session declares no channel {0:?}")]
    UnknownChannel(String),
    /// The channel was closed, or the session is closing: why.
    #[error("the channel is closed: {0}")]
    Closed(String),
    /// The item names a skill the session does not serve.
    #[error("unknown skill: the session serves no skill named {0:?}")]
    UnknownSkill(String),
    /// The interpreter did not say whether it queued it in time; it may have.
    /// Delivering it again is safe: a repeat is answered as one.
    #[error("the interpreter did not say in time whether it queued the item")]
    Unanswered,
}

/// An item the holder sent, as its backend is given it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Outbound {
    /// The channel it was sent on.
    pub channel: String,
    /// The id OutRig gave it, the same on every call about it.
    pub item: MessageId,
    /// What it is.
    pub kind: ItemKind,
    /// For a request, the session's id for it: what [`Channel::settle`] and
    /// [`ChannelBackend::withdrawn`] name it by.
    pub request: Option<AgentRequestId>,
    /// The type the channel's contract gives it: `str`, `JSON`, or a
    /// `module:Name`.
    pub body_type: String,
    /// The agent that sent it, as the interpreter's protocol names it.
    pub sender: String,
    /// An id it answers or follows, if any.
    pub correlation: Option<String>,
    /// The backend's reference for it, if one is already known.
    pub reference: Option<String>,
    /// The item itself.
    pub body: Value,
}

/// What a backend says of an item the holder sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendAnswer {
    /// It took the item durably, under its own `reference` if it gives one:
    /// on its way, not yet processed.
    Accepted {
        /// The backend's reference for it, which OutRig never reads.
        reference: Option<String>,
    },
    /// It did not take it, for `reason`: sending it again is safe.
    NotAccepted {
        /// Why.
        reason: String,
    },
    /// It cannot tell whether it took it, for `reason`.
    Uncertain {
        /// Why.
        reason: String,
    },
}

/// The notice that a holder's code received an item, as its backend is given
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Consumed {
    /// The channel.
    pub channel: String,
    /// The id the session's owner gave the item.
    pub item: String,
    /// The id OutRig delivered it under.
    pub delivery: MessageId,
    /// The backend's own reference for the item, as its push gave it.
    pub reference: Option<String>,
}

impl Consumed {
    /// The item it is of, as the registry lists it.
    fn inbound(&self) -> InboundItem {
        InboundItem {
            item: self.item.clone(),
            delivery: DeliveryId::Message(self.delivery),
            reference: self.reference.clone(),
        }
    }
}

/// The notice that a request on a channel ended, or that an answer to one
/// that had ended was refused, as its backend is given it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Settled {
    /// The channel.
    pub channel: String,
    /// The session's id for the request.
    pub request: AgentRequestId,
    /// The id the session's owner gave an inbound request; `None` for an
    /// outbound one, which the backend knows by `request`.
    pub item: Option<String>,
    /// The backend's own reference for the request, as its push or its send's
    /// acceptance gave it.
    pub reference: Option<String>,
    /// How it ended.
    pub outcome: Settlement,
}

/// How a request on a channel ended, as its backend is told.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Settlement {
    /// Its holder's code replied: the reply, as JSON.
    Replied(Value),
    /// It failed, saying why: its holder's `fail(message)`, its replies
    /// refused past the attempt limit -- each problem named -- a request that
    /// could not be built, or an outbound request's reply that did not fit the
    /// channel's type.
    Failed(String),
    /// It was cancelled.
    Cancelled {
        /// Who cancelled it.
        by: CancelledBy,
        /// Why, as OutRig says it; empty for the owner's own cancel.
        reason: String,
    },
    /// The session closed with it open.
    Closed,
    /// Its holder was released with it open.
    Released,
    /// An answer came after it had ended, and was refused: nothing changed.
    Refused {
        /// How it had ended.
        ended: RequestEnd,
    },
}

/// Who cancelled a request on a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelledBy {
    /// The session's owner, with [`Channel::cancel`]: its requester gave up.
    Owner,
    /// OutRig: its holder's rounds left it unanswered after one that warned
    /// it would be, or the holder could make no further round -- the
    /// session's token budget spent, or its model-request limit reached.
    Host,
    /// The channel's peer: the owner closed the channel.
    Peer,
}

/// An outbound request the holder's side ended, as its backend is asked to
/// withdraw it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Withdrawal {
    /// The channel.
    pub channel: String,
    /// The session's id for the request.
    pub request: AgentRequestId,
    /// The id its send was given, as [`Outbound::item`].
    pub item: MessageId,
    /// The backend's reference for it, as the send's acceptance gave it.
    pub reference: Option<String>,
    /// Why it is withdrawn.
    pub end: WithdrawEnd,
}

/// Why an outbound request is withdrawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WithdrawEnd {
    /// The holder's code cancelled it, and its handle settles on the answer.
    Cancelled,
    /// Its holder was released, or the session closed: the handle has ended
    /// already, and the answer changes it no more.
    Released,
}

/// What a backend says of an outbound request it was asked to withdraw.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WithdrawAnswer {
    /// It is withdrawn.
    Withdrawn,
    /// It had been answered already, with this reply, as JSON.
    Replied(Value),
    /// It had failed already, saying why.
    Failed(String),
}

/// An operation on an item whose outcome OutRig could not tell, and so the
/// query that resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ChannelOperation {
    /// The item's [`ChannelBackend::send`]: did the backend take it? Resolved
    /// with [`ChannelBackend::resolve_send`].
    Send,
    /// An outbound request's [`ChannelBackend::withdrawn`]: was it withdrawn,
    /// or had it been answered? Resolved with
    /// [`ChannelBackend::resolve_withdrawal`].
    Withdrawal,
}

impl fmt::Display for ChannelOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Send => "send",
            Self::Withdrawal => "withdrawal",
        })
    }
}

/// An item, or an outbound request, whose outcome OutRig could not tell, as
/// its backend is asked to resolve it with [`ChannelBackend::resolve_send`]
/// or [`ChannelBackend::resolve_withdrawal`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Query {
    /// The channel.
    pub channel: String,
    /// The id OutRig gave the item, as its [`Outbound::item`].
    pub item: MessageId,
    /// For a request, the session's id for it, as its [`Outbound::request`].
    pub request: Option<AgentRequestId>,
    /// The backend's reference for it, if one is known.
    pub reference: Option<String>,
}

/// What a backend said became of an operation OutRig asked it to resolve: a
/// send's answer, a withdrawal's, or neither.
#[derive(Debug, Clone, PartialEq)]
enum Resolution {
    /// A send: the backend took the item, under its own `reference` if it
    /// gives one.
    Accepted { reference: Option<String> },
    /// A send: the backend did not take the item, so sending it again is safe.
    NotAccepted { reason: String },
    /// A withdrawal: the request is withdrawn.
    Withdrawn,
    /// A withdrawal: the request had been answered, with this reply, as JSON.
    Replied(Value),
    /// A withdrawal: the request had failed, saying why.
    Failed(String),
    /// The backend cannot tell either, for `reason`.
    Uncertain { reason: String },
}

impl From<SendAnswer> for Resolution {
    fn from(answer: SendAnswer) -> Self {
        match answer {
            SendAnswer::Accepted { reference } => Self::Accepted { reference },
            SendAnswer::NotAccepted { reason } => Self::NotAccepted { reason },
            SendAnswer::Uncertain { reason } => Self::Uncertain { reason },
        }
    }
}

impl From<WithdrawAnswer> for Resolution {
    fn from(answer: WithdrawAnswer) -> Self {
        match answer {
            WithdrawAnswer::Withdrawn => Self::Withdrawn,
            WithdrawAnswer::Replied(value) => Self::Replied(value),
            WithdrawAnswer::Failed(message) => Self::Failed(message),
        }
    }
}

impl Resolution {
    /// What it says, as its event and the holder's code are told it: the
    /// answer, why, and the backend's reference.
    fn parts(&self) -> (Resolved, Option<&str>, Option<&str>) {
        match self {
            Self::Accepted { reference } => (Resolved::Accepted, None, reference.as_deref()),
            Self::NotAccepted { reason } => (Resolved::NotAccepted, Some(reason), None),
            Self::Withdrawn => (Resolved::Withdrawn, None, None),
            Self::Replied(_) => (Resolved::Replied, None, None),
            Self::Failed(message) => (Resolved::Failed, Some(message), None),
            Self::Uncertain { reason } => (Resolved::Uncertain, Some(reason), None),
        }
    }

    /// It as the answer to a withdrawal it is, if it is one.
    fn withdraw_answer(&self) -> Option<WithdrawAnswer> {
        match self {
            Self::Withdrawn => Some(WithdrawAnswer::Withdrawn),
            Self::Replied(value) => Some(WithdrawAnswer::Replied(value.clone())),
            Self::Failed(message) => Some(WithdrawAnswer::Failed(message.clone())),
            _ => None,
        }
    }
}

/// How the session's owner settles an outbound request, with
/// [`Channel::settle`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Settle {
    /// It was answered: the reply, as JSON, which the holder's code receives
    /// built into the channel's reply type.
    Reply(Value),
    /// It failed, saying why: the handle raises `outrig.AgentRequestFailed`.
    Fail(String),
    /// It was cancelled, saying why -- its peer finished, say: the handle
    /// raises `outrig.RequestCancelled`.
    Cancel(String),
}

/// Why [`Channel::settle`] or [`Channel::cancel`] did not end a request as
/// asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SettleError {
    /// The channel has no such request open: none of that id, or not one
    /// the call ends -- an inbound request for [`Channel::cancel`], an
    /// outbound one for [`Channel::settle`].
    #[error("the channel has no request {0} open that this ends")]
    Unknown(AgentRequestId),
    /// It had already ended, as this says: nothing changed.
    #[error("the request had already ended ({0:?}), and nothing changed")]
    Ended(RequestEnd),
    /// The reply does not fit the channel's reply type, so the request ended
    /// failed, naming each problem -- which its backend is told too.
    #[error("the reply does not fit the channel's reply type, so the request failed: {}", .0.join("; "))]
    Rejected(Vec<String>),
    /// The session ended before the request did.
    #[error("the session ended before the request did")]
    Gone,
}

/// The owner's end of one external channel: what it pushes the holder, and
/// what it says of the far end. From [`Session::channel`](super::Session::channel);
/// clones share the channel.
#[derive(Clone)]
pub struct Channel {
    table: Arc<Table>,
    name: String,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel").field("name", &self.name).finish()
    }
}

impl Channel {
    /// Push `item` for the channel's holder.
    ///
    /// It is on its way when this returns, ahead of anything pushed after it,
    /// whether or not the future is awaited; call it inside the session's
    /// tokio runtime. The future answers once the item is queued in the
    /// window -- with the id it is delivered under -- or as a repeat of an item
    /// the channel took already, or why nothing was queued. It never waits on
    /// the agent: queued is not received, and the backend hears of the item
    /// only once the holder's code receives it. A channel no agent holds yet
    /// queues it all the same, for the agent that takes the channel.
    pub fn deliver(
        &self,
        item: Inbound,
    ) -> impl Future<Output = Result<Delivered, DeliverError>> + Send + 'static + use<> {
        let table = Arc::clone(&self.table);
        let name = self.name.clone();
        let first = table.push(&name, &item);
        async move {
            let mut attempt = first;
            loop {
                match attempt {
                    Push::Answered(answer) => return answer,
                    Push::Started(answered) => {
                        return match tokio::time::timeout(POST_TIMEOUT, answered).await {
                            Ok(Ok(answer)) => answer,
                            Ok(Err(_)) => Err(DeliverError::Closed(
                                "the session ended before the item was queued".to_string(),
                            )),
                            Err(_) => Err(DeliverError::Unanswered),
                        };
                    }
                    Push::Busy => {
                        table.pushed(&name, &item.item).await;
                        attempt = table.push(&name, &item);
                    }
                }
            }
        }
    }

    /// Close the channel, saying `reason`: its far end's peer is gone. From now
    /// on its holder's `send` and `receive` raise -- what was already waiting
    /// is still received first -- a `runtime.wait` says so once, and a push is
    /// refused. A second close changes nothing.
    pub fn close(&self, reason: &str) {
        self.table.close(&self.name, reason);
    }

    /// Resolves once the window has room for another item -- on an inbound
    /// request channel, once fewer requests are unsettled than its bound --
    /// the channel is closed, or the session is closing: once a push can be
    /// answered other than [`DeliverError::Full`].
    pub fn room(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut state = self.table.state.subscribe();
        let closing = self.table.closing.clone();
        let name = self.name.clone();
        async move {
            let roomy = state.wait_for(|state| {
                state
                    .find(&name)
                    .is_none_or(|chan| chan.closed.is_some() || chan.held() < chan.bound())
            });
            tokio::select! {
                _ = roomy => {}
                () = closing.cancelled() => {}
            }
        }
    }

    /// Cancel inbound request `request`: its requester gave up. One its holder
    /// has not received is taken out of the window, and the holder never sees
    /// it; one it has received stays with it, and its answer then raises
    /// `outrig.RequestCancelled`. Cancelling never interrupts the holder's
    /// code. The end reaches [`ChannelBackend::settled`] as
    /// [`CancelledBy::Owner`], as every end of the request does; this says
    /// only whether it was asked.
    pub fn cancel(&self, request: AgentRequestId) -> Result<(), SettleError> {
        self.table.cancel(&self.name, request)
    }

    /// Settle outbound request `request`, its holder's, as `settle` says: its
    /// backend's outcome for it. A reply is built into the channel's reply
    /// type in the holder's kernel, and one that does not fit ends the request
    /// failed instead, naming each problem, which [`ChannelBackend::settled`]
    /// is told too: [`SettleError::Rejected`]. A request that has ended
    /// already -- answered, cancelled, withdrawn, its holder released -- is
    /// left as it ended: [`SettleError::Ended`], and `channel.request.refused`
    /// is recorded.
    ///
    /// Call it inside the session's tokio runtime; the future answers once the
    /// request has ended.
    pub fn settle(
        &self,
        request: AgentRequestId,
        settle: Settle,
    ) -> impl Future<Output = Result<(), SettleError>> + Send + 'static + use<> {
        let table = Arc::clone(&self.table);
        let name = self.name.clone();
        let started = table.settle(&name, request, settle);
        async move {
            let number = started?;
            table.settled_by(&name, request, number).await
        }
    }

    /// Hand the channel's backend its positions, as the registry holds them
    /// now: what a cut gives it, so the backend knows what to deliver again by
    /// item id after a resume. `Err` says why the backend did not take them.
    #[cfg_attr(not(test), allow(dead_code, reason = "a cut hands them over, 0003-65"))]
    pub(crate) async fn hand_over(&self) -> Result<(), String> {
        let (positions, calls) = {
            let state = self.table.state.borrow();
            let chan = state
                .find(&self.name)
                .ok_or_else(|| format!("the session declares no channel {:?}", self.name))?;
            (chan.positions(), chan.calls())
        };
        self.table
            .call(&calls, || calls.backend.positions(positions))
            .await
    }

    /// The agent holding the channel, as the interpreter's protocol names it --
    /// a root's [`RootHandle::agent`](super::RootHandle::agent) -- or `None`
    /// while no agent does.
    pub fn holder(&self) -> Option<String> {
        self.table
            .state
            .borrow()
            .find(&self.name)
            .and_then(|chan| chan.holder.clone())
    }
}

/// Refuse a channel the session cannot declare: a name a binding could not take
/// -- not a Python identifier, a keyword, `runtime`, `asyncio`, `outrig` -- or
/// `user`, the primary's own; one a binding `declared`, a service in `served`,
/// or another channel takes; or a definition [`ChannelDefinition::problem`]
/// refuses, `application` saying whether the session has application code.
pub(crate) fn check(
    declared: &[super::bindings::Declared],
    served: &[(String, Arc<dyn super::Service>)],
    channels: &[(ChannelDefinition, Arc<dyn ChannelBackend>)],
    application: bool,
) -> crate::error::Result<()> {
    for (index, (definition, _)) in channels.iter().enumerate() {
        let name = &definition.name;
        let earlier = channels[..index]
            .iter()
            .map(|(earlier, _)| earlier.name.as_str());
        let why = super::services::name_taken(name, "channel", declared, earlier)
            .or_else(|| {
                (name == "user")
                    .then(|| "takes the name of the primary agent's user channel".to_string())
            })
            .or_else(|| {
                served
                    .iter()
                    .any(|(service, _)| service == name)
                    .then(|| "is a service's name already".to_string())
            })
            .or_else(|| {
                definition
                    .problem(application)
                    .map(|problem| format!("cannot be declared: {problem}"))
            });
        if let Some(why) = why {
            return Err(crate::error::OutrigError::Configuration(format!(
                "the channel `{name}` given through `channel` {why}"
            )));
        }
    }
    Ok(())
}

/// What a session's children registry lends the channels: how to wake an
/// agent's runner, and the round it has open.
pub(crate) struct Rounds {
    pub(crate) wake: Box<dyn Fn(&str) + Send + Sync>,
    pub(crate) round: RoundOf,
}

/// Which round the agent named has open, if any.
pub(crate) type RoundOf = Box<dyn Fn(&str) -> Option<u32> + Send + Sync>;

/// A session's external channels. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Channels(Arc<Table>);

struct Table {
    events: Events,
    lifecycle: Arc<Lifecycle>,
    catalog: Option<Arc<Catalog>>,
    /// A handle that does not keep the interpreter's input open, and its
    /// writer, held weakly.
    interpreter: Interpreter,
    to_interpreter: ToInterpreter,
    state: watch::Sender<State>,
    rounds: OnceLock<Rounds>,
    /// Cancelled once admission closes: a send still waiting for its turn at
    /// the backend is refused.
    closing: CancellationToken,
    /// Cancelled once the session's drain has ended: what the backend has not
    /// answered is given up on.
    stopping: CancellationToken,
    /// The last of the owner's settles of an outbound request, numbered so
    /// each knows whether it ended the request.
    settles: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct State {
    /// Each channel, in the order the owner declared them.
    channels: Vec<Chan>,
    /// Whether admission has closed, as the table heard it: set in the one
    /// step that lists each message whose send is uncertain in the close's
    /// report, so what is decided under this lock has one close to go by.
    closed: bool,
}

impl State {
    fn find(&self, name: &str) -> Option<&Chan> {
        self.channels
            .iter()
            .find(|chan| chan.definition.name == name)
    }

    fn find_mut(&mut self, name: &str) -> Option<&mut Chan> {
        self.channels
            .iter_mut()
            .find(|chan| chan.definition.name == name)
    }
}

struct Chan {
    definition: ChannelDefinition,
    /// The JSON Schema text of each typed direction, receiving and sending, as
    /// the interpreter rendered it.
    schemas: (Option<String>, Option<String>),
    /// The sending direction's schema, compiled: what the host checks every
    /// send against before its backend is given it, whatever the holder's
    /// code did to its own endpoint.
    sending: Option<Arc<jsonschema::Validator>>,
    backend: Arc<dyn ChannelBackend>,
    permits: Arc<Semaphore>,
    holder: Option<String>,
    closed: Option<String>,
    /// Each item the owner pushed that the channel took, or is taking, by the
    /// owner's id for it.
    items: HashMap<String, Taken>,
    /// Each inbound request the channel took, by the request's id, until its
    /// backend has acknowledged how it ended: where it is, `items` says.
    asked: BTreeMap<u64, Asked>,
    /// Each outbound request outstanding, by its id.
    outgoing: BTreeMap<u64, Outgoing>,
    /// How each request on the channel ended, by its id: what a later settle
    /// or cancel of it is answered with.
    finished: HashMap<u64, Finished>,
    /// Each request that ended after admission closed, and how, in order.
    requests_after_close: Vec<ChannelRequestAtClose>,
    /// How many withdrawals its backend has not answered.
    withdrawing: usize,
    /// The owner's id of each item not yet acknowledged, and the backend's
    /// reference for it, by the id it was delivered under, in that order.
    deliveries: BTreeMap<MessageId, (String, Option<String>)>,
    /// The last message its holder's code received, and the last whose
    /// consumption its backend acknowledged: its two watermarks.
    consumed: Option<InboundItem>,
    acknowledged: Option<InboundItem>,
    /// Every item ever queued, and those unread.
    delivered: u64,
    unread: usize,
    /// Each send its backend has not answered, by its item id.
    flights: BTreeMap<MessageId, Flight>,
    /// Each message whose send is uncertain, by its item id, until it is
    /// resolved.
    uncertain: BTreeMap<MessageId, Doubt>,
    /// How many of its backend's resolutions have not answered.
    resolving: usize,
    /// The items each agent sent in each of its latest rounds.
    sent: BTreeMap<(String, u32), Vec<MessageId>>,
    /// Each send whose outcome came after admission closed.
    after_close: Vec<SendAtClose>,
    /// Each message uncertain when admission closed: listed as uncertain once
    /// an answer comes after the close, or the drain asks of it, and resolved
    /// by whichever answer since said more.
    uncertain_at_close: Vec<UncertainAtClose>,
    /// What its backend has not acknowledged being told, in the order it
    /// happened, and whether a task is telling it.
    notices: VecDeque<Notice>,
    notifying: bool,
}

impl Chan {
    fn bound(&self) -> usize {
        self.definition.bound
    }

    /// What counts against its bound: unread messages, or the inbound
    /// requests not yet ended.
    fn held(&self) -> usize {
        match self.definition.kind {
            ChannelKind::Request(Direction::Inbound) => {
                self.asked.keys().filter(|id| self.open(**id)).count()
            }
            _ => self.unread,
        }
    }

    /// Whether any request on it is still to end.
    fn requests_open(&self) -> bool {
        !self.outgoing.is_empty() || self.asked.keys().any(|id| self.open(*id))
    }

    /// Whether inbound request `id` has yet to end: unread, or received.
    fn open(&self, id: u64) -> bool {
        matches!(
            self.position_of(id),
            Some(Position::Unread | Position::Consumed)
        )
    }

    /// Where inbound request `id` is, while the channel keeps it.
    fn position_of(&self, id: u64) -> Option<Position> {
        let asked = self.asked.get(&id)?;
        match self.items.get(&asked.item)? {
            Taken::Queued(_, position) => Some(*position),
            Taken::Pushing => None,
        }
    }

    /// Inbound request `id`, the owner's `item` with the backend's
    /// `reference`, is now at `position`.
    fn place(&mut self, id: u64, item: &str, reference: Option<String>, position: Position) {
        let asked = self.asked.entry(id).or_insert_with(|| Asked {
            item: item.to_string(),
            reference,
            received: None,
        });
        if position == Position::Consumed {
            asked.received.get_or_insert_with(Instant::now);
        }
        let delivery = DeliveryId::Request(AgentRequestId::new(id));
        self.items
            .insert(item.to_string(), Taken::Queued(delivery, position));
    }

    /// Queue `notice` for the backend: whether a task must start to tell it.
    fn push_notice(&mut self, notice: Notice) -> bool {
        self.notices.push_back(notice);
        !std::mem::replace(&mut self.notifying, true)
    }

    /// Request `id` ended as `finished`: whether this is the first word of
    /// its end, recorded once; `at_close` lists it at the close.
    fn end_request(
        &mut self,
        id: u64,
        finished: Finished,
        at_close: Option<ChannelRequestAtClose>,
    ) -> bool {
        match self.finished.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(finished);
                self.requests_after_close.extend(at_close);
                true
            }
        }
    }

    /// Request `id`'s entry in what the close left, if it ended after the
    /// close.
    fn at_close(&mut self, id: u64) -> Option<&mut ChannelRequestAtClose> {
        self.requests_after_close
            .iter_mut()
            .find(|at| at.request.get() == id)
    }

    /// How its backend is called.
    fn calls(&self) -> Calls {
        Calls {
            backend: Arc::clone(&self.backend),
            permits: Arc::clone(&self.permits),
            bound: self.definition.call_timeout,
        }
    }

    /// Each message not yet acknowledged that is at `position`, in order: one
    /// being pushed counts as unread, in the window once the interpreter says
    /// so, perhaps already.
    fn messages_at(&self, position: Position) -> impl Iterator<Item = InboundItem> + '_ {
        self.deliveries
            .iter()
            .filter(move |(_, (item, _))| {
                let at = match self.items.get(item) {
                    Some(Taken::Queued(_, at)) => Some(*at),
                    Some(Taken::Pushing) => Some(Position::Unread),
                    None => None,
                };
                at == Some(position)
            })
            .map(|(delivery, (item, reference))| InboundItem {
                item: item.clone(),
                delivery: DeliveryId::Message(*delivery),
                reference: reference.clone(),
            })
    }

    /// Each item waiting unread in its window, in order. A channel carries
    /// messages or requests, never both: each kind in the order of its ids.
    fn unread(&self) -> Vec<InboundItem> {
        self.messages_at(Position::Unread)
            .chain(
                self.asked
                    .iter()
                    .filter(|(id, _)| self.position_of(**id) == Some(Position::Unread))
                    .map(|(id, asked)| asked.inbound(*id)),
            )
            .collect()
    }

    /// Its positions, as its backend is handed them at a cut.
    fn positions(&self) -> Positions {
        Positions {
            channel: self.definition.name.clone(),
            consumed: self.consumed.clone(),
            acknowledged: self.acknowledged.clone(),
            unread: self.unread(),
        }
    }

    /// What its registry holds now, as the owner reads it.
    fn snapshot(&self) -> ChannelSnapshot {
        let definition = &self.definition;
        ChannelSnapshot {
            name: definition.name.clone(),
            kind: definition.kind,
            receives: definition.receives.clone(),
            receives_schema: self.schemas.0.clone(),
            sends: definition.sends.clone(),
            sends_schema: self.schemas.1.clone(),
            bound: definition.bound,
            wake: definition.wake,
            holder: self.holder.clone(),
            reference: definition.reference.clone(),
            closed: self.closed.clone(),
            unread: self.unread(),
            unacknowledged: self.messages_at(Position::Consumed).collect(),
            consumed: self.consumed.clone(),
            acknowledged: self.acknowledged.clone(),
            received: self
                .asked
                .iter()
                .filter(|(id, _)| self.position_of(**id) == Some(Position::Consumed))
                .map(|(id, asked)| ReceivedRequest {
                    request: AgentRequestId::new(*id),
                    item: asked.item.clone(),
                    reference: asked.reference.clone(),
                    waited: asked.received.map(|at| at.elapsed()).unwrap_or_default(),
                })
                .collect(),
            outstanding: self
                .outgoing
                .iter()
                .map(|(id, outgoing)| OutstandingRequest {
                    request: AgentRequestId::new(*id),
                    item: outgoing.item,
                    agent: outgoing.agent.clone(),
                    state: outgoing.state.public(),
                    reference: outgoing.reference.clone(),
                })
                .collect(),
            // Listed in the close's report or not, each still uncertain.
            uncertain: self
                .uncertain
                .iter()
                .map(|(item, doubt)| UncertainItem {
                    item: *item,
                    agent: doubt.agent.clone(),
                    reason: doubt.reason.clone(),
                })
                .chain(
                    self.uncertain_at_close
                        .iter()
                        .filter(|row| row.outcome == ItemOutcome::Uncertain)
                        .map(|row| UncertainItem {
                            item: row.item,
                            agent: row.agent.clone(),
                            reason: row.reason.clone().unwrap_or_default(),
                        }),
                )
                .collect(),
            rounds: self
                .sent
                .iter()
                .map(|((agent, round), items)| SentInRound {
                    agent: agent.clone(),
                    round: *round,
                    items: items.clone(),
                })
                .collect(),
        }
    }

    /// What `agent`'s code asks its backend to resolve, as `line` names it --
    /// an uncertain message's send, by its item id; an outbound request's send
    /// or uncertain withdrawal, by its id -- or why nothing is asked.
    fn query(
        &self,
        agent: &str,
        operation: ChannelOperation,
        line: &Value,
    ) -> Result<Query, String> {
        let name = &self.definition.name;
        if self.holder.as_deref() != Some(agent) {
            return Err(format!("channel {name:?} is not held by agent {agent:?}"));
        }
        let query = |item, request, reference| Query {
            channel: name.clone(),
            item,
            request,
            reference,
        };
        match self.definition.kind {
            ChannelKind::Plain => {
                let item = line["item"].as_u64().map(MessageId::new);
                let doubt = item.and_then(|item| self.uncertain.get(&item));
                match (operation, item, doubt) {
                    (ChannelOperation::Withdrawal, ..) => Err(format!(
                        "channel {name:?} carries messages, which have no withdrawal to resolve"
                    )),
                    (_, Some(item), Some(doubt)) if doubt.agent == agent => {
                        Ok(query(item, None, None))
                    }
                    (_, item, _) => Err(format!(
                        "no message {} sent on channel {name:?} is uncertain: it was taken, not \
                         taken, resolved already, or never sent there",
                        item.map_or_else(|| line["item"].to_string(), |item| item.to_string())
                    )),
                }
            }
            ChannelKind::Request(Direction::Outbound) => {
                let id = line["request"].as_u64().unwrap_or_default();
                let Some(outgoing) = self
                    .outgoing
                    .get(&id)
                    .filter(|outgoing| outgoing.agent == agent)
                else {
                    return Err(format!(
                        "no request {id} on channel {name:?} is outstanding: it has ended, or \
                         was never made there"
                    ));
                };
                match (operation, outgoing.state) {
                    (ChannelOperation::Send, Out::Sending { .. }) => Err(format!(
                        "request {id}'s send on channel {name:?} has not been answered yet"
                    )),
                    (ChannelOperation::Withdrawal, state) if state != Out::Doubtful => Err(
                        format!("no withdrawal of request {id} on channel {name:?} is uncertain"),
                    ),
                    _ => Ok(query(
                        outgoing.item,
                        Some(AgentRequestId::new(id)),
                        outgoing.reference.clone(),
                    )),
                }
            }
            ChannelKind::Request(Direction::Inbound) => Err(format!(
                "channel {name:?} carries requests to its holder, and nothing is sent on it"
            )),
        }
    }

    /// Its backend answered `query` with `resolution`: what that settles --
    /// a message no longer uncertain, or its row in the close's report; an
    /// outbound request now waiting, or ended as the backend says, which its
    /// holder's kernel is told, as the line returned. An answer that changes
    /// nothing -- the request moved on meanwhile, or still uncertain --
    /// settles nothing.
    fn settle_by(
        &mut self,
        query: &Query,
        resolution: &Resolution,
    ) -> Option<(&'static str, Value)> {
        let Some(request) = query.request else {
            self.settle_doubt(query.item, resolution);
            return None;
        };
        let outgoing = self.outgoing.get_mut(&request.get())?;
        match (outgoing.state, resolution) {
            (Out::Uncertain, Resolution::Accepted { reference }) => {
                outgoing.state = Out::Waiting;
                outgoing.reference.clone_from(reference);
                Some(("channel.requested", json!({"state": "waiting"})))
            }
            (Out::Uncertain, Resolution::NotAccepted { reason }) => {
                outgoing.state = Out::Ending;
                Some((
                    "channel.settle",
                    json!({"outcome": "not_accepted", "message": reason}),
                ))
            }
            (Out::Doubtful, resolution) => {
                let answer = resolution.withdraw_answer()?;
                outgoing.state = Out::Ending;
                Some(("channel.settle", withdrawn_line(answer)))
            }
            _ => None,
        }
    }

    /// Its backend answered `resolution` of message `item`, whose send was
    /// uncertain: a doubt no more, unless still uncertain. Once admission has
    /// closed -- which listed it, in the step the table heard it -- its row in
    /// the close's report says what the answer says, whoever asked and in
    /// whatever order the answers came, unless an earlier answer already said
    /// whether its backend took it.
    fn settle_doubt(&mut self, item: MessageId, resolution: &Resolution) {
        if let Some(doubt) = self.uncertain.get_mut(&item) {
            match resolution {
                Resolution::Uncertain { reason } => doubt.reason.clone_from(reason),
                _ => {
                    self.uncertain.remove(&item);
                }
            }
            return;
        }
        let Some(row) = self
            .uncertain_at_close
            .iter_mut()
            .find(|row| row.item == item && row.outcome == ItemOutcome::Uncertain)
        else {
            return;
        };
        let ended = Ended::resolved(resolution.clone());
        (row.outcome, row.reason, row.reference) = (ended.outcome, ended.reason, ended.reference);
    }

    /// List each message whose send is uncertain in the close's report, as
    /// uncertain: a doubt no more, but a row an answer settles.
    fn list_doubts(&mut self) {
        for (item, doubt) in std::mem::take(&mut self.uncertain) {
            self.uncertain_at_close.push(doubt.at_close(item));
        }
    }

    /// Count `withdrawal` toward the drain, as its backend is asked it.
    fn ask_withdrawal(&mut self, withdrawal: Withdrawal) -> Asking {
        self.withdrawing += 1;
        Asking {
            withdrawal,
            calls: self.calls(),
        }
    }

    /// Its holder's code cancelled outbound request `id`, which its backend
    /// is asked to withdraw a single time, after its send has been answered:
    /// the withdrawal to ask now, if any, and the agent whose request it is.
    fn cancel_outgoing(&mut self, id: u64) -> Option<(Asking, String)> {
        let outgoing = self.outgoing.get_mut(&id)?;
        match outgoing.state {
            Out::Sending { .. } => {
                outgoing.state = Out::Sending { withdraw: true };
                None
            }
            Out::Withdrawing | Out::Doubtful | Out::Ending => None,
            Out::Waiting | Out::Uncertain => {
                outgoing.state = Out::Withdrawing;
                let withdrawal =
                    outgoing.withdrawal(&self.definition.name, id, WithdrawEnd::Cancelled);
                let agent = outgoing.agent.clone();
                Some((self.ask_withdrawal(withdrawal), agent))
            }
        }
    }

    /// Give its backend what outbound request `id`, `agent`'s, sent as `item`
    /// and taken under `reference`, is `owed` for how it ended.
    fn owe(
        &mut self,
        id: u64,
        agent: &str,
        (item, reference): (MessageId, Option<String>),
        owed: Owed,
    ) -> Giving {
        let (channel, request) = (self.definition.name.clone(), AgentRequestId::new(id));
        match owed {
            Owed::Withdrawn => Giving::Withdrawal(self.ask_withdrawal(Withdrawal {
                channel,
                request,
                item,
                reference,
                end: WithdrawEnd::Released,
            })),
            Owed::Told(outcome) => {
                let notice = Settled {
                    channel,
                    request,
                    item: None,
                    reference,
                    outcome,
                };
                let agent = Some(agent.to_string());
                Giving::Notice(self.push_notice(Notice::Settled(notice, agent)))
            }
        }
    }

    /// The send of outbound request `id` ended `wire`, as `sent` says -- its
    /// item, and the backend's reference if it took it: the request now
    /// waits for its backend's settlement, is uncertain, or ended unsent, and
    /// its holder's cancel meanwhile is asked of the backend now. One that
    /// ended as its send went is given now what it was owed, if its backend
    /// took it.
    fn request_sent(
        &mut self,
        id: u64,
        wire: &str,
        sent: (MessageId, Option<String>),
    ) -> (Sent, Option<Giving>) {
        let taken = matches!(wire, "accepted" | "uncertain");
        if let Some(outgoing) = self.outgoing.get_mut(&id) {
            if !taken {
                return (Sent::Told, None);
            }
            let withdraw = matches!(outgoing.state, Out::Sending { withdraw: true });
            outgoing.reference = sent.1;
            let (state, named) = match wire {
                "accepted" => (Out::Waiting, "waiting"),
                _ => (Out::Uncertain, "uncertain"),
            };
            outgoing.state = state;
            let cancelled = if withdraw {
                self.cancel_outgoing(id)
            } else {
                None
            };
            let giving = cancelled.map(|(asking, _)| Giving::Withdrawal(asking));
            return (Sent::Moved(named), giving);
        }
        // Ended before its send did: owed nothing by a backend that never took it.
        let owed = self
            .finished
            .get_mut(&id)
            .and_then(|finished| Some((finished.owed.take()?, finished.agent.clone()?)));
        match owed.filter(|_| taken) {
            Some((owed, agent)) => (Sent::Ended, Some(self.owe(id, &agent, sent, owed))),
            None => {
                if let Some(at) = self.at_close(id) {
                    at.acknowledged = true;
                }
                (Sent::Ended, None)
            }
        }
    }
}

/// An outbound request its backend has not settled.
struct Outgoing {
    /// The agent that made it.
    agent: String,
    /// The id its send was given, and the backend's reference once it took it.
    item: MessageId,
    reference: Option<String>,
    state: Out,
}

impl Outgoing {
    /// It, request `id` on `channel`, withdrawn as `end` says.
    fn withdrawal(&self, channel: &str, id: u64, end: WithdrawEnd) -> Withdrawal {
        Withdrawal {
            channel: channel.to_string(),
            request: AgentRequestId::new(id),
            item: self.item,
            reference: self.reference.clone(),
            end,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    /// Its send is with the backend, unanswered; and whether its holder asked
    /// to withdraw it meanwhile.
    Sending { withdraw: bool },
    /// The backend took it.
    Waiting,
    /// Whether the backend took it is not known.
    Uncertain,
    /// Its backend is asked to withdraw it, and has not answered.
    Withdrawing,
    /// Its backend was asked to withdraw it, and did not answer within the
    /// call's bound: what became of the withdrawal is not known until a
    /// resolution says. It is asked to withdraw it no more.
    Doubtful,
    /// Its backend said how it ended -- answering its withdrawal, or a
    /// resolution -- and its holder is being told.
    Ending,
}

impl Out {
    /// As the registry's snapshot says it.
    fn public(self) -> OutboundState {
        match self {
            Self::Sending { .. } => OutboundState::Sending,
            Self::Waiting => OutboundState::Waiting,
            Self::Uncertain => OutboundState::Uncertain(ChannelOperation::Send),
            Self::Withdrawing => OutboundState::Withdrawing,
            Self::Doubtful => OutboundState::Uncertain(ChannelOperation::Withdrawal),
            Self::Ending => OutboundState::Ending,
        }
    }
}

/// An inbound request a channel took.
struct Asked {
    /// The id the session's owner gave it, and the backend's reference.
    item: String,
    reference: Option<String>,
    /// When its holder's code received it.
    received: Option<Instant>,
}

impl Asked {
    /// It, request `id`, as the registry lists it.
    fn inbound(&self, id: u64) -> InboundItem {
        InboundItem {
            item: self.item.clone(),
            delivery: DeliveryId::Request(AgentRequestId::new(id)),
            reference: self.reference.clone(),
        }
    }
}

/// A message whose send is uncertain.
struct Doubt {
    /// The agent that sent it.
    agent: String,
    /// Why, as its backend or OutRig last said.
    reason: String,
}

impl Doubt {
    /// Message `item`'s row in the close's report, as uncertain as this.
    fn at_close(self, item: MessageId) -> UncertainAtClose {
        UncertainAtClose {
            item,
            agent: self.agent,
            outcome: ItemOutcome::Uncertain,
            reason: Some(self.reason),
            reference: None,
        }
    }
}

/// How a request on a channel ended.
#[derive(Debug, Clone)]
struct Finished {
    end: RequestEnd,
    /// The agent whose request it was, if any held it.
    agent: Option<String>,
    /// The owner's settle that ended it, by number.
    settle: Option<u64>,
    /// What was wrong with a reply that ended it failed.
    problems: Vec<String>,
    /// What its backend is owed once its send is answered, for an outbound
    /// request that ended first, if the backend takes it.
    owed: Option<Owed>,
}

/// What an outbound request's backend is owed for how it ended.
#[derive(Debug, Clone)]
enum Owed {
    /// Told how it ended, an end it did not give itself: a reply that did not
    /// fit, the channel's close.
    Told(Settlement),
    /// Asked to withdraw it: its holder was released, or the session closed.
    Withdrawn,
}

/// What a channel's backend is given for a request, counted toward the drain
/// in the step that decided it, so the drain never finds the request ended
/// and what its backend is owed not yet begun.
enum Giving {
    /// Asked to withdraw it.
    Withdrawal(Asking),
    /// Told how it ended: whether a task must start to tell it.
    Notice(bool),
}

/// A withdrawal to ask of a channel's backend.
struct Asking {
    withdrawal: Withdrawal,
    calls: Calls,
}

/// How a channel's backend is called: no more calls at once than it has
/// permits, each given up on past its bound.
#[derive(Clone)]
struct Calls {
    backend: Arc<dyn ChannelBackend>,
    permits: Arc<Semaphore>,
    bound: Duration,
}

/// What a channel's backend is told, in order, until it acknowledges it, with
/// the agent it is of.
#[derive(Clone)]
enum Notice {
    /// The holder's code, the agent named, received an item.
    Consumed(Consumed, String),
    /// A request ended, or an answer to it was refused: the agent named is the
    /// holder whose request it was, if it was any agent's.
    Settled(Settled, Option<String>),
}

/// A send its backend has not answered.
struct Flight {
    /// The agent that sent it.
    agent: String,
    /// Cancelled once that agent gives the channel up, since nothing would
    /// hear its answer, or once the channel closes, since its far end is gone:
    /// a send still waiting for its turn at the backend is then refused. One
    /// handed to the backend already finishes.
    unsent: CancellationToken,
}

/// An item the channel took, or is taking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Taken {
    /// Pushed, the interpreter not yet having said.
    Pushing,
    Queued(DeliveryId, Position),
}

/// How a push began.
enum Push {
    /// Answered at once.
    Answered(Result<Delivered, DeliverError>),
    /// Sent: the answer arrives here.
    Started(oneshot::Receiver<Result<Delivered, DeliverError>>),
    /// An item of its id is being pushed already: wait for that, then try
    /// again.
    Busy,
}

impl Channels {
    /// The channels `declared`, each with its backend, for `interpreter`'s
    /// session: nothing is told the interpreter until [`Channels::declare`].
    /// `catalog` holds the session's skills, which an item may name.
    pub(crate) fn new(
        declared: Vec<(ChannelDefinition, Arc<dyn ChannelBackend>)>,
        interpreter: &Interpreter,
        lifecycle: &Arc<Lifecycle>,
        catalog: Option<Arc<Catalog>>,
    ) -> Self {
        let channels = declared
            .into_iter()
            .map(|(definition, backend)| Chan {
                definition,
                schemas: (None, None),
                sending: None,
                backend,
                permits: Arc::new(Semaphore::new(CALLS_MAX)),
                holder: None,
                closed: None,
                items: HashMap::new(),
                asked: BTreeMap::new(),
                outgoing: BTreeMap::new(),
                finished: HashMap::new(),
                requests_after_close: Vec::new(),
                withdrawing: 0,
                deliveries: BTreeMap::new(),
                consumed: None,
                acknowledged: None,
                delivered: 0,
                unread: 0,
                flights: BTreeMap::new(),
                uncertain: BTreeMap::new(),
                resolving: 0,
                sent: BTreeMap::new(),
                after_close: Vec::new(),
                uncertain_at_close: Vec::new(),
                notices: VecDeque::new(),
                notifying: false,
            })
            .collect();
        let table = Arc::new(Table {
            events: lifecycle.events().clone(),
            lifecycle: Arc::clone(lifecycle),
            catalog,
            interpreter: interpreter.downgrade(),
            to_interpreter: interpreter.relay_line(),
            state: watch::Sender::new(State {
                channels,
                closed: false,
            }),
            rounds: OnceLock::new(),
            closing: CancellationToken::new(),
            stopping: CancellationToken::new(),
            settles: std::sync::atomic::AtomicU64::new(0),
        });
        // Weak: the lifecycle keeps its closers, and the table keeps it.
        let weak = Arc::downgrade(&table);
        lifecycle.on_close(Arc::new(move |_| {
            if let Some(table) = weak.upgrade() {
                table.admission_closed();
            }
        }));
        Self(table)
    }

    /// Tell the interpreter the channels, before any kernel opens, and wait
    /// within `bound` for each to be made -- a typed contract imports the
    /// application's code -- then push each definition's initial items, in
    /// order. Nothing is told for a session with none. `Err` says why the
    /// session cannot start: a channel the interpreter could not make, or an
    /// initial item one refused.
    pub(crate) async fn declare(
        &self,
        interpreter: &Interpreter,
        bound: Duration,
    ) -> Result<(), String> {
        // The initial items are the channels' to push once, and no definition
        // keeps them after.
        let (entries, initial) = self.0.with(|state| {
            let entries: Vec<Value> = state
                .channels
                .iter()
                .map(|chan| chan.definition.entry())
                .collect();
            let initial: Vec<(String, Inbound)> = state
                .channels
                .iter_mut()
                .flat_map(|chan| {
                    let name = chan.definition.name.clone();
                    std::mem::take(&mut chan.definition.initial)
                        .into_iter()
                        .map(move |item| (name.clone(), item))
                })
                .collect();
            (entries, initial)
        });
        if entries.is_empty() {
            return Ok(());
        }
        let hook = Arc::clone(&self.0);
        interpreter.attach_channels(Arc::new(move |line: Value| hook.line(line)));
        let schemas = interpreter
            .declare_channels(Value::Array(entries), bound)
            .await
            .map_err(|e| e.to_string())?;
        self.0.with(|state| -> Result<(), String> {
            for chan in &mut state.channels {
                let of = &schemas[&chan.definition.name];
                let text = |direction: &str| of[direction].as_str().map(str::to_string);
                chan.schemas = (text("receives"), text("sends"));
                if let Some(schema) = &chan.schemas.1 {
                    // Offline, as a service's schema is compiled: nothing outside it loads.
                    let compiled = serde_json::from_str(schema)
                        .map_err(|e| e.to_string())
                        .and_then(|schema: Value| {
                            jsonschema::options()
                                .offline()
                                .build(&schema)
                                .map_err(|e| e.to_string())
                        })
                        .map_err(|e| {
                            format!(
                                "channel {:?}'s sending type has a schema the host cannot check: \
                                 {e}",
                                chan.definition.name
                            )
                        })?;
                    chan.sending = Some(Arc::new(compiled));
                }
            }
            Ok(())
        })?;
        // Each is on its way, in order, as `deliver` returns: their answers are
        // then waited on together.
        let pushed: Vec<_> = initial
            .into_iter()
            .map(|(name, item)| {
                let id = item.item.clone();
                let answer = self.handle(&name).expect("declared").deliver(item);
                (name, id, answer)
            })
            .collect();
        for (name, id, answer) in pushed {
            answer
                .await
                .map_err(|e| format!("channel {name:?} refused its initial item {id:?}: {e}"))?;
        }
        Ok(())
    }

    /// Lend the channels the children registry's `rounds`.
    pub(crate) fn lend(&self, rounds: Rounds) {
        let _ = self.0.rounds.set(rounds);
    }

    /// The owner's end of the channel `name`, if the session declares it.
    pub(crate) fn handle(&self, name: &str) -> Option<Channel> {
        self.0.state.borrow().find(name)?;
        Some(Channel {
            table: Arc::clone(&self.0),
            name: name.to_string(),
        })
    }

    /// The plain channels `agent` holds whose arrivals are announced, as its
    /// announcer counts them: what waits unread, and every item ever queued.
    pub(crate) fn counts_of(&self, agent: &str) -> BTreeMap<String, Counts> {
        self.0
            .state
            .borrow()
            .channels
            .iter()
            // An inbound request channel's are counted from its holder's
            // requests, as every request of an agent's is.
            .filter(|chan| {
                chan.holder.as_deref() == Some(agent)
                    && chan.definition.wake != Wake::Silent
                    && chan.definition.kind == ChannelKind::Plain
            })
            .map(|chan| {
                (
                    chan.definition.name.clone(),
                    Counts {
                        pending: chan.unread,
                        delivered: chan.delivered,
                        requests: false,
                    },
                )
            })
            .collect()
    }

    /// What `agent`'s instructions say of each channel it holds, in the order
    /// they were declared.
    pub(crate) fn manifest_of(&self, agent: &str) -> Vec<ExternalText> {
        self.0
            .state
            .borrow()
            .channels
            .iter()
            .filter(|chan| chan.holder.as_deref() == Some(agent))
            .map(|chan| {
                let definition = &chan.definition;
                ExternalText {
                    name: definition.name.clone(),
                    kind: definition.kind,
                    receives: contract_text(&definition.receives, chan.schemas.0.as_deref()),
                    sends: contract_text(&definition.sends, chan.schemas.1.as_deref()),
                    description: definition.description.clone(),
                    wake: definition.wake,
                }
            })
            .collect()
    }

    /// Each channel `agent` sent on in its round `round`, with the items it
    /// sent, in order: the round's deliberate sends, read as the round ends.
    /// The registry keeps them for the agent's latest [`ROUNDS_KEPT`] rounds,
    /// as its handle keeps their facts, and drops what is older.
    pub(crate) fn sends_in(&self, agent: &str, round: u32) -> Vec<RoundSend> {
        let oldest = round.saturating_sub(ROUNDS_KEPT as u32 - 1);
        let key = (agent.to_string(), round);
        self.0.changing(|state| {
            let sends: Vec<RoundSend> = state
                .channels
                .iter_mut()
                .filter_map(|chan| {
                    chan.sent
                        .retain(|(sender, kept), _| sender != agent || *kept >= oldest);
                    let items = chan.sent.get(&key)?.clone();
                    Some(RoundSend::new(&chan.definition.name, items))
                })
                .collect();
            (sends, false)
        })
    }

    /// Each channel as the registry holds it now, in the order the owner
    /// declared them.
    pub(crate) fn snapshot(&self) -> Vec<ChannelSnapshot> {
        self.0
            .state
            .borrow()
            .channels
            .iter()
            .map(Chan::snapshot)
            .collect()
    }

    /// Admission has closed: ask, once, what became of each message whose
    /// send is uncertain, for the close's report. [`Channels::drained`] waits
    /// for the answers.
    pub(crate) fn resolve_uncertain(&self) {
        self.0.resolve_uncertain();
    }

    /// Resolves once no send, withdrawal or resolution is waiting on a
    /// backend, every request on a channel has ended, and every notice has
    /// been acknowledged.
    pub(crate) fn drained(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut state = self.0.state.subscribe();
        async move {
            let _ = state
                .wait_for(|state| {
                    state.channels.iter().all(|chan| {
                        chan.flights.is_empty()
                            && chan.notices.is_empty()
                            && chan.withdrawing == 0
                            && chan.resolving == 0
                            && !chan.requests_open()
                    })
                })
                .await;
        }
    }

    /// One line the interpreter sent about a request of an agent's on an
    /// inbound channel -- received, ended, or answered after it ended --
    /// which the children registry has recorded: what its backend is told.
    pub(crate) fn request_line(&self, line: &Value) {
        self.0.asked_line(line);
    }

    /// The session's drain has ended: give up on every send its backend has
    /// not answered -- each `uncertain` -- and on every notice.
    pub(crate) fn terminate(&self) {
        self.0.stopping.cancel();
    }

    /// What the close left on each channel: read last, once nothing more can
    /// reach the table.
    pub(crate) fn report(&self) -> Vec<ChannelClose> {
        self.terminate();
        let mut report = Vec::new();
        self.0.state.send_modify(|state| {
            for chan in &mut state.channels {
                let unread = chan
                    .messages_at(Position::Unread)
                    .map(|unread| unread.item)
                    .collect();
                let mut sends = std::mem::take(&mut chan.after_close);
                for (item, Flight { agent, .. }) in std::mem::take(&mut chan.flights) {
                    sends.push(SendAtClose {
                        item,
                        agent,
                        outcome: ItemOutcome::Uncertain,
                        reason: Some("the session ended before its backend answered".to_string()),
                    });
                }
                let unacknowledged = chan
                    .notices
                    .iter()
                    .filter_map(|notice| match notice {
                        Notice::Consumed(notice, _) => Some(notice.item.clone()),
                        Notice::Settled(..) => None,
                    })
                    .collect();
                let mut requests = std::mem::take(&mut chan.requests_after_close);
                // Nothing ended these: the interpreter was gone first.
                for (id, asked) in chan.asked.iter().filter(|(id, _)| chan.open(**id)) {
                    requests.push(ChannelRequestAtClose {
                        request: AgentRequestId::new(*id),
                        direction: Direction::Inbound,
                        item: Some(asked.item.clone()),
                        end: RequestEnd::Terminated,
                        acknowledged: false,
                    });
                }
                for id in chan.outgoing.keys() {
                    requests.push(ChannelRequestAtClose {
                        request: AgentRequestId::new(*id),
                        direction: Direction::Outbound,
                        item: None,
                        end: RequestEnd::Terminated,
                        acknowledged: false,
                    });
                }
                // None is left once the table has heard the close; listed all the same, so none
                // is lost.
                chan.list_doubts();
                let mut uncertain = std::mem::take(&mut chan.uncertain_at_close);
                uncertain.sort_by_key(|row| row.item);
                report.push(ChannelClose {
                    channel: chan.definition.name.clone(),
                    holder: chan.holder.clone(),
                    unread,
                    sends,
                    unacknowledged,
                    requests,
                    uncertain,
                });
            }
        });
        report
    }
}

impl Table {
    fn with<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let mut out = None;
        self.state.send_modify(|state| out = Some(change(state)));
        out.expect("the change ran")
    }

    /// [`Table::with`] for a change that may leave the state as it was: what
    /// waits on the state is woken only when `change` says it changed it.
    fn changing<T>(&self, change: impl FnOnce(&mut State) -> (T, bool)) -> T {
        let mut out = None;
        self.state.send_if_modified(|state| {
            let (value, changed) = change(state);
            out = Some(value);
            changed
        });
        out.expect("the change ran")
    }

    /// The events of `agent`, or, when there is none, the session's, which
    /// carry no subject: no agent is there to own them, the primary included.
    fn events_of(&self, agent: Option<&str>) -> Events {
        match agent {
            Some(agent) => self.events.for_agent(agent),
            None => self.events.for_session(),
        }
    }

    /// One line the interpreter sent about the session's channels, on its
    /// reader: recorded before the next is read.
    fn line(self: &Arc<Self>, mut line: Value) {
        let (Some(kind), Some(channel)) = (line["t"].as_str(), line["channel"].as_str()) else {
            tracing::warn!("ignored a channel line naming no channel: {line}");
            return;
        };
        let (kind, channel) = (kind.to_string(), channel.to_string());
        let agent = line["agent"].as_str().unwrap_or_default().to_string();
        match kind.as_str() {
            "channel.attached" => self.attached(&agent, &channel),
            "channel.detached" => self.detached(&agent, &channel),
            "channel.consumed" => match line["delivery"].as_u64() {
                Some(delivery) => self.consumed(&agent, &channel, MessageId::new(delivery)),
                None => tracing::warn!("ignored a consumed notice naming no delivery: {line}"),
            },
            "channel.send" => match line["id"].as_u64() {
                Some(ask) => self.send(&agent, Waiter::Send(ask), &channel, line["body"].take()),
                None => tracing::warn!("ignored a send with no id: {line}"),
            },
            "channel.request" => match line["id"].as_u64() {
                Some(id) => self.send(&agent, Waiter::Request(id), &channel, line["body"].take()),
                None => tracing::warn!("ignored a request with no id: {line}"),
            },
            "channel.withdraw" => match line["request"].as_u64() {
                Some(id) => self.withdraw(&channel, id),
                None => tracing::warn!("ignored a withdrawal naming no request: {line}"),
            },
            "channel.ended" => match line["request"].as_u64() {
                Some(id) => self.outbound_ended(&agent, &channel, id, &line),
                None => tracing::warn!("ignored an end naming no request: {line}"),
            },
            "channel.resolve" => match line["id"].as_u64() {
                Some(ask) => self.resolve_asked(&agent, ask, &channel, &line),
                None => tracing::warn!("ignored a resolution with no id: {line}"),
            },
            _ => tracing::warn!("ignored a {kind} line, which nothing reads"),
        }
    }

    fn attached(&self, agent: &str, channel: &str) {
        let found = self.with(|state| {
            let chan = state.find_mut(channel)?;
            chan.holder = Some(agent.to_string());
            let definition = &chan.definition;
            Some(Payload::ChannelAttached {
                channel: channel.to_string(),
                kind: definition.kind.to_string(),
                direction: definition
                    .kind
                    .direction()
                    .map(|direction| direction.to_string()),
                receives: definition.receives.to_string(),
                sends: definition.sends.to_string(),
                bound: definition.bound,
                wake: definition.wake.to_string(),
            })
        });
        match found {
            Some(payload) => self.events.for_agent(agent).emit(payload),
            None => tracing::warn!("agent {agent} took channel {channel}, which is not declared"),
        }
    }

    fn detached(&self, agent: &str, channel: &str) {
        self.changing(|state| match state.find_mut(channel) {
            Some(chan) if chan.holder.as_deref() == Some(agent) => {
                chan.holder = None;
                for flight in chan.flights.values().filter(|flight| flight.agent == agent) {
                    flight.unsent.cancel();
                }
                ((), true)
            }
            _ => ((), false),
        });
    }

    /// Begin pushing `item` on `name`.
    fn push(self: &Arc<Self>, name: &str, item: &Inbound) -> Push {
        // Read before the channels' state is locked: the close is the host's
        // table's, which is locked before it, never after.
        let closing = self.lifecycle.closed().is_some();
        // `None` once the item is reserved, to post.
        let begun = self.with(|state| {
            let Some(chan) = state.find_mut(name) else {
                return Some(Push::Answered(Err(DeliverError::UnknownChannel(
                    name.to_string(),
                ))));
            };
            match chan.items.get(&item.item) {
                Some(Taken::Pushing) => return Some(Push::Busy),
                Some(Taken::Queued(delivery, position)) => {
                    return Some(Push::Answered(Ok(Delivered::Repeat {
                        delivery: *delivery,
                        position: *position,
                    })));
                }
                None => {}
            }
            let unfit = match (chan.definition.kind, item.kind) {
                (ChannelKind::Plain, ItemKind::Message)
                | (ChannelKind::Request(Direction::Inbound), ItemKind::Request) => None,
                (ChannelKind::Request(Direction::Outbound), kind) => Some(format!(
                    "a {kind} on an outbound request channel, which carries its holder's \
                     requests: settle each with `Channel::settle`"
                )),
                (ChannelKind::Plain, kind) => Some(format!(
                    "a {kind} on a plain channel, which carries messages"
                )),
                (_, kind) => Some(format!(
                    "a {kind} on an inbound request channel, which carries requests"
                )),
            };
            let refused = if let Some(reason) = &chan.closed {
                DeliverError::Closed(reason.clone())
            } else if closing {
                DeliverError::Closed("the session is closing".to_string())
            } else if let Some(unfit) = unfit {
                DeliverError::Contract(vec![unfit])
            } else if item.kind == ItemKind::Request && item.skill.is_some() {
                DeliverError::Contract(vec!["a request names no skill".to_string()])
            } else {
                chan.items.insert(item.item.clone(), Taken::Pushing);
                return None;
            };
            Some(Push::Answered(Err(refused)))
        });
        if let Some(push) = begun {
            return push;
        }
        match self.post(name, item) {
            Ok(answered) => Push::Started(answered),
            Err(refused) => {
                self.with(|state| {
                    if let Some(chan) = state.find_mut(name) {
                        chan.items.remove(&item.item);
                    }
                });
                Push::Answered(Err(refused))
            }
        }
    }

    /// Post `item`, reserved on `name`, to the interpreter, and resolve the
    /// reservation once it answers, whoever still waits for it.
    fn post(
        self: &Arc<Self>,
        name: &str,
        item: &Inbound,
    ) -> Result<oneshot::Receiver<Result<Delivered, DeliverError>>, DeliverError> {
        let source = match &item.skill {
            None => None,
            Some(skill) => match self
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.directed(skill))
            {
                Some((source, _)) => Some(source),
                None => return Err(DeliverError::UnknownSkill(skill.clone())),
            },
        };
        let body = item.body.to_string();
        if body.len() > MESSAGE_MAX {
            return Err(DeliverError::Contract(vec![format!(
                "$: the body is {} bytes as JSON, past the {MESSAGE_MAX} a channel carries",
                body.len()
            )]));
        }
        let asking = item.kind == ItemKind::Request;
        let mut line = json!({
            "t": "channel.msg",
            "channel": name,
            "kind": item.kind.to_string(),
            "item": item.item,
            "body": item.body,
            "sender": item.sender,
        });
        if let Some(reference) = &item.reference {
            line["reference"] = json!(reference);
        }
        if let Some(skill) = &item.skill {
            line["skill"] = json!(skill);
        }
        if let Some(correlation) = &item.correlation {
            line["correlation"] = json!(correlation);
        }
        // Recorded before the line is sent, under the lock it is sent under, so the holder's
        // taking it -- which can reach the host ahead of this answer -- finds it. A request is
        // recorded by its id, which the interpreter gives it, and evented as one of its holder's.
        let mut delivery = None;
        let answer = self
            .interpreter
            .post_item(line, &mut |id| {
                let id = id.message();
                delivery = Some(id);
                if asking {
                    return;
                }
                let holder = self.with(|state| {
                    let chan = state.find_mut(name)?;
                    chan.deliveries
                        .insert(id, (item.item.clone(), item.reference.clone()));
                    Some(chan.holder.clone())
                });
                let holder = holder.flatten();
                let events = self.events_of(holder.as_deref());
                events.emit_with(|| Payload::MessageSent {
                    message: id,
                    channel: name.to_string(),
                    from: BACKEND_END.to_string(),
                    to: events.end(),
                    sender: Some(item.sender.clone()),
                    body: body.clone(),
                });
                if let (Some(skill), Some(source)) = (&item.skill, &source) {
                    events.emit_with(|| Payload::SkillDirectiveReceived {
                        skill: skill.clone(),
                        source: source.clone(),
                        message: id,
                        channel: name.to_string(),
                    });
                }
            })
            .map_err(|e| DeliverError::Closed(e.to_string()))?;
        let delivery = delivery.expect("given as the line was sent");
        let (answered, waiting) = oneshot::channel();
        let table = Arc::clone(self);
        let (name, id, reference) = (name.to_string(), item.item.clone(), item.reference.clone());
        tokio::spawn(async move {
            let posted = answer.await;
            let _ = answered.send(table.posted(&name, &id, reference, delivery, posted));
        });
        Ok(waiting)
    }

    /// The interpreter answered for item `id`, pushed on `name` with the
    /// backend's `reference` as `delivery`.
    fn posted(
        &self,
        name: &str,
        id: &str,
        reference: Option<String>,
        delivery: MessageId,
        posted: Result<Result<Posted, PostRefusal>, InterpreterError>,
    ) -> Result<Delivered, DeliverError> {
        let (refused, request) = match posted {
            Ok(Ok(posted)) => (None, posted.request),
            Ok(Err(refusal)) => (Some(refused(name, refusal)), None),
            Err(e) => (Some(DeliverError::Closed(e.to_string())), None),
        };
        let (holder, wakes) = self.with(|state| {
            let Some(chan) = state.find_mut(name) else {
                return (None, false);
            };
            if refused.is_some() {
                chan.items.remove(id);
                chan.deliveries.remove(&delivery);
            } else if let Some(request) = request {
                // Its holder may have taken it, or it may have ended, before this answer came.
                if chan.items.get(id) == Some(&Taken::Pushing) {
                    chan.place(request, id, reference, Position::Unread);
                }
                // The children registry wakes its holder, as it records the request.
                return (chan.holder.clone(), false);
            } else {
                chan.delivered += 1;
                // The holder may have taken it before this answer came.
                if let Some(taken @ Taken::Pushing) = chan.items.get_mut(id) {
                    *taken = Taken::Queued(DeliveryId::Message(delivery), Position::Unread);
                    chan.unread += 1;
                }
            }
            (chan.holder.clone(), chan.definition.wake != Wake::Silent)
        });
        if let Some(error) = refused {
            let events = self.events_of(holder.as_deref());
            events.emit(Payload::MessageRefused {
                message: delivery,
                channel: name.to_string(),
                from: BACKEND_END.to_string(),
                to: events.end(),
                reason: error.to_string(),
            });
            return Err(error);
        }
        if wakes && let (Some(holder), Some(rounds)) = (&holder, self.rounds.get()) {
            (rounds.wake)(holder);
        }
        Ok(Delivered::Queued {
            delivery: match request {
                Some(request) => DeliveryId::Request(AgentRequestId::new(request)),
                None => DeliveryId::Message(delivery),
            },
        })
    }

    /// Wait until no push of item `id` on `name` is under way.
    async fn pushed(&self, name: &str, id: &str) {
        let mut state = self.state.subscribe();
        let _ = state
            .wait_for(|state| {
                state
                    .find(name)
                    .is_none_or(|chan| chan.items.get(id) != Some(&Taken::Pushing))
            })
            .await;
    }

    /// `agent`'s code received the item delivered on `channel` as `delivery`:
    /// recorded, and its backend told, in order.
    fn consumed(self: &Arc<Self>, agent: &str, channel: &str, delivery: MessageId) {
        let start = self.with(|state| {
            let chan = state.find_mut(channel)?;
            let (id, reference) = chan.deliveries.get(&delivery)?;
            let notice = Consumed {
                channel: channel.to_string(),
                item: id.clone(),
                delivery,
                reference: reference.clone(),
            };
            let taken = chan.items.get_mut(id)?;
            match taken {
                Taken::Queued(_, position @ Position::Unread) => {
                    *position = Position::Consumed;
                    chan.unread -= 1;
                }
                // Taken before the push's answer came: never counted unread.
                Taken::Pushing => {
                    *taken = Taken::Queued(DeliveryId::Message(delivery), Position::Consumed);
                }
                Taken::Queued(..) => return None,
            }
            // Received in order, so the last received is the watermark.
            chan.consumed = Some(notice.inbound());
            Some(chan.push_notice(Notice::Consumed(notice, agent.to_string())))
        });
        let Some(start) = start else {
            tracing::warn!("agent {agent} took {delivery} on {channel}, which it was never given");
            return;
        };
        self.events.for_agent(agent).emit(Payload::MessageReceived {
            message: delivery,
            channel: channel.to_string(),
            from: BACKEND_END.to_string(),
            to: Subject::agent(agent).to_string(),
        });
        self.notifying(channel, start);
    }

    /// Start telling `channel`'s backend what it has not acknowledged, unless
    /// a task is telling it already: `start` says, as the notice was queued.
    fn notifying(self: &Arc<Self>, channel: &str, start: bool) {
        if start {
            let table = Arc::clone(self);
            let channel = channel.to_string();
            tokio::spawn(async move { table.notify(&channel).await });
        }
    }

    /// Queue `notice` for `channel`'s backend, and start telling it. Nothing
    /// waits on a notice queued, only on the last told, so no waiter is woken.
    fn queue(self: &Arc<Self>, channel: &str, notice: Notice) {
        let start = self.changing(|state| {
            let start = state
                .find_mut(channel)
                .is_some_and(|chan| chan.push_notice(notice));
            (start, false)
        });
        self.notifying(channel, start);
    }

    /// Tell `channel`'s backend of each item received and each request ended,
    /// in order, each until it acknowledges, while the session lasts.
    async fn notify(&self, channel: &str) {
        let mut wait = NOTICE_RETRY_FIRST;
        loop {
            // The next notice, or, with none left, this task's end: said in the
            // same step, so a notice queued after it starts another.
            let next = self.changing(|state| {
                let Some(chan) = state.find_mut(channel) else {
                    return (None, false);
                };
                let Some(notice) = chan.notices.front() else {
                    chan.notifying = false;
                    return (None, true);
                };
                (Some((notice.clone(), chan.calls())), false)
            });
            let Some((notice, calls)) = next else {
                return;
            };
            let what = match &notice {
                Notice::Consumed(notice, _) => format!("the notice of item {}", notice.item),
                Notice::Settled(notice, _) => {
                    format!("the notice of request {}'s end", notice.request)
                }
            };
            let answered = self
                .call(&calls, || match notice {
                    Notice::Consumed(notice, _) => calls.backend.consumed(notice),
                    Notice::Settled(notice, _) => calls.backend.settled(notice),
                })
                .await;
            match answered {
                Ok(()) => {
                    self.acknowledged(channel);
                    wait = NOTICE_RETRY_FIRST;
                }
                // The drain has ended: nothing more is told.
                Err(_) if self.stopping.is_cancelled() => {
                    self.with(|state| {
                        if let Some(chan) = state.find_mut(channel) {
                            chan.notifying = false;
                        }
                    });
                    return;
                }
                Err(why) => {
                    tracing::warn!(
                        "channel {channel}'s backend did not take {what}; telling it again in \
                         {wait:?}: {why}"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        () = self.stopping.cancelled() => {}
                    }
                    wait = (wait * 2).min(NOTICE_RETRY_MAX);
                }
            }
        }
    }

    /// `channel`'s backend acknowledged the first of its notices: nothing
    /// reads it again, and it is evented under the agent it was of.
    fn acknowledged(&self, channel: &str) {
        let told = self.with(|state| {
            let chan = state.find_mut(channel)?;
            let told = chan.notices.pop_front()?;
            match &told {
                Notice::Consumed(notice, _) => {
                    // Acknowledged: no notice, no report and no take reads it again.
                    chan.deliveries.remove(&notice.delivery);
                    if let Some(Taken::Queued(_, position)) = chan.items.get_mut(&notice.item) {
                        *position = Position::Acknowledged;
                    }
                    // Told in the order received, so the last acknowledged is the watermark.
                    chan.acknowledged = Some(notice.inbound());
                }
                Notice::Settled(notice, _) => {
                    let request = notice.request.get();
                    if let Some(at) = chan.at_close(request) {
                        at.acknowledged = true;
                    }
                    // An end, and not a refused answer after it: the request is done with.
                    if !matches!(notice.outcome, Settlement::Refused { .. })
                        && let Some(asked) = chan.asked.remove(&request)
                        && let Some(Taken::Queued(_, position)) = chan.items.get_mut(&asked.item)
                    {
                        *position = Position::Acknowledged;
                    }
                }
            }
            Some(told)
        });
        match told {
            // The receiver's, though another agent may hold the channel by now.
            Some(Notice::Consumed(notice, agent)) => {
                self.events
                    .for_agent(&agent)
                    .emit(Payload::ChannelConsumed {
                        channel: channel.to_string(),
                        item: notice.item,
                        delivery: notice.delivery,
                    });
            }
            Some(Notice::Settled(notice, agent)) => {
                self.events_of(agent.as_deref())
                    .emit(Payload::ChannelSettled {
                        channel: channel.to_string(),
                        request: notice.request,
                        item: notice.item,
                        refused: matches!(notice.outcome, Settlement::Refused { .. }),
                    });
            }
            None => {}
        }
    }

    /// `agent`'s code sent `body` on `channel` -- a message, its `send` waiting
    /// on the answer, or a request, by its id -- as `waiter` says: given an id,
    /// recorded with the round it has open, and handed to the backend, or
    /// refused.
    fn send(self: &Arc<Self>, agent: &str, waiter: Waiter, channel: &str, body: Value) {
        let item = self.interpreter.mint().message();
        // Read before the channels' state is locked, as `push` reads the close.
        let round = self.rounds.get().and_then(|rounds| (rounds.round)(agent));
        let closing = self.lifecycle.closed().is_some();
        let text = body.to_string();
        // Checked here whatever the holder's code checked: what it did to its own endpoint, or
        // a line it wrote itself, never gives the backend what the channel does not send.
        let contract = {
            let state = self.state.borrow();
            state
                .find(channel)
                .map(|chan| (chan.definition.sends.clone(), chan.sending.clone()))
        };
        let refused = contract.and_then(|(sends, schema)| {
            unsendable(channel, &sends, schema.as_deref(), &body, text.len())
        });
        let unsent = CancellationToken::new();
        let decided = self.with(|state| {
            let Some(chan) = state.find_mut(channel) else {
                return Err(Ended::refused(
                    "refused",
                    &format!("the session declares no channel {channel:?}"),
                ));
            };
            if chan.holder.as_deref() != Some(agent) {
                return Err(Ended::refused(
                    "refused",
                    &format!("channel {channel:?} is not held by agent {agent:?}"),
                ));
            }
            if chan.definition.kind != waiter.carried_on() {
                return Err(Ended::refused(
                    "refused",
                    &format!("channel {channel:?} carries no such item"),
                ));
            }
            // A send between rounds belongs to none, and no round's facts read it.
            if let Some(round) = round {
                chan.sent
                    .entry((agent.to_string(), round))
                    .or_default()
                    .push(item);
            }
            if let Some(reason) = &chan.closed {
                return Err(Ended::refused("closed", reason));
            }
            if closing {
                return Err(Ended::refused("closing", "the session is closing"));
            }
            if let Some(why) = refused {
                return Err(Ended::refused("refused", &why));
            }
            chan.flights.insert(
                item,
                Flight {
                    agent: agent.to_string(),
                    unsent: unsent.clone(),
                },
            );
            if let Some(id) = waiter.request() {
                chan.outgoing.insert(
                    id,
                    Outgoing {
                        agent: agent.to_string(),
                        item,
                        reference: None,
                        state: Out::Sending { withdraw: false },
                    },
                );
            }
            Ok((chan.calls(), chan.definition.sends.to_string()))
        });
        let events = self.events.for_agent(agent);
        match waiter {
            Waiter::Send(_) => events.emit_with(|| Payload::MessageSent {
                message: item,
                channel: channel.to_string(),
                from: Subject::agent(agent).to_string(),
                to: BACKEND_END.to_string(),
                sender: None,
                body: text,
            }),
            Waiter::Request(id) => events.emit_with(|| Payload::ChannelRequestSent {
                channel: channel.to_string(),
                request: AgentRequestId::new(id),
                item,
                body: text,
            }),
        }
        let (calls, body_type) = match decided {
            Ok(decided) => decided,
            Err(refused) => {
                self.finish(agent, waiter, channel, item, refused, false);
                return;
            }
        };
        let request = waiter.request().map(AgentRequestId::new);
        let outbound = Outbound {
            channel: channel.to_string(),
            item,
            kind: match request {
                None => ItemKind::Message,
                Some(_) => ItemKind::Request,
            },
            request,
            body_type,
            sender: agent.to_string(),
            correlation: None,
            reference: None,
            body,
        };
        let table = Arc::clone(self);
        let (agent, channel) = (agent.to_string(), channel.to_string());
        tokio::spawn(async move {
            let ended = table.handed(&agent, &calls, &unsent, outbound).await;
            table.finish(&agent, waiter, &channel, item, ended, true);
        });
    }

    /// Hand `outbound` to its backend once one of the permits `calls` holds
    /// is free, unless the session closes, or the channel closes or its
    /// sender gives it up -- `unsent` -- first, and say what became of it. A
    /// send that may have been taken is evented so, and its backend asked
    /// once, holding the same permit, what became of it: its answer is what
    /// the sender is told.
    async fn handed(
        &self,
        agent: &str,
        calls: &Calls,
        unsent: &CancellationToken,
        outbound: Outbound,
    ) -> Ended {
        let permit = tokio::select! {
            biased;
            () = self.closing.cancelled() => {
                return Ended::refused("closing", "the session closed before it was sent");
            }
            () = unsent.cancelled() => return self.unsent(&outbound.channel),
            permit = calls.permits.acquire() => permit.ok(),
        };
        let Some(_permit) = permit else {
            return Ended::refused("closing", "the session closed before it was sent");
        };
        // Said again with the permit in hand: a close that came as it was granted refuses
        // the send, and one after it finds the send handed.
        if unsent.is_cancelled() {
            return self.unsent(&outbound.channel);
        }
        let query = Query {
            channel: outbound.channel.clone(),
            item: outbound.item,
            request: outbound.request,
            reference: None,
        };
        let doubt = match self
            .bounded(calls.bound, calls.backend.send(outbound))
            .await
        {
            Ok(SendAnswer::Accepted { reference }) => return Ended::accepted(reference),
            Ok(SendAnswer::NotAccepted { reason }) => return Ended::not_accepted(reason),
            Ok(SendAnswer::Uncertain { reason }) => reason,
            // The drain has ended: nothing more is asked.
            Err(why) if self.stopping.is_cancelled() => return Ended::uncertain(&why),
            Err(why) => why,
        };
        // Evented as it is, before the backend is asked: the resolution follows it.
        self.events
            .for_agent(agent)
            .emit(Payload::ChannelItemOutcome {
                channel: query.channel.clone(),
                item: query.item,
                outcome: ItemOutcome::Uncertain,
                reason: Some(doubt.clone()),
                reference: None,
            });
        let resolution = self.resolution(calls, &query, ChannelOperation::Send).await;
        self.resolved(agent, &query, ChannelOperation::Send, &resolution);
        let mut ended = Ended::resolved(resolution);
        if let Some(reason) = ended
            .reason
            .as_mut()
            .filter(|_| ended.outcome == ItemOutcome::Uncertain)
        {
            *reason =
                format!("{doubt}; asked what became of it, the backend could not tell: {reason}");
        }
        ended.evented = true;
        ended
    }

    /// Ask the backend `calls` names to resolve `operation` on `query`, within
    /// its bound and the drain, the caller holding one of its permits: its
    /// answer, or uncertain when there is none.
    async fn resolution(
        &self,
        calls: &Calls,
        query: &Query,
        operation: ChannelOperation,
    ) -> Resolution {
        let (backend, query) = (&calls.backend, query.clone());
        let answered = match operation {
            ChannelOperation::Send => self
                .bounded(calls.bound, backend.resolve_send(query))
                .await
                .map(Resolution::from),
            ChannelOperation::Withdrawal => self
                .bounded(calls.bound, backend.resolve_withdrawal(query))
                .await
                .and_then(|answer| answer.map(Resolution::from).map_err(|e| e.to_string())),
        };
        answered.unwrap_or_else(|reason| Resolution::Uncertain { reason })
    }

    /// Ask the backend `calls` names to resolve `operation` on `query`,
    /// `agent`'s, once one
    /// of its permits is free; event its answer; and settle what it settles,
    /// counted off the drain in the same step. The answer, and what the
    /// request's kernel is to be told of it.
    async fn resolve(
        &self,
        agent: &str,
        calls: &Calls,
        query: &Query,
        operation: ChannelOperation,
    ) -> (Resolution, Option<(&'static str, Value)>) {
        let resolution = match self.permit(calls).await {
            Ok(_permit) => self.resolution(calls, query, operation).await,
            Err(reason) => Resolution::Uncertain { reason },
        };
        self.resolved(agent, query, operation, &resolution);
        let told = self.with(|state| {
            let chan = state.find_mut(&query.channel)?;
            chan.resolving -= 1;
            chan.settle_by(query, &resolution)
        });
        (resolution, told)
    }

    /// The backend answered `operation` on `query`, of `agent`'s item, with
    /// `resolution`: evented under that agent.
    fn resolved(
        &self,
        agent: &str,
        query: &Query,
        operation: ChannelOperation,
        resolution: &Resolution,
    ) {
        let (answer, reason, reference) = resolution.parts();
        self.events
            .for_agent(agent)
            .emit(Payload::ChannelItemResolved {
                channel: query.channel.clone(),
                item: query.item,
                request: query.request,
                operation,
                answer,
                reason: reason.map(str::to_string),
                reference: reference.map(str::to_string),
            });
    }

    /// Why a send on `channel` was never handed to its backend, once its
    /// flight's token was cancelled: the channel closed, or its sender gave
    /// it up.
    fn unsent(&self, channel: &str) -> Ended {
        let closed = self
            .state
            .borrow()
            .find(channel)
            .and_then(|chan| chan.closed.clone());
        match closed {
            Some(reason) => Ended::refused("closed", &reason),
            None => Ended::refused(
                "refused",
                "its agent gave the channel up before it was sent",
            ),
        }
    }

    /// The send of `item` on `channel`, which `agent` -- or its request --
    /// waits on as `waiter` says, has `ended`: recorded in the report once
    /// admission has closed, evented, and answered. One `in_flight` that the
    /// report gave up on already is left as the report says it.
    fn finish(
        self: &Arc<Self>,
        agent: &str,
        waiter: Waiter,
        channel: &str,
        item: MessageId,
        ended: Ended,
        in_flight: bool,
    ) {
        // What a request's send leaves to do is decided in the step that ends its flight, so the
        // drain never finds the flight gone and what its backend is owed not yet counted.
        let next = self.with(|state| {
            // The close the doubts were listed by: one that ends uncertain is a doubt before it,
            // and the close's send after.
            let closed = state.closed;
            let Some(chan) = state.find_mut(channel) else {
                return (!in_flight).then_some((Sent::Told, None));
            };
            if in_flight && chan.flights.remove(&item).is_none() {
                return None;
            }
            if closed {
                chan.after_close.push(SendAtClose {
                    item,
                    agent: agent.to_string(),
                    outcome: ended.outcome,
                    reason: ended.reason.clone(),
                });
            } else if waiter.request().is_none() && ended.outcome == ItemOutcome::Uncertain {
                // Its sender is told it is uncertain, with its id, by which it is resolved.
                chan.uncertain.insert(
                    item,
                    Doubt {
                        agent: agent.to_string(),
                        reason: ended.reason.clone().unwrap_or_default(),
                    },
                );
            }
            Some(match waiter.request() {
                Some(id) => chan.request_sent(id, ended.wire, (item, ended.reference.clone())),
                None => (Sent::Told, None),
            })
        });
        let Some((sent, giving)) = next else {
            return;
        };
        let reason = ended.reason.unwrap_or_default();
        if !ended.evented {
            self.events
                .for_agent(agent)
                .emit(Payload::ChannelItemOutcome {
                    channel: channel.to_string(),
                    item,
                    outcome: ended.outcome,
                    reason: (!reason.is_empty()).then(|| reason.clone()),
                    reference: ended.reference,
                });
        }
        match (waiter, sent) {
            (Waiter::Send(ask), _) => (self.to_interpreter)(json!({
                "t": "channel.sent",
                "agent": agent,
                "id": ask,
                "outcome": ended.wire,
                "reason": reason,
                "item": item,
            })),
            (Waiter::Request(id), Sent::Told) => self.tell(
                "channel.settle",
                agent,
                id,
                json!({"outcome": ended.wire, "message": reason}),
            ),
            (Waiter::Request(id), Sent::Moved(state)) => {
                self.tell("channel.requested", agent, id, json!({"state": state}));
            }
            (Waiter::Request(_), Sent::Ended) => {}
        }
        if let Some(giving) = giving {
            self.give(channel, agent, giving);
        }
    }

    /// Begin giving `channel`'s backend what `giving` says, for a request of
    /// `agent`'s.
    fn give(self: &Arc<Self>, channel: &str, agent: &str, giving: Giving) {
        match giving {
            Giving::Withdrawal(asking) => self.withdrawal(asking, agent),
            Giving::Notice(start) => self.notifying(channel, start),
        }
    }

    /// The holder's code cancelled its outbound request `id` on `channel`: its
    /// backend is asked to withdraw it, once its send has been answered.
    fn withdraw(self: &Arc<Self>, channel: &str, id: u64) {
        // No waiter reads an outstanding request's state, and one more withdrawal to drain can
        // only keep `drained` waiting.
        let asked = self.changing(|state| {
            let asked = state
                .find_mut(channel)
                .and_then(|chan| chan.cancel_outgoing(id));
            (asked, false)
        });
        if let Some((asking, agent)) = asked {
            self.withdrawal(asking, &agent);
        }
    }

    /// Ask the backend `asking` names to withdraw its request, `agent`'s,
    /// counted toward the drain already. One its holder cancelled settles on
    /// the answer, or reads uncertain without one; one released is told only,
    /// the request having ended.
    fn withdrawal(self: &Arc<Self>, asking: Asking, agent: &str) {
        let table = Arc::clone(self);
        let agent = agent.to_string();
        tokio::spawn(async move {
            let Asking { withdrawal, calls } = asking;
            let channel = withdrawal.channel.clone();
            let (id, end) = (withdrawal.request.get(), withdrawal.end);
            let answer = table
                .call(&calls, || calls.backend.withdrawn(withdrawal))
                .await;
            let outstanding = table.with(|state| {
                let Some(chan) = state.find_mut(&channel) else {
                    return false;
                };
                chan.withdrawing -= 1;
                // The answer its close row waits on, when the request ended released or closed:
                // this withdrawal, or its holder's cancel's, standing for that end's.
                let released = chan.finished.get(&id).is_some_and(|finished| {
                    matches!(finished.end, RequestEnd::Released | RequestEnd::Closed)
                });
                if answer.is_ok()
                    && released
                    && let Some(at) = chan.at_close(id)
                {
                    at.acknowledged = true;
                }
                let Some(outgoing) = chan.outgoing.get_mut(&id) else {
                    return false;
                };
                if outgoing.state == Out::Withdrawing {
                    outgoing.state = if answer.is_ok() {
                        Out::Ending
                    } else {
                        Out::Doubtful
                    };
                }
                true
            });
            match (end, answer) {
                (WithdrawEnd::Released, Ok(_)) => {}
                (WithdrawEnd::Released, Err(why)) => tracing::warn!(
                    "channel {channel}'s backend did not answer the withdrawal of request {id}, \
                     whose holder is gone: {why}"
                ),
                // Ended meanwhile -- released, closed, settled -- so the answer changes nothing.
                (_, Ok(_)) if !outstanding => {}
                (_, Ok(answer)) => {
                    table.tell("channel.settle", &agent, id, withdrawn_line(answer));
                }
                (_, Err(why)) => {
                    tracing::warn!(
                        "channel {channel}'s backend did not answer the withdrawal of request \
                         {id}, which is uncertain until it is resolved: {why}"
                    );
                    if outstanding {
                        table.tell(
                            "channel.requested",
                            &agent,
                            id,
                            json!({"state": "uncertain"}),
                        );
                    }
                }
            }
        });
    }

    /// `agent`'s outbound request `id` on `channel` ended as `line` says:
    /// recorded, evented, and its backend told what it did not give itself --
    /// a reply that did not fit, the channel's close -- or asked to withdraw it,
    /// when its holder was released or the session closed.
    fn outbound_ended(self: &Arc<Self>, agent: &str, channel: &str, id: u64, line: &Value) {
        let end = RequestEnd::named(line["outcome"].as_str().unwrap_or_default());
        let problems: Vec<String> = line["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|problem| problem.as_str().map(str::to_string))
            .collect();
        let message = said_why(line);
        let closed = self.lifecycle.closed().is_some();
        let request = AgentRequestId::new(id);
        let owed = if matches!(end, RequestEnd::Released | RequestEnd::Closed) {
            Some(Owed::Withdrawn)
        } else if !problems.is_empty() {
            Some(Owed::Told(Settlement::Failed(message.clone())))
        } else if end == RequestEnd::Cancelled && line["by"].as_str() == Some("peer") {
            Some(Owed::Told(Settlement::Cancelled {
                by: CancelledBy::Peer,
                reason: message.clone(),
            }))
        } else {
            None
        };
        // What its backend is owed is begun in the step that ends it, so the drain never finds
        // it ended and that not yet counted.
        let given = self.with(|state| {
            let chan = state.find_mut(channel)?;
            // Owed nothing by a backend never handed it; by one whose send is unanswered, only
            // once it is answered, so the backend has the request before it hears its end.
            let state = chan.outgoing.get(&id).map(|out| out.state);
            let (owed, acknowledged) = match (owed, state) {
                (_, None) => (None, true),
                // Asked to withdraw it already, by its holder's cancel: that withdrawal stands for
                // this one, and its answer is the one the close waits on.
                (Some(Owed::Withdrawn), Some(Out::Withdrawing | Out::Doubtful)) => (None, false),
                (Some(Owed::Withdrawn), Some(Out::Ending)) => (None, true),
                (owed, Some(_)) => {
                    let acknowledged = owed.is_none();
                    (owed, acknowledged)
                }
            };
            let at_close = closed.then_some(ChannelRequestAtClose {
                request,
                direction: Direction::Outbound,
                item: None,
                end,
                acknowledged,
            });
            let (now, later) = match state {
                Some(Out::Sending { .. }) => (None, owed),
                _ => (owed, None),
            };
            let finished = Finished {
                end,
                agent: Some(agent.to_string()),
                settle: line["settle"].as_u64(),
                problems,
                owed: later,
            };
            if !chan.end_request(id, finished, at_close) {
                return None;
            }
            let outgoing = chan.outgoing.remove(&id);
            Some(
                outgoing
                    .zip(now)
                    .map(|(out, owed)| chan.owe(id, agent, (out.item, out.reference), owed)),
            )
        });
        let Some(giving) = given else {
            return;
        };
        self.events
            .for_agent(agent)
            .emit(Payload::ChannelRequestEnded {
                channel: channel.to_string(),
                request,
                outcome: end,
                reason: (!message.is_empty()).then_some(message),
            });
        if let Some(giving) = giving {
            self.give(channel, agent, giving);
        }
    }

    /// A line about an inbound request -- received, ended, or answered after
    /// it ended -- naming its channel, the owner's id for it and the backend's
    /// reference: recorded, and its backend told each end.
    fn asked_line(self: &Arc<Self>, line: &Value) {
        let external = &line["external"];
        let (Some(channel), Some(item), Some(id)) = (
            external["channel"].as_str(),
            external["item"].as_str(),
            line["request"].as_u64(),
        ) else {
            return;
        };
        let agent = line["agent"]
            .as_str()
            .filter(|agent| !agent.is_empty())
            .map(str::to_string);
        let request = AgentRequestId::new(id);
        let reference = || external["reference"].as_str().map(str::to_string);
        let notice = |outcome| Settled {
            channel: channel.to_string(),
            request,
            item: Some(item.to_string()),
            reference: reference(),
            outcome,
        };
        match line["t"].as_str().unwrap_or_default() {
            "agent.received" => self.changing(|state| match state.find_mut(channel) {
                // Received is as open as unread: no waiter is woken for it.
                Some(chan) if !chan.finished.contains_key(&id) => {
                    chan.place(id, item, reference(), Position::Consumed);
                    ((), false)
                }
                _ => ((), false),
            }),
            "agent.ended" => {
                let end = RequestEnd::named(line["outcome"].as_str().unwrap_or_default());
                let outcome = match end {
                    RequestEnd::Accepted => Settlement::Replied(
                        serde_json::from_str(line["json"].as_str().unwrap_or("null"))
                            .unwrap_or(Value::Null),
                    ),
                    RequestEnd::Rejected => Settlement::Failed(text_of(&line["error"])),
                    RequestEnd::Cancelled => Settlement::Cancelled {
                        by: match line["by"].as_str() {
                            Some("owner") => CancelledBy::Owner,
                            Some("peer") => CancelledBy::Peer,
                            _ => CancelledBy::Host,
                        },
                        reason: text_of(&line["message"]),
                    },
                    RequestEnd::Budget | RequestEnd::Limit => Settlement::Cancelled {
                        by: CancelledBy::Host,
                        reason: said_why(line),
                    },
                    RequestEnd::Released => Settlement::Released,
                    RequestEnd::Closed | RequestEnd::Terminated => Settlement::Closed,
                    _ => Settlement::Failed(said_why(line)),
                };
                let closed = self.lifecycle.closed().is_some();
                let start = self.with(|state| {
                    let chan = state.find_mut(channel)?;
                    let finished = Finished {
                        end,
                        agent: agent.clone(),
                        settle: None,
                        problems: Vec::new(),
                        owed: None,
                    };
                    let at_close = closed.then(|| ChannelRequestAtClose {
                        request,
                        direction: Direction::Inbound,
                        item: Some(item.to_string()),
                        end,
                        acknowledged: false,
                    });
                    // Told again, for a settle sent again: it ended once.
                    if !chan.end_request(id, finished, at_close) {
                        return None;
                    }
                    chan.place(id, item, reference(), Position::Settled);
                    // Queued in the step that ends it, so the drain never finds it ended and its
                    // notice not yet queued.
                    Some(chan.push_notice(Notice::Settled(notice(outcome), agent)))
                });
                if let Some(start) = start {
                    self.notifying(channel, start);
                }
            }
            "agent.refused" => {
                let ended = RequestEnd::named(line["ended"].as_str().unwrap_or_default());
                let notice = notice(Settlement::Refused { ended });
                self.queue(channel, Notice::Settled(notice, agent));
            }
            _ => {}
        }
    }

    /// `agent`'s code asked, under `ask`, what became of the operation `line`
    /// names on `channel`: a message's send, by its item id, or an outbound
    /// request's send or withdrawal, by its id. Its backend is asked, and the
    /// answer settles what it can -- a message no longer uncertain, a request
    /// now waiting or ended as the backend says -- before the code is told
    /// it. One that names nothing uncertain is refused, the backend not
    /// asked.
    fn resolve_asked(self: &Arc<Self>, agent: &str, ask: u64, channel: &str, line: &Value) {
        let operation = match line["operation"].as_str() {
            Some("withdrawal") => ChannelOperation::Withdrawal,
            _ => ChannelOperation::Send,
        };
        let decided = self.changing(|state| {
            let Some(chan) = state.find_mut(channel) else {
                return (
                    Err(format!("the session declares no channel {channel:?}")),
                    false,
                );
            };
            let query = chan.query(agent, operation, line);
            if query.is_ok() {
                chan.resolving += 1;
            }
            (query.map(|query| (query, chan.calls())), false)
        });
        let (query, calls) = match decided {
            Ok(decided) => decided,
            Err(why) => return self.tell("channel.resolved", agent, ask, json!({"error": why})),
        };
        let table = Arc::clone(self);
        let agent = agent.to_string();
        tokio::spawn(async move {
            let (resolution, told) = table.resolve(&agent, &calls, &query, operation).await;
            // What the answer settles is told the request's kernel before the code hears the
            // answer, so a status read after it has moved.
            if let (Some((kind, line)), Some(id)) = (told, query.request) {
                table.tell(kind, &agent, id.get(), line);
            }
            let (outcome, reason, reference) = resolution.parts();
            table.tell(
                "channel.resolved",
                &agent,
                ask,
                json!({"outcome": outcome, "reason": reason, "reference": reference}),
            );
        });
    }

    /// Admission has closed, as each of the lifecycle's closers hears: in one
    /// step, the table says so and lists each message whose send is uncertain
    /// in the close's report, so an answer that comes after -- to the agent's
    /// own question still running, or to the drain's -- settles its row; and a
    /// send still waiting for its turn at the backend is refused.
    fn admission_closed(&self) {
        self.with(|state| {
            state.closed = true;
            for chan in &mut state.channels {
                chan.list_doubts();
            }
        });
        self.closing.cancel();
    }

    /// The drain has begun: ask each message's backend, once, what became of
    /// each message uncertain at the close that no answer since has settled.
    /// The drain waits for the answers.
    fn resolve_uncertain(self: &Arc<Self>) {
        // Admission has closed, but the lifecycle may not have told the table yet: an exit's
        // hook still to run, or another caller's close still telling its closers. Hearing it
        // twice changes nothing.
        self.admission_closed();
        let asked: Vec<(String, Query, Calls)> = self.with(|state| {
            let mut asked = Vec::new();
            for chan in &mut state.channels {
                let calls = chan.calls();
                let before = asked.len();
                for row in &chan.uncertain_at_close {
                    if row.outcome == ItemOutcome::Uncertain {
                        let query = Query {
                            channel: chan.definition.name.clone(),
                            item: row.item,
                            request: None,
                            reference: None,
                        };
                        asked.push((row.agent.clone(), query, calls.clone()));
                    }
                }
                chan.resolving += asked.len() - before;
            }
            asked
        });
        for (agent, query, calls) in asked {
            let table = Arc::clone(self);
            tokio::spawn(async move {
                table
                    .resolve(&agent, &calls, &query, ChannelOperation::Send)
                    .await
            });
        }
    }

    /// The owner cancelled inbound request `id` on `name`.
    fn cancel(&self, name: &str, id: AgentRequestId) -> Result<(), SettleError> {
        {
            let state = self.state.borrow();
            let chan = state.find(name).ok_or(SettleError::Unknown(id))?;
            if let Some(finished) = chan.finished.get(&id.get()) {
                return Err(SettleError::Ended(finished.end));
            }
            if !chan.open(id.get()) {
                return Err(SettleError::Unknown(id));
            }
        }
        (self.to_interpreter)(json!({"t": "channel.cancel", "channel": name, "request": id}));
        Ok(())
    }

    /// Begin the owner's `settle` of outbound request `id` on `name`: the
    /// settle's number, which its end names if this settle made it.
    fn settle(&self, name: &str, id: AgentRequestId, settle: Settle) -> Result<u64, SettleError> {
        let number = self
            .settles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        // Whose it is, or how it ended, or neither.
        let found = {
            let state = self.state.borrow();
            state.find(name).and_then(|chan| {
                if let Some(finished) = chan.finished.get(&id.get()) {
                    return Some(Err((finished.end, finished.agent.clone())));
                }
                Some(Ok(chan.outgoing.get(&id.get())?.agent.clone()))
            })
        };
        let agent = match found {
            Some(Ok(agent)) => agent,
            Some(Err((end, agent))) => {
                self.refused_settle(name, id, end, agent.as_deref());
                return Err(SettleError::Ended(end));
            }
            None => return Err(SettleError::Unknown(id)),
        };
        let mut line = settle_line(settle);
        line["by"] = json!("owner");
        line["settle"] = json!(number);
        self.tell("channel.settle", &agent, id.get(), line);
        Ok(number)
    }

    /// The answer to the owner's settle `number` of outbound request `id` on
    /// `name`, once the request has ended.
    async fn settled_by(
        &self,
        name: &str,
        id: AgentRequestId,
        number: u64,
    ) -> Result<(), SettleError> {
        let mut state = self.state.subscribe();
        let finished = tokio::select! {
            found = state.wait_for(|state| {
                state.find(name).is_some_and(|chan| chan.finished.contains_key(&id.get()))
            }) => found.ok().and_then(|state| state.find(name)?.finished.get(&id.get()).cloned()),
            () = self.stopping.cancelled() => None,
        };
        let finished = finished.ok_or(SettleError::Gone)?;
        if finished.settle != Some(number) {
            self.refused_settle(name, id, finished.end, finished.agent.as_deref());
            return Err(SettleError::Ended(finished.end));
        }
        match finished.problems {
            problems if problems.is_empty() => Ok(()),
            problems => Err(SettleError::Rejected(problems)),
        }
    }

    /// The owner's settle of request `id` on `name`, `agent`'s, came after it
    /// ended `end`, and was refused.
    fn refused_settle(&self, name: &str, id: AgentRequestId, end: RequestEnd, agent: Option<&str>) {
        self.events_of(agent).emit(Payload::ChannelRequestRefused {
            channel: name.to_string(),
            request: id,
            ended: end,
        });
    }

    /// Call a channel's backend as `call` does, once one of the permits
    /// `calls` holds is free, within its call timeout and the drain: its
    /// answer, or why there is none.
    async fn call<'a, T>(
        &self,
        calls: &Calls,
        call: impl FnOnce() -> BackendFuture<'a, io::Result<T>>,
    ) -> Result<T, String> {
        let _permit = self.permit(calls).await?;
        self.bounded(calls.bound, call())
            .await?
            .map_err(|e| e.to_string())
    }

    /// One of the permits `calls` holds, once it is free, held while the
    /// call it is for runs; or why none came: the drain ended first.
    async fn permit<'a>(
        &self,
        calls: &'a Calls,
    ) -> Result<tokio::sync::SemaphorePermit<'a>, String> {
        tokio::select! {
            // The permits are never closed.
            permit = calls.permits.acquire() => permit.map_err(|e| e.to_string()),
            () = self.stopping.cancelled() => Err(DRAINED.to_string()),
        }
    }

    /// `call`, a call of a channel's backend, given up on past `bound` or
    /// once the session's drain has ended: its answer, or why there is none.
    async fn bounded<T>(
        &self,
        bound: Duration,
        call: impl Future<Output = T>,
    ) -> Result<T, String> {
        tokio::select! {
            answer = tokio::time::timeout(bound, call) => {
                answer.map_err(|_| format!("the backend did not answer within {bound:?}"))
            }
            () = self.stopping.cancelled() => Err(DRAINED.to_string()),
        }
    }

    /// Tell `agent`'s kernel `kind` of request `id`, with `line`'s fields.
    fn tell(&self, kind: &str, agent: &str, id: u64, mut line: Value) {
        line["t"] = json!(kind);
        line["agent"] = json!(agent);
        line["id"] = json!(id);
        (self.to_interpreter)(line);
    }

    /// The owner closed `name`, saying `reason`.
    fn close(&self, name: &str, reason: &str) {
        let closed = self.changing(|state| match state.find_mut(name) {
            Some(chan) if chan.closed.is_none() => {
                chan.closed = Some(reason.to_string());
                // Its far end is gone: what waits for its turn is never handed over.
                for flight in chan.flights.values() {
                    flight.unsent.cancel();
                }
                (Some(chan.holder.clone()), true)
            }
            _ => (None, false),
        });
        let Some(holder) = closed else {
            return;
        };
        (self.to_interpreter)(json!({"t": "channel.close", "channel": name, "reason": reason}));
        self.events_of(holder.as_deref())
            .emit(Payload::ChannelClosed {
                channel: name.to_string(),
                reason: reason.to_string(),
            });
    }
}

/// What waits on a send's answer: a message's `send`, under the interpreter's
/// ask; or an outbound request, by its id.
#[derive(Debug, Clone, Copy)]
enum Waiter {
    Send(u64),
    Request(u64),
}

impl Waiter {
    /// The request's id, for a request.
    fn request(self) -> Option<u64> {
        match self {
            Self::Send(_) => None,
            Self::Request(id) => Some(id),
        }
    }

    /// The kind of channel it is sent on.
    fn carried_on(self) -> ChannelKind {
        match self {
            Self::Send(_) => ChannelKind::Plain,
            Self::Request(_) => ChannelKind::Request(Direction::Outbound),
        }
    }
}

/// What a send's answer leaves its waiter to be told.
enum Sent {
    /// How the send ended: a message's `send` is told whatever it was, and a
    /// request's holder that it ended unsent.
    Told,
    /// The request waits on its backend, or is uncertain, as the state named
    /// says.
    Moved(&'static str),
    /// The request had ended: nothing.
    Ended,
}

/// What became of a send at its backend.
struct Ended {
    outcome: ItemOutcome,
    /// As the interpreter is told it.
    wire: &'static str,
    reason: Option<String>,
    reference: Option<String>,
    /// Whether its `channel.item.outcome` was evented already: an uncertain
    /// send's is, before its backend is asked to resolve it.
    evented: bool,
}

impl Ended {
    /// What a send's `resolution` makes of it.
    fn resolved(resolution: Resolution) -> Self {
        match resolution {
            Resolution::Accepted { reference } => Self::accepted(reference),
            Resolution::NotAccepted { reason } => Self::not_accepted(reason),
            // A send's resolution is one of the three a send answers.
            other => Self::uncertain(other.parts().1.unwrap_or_default()),
        }
    }

    fn accepted(reference: Option<String>) -> Self {
        Self {
            outcome: ItemOutcome::Accepted,
            wire: "accepted",
            reason: None,
            reference,
            evented: false,
        }
    }

    fn not_accepted(reason: String) -> Self {
        Self {
            outcome: ItemOutcome::NotAccepted,
            wire: "not_accepted",
            reason: Some(reason),
            reference: None,
            evented: false,
        }
    }

    fn refused(wire: &'static str, reason: &str) -> Self {
        Self {
            outcome: ItemOutcome::Refused,
            wire,
            reason: Some(reason.to_string()),
            reference: None,
            evented: false,
        }
    }

    fn uncertain(reason: &str) -> Self {
        Self {
            outcome: ItemOutcome::Uncertain,
            wire: "uncertain",
            reason: Some(reason.to_string()),
            reference: None,
            evented: false,
        }
    }
}

/// What crosses one way under `contract`, as the holder's instructions say it:
/// a typed one with its JSON Schema, `schema`, when the interpreter rendered
/// one.
fn contract_text(contract: &Contract, schema: Option<&str>) -> String {
    match (contract, schema) {
        (Contract::Text, _) => "a `str`".to_string(),
        (Contract::Json, _) => {
            "any JSON value, as the `dict`, `list` or scalar it decodes to".to_string()
        }
        (Contract::Nothing, _) => "nothing".to_string(),
        (Contract::Typed(path), schema) => {
            let (module, name) = path.split_once(':').unwrap_or(("", path));
            let mut text = format!("a `{name}`, the class `{module}` holds");
            if let Some(schema) = schema {
                text.push_str(", whose JSON Schema is:");
                for line in schema.lines() {
                    text.push_str("\n      ");
                    text.push_str(line);
                }
            }
            text
        }
    }
}

/// Why `body`, `size` bytes as JSON, cannot be sent on `channel`, whose
/// contract that way is `sends` -- `schema` compiled for a typed one -- if it
/// cannot.
fn unsendable(
    channel: &str,
    sends: &Contract,
    schema: Option<&jsonschema::Validator>,
    body: &Value,
    size: usize,
) -> Option<String> {
    if size > MESSAGE_MAX {
        return Some(format!(
            "the message is {size} bytes as JSON, past the {MESSAGE_MAX} a channel carries"
        ));
    }
    match (sends, schema) {
        (Contract::Nothing, _) => Some(format!("channel {channel:?} sends nothing")),
        (Contract::Text, _) if !body.is_string() => {
            Some(format!("channel {channel:?} sends str, not that"))
        }
        (Contract::Typed(path), Some(schema)) => schema.validate(body).err().map(|error| {
            // Masked, so the message names the problem and not the value.
            let at = error.instance_path().to_string();
            format!(
                "channel {channel:?} sends {path}: at {}: {}",
                if at.is_empty() { "/" } else { &at },
                error.masked()
            )
        }),
        (Contract::Typed(path), None) => Some(format!(
            "channel {channel:?} sends {path}, whose schema the host does not have"
        )),
        _ => None,
    }
}

/// The text of `value`, a field of a line, or nothing.
fn text_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// Why a request ended, as its end's line says it: its message, else its
/// error's text.
fn said_why(line: &Value) -> String {
    match text_of(&line["message"]) {
        message if message.is_empty() => text_of(&line["error"]),
        message => message,
    }
}

/// The fields of a `channel.settle` line ending an outbound request as
/// `settle` says.
fn settle_line(settle: Settle) -> Value {
    match settle {
        Settle::Reply(value) => json!({"outcome": "replied", "value": value}),
        Settle::Fail(message) => json!({"outcome": "failed", "message": message}),
        Settle::Cancel(reason) => json!({"outcome": "cancelled", "message": reason}),
    }
}

/// The `channel.settle` line ending an outbound request as its backend's
/// `answer` to its withdrawal says.
fn withdrawn_line(answer: WithdrawAnswer) -> Value {
    let settle = match answer {
        WithdrawAnswer::Withdrawn => Settle::Cancel("its far end withdrew it".to_string()),
        WithdrawAnswer::Replied(value) => Settle::Reply(value),
        WithdrawAnswer::Failed(message) => Settle::Fail(message),
    };
    let mut line = settle_line(settle);
    line["by"] = json!("backend");
    line
}

/// What the interpreter's `refusal` of an item pushed on `name` is to its
/// owner.
fn refused(name: &str, refusal: PostRefusal) -> DeliverError {
    match refusal.code.as_deref() {
        Some("full") => DeliverError::Full,
        Some("closed") => DeliverError::Closed(refusal.message),
        Some("unknown") => DeliverError::UnknownChannel(name.to_string()),
        _ if !refusal.problems.is_empty() => DeliverError::Contract(refusal.problems),
        _ => DeliverError::Contract(vec![refusal.message]),
    }
}
