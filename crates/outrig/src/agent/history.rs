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
//! The **view** is what a model call is sent of the turns before its round:
//! the conversation's first rounds, its most recent ones, and every turn the
//! agent promoted, each in its place. The round in progress is sent whole, but
//! not from here: rig holds it (`round.rs`), and nothing earlier.
//!
//! `plan/phase/0003-python/history.md` designs both.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig::completion::Message;
use rig::completion::message::{AssistantContent, ToolCall, ToolResultContent, UserContent};
use serde_json::{Value, json};

use crate::python::host::Interpreter;

/// Which earlier rounds a model call is sent, besides the turns the agent
/// promoted. The round in progress is always sent.
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

/// The agent's conversation. Cheap to clone; every clone is the one store.
#[derive(Clone)]
pub(crate) struct History {
    store: Arc<Mutex<Store>>,
    /// Where each turn is mirrored.
    interpreter: Interpreter,
}

impl History {
    /// A store for `interpreter`'s agent, whose model calls are sent `window`
    /// of it. The agent's promotions reach it as the interpreter reports them.
    pub(crate) fn new(interpreter: Interpreter, window: Window) -> Self {
        let store = Arc::new(Mutex::new(Store {
            window,
            ..Store::default()
        }));
        let promoted = Arc::clone(&store);
        interpreter.on_promote(move |ids| lock(&promoted).promote(ids));
        Self { store, interpreter }
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// Start a round that `opening` opens. Every model call in it is sent a
    /// view of the turns before it, and its first turn begins with `opening`.
    pub(crate) fn begin_round(&self, opening: Message) {
        self.store().begin_round(opening);
    }

    /// Commit `messages` as the next turn, and mirror it into the interpreter.
    pub(crate) fn commit(&self, messages: Vec<Message>) {
        self.commit_in(&mut self.store(), messages);
    }

    /// End the round in progress. An opening no turn began with -- the model's
    /// first reply was empty -- was sent all the same, and is a turn of its
    /// own.
    pub(crate) fn finish_round(&self) {
        let mut store = self.store();
        if store.opening.is_some() {
            self.commit_in(&mut store, Vec::new());
        }
    }

    /// Under the store's lock, so turns reach the interpreter in id order.
    fn commit_in(&self, store: &mut Store, messages: Vec<Message>) {
        let (id, turn) = store.commit(messages);
        self.interpreter
            .push_turn(id as u64, mirror(turn.round, &turn.messages));
    }

    /// What a model call in the round in progress is sent of the turns before
    /// it: the window and what the agent promoted, oldest first.
    pub(crate) fn view(&self) -> Vec<Message> {
        let store = self.store();
        store
            .select(store.start)
            .flat_map(|turn| turn.messages.iter().cloned())
            .collect()
    }

    /// How many turns a round starting now would not be sent.
    pub(crate) fn omitted(&self) -> usize {
        let store = self.store();
        store.turns.len() - store.select(store.turns.len()).count()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.store().turns.len()
    }

    #[cfg(test)]
    pub(crate) fn set_window(&self, window: Window) {
        self.store().window = window;
    }
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The store itself, apart from the interpreter it is mirrored into.
#[derive(Default)]
struct Store {
    /// Every committed turn, oldest first. A turn's id is its index.
    turns: Vec<Turn>,
    /// The ids of the turns the agent promoted.
    promoted: BTreeSet<usize>,
    /// Where the round in progress begins.
    start: usize,
    /// The line that opened the round in progress, until the round's first
    /// turn commits and begins with it.
    opening: Option<Message>,
    window: Window,
}

struct Turn {
    round: u32,
    messages: Vec<Message>,
}

impl Store {
    fn begin_round(&mut self, opening: Message) {
        self.start = self.turns.len();
        self.opening = Some(opening);
    }

    /// Commit `messages` as the next turn, after the round's opening if this
    /// is its first. A round is numbered as that turn commits, so one that
    /// commits none leaves no gap.
    fn commit(&mut self, mut messages: Vec<Message>) -> (usize, &Turn) {
        let mut round = self.turns.last().map_or(0, |turn| turn.round);
        if let Some(opening) = self.opening.take() {
            messages.insert(0, opening);
            round += 1;
        }
        let id = self.turns.len();
        self.turns.push(Turn { round, messages });
        (id, &self.turns[id])
    }

    /// Promote `ids`, each a turn the store holds. Another is not the agent's
    /// to name, and is ignored.
    fn promote(&mut self, ids: Vec<u64>) {
        let held = self.turns.len();
        for id in ids {
            match usize::try_from(id) {
                Ok(id) if id < held => {
                    self.promoted.insert(id);
                }
                _ => tracing::warn!("ignored a promotion of turn {id}, which was never committed"),
            }
        }
    }

    /// The turns among the first `before` that a model call is sent, in
    /// order: those of the first rounds, of the most recent, and those
    /// promoted. Rounds are numbered without gaps, so both ends of the window
    /// are ranges of round numbers.
    fn select(&self, before: usize) -> impl Iterator<Item = &Turn> {
        let turns = &self.turns[..before];
        let last = turns.last().map_or(0, |turn| turn.round);
        let Window { first, recent } = self.window;
        turns.iter().enumerate().filter_map(move |(id, turn)| {
            (turn.round <= first
                || turn.round > last.saturating_sub(recent)
                || self.promoted.contains(&id))
            .then_some(turn)
        })
    }
}

/// Split `messages` -- a stretch of one round's, oldest first, from a model's
/// reply on -- into turns, each beginning at a reply.
pub(crate) fn split_turns(messages: Vec<Message>) -> Vec<Vec<Message>> {
    let mut turns: Vec<Vec<Message>> = Vec::new();
    for message in messages {
        match turns.last_mut() {
            Some(turn) if !matches!(message, Message::Assistant { .. }) => turn.push(message),
            _ => turns.push(vec![message]),
        }
    }
    turns
}

/// A turn as the interpreter holds it: what opened the round, what the model
/// wrote, and each call it made with the result it read.
fn mirror(round: u32, messages: &[Message]) -> Value {
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
                            results.insert(result.id.as_str(), texts(result.content.iter()));
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for part in content.iter() {
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
            let arguments = &call.function.arguments;
            let source = match arguments.get("source").and_then(Value::as_str) {
                Some(source) => source.to_string(),
                None => arguments.to_string(),
            };
            let result = results.remove(call.id.as_str()).unwrap_or_default();
            json!({"source": source, "result": result})
        })
        .collect();
    json!({
        "round": round,
        "prompt": (!prompt.is_empty()).then(|| prompt.join("\n")),
        "text": text.join("\n"),
        "calls": calls,
    })
}

/// The text of a tool result's parts, joined.
fn texts<'a>(parts: impl Iterator<Item = &'a ToolResultContent>) -> String {
    parts
        .filter_map(|part| match part {
            ToolResultContent::Text(part) => Some(part.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod history_tests;
