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
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::event::{ItemOutcome, MessageId, Payload, Subject};
use super::lifecycle::Lifecycle;
use super::report::{ChannelClose, SendAtClose};
use super::skills::Catalog;
use super::stop::RoundSend;
use crate::agent::orientation::ExternalText;
use crate::events::Events;
use crate::python::host::{Counts, Interpreter, InterpreterError, MESSAGE_MAX, PostRefusal};
use crate::python::hosted::ToInterpreter;

/// What a [`ChannelBackend`]'s methods return: a future of their answer, which
/// OutRig gives up on past the channel's [`ChannelDefinition::call_timeout`].
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The far end of an external channel: the session owner's own messaging.
///
/// OutRig calls it for every item the holder sends and every item the holder
/// receives, and never the other way: what arrives for the holder, the owner
/// pushes through [`Channel::deliver`]. Each call runs on the session's tokio
/// runtime, at most eight at once per channel, and is dropped -- its future
/// never polled again -- once it has run past the channel's
/// [`ChannelDefinition::call_timeout`]: a send dropped so is
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
}

/// The most items a channel holds unread, and its default bound.
pub const CHANNEL_BOUND_MAX: usize = 256;

/// How long a backend's method has to answer, unless the definition says.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How many of one channel's sends its backend is given at once.
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

impl ChannelDefinition {
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
        }
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
    /// push past it is refused [`DeliverError::Full`].
    #[must_use]
    pub fn bound(mut self, bound: usize) -> Self {
        self.bound = bound;
        self
    }

    /// What an arrival does.
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

    /// How long a call of the backend may run before OutRig gives up on it.
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
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain => "plain",
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
    /// back with the item's [`Consumed`] notice.
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
    /// A request, which owes one: a plain channel refuses it.
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
        delivery: MessageId,
    },
    /// The channel had taken an item of this id already, under `delivery`, and
    /// it is now at `position`: nothing more was queued.
    Repeat {
        /// The id OutRig delivered it under.
        delivery: MessageId,
        /// Where it is.
        position: Position,
    },
}

impl Delivered {
    /// The id the item was delivered under, the first time or this one.
    pub fn delivery(&self) -> MessageId {
        match self {
            Self::Queued { delivery } | Self::Repeat { delivery, .. } => *delivery,
        }
    }
}

/// Where an item a channel took is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Position {
    /// In the window, unread.
    Unread,
    /// Received by the holder's code; the backend has not yet acknowledged
    /// [`ChannelBackend::consumed`].
    Consumed,
    /// Received, and acknowledged.
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

    /// Resolves once the window has room for another item, the channel is
    /// closed, or the session is closing: once a push can be answered other
    /// than [`DeliverError::Full`].
    pub fn room(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut state = self.table.state.subscribe();
        let closing = self.table.closing.clone();
        let name = self.name.clone();
        async move {
            let roomy = state.wait_for(|state| {
                state
                    .find(&name)
                    .is_none_or(|chan| chan.closed.is_some() || chan.unread < chan.bound())
            });
            tokio::select! {
                _ = roomy => {}
                () = closing.cancelled() => {}
            }
        }
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
}

#[derive(Default)]
struct State {
    /// Each channel, in the order the owner declared them.
    channels: Vec<Chan>,
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
    /// The owner's id of each item not yet acknowledged, and the backend's
    /// reference for it, by the id it was delivered under, in that order.
    deliveries: BTreeMap<MessageId, (String, Option<String>)>,
    /// Every item ever queued, and those unread.
    delivered: u64,
    unread: usize,
    /// Each send its backend has not answered, by its item id.
    flights: BTreeMap<MessageId, Flight>,
    /// The items each agent sent in each of its rounds, until the round's
    /// facts read them.
    sent: HashMap<(String, u32), Vec<MessageId>>,
    /// Each send whose outcome came after admission closed.
    after_close: Vec<SendAtClose>,
    /// The items received whose notice the backend has not acknowledged, each
    /// with the agent that received it, in the order they were received, and
    /// whether a task is telling it.
    notices: VecDeque<(MessageId, String)>,
    notifying: bool,
}

