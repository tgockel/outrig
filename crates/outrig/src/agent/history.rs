//! The conversation, kept whole, and what of it each model call is sent.
//!
//! The **store** is every turn the agent's rounds have committed, in order,
//! each under an id that never changes. A turn is one model call and the tool
//! results it asked for: the span that keeps a tool call beside its result, so
//! a conversation cut between turns needs no repair. Each turn is mirrored into
//! the interpreter as it commits, where the agent's code reads it as
//! `runtime.history.turns` -- ordinary Python data, which costs no context to
//! scan, since only what the code prints is seen.
//!
//! The **view** is what a model call is sent: the round in progress, every
//! turn the agent promoted, and the conversation's first rounds and its most
//! recent ones, held to the call's [`Budget`]. While that fits, it is sent
//! whole. Once it does not, it is cut: chosen again in half the room beside
//! the call's prompt, where the default window goes first, from its oldest
//! turn, then promotions, oldest first, then the round's own earlier turns,
//! and last the turn that opens each first round and the round in progress
//! ([`Window`]). Each later call of the round is sent what the cut kept and
//! every turn committed since, so each request begins with the one before it,
//! which is all a provider's prompt cache can match, until that no longer
//! fits and the view is cut again ([`Store::view`]). The latest turn, whose
//! results the call answers, is never left out; when it cannot fit on its
//! own, the call is not made ([`TooLarge`]). For a provider that requires the
//! user's and the model's turns to alternate, the turns that would put one
//! role after itself are withheld as well ([`Store::alternate`]). A turn whose
//! results a provider refused a call for is sent with a notice in place of
//! each result ([`Refused`]), while the store keeps them whole.
//!
//! To Anthropic, a call also leaves out the thinking a change to what came
//! before it made stale, and thinking no signature of Anthropic's vouches for
//! ([`Sending`]). Anthropic's newer models bind each thinking block to
//! everything sent before it, and refuse one sent back after that changed.
//! From the first turn whose earlier context a call changed, on, each block
//! stays out for good; those before it, and those produced since, still go.
//! rig would send what is left out as the model's own text instead, which
//! says the thinking aloud in the model's turn.
//!
//! Each call's [`Manifest`] records what it carried and why, which is the only
//! answer to what a model had in front of it: a promotion asks, and the window,
//! the budget, and the store's order decide.
//!
//! `plan/phase/0003-python/history.md` designs both.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig::completion::Message;
use rig::completion::message::{AssistantContent, ToolCall, ToolResultContent, UserContent};
use serde_json::{Value, json};

use super::budget::{self, Budget, Scale, Wire};
use crate::config::RoleAlternation;
use crate::events;
use crate::harness::event::{self, CallId, Payload, WindowSource};
use crate::python::host::{ContextChange, Interpreter};

/// Which earlier rounds a model call is sent, besides the turns the agent
/// promoted, before the budget has its say. The round in progress is always
/// chosen.
///
/// When what is chosen does not fit, the budget keeps the turn that opens each
/// first round and the round in progress longest after the turn the call
/// answers: the agent read what the user asked there. The rest of the window
/// goes first, the first rounds with the recent ones, oldest first, so a first
/// round longer than the room keeps its end -- where its work was done --
/// rather than its beginning. See [`Store::select`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// The conversation's first rounds, where the user said what they wanted.
    /// At least one, so what is sent always opens where the conversation did.
    pub(crate) first: u32,
    /// The rounds just before the one in progress.
    pub(crate) recent: u32,
}

impl Window {
    pub(crate) const DEFAULT: Window = Window {
        first: 2,
        recent: 6,
    };
}

impl Default for Window {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a turn was chosen for a call. Each is one reason only: a turn that is
/// both promoted and in the window is chosen once, as the stronger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The turn whose results the call answers. Never left out.
    Latest,
    /// An earlier turn of the round in progress.
    Round,
    /// A turn the agent promoted.
    Promoted,
    /// A turn of the conversation's first rounds.
    First,
    /// A turn of the rounds just before this one.
    Recent,
}

/// Which side of the conversation a message is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    fn of(message: &Message) -> Option<Role> {
        match message {
            Message::User { .. } => Some(Role::User),
            Message::Assistant(_) => Some(Role::Assistant),
            Message::System { .. } => None,
        }
    }
}

/// Two messages of one role in a row in the conversation a call is sent, where
/// its view left turns out between them: two of the model's replies, or a
/// prompt after a prompt or tool results the model never answered.
///
/// That is the conversation as rig holds it, not necessarily what the wire
/// repeats. Anthropic's API carries tool results in user messages and merges
/// such a pair; OpenAI's carries them as `tool` messages, so a prompt after
/// results is no repeat there, and it accepts two replies in a row. A provider
/// or gateway that requires turns to alternate -- or turns results back into
/// user messages -- may refuse the call, unless its row says it is
/// [`RoleAlternation::Strict`], when the view withholds what would make a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Adjacent {
    /// The turn that opens on the role the one before it ended on, or `None`
    /// for the round's opening, sent after it.
    pub(crate) turn: Option<usize>,
    pub(crate) role: Role,
}

impl fmt::Display for Adjacent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = match self.turn {
            Some(turn) => format!("turn {turn}"),
            None => "the round's opening".to_string(),
        };
        match self.role {
            Role::Assistant => write!(f, "two of the model's replies in a row, where {at} begins"),
            Role::User => write!(
                f,
                "a prompt right after a prompt or tool results the model never answered, where \
                 {at} begins"
            ),
        }
    }
}

