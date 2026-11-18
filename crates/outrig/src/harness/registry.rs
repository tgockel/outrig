//! The session's registry of its external channels: what [`Session::channels`]
//! reads, and what a channel's backend is handed with
//! [`ChannelBackend::positions`].
//!
//! The host keeps it as it hands items across, never from the event stream: a
//! snapshot is read under the channels' lock, so it is of one moment, and each
//! id in it is in one place -- unread, consumed and unacknowledged, received,
//! outstanding, or uncertain.
//!
//! [`Session::channels`]: super::Session::channels
//! [`ChannelBackend::positions`]: super::ChannelBackend::positions

use std::time::Duration;

use super::channels::{ChannelKind, ChannelOperation, Contract, DeliveryId, Wake};
use super::event::{AgentRequestId, MessageId};

/// One external channel, as the session's registry holds it at one moment.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ChannelSnapshot {
    /// The channel's name.
    pub name: String,
    /// What it carries.
    pub kind: ChannelKind,
    /// What may arrive for its holder.
    pub receives: Contract,
    /// The JSON Schema text of [`ChannelSnapshot::receives`], for a typed one.
    pub receives_schema: Option<String>,
    /// What its holder may send.
    pub sends: Contract,
    /// The JSON Schema text of [`ChannelSnapshot::sends`], for a typed one.
    pub sends_schema: Option<String>,
    /// How many items it holds unread, or requests open.
    pub bound: usize,
    /// What an arrival does.
    pub wake: Wake,
    /// The agent holding it, as the interpreter's protocol names it, or `None`
    /// while no agent does.
    pub holder: Option<String>,
    /// The backend's own reference for the channel, as its definition gave it.
    pub reference: Option<String>,
    /// Why the owner closed it, once it has.
    pub closed: Option<String>,
    /// Each item waiting unread in its window -- a message, or an inbound
    /// request not yet received -- in the order it arrived.
    pub unread: Vec<InboundItem>,
    /// Each message its holder's code received whose consumption the backend
    /// has not yet acknowledged, in order: those past
    /// [`ChannelSnapshot::acknowledged`], up to [`ChannelSnapshot::consumed`].
    pub unacknowledged: Vec<InboundItem>,
    /// The consumed watermark: the last message its holder's code received.
    /// Every message before it on the channel was received too.
    pub consumed: Option<InboundItem>,
    /// The acknowledged watermark: the last message whose consumption the
    /// backend acknowledged, every one before it acknowledged too.
    pub acknowledged: Option<InboundItem>,
    /// Each inbound request its holder's code received and has not answered.
    pub received: Vec<ReceivedRequest>,
    /// Each outbound request the backend has not settled.
    pub outstanding: Vec<OutstandingRequest>,
    /// Each message sent whose send is uncertain: the backend may have taken
    /// it. An outbound request whose send is uncertain is in
    /// [`ChannelSnapshot::outstanding`] instead.
    pub uncertain: Vec<UncertainItem>,
    /// What each agent sent on the channel in each of its latest rounds, by
    /// agent and round: the deliberate sends an owner's reply policy reads.
    pub rounds: Vec<SentInRound>,
}

/// An item the session's owner pushed on a channel, by both its ids.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InboundItem {
    /// The id the session's owner gave it.
    pub item: String,
    /// The id OutRig delivered it under: what its holder's code reads as
    /// `delivery.id`.
    pub delivery: DeliveryId,
    /// The backend's own reference for it, as its push gave it.
    pub reference: Option<String>,
}

/// An inbound request its holder's code received and has not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceivedRequest {
    /// The session's id for it.
    pub request: AgentRequestId,
    /// The id the session's owner gave it.
    pub item: String,
    /// The backend's own reference for it, as its push gave it.
    pub reference: Option<String>,
    /// How long ago its holder's code received it.
    pub waited: Duration,
}

/// An outbound request its backend has not settled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutstandingRequest {
    /// The session's id for it: its handle's `id`.
    pub request: AgentRequestId,
    /// The id its send was given, as [`Outbound::item`](super::Outbound::item).
    pub item: MessageId,
    /// The agent that made it.
    pub agent: String,
    /// What it is waiting on.
    pub state: OutboundState,
    /// The backend's reference for it, once its send's acceptance gave one.
    pub reference: Option<String>,
}

/// What an outbound request is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutboundState {
    /// Its send is with the backend, unanswered.
    Sending,
    /// The backend took it: it waits for the backend's settlement.
    Waiting,
    /// Its holder's code cancelled it, and the backend is asked to withdraw
    /// it.
    Withdrawing,
    /// It is not known what became of the operation named: whether the
    /// backend took its send, which
    /// [`resolve_send`](super::ChannelBackend::resolve_send) answers, or what
    /// became of its withdrawal, which
    /// [`resolve_withdrawal`](super::ChannelBackend::resolve_withdrawal)
    /// answers.
    Uncertain(ChannelOperation),
    /// It has ended as its backend said, and its holder is being told.
    Ending,
}

/// A message sent on a channel whose send is uncertain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UncertainItem {
    /// The id OutRig gave it, which its backend was given.
    pub item: MessageId,
    /// The agent that sent it.
    pub agent: String,
    /// Why it is uncertain, as the backend or OutRig last said.
    pub reason: String,
}

/// What one agent sent on a channel in one of its rounds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentInRound {
    /// The agent, as the interpreter's protocol names it.
    pub agent: String,
    /// The round, by the number its `model.round.*` events carry.
    pub round: u32,
    /// Each item it sent, by the id OutRig gave it, in order.
    pub items: Vec<MessageId>,
}

/// A channel's positions, as its backend is handed them with
/// [`ChannelBackend::positions`](super::ChannelBackend::positions).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Positions {
    /// The channel.
    pub channel: String,
    /// The last message its holder's code received: every one before it was
    /// received too.
    pub consumed: Option<InboundItem>,
    /// The last message whose consumption the backend acknowledged: every one
    /// before it was acknowledged too. One past it and up to
    /// [`Positions::consumed`] is to be delivered again after a resume.
    pub acknowledged: Option<InboundItem>,
    /// Each item waiting unread, in the order it arrived: each is to be
    /// delivered again after a resume.
    pub unread: Vec<InboundItem>,
}