impl Chan {
    fn bound(&self) -> usize {
        self.definition.bound
    }
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
    Queued(MessageId, Position),
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
                deliveries: BTreeMap::new(),
                delivered: 0,
                unread: 0,
                flights: BTreeMap::new(),
                sent: HashMap::new(),
                after_close: Vec::new(),
                notices: VecDeque::new(),
                notifying: false,
            })
            .collect();
        let closing = CancellationToken::new();
        let table = Arc::new(Table {
            events: lifecycle.events().clone(),
            lifecycle: Arc::clone(lifecycle),
            catalog,
            interpreter: interpreter.downgrade(),
            to_interpreter: interpreter.relay_line(),
            state: watch::Sender::new(State { channels }),
            rounds: OnceLock::new(),
            closing: closing.clone(),
            stopping: CancellationToken::new(),
        });
        lifecycle.on_close(Arc::new(move |_| closing.cancel()));
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

    /// The channels `agent` holds whose arrivals are announced, as its
    /// announcer counts them: what waits unread, and every item ever queued.
    pub(crate) fn counts_of(&self, agent: &str) -> BTreeMap<String, Counts> {
        self.0
            .state
            .borrow()
            .channels
            .iter()
            .filter(|chan| {
                chan.holder.as_deref() == Some(agent) && chan.definition.wake != Wake::Silent
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
                    kind: definition.kind.to_string(),
                    receives: contract_text(&definition.receives, chan.schemas.0.as_deref()),
                    sends: contract_text(&definition.sends, chan.schemas.1.as_deref()),
                    description: definition.description.clone(),
                    wake: definition.wake,
                }
            })
            .collect()
    }

    /// Each channel `agent` sent on in its round `round`, with the items it
    /// sent, in order: the round's deliberate sends, read once, as the round
    /// ends.
    pub(crate) fn sends_in(&self, agent: &str, round: u32) -> Vec<RoundSend> {
        let key = (agent.to_string(), round);
        self.0.changing(|state| {
            let sends: Vec<RoundSend> = state
                .channels
                .iter_mut()
                .filter_map(|chan| {
                    let items = chan.sent.remove(&key)?;
                    Some(RoundSend::new(&chan.definition.name, items))
                })
                .collect();
            (sends, false)
        })
    }

    /// Resolves once no send is waiting on a backend and every received item's
    /// notice has been acknowledged.
    pub(crate) fn drained(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut state = self.0.state.subscribe();
        async move {
            let _ = state
                .wait_for(|state| {
                    state
                        .channels
                        .iter()
                        .all(|chan| chan.flights.is_empty() && chan.notices.is_empty())
                })
                .await;
        }
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
                    .deliveries
                    .values()
                    .filter(|(item, _)| {
                        matches!(
                            chan.items.get(item),
                            Some(Taken::Queued(_, Position::Unread) | Taken::Pushing)
                        )
                    })
                    .map(|(item, _)| item.clone())
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
                    .filter_map(|(delivery, _)| {
                        chan.deliveries.get(delivery).map(|(id, _)| id.clone())
                    })
                    .collect();
                report.push(ChannelClose {
                    channel: chan.definition.name.clone(),
                    holder: chan.holder.clone(),
                    unread,
                    sends,
                    unacknowledged,
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
                Some(ask) => self.send(&agent, ask, &channel, line["body"].take()),
                None => tracing::warn!("ignored a send with no id: {line}"),
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
            let refused = if let Some(reason) = &chan.closed {
                DeliverError::Closed(reason.clone())
            } else if closing {
                DeliverError::Closed("the session is closing".to_string())
            } else if item.kind != ItemKind::Message {
                DeliverError::Contract(vec![format!(
                    "a {} on a plain channel, which carries messages",
                    item.kind
                )])
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
        let mut line = json!({
            "t": "channel.msg",
            "channel": name,
            "body": item.body,
            "sender": item.sender,
        });
        if let Some(skill) = &item.skill {
            line["skill"] = json!(skill);
        }
        if let Some(correlation) = &item.correlation {
            line["correlation"] = json!(correlation);
        }
        // Recorded before the line is sent, under the lock it is sent under, so the holder's
        // taking it -- which can reach the host ahead of this answer -- finds it.
        let mut delivery = None;
        let answer = self
            .interpreter
            .post_item(line, &mut |id| {
                let id = id.message();
                delivery = Some(id);
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
        let (name, id) = (name.to_string(), item.item.clone());
        tokio::spawn(async move {
            let posted = answer.await;
            let _ = answered.send(table.posted(&name, &id, delivery, posted));
        });
        Ok(waiting)
    }

    /// The interpreter answered for item `id`, pushed on `name` as `delivery`.
    fn posted(
        &self,
        name: &str,
        id: &str,
        delivery: MessageId,
        posted: Result<Result<usize, PostRefusal>, InterpreterError>,
    ) -> Result<Delivered, DeliverError> {
        let refused = match posted {
            Ok(Ok(_)) => None,
            Ok(Err(refusal)) => Some(refused(name, refusal)),
            Err(e) => Some(DeliverError::Closed(e.to_string())),
        };
        let (holder, wakes) = self.with(|state| {
            let Some(chan) = state.find_mut(name) else {
                return (None, false);
            };
            if refused.is_some() {
                chan.items.remove(id);
                chan.deliveries.remove(&delivery);
            } else {
                chan.delivered += 1;
                // The holder may have taken it before this answer came.
                if let Some(taken @ Taken::Pushing) = chan.items.get_mut(id) {
                    *taken = Taken::Queued(delivery, Position::Unread);
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
        Ok(Delivered::Queued { delivery })
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
            let (id, _) = chan.deliveries.get(&delivery)?;
            let taken = chan.items.get_mut(id)?;
            match taken {
                Taken::Queued(_, position @ Position::Unread) => {
                    *position = Position::Consumed;
                    chan.unread -= 1;
                }
                // Taken before the push's answer came: never counted unread.
                Taken::Pushing => *taken = Taken::Queued(delivery, Position::Consumed),
                Taken::Queued(..) => return None,
            }
            chan.notices.push_back((delivery, agent.to_string()));
            let start = !chan.notifying;
            chan.notifying = true;
            Some(start)
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
        if start {
            let table = Arc::clone(self);
            let channel = channel.to_string();
            tokio::spawn(async move { table.notify(&channel).await });
        }
    }

    /// Tell `channel`'s backend of each item received, in order, each until it
    /// acknowledges, while the session lasts.
    async fn notify(&self, channel: &str) {
        let mut wait = NOTICE_RETRY_FIRST;
        loop {
            // The next notice, or, with none left, this task's end: said in the
            // same step, so a notice queued after it starts another.
            let next = self.changing(|state| {
                let Some(chan) = state.find_mut(channel) else {
                    return (None, false);
                };
                let Some((delivery, agent)) = chan.notices.front().cloned() else {
                    chan.notifying = false;
                    return (None, true);
                };
                let (item, reference) = chan.deliveries.get(&delivery).cloned().unwrap_or_default();
                let next = (
                    delivery,
                    agent,
                    item,
                    reference,
                    Arc::clone(&chan.backend),
                    chan.definition.call_timeout,
                );
                (Some(next), false)
            });
            let Some((delivery, agent, item, reference, backend, bound)) = next else {
                return;
            };
            let notice = Consumed {
                channel: channel.to_string(),
                item: item.clone(),
                delivery,
                reference,
            };
            let told = tokio::select! {
                told = tokio::time::timeout(bound, backend.consumed(notice)) => told,
                () = self.stopping.cancelled() => {
                    self.with(|state| {
                        if let Some(chan) = state.find_mut(channel) {
                            chan.notifying = false;
                        }
                    });
                    return;
                }
            };
            match told {
                Ok(Ok(())) => {
                    self.with(|state| {
                        if let Some(chan) = state.find_mut(channel) {
                            chan.notices.pop_front();
                            // Acknowledged: no notice, no report and no take reads it again.
                            chan.deliveries.remove(&delivery);
                            if let Some(Taken::Queued(_, position)) = chan.items.get_mut(&item) {
                                *position = Position::Acknowledged;
                            }
                        }
                    });
                    // The receiver's, though another agent may hold the channel by now.
                    self.events
                        .for_agent(&agent)
                        .emit(Payload::ChannelConsumed {
                            channel: channel.to_string(),
                            item,
                            delivery,
                        });
                    wait = NOTICE_RETRY_FIRST;
                }
                failed => {
                    let why = match failed {
                        Ok(Err(e)) => e.to_string(),
                        _ => format!("it did not answer within {bound:?}"),
                    };
                    tracing::warn!(
                        "channel {channel}'s backend did not take the notice of item {item}; \
                         telling it again in {wait:?}: {why}"
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

    /// `agent`'s code sent `body` on `channel`, waiting on the answer under
    /// `ask`: given an id, recorded with the round it has open, and handed to
    /// the backend, or refused.
    fn send(self: &Arc<Self>, agent: &str, ask: u64, channel: &str, body: Value) {
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
            Ok((
                Arc::clone(&chan.backend),
                Arc::clone(&chan.permits),
                chan.definition.call_timeout,
                chan.definition.sends.to_string(),
            ))
        });
        self.events
            .for_agent(agent)
            .emit_with(|| Payload::MessageSent {
                message: item,
                channel: channel.to_string(),
                from: Subject::agent(agent).to_string(),
                to: BACKEND_END.to_string(),
                sender: None,
                body: text,
            });
        let (backend, permits, bound, body_type) = match decided {
            Ok(decided) => decided,
            Err(refused) => {
                self.finish(agent, ask, channel, item, refused, false);
                return;
            }
        };
        let outbound = Outbound {
            channel: channel.to_string(),
            item,
            kind: ItemKind::Message,
            body_type,
            sender: agent.to_string(),
            correlation: None,
            reference: None,
            body,
        };
        let table = Arc::clone(self);
        let (agent, channel) = (agent.to_string(), channel.to_string());
        tokio::spawn(async move {
            let ended = table
                .handed(&backend, &permits, &unsent, bound, outbound)
                .await;
            table.finish(&agent, ask, &channel, item, ended, true);
        });
    }

    /// Hand `outbound` to `backend` once one of `permits` is free, unless the
    /// session closes, or the channel closes or its sender gives it up --
    /// `unsent` -- first, and say what became of it.
    async fn handed(
        &self,
        backend: &Arc<dyn ChannelBackend>,
        permits: &Arc<Semaphore>,
        unsent: &CancellationToken,
        bound: Duration,
        outbound: Outbound,
    ) -> Ended {
        let permit = tokio::select! {
            biased;
            () = self.closing.cancelled() => {
                return Ended::refused("closing", "the session closed before it was sent");
            }
            () = unsent.cancelled() => return self.unsent(&outbound.channel),
            permit = Arc::clone(permits).acquire_owned() => permit.ok(),
        };
        let Some(_permit) = permit else {
            return Ended::refused("closing", "the session closed before it was sent");
        };
        // Said again with the permit in hand: a close that came as it was granted refuses
        // the send, and one after it finds the send handed.
        if unsent.is_cancelled() {
            return self.unsent(&outbound.channel);
        }
        let answered = tokio::select! {
            answered = tokio::time::timeout(bound, backend.send(outbound)) => answered,
            () = self.stopping.cancelled() => {
                return Ended::uncertain("the session's drain ended before the backend answered");
            }
        };
        match answered {
            Ok(SendAnswer::Accepted { reference }) => Ended {
                outcome: ItemOutcome::Accepted,
                wire: "accepted",
                reason: None,
                reference,
            },
            Ok(SendAnswer::NotAccepted { reason }) => Ended {
                outcome: ItemOutcome::NotAccepted,
                wire: "not_accepted",
                reason: Some(reason),
                reference: None,
            },
            Ok(SendAnswer::Uncertain { reason }) => Ended::uncertain(&reason),
            Err(_) => Ended::uncertain(&format!("the backend did not answer within {bound:?}")),
        }
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

    /// The send of `item` on `channel`, which `agent` waits on under `ask`, has
    /// `ended`: recorded in the report once admission has closed, evented, and
    /// answered. One `in_flight` that the report gave up on already is left as
    /// the report says it.
    fn finish(
        &self,
        agent: &str,
        ask: u64,
        channel: &str,
        item: MessageId,
        ended: Ended,
        in_flight: bool,
    ) {
        let closed = self.lifecycle.closed().is_some();
        let live = self.with(|state| {
            let Some(chan) = state.find_mut(channel) else {
                return !in_flight;
            };
            if in_flight && chan.flights.remove(&item).is_none() {
                return false;
            }
            if closed {
                chan.after_close.push(SendAtClose {
                    item,
                    agent: agent.to_string(),
                    outcome: ended.outcome,
                    reason: ended.reason.clone(),
                });
            }
            true
        });
        if !live {
            return;
        }
        let reason = ended.reason.unwrap_or_default();
        self.events
            .for_agent(agent)
            .emit(Payload::ChannelItemOutcome {
                channel: channel.to_string(),
                item,
                outcome: ended.outcome,
                reason: (!reason.is_empty()).then(|| reason.clone()),
                reference: ended.reference,
            });
        (self.to_interpreter)(json!({
            "t": "channel.sent",
            "agent": agent,
            "id": ask,
            "outcome": ended.wire,
            "reason": reason,
            "item": item,
        }));
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

/// What became of a send at its backend.
struct Ended {
    outcome: ItemOutcome,
    /// As the interpreter is told it.
    wire: &'static str,
    reason: Option<String>,
    reference: Option<String>,
}

impl Ended {
    fn refused(wire: &'static str, reason: &str) -> Self {
        Self {
            outcome: ItemOutcome::Refused,
            wire,
            reason: Some(reason.to_string()),
            reference: None,
        }
    }

    fn uncertain(reason: &str) -> Self {
        Self {
            outcome: ItemOutcome::Uncertain,
            wire: "uncertain",
            reason: Some(reason.to_string()),
            reference: None,
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