/// What one model call was sent of the conversation, and why: the ids of the
/// turns it carried in the order it carried them, what the budget left out,
/// and what the budget was. With the store, it rebuilds the call's history
/// exactly -- the round's opening included, which is carried here while no
/// turn holds it yet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Manifest {
    /// This call's place among the agent's model calls, from 0.
    pub(crate) call: u64,
    /// The round the call belongs to.
    pub(crate) round: u32,
    /// What the call was held to, and whose it was.
    pub(crate) budget: Budget,
    /// The estimated tokens of the whole request: the conversation carried,
    /// the system prompt, and the tool definitions.
    pub(crate) estimate: u64,
    /// The turns sent, oldest first, each once.
    pub(crate) carried: Vec<(usize, Why)>,
    /// The turns chosen but not sent because they did not fit, oldest first.
    pub(crate) evicted: Vec<(usize, Why)>,
    /// The turns chosen and fitting that the call was not sent so that the
    /// user's and the model's turns alternate, for a provider that requires
    /// it: see [`Store::alternate`]. Oldest first; empty for any other.
    pub(crate) withheld: Vec<(usize, Why)>,
    /// The round's opening, when it was the call's prompt: on a round's first
    /// call, before any turn has taken it.
    pub(crate) opening: Option<Message>,
    /// Where one role follows itself in the conversation sent, whether or not
    /// the wire repeats it. For a provider that requires turns to alternate,
    /// only where the turn the call answers opens on a reply and nothing before
    /// it ends on the user's side, which no withholding could clear.
    pub(crate) adjacent: Vec<Adjacent>,
    /// The parts of the carried turns the call's provider cannot take, which
    /// it was not sent: see [`Wire`]. Oldest first.
    pub(crate) left_out: Vec<LeftOut>,
    /// The carried turns whose results it was sent a notice in place of, each
    /// with the notice: see [`Refused`]. Oldest first.
    pub(crate) refused: Vec<(usize, String)>,
    /// The room, in estimated tokens, the view was cut down to, on the call
    /// that cut it: see [`Store::view`].
    pub(crate) cut: Option<u64>,
}

/// One part of a carried turn a call was not sent: in turn `turn`, the
/// `part`-th part of its `message`-th message, each counted from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeftOut {
    pub(crate) turn: usize,
    pub(crate) message: usize,
    pub(crate) part: usize,
}

/// How a call is sent each turn's replies: in its provider's protocol, and,
/// to Anthropic, without the thinking of a turn in `stale` -- one a later call
/// changed what came before, which the API would refuse as bound to another
/// conversation. Leaving out the first such block and every one after it is
/// what `drop_block` does for one request; recorded in the store, it holds
/// for every later one, and a block produced since stays valid.
#[derive(Clone, Copy)]
struct Sending<'a> {
    wire: Wire,
    stale: &'a BTreeSet<usize>,
}

impl Sending<'_> {
    /// Whether a call sends `part` of a reply in turn `turn`.
    fn takes(self, turn: usize, part: &AssistantContent) -> bool {
        self.wire.takes(part, self.stale.contains(&turn))
    }

    /// Whether a call sends any of `message`, of turn `turn`: a reply none of
    /// whose parts it takes is left out whole, and anything else goes.
    fn keeps(self, turn: usize, message: &Message) -> bool {
        match message {
            Message::Assistant(reply) => reply.content.iter().any(|part| self.takes(turn, part)),
            _ => true,
        }
    }
}

/// Turn `turn`'s `messages` as a call is sent them: each part of a reply
/// `sending` does not take left out, and pushed onto `left_out`, and a reply
/// left with nothing left out with it. The turn itself is not changed.
fn fit(
    sending: Sending,
    turn: usize,
    messages: &[Message],
    left_out: &mut Vec<LeftOut>,
) -> Vec<Message> {
    let mut sent = Vec::with_capacity(messages.len());
    for (message, original) in messages.iter().enumerate() {
        let Message::Assistant(reply) = original else {
            sent.push(original.clone());
            continue;
        };
        if !sending.keeps(turn, original) {
            // A reply of nothing the call takes made no tool call, so leaving
            // it out whole leaves no result unanswered.
            left_out.extend(reply.content.iter().enumerate().map(|(part, _)| LeftOut {
                turn,
                message,
                part,
            }));
            continue;
        }
        if reply.content.iter().all(|part| sending.takes(turn, part)) {
            sent.push(original.clone());
            continue;
        }
        let mut kept = Vec::new();
        for (part, content) in reply.content.iter().enumerate() {
            if sending.takes(turn, content) {
                kept.push(content.clone());
            } else {
                left_out.push(LeftOut {
                    turn,
                    message,
                    part,
                });
            }
        }
        // Kept has a part, since the call keeps the reply. The reply's origin
        // and stop go with it: they are what lets its provider replay what is
        // kept as its own.
        sent.push(Message::Assistant(reply.clone().with_content(kept)));
    }
    sent
}

/// The roles turn `turn`'s `messages` open and end on as a call is sent them
/// -- what [`fit`] would leave, without building it -- or `None` when it
/// would leave nothing.
fn ends(sending: Sending, turn: usize, messages: &[Message]) -> Option<(Role, Role)> {
    let mut roles = messages
        .iter()
        .filter(|message| sending.keeps(turn, message))
        .filter_map(Role::of);
    let first = roles.next()?;
    Some((first, roles.next_back().unwrap_or(first)))
}

/// A provider's refusal of a call for what it carried, made of a reply it
/// ended with a finish reason such as `content_filter`. The results of
/// `turn`, the turn the call answered, are the one thing that call carried no
/// earlier one had, so each is sent as a [`notice`] in their place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub(crate) turn: usize,
    pub(crate) finish_reason: String,
}

/// What each result of turn `turn` is sent as, once a provider refused a call
/// that carried them with `finish_reason`: why, and where they are, so the
/// model neither works blind nor repeats what it ran to see them again.
fn notice(turn: usize, finish_reason: &str) -> String {
    format!(
        "[outrig: not sent -- the model's provider refused a request that carried this turn's \
         results (finish reason {finish_reason}), so each call is sent this note in their place. \
         They are kept whole in runtime.history.turns[{turn}]: work with them there in code, and \
         print only what you need of them.]"
    )
}

/// `messages`, with what each tool result holds replaced by `notice`. Every
/// result stays beside the call it answers, in the message and the part it was
/// in, so the roles, and what a [`LeftOut`] names, are as they were.
fn without_results(mut messages: Vec<Message>, notice: &str) -> Vec<Message> {
    for message in &mut messages {
        let Message::User { content } = message else {
            continue;
        };
        for part in content.iter_mut() {
            if let UserContent::ToolResult(result) = part {
                result.content = vec![ToolResultContent::text(notice)];
            }
        }
    }
    messages
}

/// Turn `turn`'s `messages` as the call `left_out` describes was sent them:
/// each part it names left out, and a message all of whose parts it names
/// left out with them. What [`fit`] decided, rebuilt from the record alone.
#[cfg(test)]
pub(crate) fn as_sent(turn: usize, messages: &[Message], left_out: &[LeftOut]) -> Vec<Message> {
    let named = |message: usize, part: usize| {
        left_out.contains(&LeftOut {
            turn,
            message,
            part,
        })
    };
    messages
        .iter()
        .enumerate()
        .filter_map(|(message, original)| match original {
            Message::Assistant(reply) => {
                let kept: Vec<_> = reply
                    .content
                    .iter()
                    .enumerate()
                    .filter(|&(part, _)| !named(message, part))
                    .map(|(_, content)| content.clone())
                    .collect();
                (!kept.is_empty()).then(|| Message::Assistant(reply.clone().with_content(kept)))
            }
            other => Some(other.clone()),
        })
        .collect()
}

