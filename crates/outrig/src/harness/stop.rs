//! What an agent is doing, and why it is at rest, as its owner reads it.
//!
//! The registry of a session's agents keeps, for each child, the state its runner last moved it
//! to and, for a root, the facts of the rounds it ended, and changes them only under its lock. Each change is
//! numbered from one counter the session's agents share, so a read taken later never shows an
//! earlier state, and a wait names the number it has seen. The reads are served from that state
//! alone: no event is consulted, so a subscription that drops events, or none at all, changes
//! nothing read here, and the interpreter is never asked.

use std::collections::VecDeque;
use std::time::SystemTime;

use super::event::AttemptId;

/// How many ended rounds' facts an agent keeps. An owner reads each round's as it ends, so a
/// longer history serves only one that fell behind.
pub(crate) const ROUNDS_KEPT: usize = 16;

/// What an agent is doing: a round open, or why it is at rest.
///
/// A state at rest names the round it belongs to, by the number its `model.round.*` events
/// carry, and the request that decided it, where one did, by its `model.attempt` event's
/// `attempt_id`. The task an agent was given finishing is not a state: completing one is the
/// application's own operation, and nothing here infers it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentState {
    /// Launched, and no round has opened yet.
    Launching,
    /// A round is open.
    #[non_exhaustive]
    Running {
        /// The round.
        round: u32,
    },
    /// The last round's model call failed in a way that may clear -- a server error, a rate
    /// limit, a refused connection, a reply it could not use -- and the agent will try again.
    /// What it holds stays open meanwhile.
    #[non_exhaustive]
    Unavailable {
        /// Why the call failed, as its last request's `model.attempt` event says it, such as
        /// `HTTP 503 Service Unavailable`; the round's error when no request was sent.
        class: String,
        /// When the first of this run of failed rounds failed.
        since: SystemTime,
        /// When the agent tries again.
        next_attempt: SystemTime,
        /// The round that failed.
        round: u32,
        /// The last request the failed call sent.
        attempt: Option<AttemptId>,
    },
    /// The last round ended, and the agent holds nothing: no request waits on it, and none it
    /// took is unanswered.
    #[non_exhaustive]
    Idle {
        /// The round.
        round: u32,
    },
    /// The provider would not answer for what the last round carried: a reply it ended with
    /// `content_filter` or `refusal`, sent again without the turn's results and refused again.
    /// It is not sent a third time. The agent keeps what it holds, and its next round opens
    /// once that changes -- a new request arrives, or one it holds ends; the refused round
    /// taking one is no change.
    #[non_exhaustive]
    Refused {
        /// The round the provider refused.
        round: u32,
        /// The request it refused last.
        attempt: Option<AttemptId>,
    },
    /// The agent's model can answer nothing more, so no round opens: a failure that does not
    /// clear without a change to the configuration -- a key every model refuses, a model that
    /// does not exist -- one that failed the same round five times in a row, one that was no
    /// model call's, or the session's spent token budget. What it holds, and anything sent to it
    /// later, ends failed. Only a release or the session's end changes it.
    #[non_exhaustive]
    Failed {
        /// Why: as [`AgentState::Unavailable`]'s `class` for a model call, or the error itself.
        class: String,
        /// The last round it opened, or `None` when it opened none.
        round: Option<u32>,
        /// The last request the failed call sent, when a model call failed.
        attempt: Option<AttemptId>,
    },
    /// The agent was released: by its owner, or by the session's close.
    Released,
    /// The agent was released before its first round opened.
    Cancelled,
    /// The session's interpreter died under the agent.
    Terminated,
}

impl AgentState {
    /// Whether the agent is at rest: every state but [`AgentState::Launching`] and
    /// [`AgentState::Running`].
    pub fn at_rest(&self) -> bool {
        !matches!(self, Self::Launching | Self::Running { .. })
    }

    /// Whether nothing changes it any more.
    pub(crate) fn is_final(&self) -> bool {
        matches!(self, Self::Released | Self::Cancelled | Self::Terminated)
    }
}

/// What an agent is doing, read at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Status {
    /// The change that put the agent in `state`, by the session's count of changes to its
    /// agents' states: a later change to this agent has a larger number.
    pub seq: u64,
    /// What the agent is doing.
    pub state: AgentState,
    /// The latest round that ended with a reply, which is the round `state` names when that
    /// round ended rather than failed; `None` before one has.
    pub last: Option<RoundFacts>,
}

/// What one round of an agent's said and sent, for an owner deciding what its user sees.
/// Nothing here is delivered anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundFacts {
    /// The round, by the number its `model.round.*` events carry.
    pub round: u32,
    /// The model's closing text: what it said last, less its tool calls.
    pub reply: String,
    /// Why the round was cut short -- the tool-call cap, the token budget -- or `None` when the
    /// model ended it.
    pub stopped: Option<String>,
    /// The final message's reasoning, when it held no text.
    pub reasoning: Option<String>,
    /// The deliberate sends the round made on each external channel, in order. Always empty
    /// for now: external channels are not built yet.
    pub sends: Vec<RoundSend>,
}

impl RoundFacts {
    /// Whether the round sent anything on an external channel.
    pub fn any_send(&self) -> bool {
        !self.sends.is_empty()
    }
}

/// What a round sent on one external channel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoundSend {
    /// The channel's name.
    pub channel: String,
    /// Each item it sent there, by its id, in order.
    pub items: Vec<String>,
}

/// What the registry keeps of one child for its readers: its state, numbered, and its latest
/// rounds' facts.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) seq: u64,
    pub(crate) state: AgentState,
    pub(crate) rounds: VecDeque<RoundFacts>,
}

impl Snapshot {
    /// A child launched as change `seq`.
    pub(crate) fn launching(seq: u64) -> Self {
        Self {
            seq,
            state: AgentState::Launching,
            rounds: VecDeque::new(),
        }
    }

    /// Keep `facts`, the latest round's, dropping the oldest past [`ROUNDS_KEPT`].
    pub(crate) fn keep(&mut self, facts: RoundFacts) {
        if self.rounds.len() == ROUNDS_KEPT {
            self.rounds.pop_front();
        }
        self.rounds.push_back(facts);
    }

    pub(crate) fn status(&self) -> Status {
        Status {
            seq: self.seq,
            state: self.state.clone(),
            last: self.rounds.back().cloned(),
        }
    }
}