impl Manifest {
    /// As the event log records it, for model call `call`.
    fn event(&self, call: CallId) -> event::ModelCall {
        let chosen = |turns: &[(usize, Why)]| {
            turns
                .iter()
                .map(|&(turn, why)| event::Chosen {
                    turn: turn as u64,
                    why: why.public(),
                })
                .collect()
        };
        event::ModelCall {
            call: self.call,
            call_id: call,
            round: self.round,
            budget: event::CallBudget {
                model: self.budget.model.clone(),
                window: self.budget.window,
                window_source: self.budget.window_source,
                reserve: self.budget.reserve,
                overhead: self.budget.overhead,
                estimate_scale: self.budget.scale.ratio(),
                max_tokens: self.budget.max_tokens,
                role_alternation: self.budget.alternation,
            },
            estimate: self.estimate,
            carried: chosen(&self.carried),
            evicted: chosen(&self.evicted),
            withheld: chosen(&self.withheld),
            opening: self.opening.as_ref().map(rig_json),
            adjacent: self
                .adjacent
                .iter()
                .map(|adjacent| event::Repeat {
                    turn: adjacent.turn.map(|turn| turn as u64),
                    role: match adjacent.role {
                        Role::User => event::Role::User,
                        Role::Assistant => event::Role::Assistant,
                    },
                })
                .collect(),
            left_out: self
                .left_out
                .iter()
                .map(
                    |&LeftOut {
                         turn,
                         message,
                         part,
                     }| event::LeftOut {
                        turn: turn as u64,
                        message: message as u64,
                        part: part as u64,
                    },
                )
                .collect(),
            refused: self
                .refused
                .iter()
                .map(|(turn, notice)| event::RefusedTurn {
                    turn: *turn as u64,
                    notice: notice.clone(),
                })
                .collect(),
            cut: self.cut,
        }
    }
}

impl Why {
    /// As an event names it.
    fn public(self) -> event::Why {
        match self {
            Why::Latest => event::Why::Latest,
            Why::Round => event::Why::Round,
            Why::Promoted => event::Why::Promoted,
            Why::First => event::Why::First,
            Why::Recent => event::Why::Recent,
        }
    }
}

/// `message` as rig encodes it, which is how an event carries a message: no
/// rig type crosses the public surface, and the form can change with rig.
pub(crate) fn rig_json(message: &Message) -> serde_json::Value {
    serde_json::to_value(message).expect("rig's messages encode as JSON")
}

/// A call that was not made, because what it answers would not fit on its own.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TooLarge {
    /// The turn, or `None` for the round's opening.
    pub(crate) turn: Option<usize>,
    pub(crate) round: u32,
    /// Its estimated tokens.
    pub(crate) estimate: u64,
    pub(crate) budget: Budget,
}

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Budget {
            model,
            window,
            window_source,
            reserve,
            overhead,
            ..
        } = &self.budget;
        match self.turn {
            Some(turn) => write!(
                f,
                "turn {turn} of round {} -- the model's last call and what it returned -- is \
                 about {} tokens",
                self.round, self.estimate
            )?,
            None => write!(
                f,
                "round {}'s opening is about {} tokens",
                self.round, self.estimate
            )?,
        }
        write!(
            f,
            ", and a call to {model} has room for about {}: a {window}-token context window",
            self.budget.room()
        )?;
        match window_source {
            WindowSource::Configured => {}
            WindowSource::Provider => write!(f, " (reported by {model}'s provider)")?,
            WindowSource::Assumed => {
                write!(f, " (assumed: [models.{model}] sets no context-window)")?
            }
        }
        write!(
            f,
            ", less {reserve} reserved for the reply and about {overhead} for the system prompt \
             and tool definition"
        )?;
        if self.budget.scale != Scale::RULE {
            let ratio = self.budget.scale.ratio();
            write!(
                f,
                ", with the window and the reserve counted down {ratio:.2}x, since {model}'s \
                 provider has reported requests larger than OutRig estimated them"
            )?;
        }
        write!(
            f,
            ". The turn stays in runtime.history, and later calls leave it out. "
        )?;
        if *window_source != WindowSource::Configured {
            write!(
                f,
                "Set [models.{model}].context-window to the model's own window, or lower \
                 tool-result-max, to send a turn this large."
            )
        } else {
            f.write_str("Lower tool-result-max or max-tokens to send a turn this large.")
        }
    }
}

/// An error so a failover chain can give it as the reason a candidate it
/// moved to could not be sent the call.
impl std::error::Error for TooLarge {}

/// Where a test's observer of each manifest is put, for the store to find.
#[cfg(test)]
type ObserverSlot<T> = Arc<Mutex<Option<Box<dyn Fn(&T) + Send + Sync>>>>;

/// The agent's conversation. Cheap to clone; every clone is the one store.
#[derive(Clone)]
pub(crate) struct History {
    store: Arc<Mutex<Store>>,
    /// Where each turn is mirrored.
    interpreter: Interpreter,
    #[cfg(test)]
    manifests: ObserverSlot<Manifest>,
}

impl History {
    /// A store for `interpreter`'s agent, whose model calls are chosen from
    /// with `window`. The agent's promotions and demotions reach it as the
    /// interpreter reports them.
    pub(crate) fn new(interpreter: Interpreter, window: Window) -> Self {
        let store = Arc::new(Mutex::new(Store {
            window,
            ..Store::default()
        }));
        let changed = Arc::clone(&store);
        interpreter.on_context(move |change| lock(&changed).change(change));
        Self {
            store,
            interpreter,
            #[cfg(test)]
            manifests: ObserverSlot::default(),
        }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// Where the agent's events are recorded.
    pub(crate) fn events(&self) -> &events::Events {
        self.interpreter.events()
    }

    /// Call `observer` with the manifest of each model call, as the call is
    /// about to be made. Replaces any earlier observer.
    #[cfg(test)]
    pub(crate) fn on_manifest(&self, observer: impl Fn(&Manifest) + Send + Sync + 'static) {
        *lock(&self.manifests) = Some(Box::new(observer));
    }

    /// Start a round, which opens on the line `line` writes given how many
    /// turns the round's first call will not be sent, held to `budget`.
    /// Returns that line as a message: both the first thing rig is handed and
    /// what the round's first turn begins with.
    ///
    /// The count is exact. It changes the line's length, and the length what
    /// fits, so the opening is costed at the longest line it can be -- the one
    /// counting every turn -- both here and when its first call is assembled.
    pub(crate) fn open_round(&self, budget: &Budget, line: impl Fn(usize) -> String) -> Message {
        self.store().open_round(budget, line)
    }

    /// The number of the round in progress, or of the last one if none is. A
    /// round is numbered as its first turn commits, so one that ends having
    /// committed none leaves its number to the next.
    pub(crate) fn round(&self) -> u32 {
        self.store().prompt().0
    }

    /// The number the next round will have, while none is in progress.
    pub(crate) fn next_round(&self) -> u32 {
        self.store().last_round() + 1
    }

    /// Start a round that `opening` opens, costed at its own size.
    #[cfg(test)]
    pub(crate) fn begin_round(&self, opening: Message) {
        self.store().begin_round(opening);
    }

    /// Commit `messages` as the next turn, and mirror it into the interpreter.
    /// An `incomplete` turn is one the round ended while its calls ran, so
    /// some of their results are OutRig's note saying so rather than what the
    /// code did.
    pub(crate) fn commit(&self, messages: Vec<Message>, incomplete: bool) {
        self.commit_in(&mut self.store(), messages, incomplete);
    }

    /// End the round in progress. An opening no turn began with -- the model's
    /// first reply was empty -- was sent all the same, and is a turn of its
    /// own.
    pub(crate) fn finish_round(&self) {
        let mut store = self.store();
        if store.opening.is_some() {
            self.commit_in(&mut store, Vec::new(), false);
        }
    }

    /// Under the store's lock, so turns reach the interpreter, and the event
    /// log, in id order.
    fn commit_in(&self, store: &mut Store, messages: Vec<Message>, incomplete: bool) {
        let (id, turn) = store.commit(messages);
        self.interpreter
            .events()
            .emit_with(|| Payload::TurnCommitted {
                turn: id as u64,
                round: turn.round,
                incomplete,
                messages: turn.messages.iter().map(rig_json).collect(),
            });
        self.interpreter
            .push_turn(id as u64, mirror(turn.round, &turn.messages, incomplete));
    }

    /// What the next model call of the round in progress is sent, held to
    /// `budget`: the conversation's messages, oldest first, ending with the
    /// call's prompt, and the call's manifest. The prompt is the round's
    /// opening on its first call, and on every later one the last message of
    /// the latest turn, which the round has committed by then.
    ///
    /// The manifest is recorded under `call`, the model call it is assembled
    /// for. `Err` when the prompt's turn cannot fit on its own. The call is
    /// not made, and nothing is counted.
    pub(crate) fn assemble(
        &self,
        budget: &Budget,
        call: CallId,
    ) -> Result<(Vec<Message>, Manifest), TooLarge> {
        self.assemble_with(budget, call, None)
    }

    /// [`Self::assemble`], for the call a provider has just refused for what it
    /// carried: each result of the turn it answers, `refused.turn`, is sent as
    /// a notice in its place. Recorded as the call is sent again, before its
    /// manifest. The store keeps the results, and only this view leaves them
    /// out until [`Self::withhold`].
    pub(crate) fn assemble_without(
        &self,
        budget: &Budget,
        call: CallId,
        refused: &Refused,
    ) -> Result<(Vec<Message>, Manifest), TooLarge> {
        self.assemble_with(budget, call, Some(refused))
    }

    fn assemble_with(
        &self,
        budget: &Budget,
        call: CallId,
        refused: Option<&Refused>,
    ) -> Result<(Vec<Message>, Manifest), TooLarge> {
        let (sent, manifest) = self.store().assemble(budget, refused)?;
        // Outside the store's lock: an observer may do anything.
        #[cfg(test)]
        if let Some(observer) = &*lock(&self.manifests) {
            observer(&manifest);
        }
        let events = self.interpreter.events();
        if let Some(Refused {
            turn,
            finish_reason,
        }) = refused
        {
            // Said beside the retry lines, as a move to another model is.
            tracing::warn!(
                "model {} refused the call for what it carried (finish reason {finish_reason}); \
                 sending it again without turn {turn}'s results",
                budget.model
            );
            events.emit(Payload::ModelWithheld {
                model: budget.model.clone(),
                call_id: call,
                turn: *turn as u64,
                finish_reason: finish_reason.clone(),
            });
        }
        events.emit_with(|| Payload::ModelCall(manifest.event(call)));
        tracing::debug!(
            call = manifest.call,
            round = manifest.round,
            estimate = manifest.estimate,
            carried = manifest.carried.len(),
            evicted = manifest.evicted.len(),
            withheld = manifest.withheld.len(),
            cut = manifest.cut,
            "assembled a model call's view"
        );
        Ok((sent, manifest))
    }

    /// Send every later call the notice `refused` names in place of each of
    /// its turn's results: a call sent so was answered.
    pub(crate) fn withhold(&self, refused: Refused) {
        self.store().withhold(&refused);
    }

    /// The messages `manifest`'s call was sent, its prompt included.
    #[cfg(test)]
    pub(crate) fn reconstruct(&self, manifest: &Manifest) -> Vec<Message> {
        self.store().reconstruct(manifest)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.store().turns.len()
    }

    #[cfg(test)]
    pub(crate) fn set_window(&self, window: Window) {
        self.store().window = window;
    }

    /// Turn `id`'s estimated tokens.
    #[cfg(test)]
    pub(crate) fn tokens_of(&self, id: usize) -> u64 {
        self.store().turns[id].tokens
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The store itself, apart from the interpreter it is mirrored into.
#[derive(Default)]
struct Store {
    /// Every committed turn, oldest first. A turn's id is its index.
    turns: Vec<Turn>,
    /// The ids of the turns the agent promoted, until it demotes them.
    promoted: BTreeSet<usize>,
    /// Where the round in progress begins.
    start: usize,
    /// The line that opened the round in progress, until the round's first
    /// turn commits and begins with it.
    opening: Option<Opening>,
    window: Window,
    /// How many model calls have been assembled.
    calls: u64,
    /// The turns whose results every call is sent a notice in place of: see
    /// [`Refused`].
    refused: BTreeMap<usize, Withheld>,
    /// What the round in progress's latest cut left out, which every later
    /// call of the round leaves out too, until a promotion or a demotion
    /// changes what is chosen: see [`Store::view`]. Empty before a cut.
    cut_out: BTreeSet<usize>,
    /// The turns whose thinking a call to Anthropic leaves out: a later call
    /// changed what came before each. Only grows, since a block put back
    /// would break those produced while it was gone. See [`Sending`].
    stale: BTreeSet<usize>,
    /// The turns the latest call was sent anything of, oldest first: what
    /// the next is checked against for a change before its end. Whatever
    /// model it was for, and whether or not it was answered, which only
    /// makes more stale.
    last: Vec<usize>,
}

/// A turn whose results every call is sent a notice in place of.
struct Withheld {
    notice: String,
    /// The turn's estimated tokens as it is sent so, which a view counts in
    /// place of its own: a notice can be longer than a short result.
    tokens: u64,
}

struct Turn {
    round: u32,
    messages: Vec<Message>,
    /// Its estimated tokens, counted once, as it commits.
    tokens: u64,
}

struct Opening {
    message: Message,
    tokens: u64,
}

/// A call's prompt, which it answers: on a round's first call the opening,
/// which no turn holds yet, and on every later one the latest turn's results.
#[derive(Clone, Copy)]
enum Prompt {
    Opening { tokens: u64 },
    Turn(usize),
}

impl Prompt {
    fn turn(self) -> Option<usize> {
        match self {
            Prompt::Opening { .. } => None,
            Prompt::Turn(id) => Some(id),
        }
    }
}

/// Turns by id, each with why it was chosen, oldest first.
type Chosen = Vec<(usize, Why)>;

/// Which turns a call carries, which the budget left out, and which were
/// withheld so the roles alternate, each by id.
#[derive(Default)]
struct Selection {
    carried: Chosen,
    evicted: Chosen,
    /// Empty until [`Store::view`] has had its say, and for a provider that
    /// does not require the roles to alternate.
    withheld: Chosen,
    /// The estimated tokens of what is carried, the prompt included.
    tokens: u64,
    /// The room the selection was cut down to, when it was a cut.
    cut: Option<u64>,
    /// The turns whose thinking a call to Anthropic leaves out, those this
    /// call makes stale included: see [`Store::spoil`]. Empty until
    /// [`Store::view`] has had its say.
    stale: BTreeSet<usize>,
}

impl Store {
    /// See [`History::open_round`].
    fn open_round(&mut self, budget: &Budget, line: impl Fn(usize) -> String) -> Message {
        let allowance = budget::tokens([&Message::user(line(self.turns.len()))]);
        // As its first call will be: see `Store::assemble`.
        let budget = budget.making_room_for(allowance);
        let omitted = self.omitted(&budget, allowance);
        let opening = Message::user(line(omitted));
        self.begin(opening.clone(), allowance);
        opening
    }

    #[cfg(test)]
    fn begin_round(&mut self, opening: Message) {
        let tokens = budget::tokens([&opening]);
        self.begin(opening, tokens);
    }

    /// Start a round on `opening`, costed at `tokens` until its first turn
    /// commits. Its first call is chosen afresh.
    fn begin(&mut self, opening: Message, tokens: u64) {
        self.cut_out.clear();
        self.start = self.turns.len();
        self.opening = Some(Opening {
            message: opening,
            tokens,
        });
    }

    /// Commit `messages` as the next turn, after the round's opening if this
    /// is its first. A round is numbered as that turn commits, so one that
    /// commits none leaves no gap.
    fn commit(&mut self, mut messages: Vec<Message>) -> (usize, &Turn) {
        let mut round = self.last_round();
        if let Some(opening) = self.opening.take() {
            messages.insert(0, opening.message);
            round += 1;
        }
        let id = self.turns.len();
        self.turns.push(Turn {
            round,
            tokens: budget::tokens(&messages),
            messages,
        });
        (id, &self.turns[id])
    }

    fn last_round(&self) -> u32 {
        self.turns.last().map_or(0, |turn| turn.round)
    }

    /// Apply a promotion or a demotion. A turn the store does not hold is not
    /// the agent's to name, and is ignored; demoting one that was not promoted
    /// changes nothing. One that changes what is chosen has the next call
    /// chosen afresh, as the agent asked.
    fn change(&mut self, change: ContextChange) {
        let mut changed = false;
        match change {
            ContextChange::Promote(ids) => {
                let committed = self.turns.len();
                for id in ids {
                    match usize::try_from(id) {
                        Ok(id) if id < committed => changed |= self.promoted.insert(id),
                        _ => tracing::warn!(
                            "ignored a promotion of turn {id}, which was never committed"
                        ),
                    }
                }
            }
            ContextChange::Demote(ids) => {
                for id in ids {
                    if let Ok(id) = usize::try_from(id) {
                        changed |= self.promoted.remove(&id);
                    }
                }
            }
        }
        if changed {
            self.cut_out.clear();
        }
    }

    /// The round the next call belongs to, and its prompt. Every call is made
    /// inside a round, which holds either its opening or a turn of its own; a
    /// store outside one answers nothing, and is sent as an opening of nothing.
    fn prompt(&self) -> (u32, Prompt) {
        match &self.opening {
            Some(opening) => (
                self.last_round() + 1,
                Prompt::Opening {
                    tokens: opening.tokens,
                },
            ),
            None if self.turns.len() > self.start => {
                (self.last_round(), Prompt::Turn(self.turns.len() - 1))
            }
            None => (self.last_round(), Prompt::Opening { tokens: 0 }),
        }
    }

    /// The estimated tokens of `prompt`.
    fn prompt_tokens(&self, prompt: Prompt) -> u64 {
        match prompt {
            Prompt::Opening { tokens } => tokens,
            Prompt::Turn(id) => self.tokens(id),
        }
    }

    /// Turn `id`'s estimated tokens as a call is sent it, before [`fit`]: its
    /// own, or with a notice in place of each result, once it has one.
    fn tokens(&self, id: usize) -> u64 {
        self.refused
            .get(&id)
            .map_or(self.turns[id].tokens, |withheld| withheld.tokens)
    }

    /// Send every later call a notice in place of each result of the turn
    /// `refused` names, counted as it is sent so.
    fn withhold(&mut self, refused: &Refused) {
        let Refused {
            turn,
            finish_reason,
        } = refused;
        if self.refused.contains_key(turn) {
            return;
        }
        let notice = notice(*turn, finish_reason);
        let tokens = budget::tokens(&without_results(
            self.turns[*turn].messages.clone(),
            &notice,
        ));
        self.refused.insert(*turn, Withheld { notice, tokens });
    }

    /// See [`History::assemble`], and, with `refusing`, [`History::assemble_without`].
    fn assemble(
        &mut self,
        budget: &Budget,
        refusing: Option<&Refused>,
    ) -> Result<(Vec<Message>, Manifest), TooLarge> {
        // The call sent without the results is chosen, counted, and assembled
        // as every later one is once it is answered: the store withholds the
        // turn while it assembles this one, and no longer.
        let tentative = refusing
            .filter(|refusing| !self.refused.contains_key(&refusing.turn))
            .map(|refusing| {
                self.withhold(refusing);
                refusing.turn
            });
        let assembled = self.assemble_held(budget);
        if let Some(turn) = tentative {
            self.refused.remove(&turn);
        }
        assembled
    }

    /// What the next call is sent of what the store holds now: see
    /// [`History::assemble`].
    fn assemble_held(&mut self, budget: &Budget) -> Result<(Vec<Message>, Manifest), TooLarge> {
        let (round, prompt) = self.prompt();
        // The call is held to its allowance, but not to a scale that would
        // refuse a prompt the rule admits.
        let budget = &budget.making_room_for(self.prompt_tokens(prompt));
        let Selection {
            carried,
            evicted,
            withheld,
            tokens,
            cut,
            stale,
        } = self
            .view(self.start, prompt, budget, &self.cut_out)
            .map_err(|estimate| TooLarge {
                turn: prompt.turn(),
                round,
                estimate,
                budget: budget.clone(),
            })?;
        if cut.is_some() {
            self.cut_out = evicted.iter().map(|&(id, _)| id).collect();
        }

        let mut sent: Vec<Message> = Vec::new();
        let mut adjacent = Vec::new();
        let mut left_out = Vec::new();
        let mut refused = Vec::new();
        let opening = match prompt {
            Prompt::Opening { .. } => self.opening.as_ref().map(|o| o.message.clone()),
            Prompt::Turn(_) => None,
        };
        // Each turn as the call's provider takes it, and a turn whose results
        // a provider refused with a notice in their place. The estimate above
        // counts each turn whole, a refused one with its notices, so what is
        // left out only makes it more conservative.
        let sending = Sending {
            wire: budget.wire,
            stale: &stale,
        };
        let turns = carried.iter().map(|&(id, _)| {
            let messages = fit(sending, id, &self.turns[id].messages, &mut left_out);
            let Some(withheld) = self.refused.get(&id) else {
                return (Some(id), messages);
            };
            refused.push((id, withheld.notice.clone()));
            (Some(id), without_results(messages, &withheld.notice))
        });
        for (turn, messages) in turns.chain(opening.iter().map(|o| (None, vec![o.clone()]))) {
            if let (Some(before), Some(first)) = (sent.last(), messages.first())
                && let Some(role) = Role::of(first).filter(|role| Role::of(before) == Some(*role))
            {
                adjacent.push(Adjacent { turn, role });
            }
            sent.extend(messages);
        }

        let manifest = Manifest {
            call: self.calls,
            round,
            budget: budget.clone(),
            estimate: tokens + budget.overhead,
            carried,
            evicted,
            opening,
            adjacent,
            left_out,
            withheld,
            refused,
            cut,
        };
        self.last = self.sent(&manifest.carried, sending);
        self.stale = stale;
        self.calls += 1;
        Ok((sent, manifest))
    }

    /// How many turns a round starting now, opening on a line of
    /// `opening_tokens`, would not send on its first call, held to `budget`.
    /// That call is chosen afresh, holding no cut.
    fn omitted(&self, budget: &Budget, opening_tokens: u64) -> usize {
        let prompt = Prompt::Opening {
            tokens: opening_tokens,
        };
        match self.view(self.turns.len(), prompt, budget, &BTreeSet::new()) {
            Ok(selection) => self.turns.len() - selection.carried.len(),
            Err(_) => self.turns.len(),
        }
    }

    /// The messages `manifest`'s call was sent, its prompt included: the turns
    /// it carried as `left_out` leaves them, each result of one `refused`
    /// names as its notice, then the opening.
    #[cfg(test)]
    fn reconstruct(&self, manifest: &Manifest) -> Vec<Message> {
        manifest
            .carried
            .iter()
            .flat_map(|&(id, _)| {
                let sent = as_sent(id, &self.turns[id].messages, &manifest.left_out);
                match manifest.refused.iter().find(|(turn, _)| *turn == id) {
                    Some((_, notice)) => without_results(sent, notice),
                    None => sent,
                }
            })
            .chain(manifest.opening.iter().cloned())
            .collect()
    }

    /// What a call of a round beginning at `start` is sent, held to `budget`,
    /// less what [`Store::alternate`] withholds when the provider requires
    /// turns to alternate. `Err` as [`Store::select`].
    ///
    /// A provider's prompt cache matches only a request that begins as an
    /// earlier one did, so the view is kept as it was for as long as it fits:
    /// everything chosen but what the round's latest cut left out, `cut_out`
    /// -- before a cut, everything. A round's calls only add turns to that.
    /// Once it does not fit, it is cut ([`Store::cut`]): what `select`
    /// chooses in half the room beside the prompt, which the calls after it
    /// are sent with each turn committed since, for some while. A turn a cut
    /// leaves out stays out until the next, however much room comes free.
    ///
    /// One cut serves every candidate, so a model a call moves to is sent
    /// what the one before it was. A candidate whose window it does not fit
    /// cuts for its own, and the next call is sent that.
    fn view(
        &self,
        start: usize,
        prompt: Prompt,
        budget: &Budget,
        cut_out: &BTreeSet<usize>,
    ) -> Result<Selection, u64> {
        let room = budget.room();
        let selection = match self.extend(cut_out, start, prompt, room) {
            Some(selection) => selection,
            None => self.cut(start, prompt, room)?,
        };
        // Which thinking is stale turns on which turns are sent before it,
        // and which turns are sent turns on the thinking: a reply of stale
        // thinking alone is left out, which can leave its turn sent as
        // nothing, and, where the roles must alternate, with neither role.
        // So the two are settled together, until what is sent makes nothing
        // more stale; `stale` only grows, so it does.
        let mut stale = self.stale.clone();
        let (carried, withheld) = loop {
            let (carried, withheld) = match budget.alternation {
                RoleAlternation::Relaxed => (selection.carried.clone(), Vec::new()),
                RoleAlternation::Strict => {
                    let sending = Sending {
                        wire: budget.wire,
                        stale: &stale,
                    };
                    self.alternate(&selection.carried, prompt, sending)
                }
            };
            let known = stale.len();
            self.spoil(&carried, budget.wire, &mut stale);
            if stale.len() == known {
                break (carried, withheld);
            }
        };
        let tokens =
            selection.tokens - withheld.iter().map(|&(id, _)| self.tokens(id)).sum::<u64>();
        Ok(Selection {
            carried,
            withheld,
            tokens,
            stale,
            ..selection
        })
    }

    /// `carried`, oldest first, split into what a call answering `prompt` is
    /// sent and what it is not, so that what is sent alternates between the
    /// user's side and the model's for a provider that requires it
    /// ([`RoleAlternation::Strict`]). Both oldest first.
    ///
    /// Each turn is a fragment that opens on one role and ends on one, as
    /// `sending` sends it; a turn [`fit`] leaves nothing of has none and
    /// stays carried out of the way. The fragments are walked oldest first
    /// onto a stack of what is kept, which begins as if on a reply so the view
    /// opens on the user's side. Where a fragment opens on the role the stack
    /// ends on:
    ///
    /// - **After results the model never answered.** Only a round's first turn
    ///   and the opening open on the user's side, so the stack ends on a round
    ///   cut short. The earlier side yields: the stack is popped until it ends
    ///   on a reply. The fragment itself is never dropped, since dropping a
    ///   round's first turn would lose the user's prompt while the round's
    ///   later turns stayed.
    /// - **After a reply.** A promotion or a turn of the window yields and is
    ///   withheld. A turn of the round in progress, or the latest, is instead
    ///   sent after the nearest kept fragment that ends on the user's side,
    ///   withholding what was kept after it; when there is none, the pair
    ///   stands, and the manifest's `adjacent` records it.
    ///
    /// A turn of the round in progress is never popped: one followed by
    /// another ends on results, so it never ends a reply-after-reply seam, and
    /// the later side of a user-after-user seam is never of the round in
    /// progress. Were it reached all the same, the pair would stand.
    fn alternate(
        &self,
        carried: &[(usize, Why)],
        prompt: Prompt,
        sending: Sending,
    ) -> (Chosen, Chosen) {
        // A turn of the round in progress, or the latest, is never withheld.
        let yields = |at: usize| !matches!(carried[at].1, Why::Latest | Why::Round);
        // Each kept fragment: its place in `carried`, and the role it ends on.
        let mut stack: Vec<(usize, Role)> = Vec::new();
        let mut withheld: Vec<usize> = Vec::new();
        let ends_on =
            |stack: &[(usize, Role)]| stack.last().map_or(Role::Assistant, |&(_, last)| last);
        // Pop the stack while it ends on the user's side and may yield.
        let unanswered = |stack: &mut Vec<(usize, Role)>, withheld: &mut Vec<usize>| {
            while let Some(&(top, Role::User)) = stack.last()
                && yields(top)
            {
                stack.pop();
                withheld.push(top);
            }
        };
        for (at, &(id, _)) in carried.iter().enumerate() {
            // A turn `fit` leaves nothing of has no roles, and stays carried.
            let Some((first, last)) = ends(sending, id, &self.turns[id].messages) else {
                continue;
            };
            match (first, ends_on(&stack)) {
                (Role::User, Role::User) => unanswered(&mut stack, &mut withheld),
                (Role::Assistant, Role::Assistant) => {
                    if yields(at) {
                        withheld.push(at);
                        continue;
                    }
                    if let Some(cut) = stack.iter().rposition(|&(_, last)| last == Role::User)
                        && stack[cut + 1..].iter().all(|&(top, _)| yields(top))
                    {
                        withheld.extend(stack.drain(cut + 1..).map(|(top, _)| top));
                    }
                }
                _ => {}
            }
            stack.push((at, last));
        }
        if matches!(prompt, Prompt::Opening { .. }) {
            unanswered(&mut stack, &mut withheld);
        }
        withheld.sort_unstable();
        let kept = carried
            .iter()
            .enumerate()
            .filter(|(at, _)| withheld.binary_search(at).is_err())
            .map(|(_, &turn)| turn)
            .collect();
        let withheld = withheld.iter().map(|&at| carried[at]).collect();
        (kept, withheld)
    }

    /// Why each turn is chosen for a call of a round that begins at `start`
    /// and answers `latest`, if it is: see [`Why`]. Rounds are numbered
    /// without gaps, so both ends of the window are ranges of round numbers.
    fn chosen(&self, start: usize, latest: Option<usize>) -> Vec<(usize, Why)> {
        let before = self.turns[..start].last().map_or(0, |turn| turn.round);
        let Window { first, recent } = self.window;
        self.turns
            .iter()
            .enumerate()
            .filter_map(|(id, turn)| {
                let why = if Some(id) == latest {
                    Why::Latest
                } else if id >= start {
                    Why::Round
                } else if self.promoted.contains(&id) {
                    Why::Promoted
                } else if turn.round <= first {
                    Why::First
                } else if turn.round > before.saturating_sub(recent) {
                    Why::Recent
                } else {
                    return None;
                };
                Some((id, why))
            })
            .collect()
    }

    /// Whether turn `id` is its round's first, which begins with the round's
    /// opening.
    fn opens_round(&self, id: usize) -> bool {
        id == 0 || self.turns[id - 1].round != self.turns[id].round
    }

    /// Adds to `stale` the turns whose thinking a call to Anthropic sent
    /// `carried`, oldest first, in `wire`'s protocol, leaves out for what came
    /// before it: when the turns it sends anything of do not begin with those
    /// the latest call did, the first that does not and every one after it.
    /// What came before each of those changed: a cut, a round whose window
    /// moved past a middle one, a promotion, a turn withheld so the roles
    /// alternate and later sent, a model a call moved to cutting for its own
    /// window. Turns before the change keep their thinking, and so do those
    /// produced after; a turn sent as nothing, such as another provider's
    /// thinking alone, changes nothing.
    fn spoil(&self, carried: &[(usize, Why)], wire: Wire, stale: &mut BTreeSet<usize>) {
        let sent = self.sent(carried, Sending { wire, stale });
        let same = sent
            .iter()
            .zip(&self.last)
            .take_while(|(id, last)| id == last)
            .count();
        if same < self.last.len() {
            stale.extend(&sent[same..]);
        }
    }

    /// The turns of `carried` a call `sending` them sends anything of, by
    /// id, oldest first.
    fn sent(&self, carried: &[(usize, Why)], sending: Sending) -> Vec<usize> {
        carried
            .iter()
            .map(|&(id, _)| id)
            .filter(|&id| ends(sending, id, &self.turns[id].messages).is_some())
            .collect()
    }

    /// What a call of a round beginning at `start` carries of what is chosen
    /// but `cut_out`, when that and its prompt fit in `room`.
    fn extend(
        &self,
        cut_out: &BTreeSet<usize>,
        start: usize,
        prompt: Prompt,
        room: u64,
    ) -> Option<Selection> {
        let (carried, evicted): (Chosen, Chosen) = self
            .chosen(start, prompt.turn())
            .into_iter()
            .partition(|&(id, why)| why == Why::Latest || !cut_out.contains(&id));
        let tokens = carried
            .iter()
            .filter(|&&(_, why)| why != Why::Latest)
            .map(|&(id, _)| self.tokens(id))
            .sum::<u64>()
            + self.prompt_tokens(prompt);
        (tokens <= room).then_some(Selection {
            carried,
            evicted,
            tokens,
            ..Selection::default()
        })
    }

    /// A cut for a call of a round beginning at `start`: its prompt, and what
    /// it carries of the rest in half of `room`, or in what the prompt leaves
    /// of it when that is less. A prompt too large to leave half keeps what
    /// fits beside it, as the whole room would, rather than nothing but
    /// itself: the round's opening turn among it. `Err` as [`Store::select`].
    fn cut(&self, start: usize, prompt: Prompt, room: u64) -> Result<Selection, u64> {
        let mark = (room / 2 + self.prompt_tokens(prompt)).min(room);
        let selection = self.select(start, prompt, mark)?;
        Ok(Selection {
            cut: Some(mark),
            ..selection
        })
    }

    /// What a call of a round beginning at `start` carries in `room`, or the
    /// estimate of its prompt when that alone does not fit: a cut, in what
    /// [`Store::cut`] leaves it.
    ///
    /// Turns are admitted in the order they are kept longest, for the reasons
    /// [`Window`] gives: the turn that opens each first round and the round in
    /// progress, oldest first; then the round's own, newest first; then
    /// promotions, newest first; then the rest of the window, the first rounds
    /// with the recent ones, newest first. So what is left out goes the other
    /// way. A turn that does not fit is left out and the rest are still tried,
    /// so one large turn costs only itself.
    fn select(&self, start: usize, prompt: Prompt, room: u64) -> Result<Selection, u64> {
        let latest = prompt.turn();
        let prompt_tokens = self.prompt_tokens(prompt);
        if prompt_tokens > room {
            return Err(prompt_tokens);
        }
        let mut left = room - prompt_tokens;

        let chosen = self.chosen(start, latest);
        let opening = |&(id, why): &(usize, Why)| {
            why != Why::Latest
                && self.opens_round(id)
                && (id == start || self.turns[id].round <= self.window.first)
        };
        let of = |whys: &'static [Why]| {
            chosen
                .iter()
                .filter(move |turn| whys.contains(&turn.1) && !opening(turn))
                .copied()
        };
        let order = chosen
            .iter()
            .filter(|turn| opening(turn))
            .copied()
            .chain(of(&[Why::Round]).rev())
            .chain(of(&[Why::Promoted]).rev())
            .chain(of(&[Why::First, Why::Recent]).rev());
        let mut carried: Vec<(usize, Why)> =
            latest.map(|id| (id, Why::Latest)).into_iter().collect();
        let mut evicted = Vec::new();
        for (id, why) in order {
            let tokens = self.tokens(id);
            if tokens <= left {
                left -= tokens;
                carried.push((id, why));
            } else {
                evicted.push((id, why));
            }
        }
        carried.sort_unstable_by_key(|(id, _)| *id);
        evicted.sort_unstable_by_key(|(id, _)| *id);
        Ok(Selection {
            carried,
            evicted,
            tokens: room - left,
            ..Selection::default()
        })
    }
}

/// Split `messages` -- a stretch of one round's, oldest first, from a model's
/// reply on -- into turns, each beginning at a reply.
pub(crate) fn split_turns(messages: Vec<Message>) -> Vec<Vec<Message>> {
    let mut turns: Vec<Vec<Message>> = Vec::new();
    for message in messages {
        match turns.last_mut() {
            Some(turn) if !matches!(message, Message::Assistant(_)) => turn.push(message),
            _ => turns.push(vec![message]),
        }
    }
    turns
}

/// A turn as the interpreter holds it: what opened the round, what the model
/// wrote, each call it made with the result it read, and whether the round
/// ended while those calls ran.
fn mirror(round: u32, messages: &[Message], incomplete: bool) -> Value {
    let mut prompt = Vec::new();
    let mut text = Vec::new();
    let mut calls: Vec<&ToolCall> = Vec::new();
    let mut results = HashMap::new();
    for message in messages {
        match message {
            Message::User { content } => {
                for part in content.iter() {
                    match part {
                        UserContent::Text(part) => prompt.push(part.text.as_str()),
                        UserContent::ToolResult(result) => {
                            results.insert(result.call.clone(), texts(result.content.iter()));
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant(reply) => {
                for part in reply.content.iter() {
                    match part {
                        AssistantContent::Text(part) => text.push(part.text.as_str()),
                        AssistantContent::ToolCall(call) => calls.push(call),
                        _ => {}
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    let calls: Vec<Value> = calls
        .into_iter()
        .map(|call| {
            let function = &call.function;
            // What the model sent, when it was not the object the tool takes:
            // rig keeps the text and leaves the arguments empty.
            let source = match (
                function.arguments.get("source").and_then(Value::as_str),
                &function.invalid_arguments,
            ) {
                (Some(source), _) => source.to_string(),
                (None, Some(raw)) => raw.clone(),
                (None, None) => Value::Object(function.arguments.clone()).to_string(),
            };
            let result = results.remove(&call.id).unwrap_or_default();
            json!({"source": source, "result": result})
        })
        .collect();
    json!({
        "round": round,
        "prompt": (!prompt.is_empty()).then(|| prompt.join("\n")),
        "text": text.join("\n"),
        "calls": calls,
        "incomplete": incomplete,
    })
}

/// The text of a tool result's parts, joined.
fn texts<'a>(parts: impl Iterator<Item = &'a ToolResultContent>) -> String {
    parts
        .filter_map(|part| match part {
            ToolResultContent::Text(part) => Some(part.text.clone()),
            ToolResultContent::Json { value } => Some(value.to_string()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod history_tests;
