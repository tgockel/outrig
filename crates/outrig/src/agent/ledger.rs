//! A session's model spend, attempt by attempt.
//!
//! Every model call the loop decides to make has a [`CallId`], and every
//! request actually sent for it -- a retry, a move to the next model of a
//! chain -- an [`AttemptId`] of its own. Each attempt is recorded once, as a
//! `model.attempt` event, failed ones included, with what the provider said it
//! used or `null` when it said nothing. A `200` rig could not make a
//! completion of is billed all the same, so its usage is read from the body
//! ([`body_usage`]) rather than from the completion there is none of.
//!
//! [`body_usage`]: super::retry::body_usage
//!
//! Every total is derived from those attempts, never stored: a round's is the
//! sum over the attempts it made, the session's over all of them. A usage
//! record that arrives after its attempt's event replaces that attempt's null
//! once ([`Ledger::late_usage`]), and every total that counted the attempt
//! reads the new figure from then on. A parent's inclusive total is never
//! added to its parts, which would count each attempt twice.
//!
//! [`Book`] is the one fold. The live session feeds it as it records, and a
//! reader of the event log feeds it the same records back, so the two cannot
//! disagree about which record counted.
//!
//! The ledger also holds what bounds a session's model calls as a whole: its
//! places for requests in flight, which every request takes one of from just
//! before it is sent until its attempt is recorded, and its token budget,
//! which every request checks once it holds its place. Each agent's
//! [`CallSlot`] reaches both, and a child's slot knows the round of its
//! parent's it was launched in, so what the child spends is added to that
//! round's tree.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use bytes::Bytes;
use rig::completion::{CompletionError, CompletionResponse};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::budget::Budget;
use super::history::Adjacent;
use crate::events::Events;
use crate::harness::event::{AttemptId, CallId, ModelAttempt, Payload, Usage, UsageRefusal};

/// What a provider reported, or `None` when it reported nothing: rig fills a
/// usage it was not given with zeros, and a zero is a count nobody gave.
pub(crate) fn reported(usage: rig::completion::Usage) -> Option<Usage> {
    let usage = Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
    };
    (usage != Usage::default()).then_some(usage)
}

/// A session's attempts, and the counters that name its calls and attempts.
/// One per session, so two sessions in one process never share what an id
/// means. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct Ledger {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    events: Events,
    calls: AtomicU64,
    attempts: AtomicU64,
    book: Mutex<Book>,
    /// How many model requests the session may have in flight at once, every
    /// agent's together, and its places for them: a request holds one from
    /// just before it is sent until its attempt has been recorded with what it
    /// used, or has failed, and never through a backoff or while an agent's
    /// code runs. So a request that takes a place given back sees the budget
    /// as the attempt that held it left it. `None` bounds nothing.
    places: Option<(usize, Arc<Semaphore>)>,
    /// What the session may spend, when it has a budget.
    budget: Option<u64>,
    /// Whether the budget is spent: set once, as the attempts recorded reach
    /// it, and heard by whoever waits for it.
    out: watch::Sender<bool>,
}

/// The fold every total is derived from: each attempt once, by its id.
#[derive(Debug, Default)]
pub(crate) struct Book {
    attempts: BTreeMap<AttemptId, Entry>,
}

#[derive(Debug)]
struct Entry {
    call: CallId,
    usage: Option<Usage>,
    /// Whether a late record has already replaced its null usage.
    replaced: bool,
}

/// What a usage report for an attempt, arriving after it, did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Late {
    Replaced {
        call: CallId,
    },
    Refused {
        call: Option<CallId>,
        reason: UsageRefusal,
    },
}

impl Book {
    /// Record an attempt, once: an attempt its id already names is counted
    /// once, however many events name it.
    pub(crate) fn attempt(&mut self, call: CallId, attempt: AttemptId, usage: Option<Usage>) {
        self.attempts.entry(attempt).or_insert(Entry {
            call,
            usage,
            replaced: false,
        });
    }

    /// Take `usage` as `attempt`'s, arriving after its event: it replaces a
    /// null once, and is refused otherwise.
    pub(crate) fn late(&mut self, attempt: AttemptId, usage: Usage) -> Late {
        let Some(entry) = self.attempts.get_mut(&attempt) else {
            return Late::Refused {
                call: None,
                reason: UsageRefusal::Unknown,
            };
        };
        let reason = match (entry.replaced, entry.usage) {
            (true, _) => UsageRefusal::Replaced,
            (false, Some(_)) => UsageRefusal::Reported,
            (false, None) => {
                entry.usage = Some(usage);
                entry.replaced = true;
                return Late::Replaced { call: entry.call };
            }
        };
        Late::Refused {
            call: Some(entry.call),
            reason,
        }
    }

    /// What `attempts` used, each counted once: `None` when none of them
    /// reported anything.
    pub(crate) fn total(&self, attempts: &[AttemptId]) -> Option<Usage> {
        let unique: BTreeSet<&AttemptId> = attempts.iter().collect();
        Self::sum(unique.into_iter().filter_map(|id| self.attempts.get(id)))
    }

    /// What every attempt of the session used.
    pub(crate) fn session_total(&self) -> Option<Usage> {
        Self::sum(self.attempts.values())
    }

    pub(crate) fn usage_of(&self, attempt: AttemptId) -> Option<Usage> {
        self.attempts.get(&attempt).and_then(|entry| entry.usage)
    }

    fn sum<'a>(entries: impl Iterator<Item = &'a Entry>) -> Option<Usage> {
        let reported: Vec<Usage> = entries.filter_map(|entry| entry.usage).collect();
        (!reported.is_empty()).then(|| reported.into_iter().sum())
    }
}

impl Ledger {
    /// A ledger recording its attempts in `events`, with at most `places`
    /// model requests in flight and `budget` tokens to spend, when each is
    /// given.
    pub(crate) fn new(events: Events, places: Option<usize>, budget: Option<u64>) -> Self {
        Self {
            inner: Arc::new(Inner {
                events,
                places: places.map(|max| (max, Arc::new(Semaphore::new(max)))),
                budget,
                ..Inner::default()
            }),
        }
    }

    /// A place for one model request, held until it is dropped, waiting in
    /// turn while every place is held: a wait dropped leaves its turn having
    /// taken nothing. `None` at once when the session bounds nothing.
    pub(crate) async fn place(&self) -> Option<OwnedSemaphorePermit> {
        let (_, places) = self.inner.places.as_ref()?;
        // Never closed, so never refused.
        Arc::clone(places).acquire_owned().await.ok()
    }

    /// How many places for model requests are held now.
    #[cfg(test)]
    pub(crate) fn places_held(&self) -> usize {
        self.inner
            .places
            .as_ref()
            .map_or(0, |(max, places)| max - places.available_permits())
    }

    /// The session's token budget, when it has one.
    pub(crate) fn budget(&self) -> Option<u64> {
        self.inner.budget
    }

    /// Whether the session has spent its token budget: no model call may be
    /// made from here on.
    pub(crate) fn budget_spent(&self) -> bool {
        *self.inner.out.borrow()
    }

    /// Whether the budget is spent, as it changes: `true` from the moment it
    /// is, for every receiver.
    pub(crate) fn budget_watch(&self) -> watch::Receiver<bool> {
        self.inner.out.subscribe()
    }

    /// Mark the budget spent once what `book`, just changed, totals reaches
    /// it.
    fn count(&self, book: &Book) {
        let reached = self.inner.budget.is_some_and(|budget| {
            book.session_total()
                .is_some_and(|spent| spent.total_tokens >= budget)
        });
        if reached {
            self.inner
                .out
                .send_if_modified(|spent| !std::mem::replace(spent, true));
        }
    }

    fn mint_call(&self) -> CallId {
        CallId::new(self.inner.calls.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn mint_attempt(&self) -> AttemptId {
        AttemptId::new(self.inner.attempts.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.inner
            .book
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `attempt`, and publish it through `events`, its agent's. Both
    /// under the book's lock, so the order the log holds the records in is the
    /// order they were applied in, and a replay of it applies them the same
    /// way.
    fn record(&self, attempt: ModelAttempt, events: &Events) {
        let mut book = self.book();
        book.attempt(attempt.call_id, attempt.attempt_id, attempt.usage);
        self.count(&book);
        events.emit(Payload::ModelAttempt(attempt));
    }

    /// Take `usage` as attempt `attempt`'s, arriving after its event: it
    /// replaces a null usage once, and is refused -- and the refusal recorded
    /// -- otherwise. Spend from a later attempt of the same call is that
    /// attempt's, under its own id, never a correction of this one.
    ///
    /// Nothing calls this yet: every provider this loop speaks reports usage
    /// in the response that ends its attempt, a request that failed before a
    /// response reports none, and nothing streams. A streamed reply's closing usage, or a provider's
    /// billing callback, is what would.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn late_usage(&self, attempt: AttemptId, usage: Usage) -> Late {
        let mut book = self.book();
        let late = book.late(attempt, usage);
        self.count(&book);
        self.inner.events.emit(match late {
            Late::Replaced { call } => Payload::ModelUsageReplaced {
                call_id: call,
                attempt_id: attempt,
                usage,
            },
            Late::Refused { call, reason } => Payload::ModelUsageRefused {
                call_id: call,
                attempt_id: attempt,
                usage,
                reason,
            },
        });
        late
    }

    /// What `attempts` used, each counted once.
    pub(crate) fn total(&self, attempts: &[AttemptId]) -> Option<Usage> {
        self.book().total(attempts)
    }

    pub(crate) fn usage_of(&self, attempt: AttemptId) -> Option<Usage> {
        self.book().usage_of(attempt)
    }

    #[cfg(test)]
    pub(crate) fn session_total(&self) -> Option<Usage> {
        self.book().session_total()
    }
}

/// A session's book rebuilt from its event log alone, through the same fold
/// the live session used: each `model.attempt` once, and each later usage
/// record as the session applied it.
#[cfg(test)]
pub(crate) fn replay(records: &[serde_json::Value]) -> Book {
    let id = |value: &serde_json::Value| value.as_u64().expect("an id");
    let usage = |value: &serde_json::Value| -> Option<Usage> {
        value.as_object().map(|fields| {
            let field = |name: &str| fields[name].as_u64().expect("a count");
            Usage {
                input_tokens: field("input_tokens"),
                output_tokens: field("output_tokens"),
                total_tokens: field("total_tokens"),
                cached_input_tokens: field("cached_input_tokens"),
                cache_creation_input_tokens: field("cache_creation_input_tokens"),
                reasoning_tokens: field("reasoning_tokens"),
            }
        })
    };
    let mut book = Book::default();
    for record in records {
        let data = &record["data"];
        let attempt = || AttemptId::new(id(&data["attempt_id"]));
        match record["type"].as_str() {
            Some("org.outrig.model.attempt") => {
                book.attempt(
                    CallId::new(id(&data["call_id"])),
                    attempt(),
                    usage(&data["usage"]),
                );
            }
            Some(kind @ ("org.outrig.model.usage.replaced" | "org.outrig.model.usage.refused")) => {
                let late = book.late(
                    attempt(),
                    usage(&data["usage"]).expect("a late record carries usage"),
                );
                assert_eq!(
                    matches!(late, Late::Replaced { .. }),
                    kind.ends_with("replaced"),
                    "the replay applies {kind} as the session did"
                );
            }
            // Every other kind is an inclusive total or a reference to an
            // attempt; adding it is the double count this fold exists to
            // avoid.
            _ => {}
        }
    }
    book
}

/// Where a chain's calls and attempts are named, shared by the round, the
/// chain, and every candidate's retry loops beneath it -- the way
/// [`ChainDeadline`] reaches into a `'static` request future. A chain's calls
/// are made one at a time, so one call is in flight at most. One per agent.
///
/// [`ChainDeadline`]: super::retry::ChainDeadline
#[derive(Clone, Default)]
pub(crate) struct CallSlot {
    ledger: Ledger,
    state: Arc<Mutex<SlotState>>,
    /// Where the agent's model events go.
    events: Events,
    /// For a child: its parent's slot, and the round of its parent's it was
    /// launched in, whose tree its spend is added to.
    parent: Option<Arc<(CallSlot, Option<u32>)>>,
}

#[derive(Default)]
struct SlotState {
    call: Option<InFlight>,
    /// The attempts the round in progress has made, failed ones included: its
    /// members, which its total is the sum over.
    round: Vec<AttemptId>,
    /// The attempts children made under the round in progress, at any depth,
    /// while it ran.
    descendants: Vec<AttemptId>,
    /// The round in progress, by its number in the conversation.
    open: Option<u32>,
    /// The latest round begun, in progress or not.
    last: Option<u32>,
}

struct InFlight {
    call: CallId,
    settings: Arc<Settings>,
    /// The attempt a `2xx` answered, until the model layer settles it.
    answered: Option<AnsweredAttempt>,
    /// The latest attempt the current candidate was sent.
    last: Option<AttemptId>,
    /// The first request a candidate refused, of those whose conversation put
    /// one role after itself.
    refusal: Option<Refusal>,
}

/// A candidate's refusal of a request whose conversation put one role after
/// itself: what the round has to explain it, which only the chain, having
/// assembled that view, holds.
#[derive(Clone)]
pub(crate) struct Refusal {
    /// What a call to the candidate was held to, whose `model` and `provider`
    /// name the rows that refused.
    pub(crate) budget: Budget,
    /// Where the conversation it was sent put one role after itself.
    pub(crate) adjacent: Adjacent,
}

/// A `2xx` the model layer has yet to settle.
struct AnsweredAttempt {
    attempt: AttemptId,
    /// The body that came with it, set once the HTTP layer has read it: what
    /// [`body_usage`] is read from when rig can make nothing of it. The bytes
    /// rig is reading, held a second time; where rig turns them into a `Vec`
    /// that costs a copy, paid on every call for the failures it serves.
    ///
    /// [`body_usage`]: super::retry::body_usage
    body: Arc<OnceLock<Bytes>>,
    /// The request's place among those in flight, given back as the attempt
    /// is settled, once its usage is in the book.
    _place: Option<OwnedSemaphorePermit>,
}

/// What a request was sent with: its candidate's row, the identifier it named,
/// and its output-token ceiling.
#[derive(Default)]
struct Settings {
    model: String,
    identifier: String,
    max_tokens: Option<u32>,
}

impl CallSlot {
    /// The slot of an agent whose model events go to `events`; for a child,
    /// `parent` is its parent's slot and the round of it the child was
    /// launched in.
    pub(crate) fn new(
        ledger: Ledger,
        events: Events,
        parent: Option<(CallSlot, Option<u32>)>,
    ) -> Self {
        Self {
            ledger,
            state: Arc::default(),
            events,
            parent: parent.map(Arc::new),
        }
    }

    pub(crate) fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Where the agent's model events go.
    pub(crate) fn events(&self) -> &Events {
        &self.events
    }

    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Round `round` begins: the attempts from here on are its, and so are
    /// those its children make while it runs.
    pub(crate) fn open_round(&self, round: u32) {
        let mut state = self.state();
        state.round.clear();
        state.descendants.clear();
        state.open = Some(round);
        state.last = Some(round);
    }

    /// The round ends: the attempts it made, and those its children made
    /// under it while it ran. An attempt a child makes under it from here on
    /// is published apart, naming it.
    pub(crate) fn close_round(&self) -> (Vec<AttemptId>, Vec<AttemptId>) {
        let mut state = self.state();
        state.open = None;
        (
            std::mem::take(&mut state.round),
            std::mem::take(&mut state.descendants),
        )
    }

    /// The round a child launched now is launched in: the one in progress, or
    /// the latest, for code a round left running.
    pub(crate) fn current_round(&self) -> Option<u32> {
        let state = self.state();
        state.open.or(state.last)
    }

    /// A child under this slot's agent made `attempt`, under its round
    /// `round`: counted into that round while it runs, and published as spend
    /// added to it once it has ended.
    fn descendant(&self, round: u32, attempt: AttemptId, usage: Option<Usage>) {
        {
            let mut state = self.state();
            if state.open == Some(round) {
                state.descendants.push(attempt);
                return;
            }
        }
        self.events.emit(Payload::ModelRoundUsage {
            round,
            attempts: vec![attempt],
            usage,
        });
    }

    /// The call in flight and the request that answered it, once one has.
    pub(crate) fn answered_by(&self) -> Option<(CallId, AttemptId)> {
        let state = self.state();
        let call = state.call.as_ref()?;
        Some((call.call, call.last?))
    }

    /// A call begins, and is named.
    pub(crate) fn begin_call(&self) -> CallId {
        let call = self.ledger.mint_call();
        self.state().call = Some(InFlight {
            call,
            settings: Arc::default(),
            answered: None,
            last: None,
            refusal: None,
        });
        call
    }

    /// The call moves to a candidate: what its requests go with from here.
    pub(crate) fn candidate(&self, model: &str, identifier: &str, max_tokens: Option<u32>) {
        if let Some(call) = &mut self.state().call {
            call.settings = Arc::new(Settings {
                model: model.to_string(),
                identifier: identifier.to_string(),
                max_tokens,
            });
            call.answered = None;
            call.last = None;
        }
    }

    /// The latest request the current candidate was sent, if any was.
    pub(crate) fn last_attempt(&self) -> Option<AttemptId> {
        self.state().call.as_ref().and_then(|call| call.last)
    }

    /// The current candidate refused a request whose conversation put one role
    /// after itself, at `adjacent`. Only the call's first such refusal is
    /// kept, which is the one the round speaks of.
    pub(crate) fn refused(&self, budget: &Budget, adjacent: Adjacent) {
        if let Some(call) = &mut self.state().call {
            call.refusal.get_or_insert_with(|| Refusal {
                budget: budget.clone(),
                adjacent,
            });
        }
    }

    /// The latest call's first refusal of that kind, if it met one.
    pub(crate) fn refusal(&self) -> Option<Refusal> {
        self.state().call.as_ref()?.refusal.clone()
    }

    /// A request is about to be sent: its attempt, which records itself when
    /// it fails or is dropped unanswered. `None` outside a call -- a client
    /// rig built for itself, or a test's -- where nothing is recorded.
    pub(crate) fn sending(&self) -> Option<Pending> {
        let (call, settings) = {
            let state = self.state();
            let call = state.call.as_ref()?;
            (call.call, Arc::clone(&call.settings))
        };
        Some(Pending {
            slot: self.clone(),
            call,
            attempt: self.ledger.mint_attempt(),
            settings,
            done: false,
        })
    }

    /// The model layer is about to ask for a completion: what settles the
    /// attempt a `2xx` answered, once rig has made what it can of the body.
    pub(crate) fn awaiting(&self) -> Settling {
        Settling {
            slot: self.clone(),
            done: false,
        }
    }

    fn record(
        &self,
        call: CallId,
        attempt: AttemptId,
        settings: &Settings,
        error: Option<String>,
        usage: Option<Usage>,
    ) {
        self.ledger.record(
            ModelAttempt {
                call_id: call,
                attempt_id: attempt,
                model: settings.model.clone(),
                identifier: settings.identifier.clone(),
                max_tokens: settings.max_tokens,
                error,
                usage,
            },
            self.events(),
        );
        {
            let mut state = self.state();
            if let Some(in_flight) = &mut state.call
                && in_flight.call == call
            {
                in_flight.last = Some(attempt);
            }
            state.round.push(attempt);
        }
        // Up the tree, outside this slot's lock: each round above this agent's
        // that it was launched under counts what it spent.
        let mut link = self.parent.as_deref();
        while let Some((parent, round)) = link {
            if let Some(round) = *round {
                parent.descendant(round, attempt, usage);
            }
            link = parent.parent.as_deref();
        }
    }
}

/// One request in flight. Settled as failed, handed up as answered, or --
/// dropped with neither, its future cancelled -- recorded as cancelled.
pub(crate) struct Pending {
    slot: CallSlot,
    call: CallId,
    attempt: AttemptId,
    settings: Arc<Settings>,
    done: bool,
}

impl Pending {
    /// It failed for `error`, reporting no usage. The ids, for the retry it
    /// leads to.
    pub(crate) fn failed(mut self, error: String) -> (CallId, AttemptId) {
        self.done = true;
        self.slot
            .record(self.call, self.attempt, &self.settings, Some(error), None);
        (self.call, self.attempt)
    }

    /// A `2xx` came back: the model layer, which learns whether its body was
    /// usable, records it, and `place`, the request's place among those in
    /// flight, is held until then. The HTTP layer reports the body through
    /// what this returns, once it has read it.
    pub(crate) fn answered(mut self, place: Option<OwnedSemaphorePermit>) -> AnsweredBody {
        self.done = true;
        let body = Arc::new(OnceLock::new());
        if let Some(call) = &mut self.slot.state().call
            && call.call == self.call
        {
            call.answered = Some(AnsweredAttempt {
                attempt: self.attempt,
                body: Arc::clone(&body),
                _place: place,
            });
        }
        AnsweredBody(body)
    }
}

/// The HTTP layer's hold on the body of the `2xx` an attempt got, to report it
/// once it is read. One per attempt, so a report that arrives after its attempt
/// settled lands where nothing reads it, never on the next attempt's.
pub(crate) struct AnsweredBody(Arc<OnceLock<Bytes>>);

impl AnsweredBody {
    /// The body of the response arrived.
    pub(crate) fn arrived(&self, body: &Bytes) {
        // A response has one body, so the cell is empty; were it not, the
        // first is the one rig read.
        let _ = self.0.set(body.clone());
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.done {
            self.slot.record(
                self.call,
                self.attempt,
                &self.settings,
                Some("cancelled".to_string()),
                None,
            );
        }
    }
}

/// The model layer's half of an attempt that got a `2xx`.
pub(crate) struct Settling {
    slot: CallSlot,
    done: bool,
}

impl Settling {
    /// Record the answered attempt as `outcome` says: its usage when rig made a
    /// completion of it, and why not otherwise -- with the usage its body
    /// reported, which the provider billed whether or not rig could use it.
    /// The ids, or `None` when no request was answered -- a failure the HTTP
    /// layer already recorded.
    pub(crate) fn settle<R>(
        mut self,
        outcome: &Result<CompletionResponse<R>, CompletionError>,
    ) -> Option<(CallId, AttemptId)> {
        self.done = true;
        let (call, answered, settings) = {
            let mut state = self.slot.state();
            let in_flight = state.call.as_mut()?;
            let answered = in_flight.answered.take()?;
            (in_flight.call, answered, Arc::clone(&in_flight.settings))
        };
        let from_body = || {
            answered
                .body
                .get()
                .and_then(|body| super::retry::body_usage(body))
        };
        let (error, usage) = match outcome {
            Ok(response) => (None, reported(response.usage)),
            Err(CompletionError::ResponseError(message)) => {
                (Some(super::retry::unusable(message)), from_body())
            }
            Err(error) => (
                Some(super::retry::candidate_failure_label(error)),
                from_body(),
            ),
        };
        self.slot
            .record(call, answered.attempt, &settings, error, usage);
        // `answered` gives its request's place back as it drops, after this
        // record.
        Some((call, answered.attempt))
    }
}

impl Drop for Settling {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let answered = {
            let mut state = self.slot.state();
            state.call.as_mut().and_then(|call| {
                let answered = call.answered.take()?;
                Some((call.call, answered.attempt, Arc::clone(&call.settings)))
            })
        };
        if let Some((call, attempt, settings)) = answered {
            self.slot.record(
                call,
                attempt,
                &settings,
                Some("cancelled".to_string()),
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::mock_http::MODEL;

    fn usage(total: u64) -> Usage {
        Usage::total(total)
    }

    #[test]
    fn a_late_record_replaces_a_null_once() {
        let mut book = Book::default();
        let (call, attempt) = (CallId::new(1), AttemptId::new(1));
        book.attempt(call, attempt, None);
        assert_eq!(book.total(&[attempt]), None, "null, never zero");
        assert_eq!(book.late(attempt, usage(7)), Late::Replaced { call });
        assert_eq!(
            book.late(attempt, usage(9)),
            Late::Refused {
                call: Some(call),
                reason: UsageRefusal::Replaced
            }
        );
        assert_eq!(book.total(&[attempt]), Some(usage(7)));
    }

    #[test]
    fn a_late_record_for_reported_or_unknown_usage_is_refused() {
        let mut book = Book::default();
        let (call, attempt) = (CallId::new(1), AttemptId::new(1));
        book.attempt(call, attempt, Some(usage(5)));
        assert_eq!(
            book.late(attempt, usage(9)),
            Late::Refused {
                call: Some(call),
                reason: UsageRefusal::Reported
            }
        );
        assert_eq!(
            book.late(AttemptId::new(2), usage(9)),
            Late::Refused {
                call: None,
                reason: UsageRefusal::Unknown
            }
        );
        assert_eq!(book.session_total(), Some(usage(5)));
    }

    #[test]
    fn an_attempt_named_twice_is_counted_once() {
        let mut book = Book::default();
        let (call, one) = (CallId::new(1), AttemptId::new(1));
        book.attempt(call, one, Some(usage(4)));
        book.attempt(call, one, Some(usage(4)));
        assert_eq!(book.total(&[one, one]), Some(usage(4)));
        assert_eq!(book.session_total(), Some(usage(4)));
    }

    #[test]
    fn a_usage_rig_filled_with_zeros_is_none() {
        assert_eq!(reported(rig::completion::Usage::new()), None);
        let mut some = rig::completion::Usage::new();
        some.output_tokens = 3;
        assert_eq!(reported(some).map(|usage| usage.output_tokens), Some(3));
    }

    /// What a provider that reported input and output tokens and nothing else
    /// used.
    fn plain(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            ..Usage::default()
        }
    }

    fn bytes(body: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&body).expect("serializes"))
    }

    /// A slot with a call in flight, and the attempt it was just sent.
    fn sent() -> (CallSlot, Pending) {
        let slot = CallSlot::default();
        slot.begin_call();
        slot.candidate("sonnet", MODEL, Some(4096));
        let pending = slot.sending().expect("a call is in flight");
        (slot, pending)
    }

    fn unusable_outcome() -> Result<CompletionResponse<()>, CompletionError> {
        Err(CompletionError::ResponseError("empty".into()))
    }

    #[test]
    fn an_answered_attempt_rig_could_not_use_records_its_bodys_usage() {
        let (slot, pending) = sent();
        pending.answered(None).arrived(&bytes(serde_json::json!({
            "usage": { "input_tokens": 12, "output_tokens": 7 },
        })));

        let (_, attempt) = slot
            .awaiting()
            .settle(&unusable_outcome())
            .expect("an answered attempt");
        assert_eq!(slot.ledger().usage_of(attempt), Some(plain(12, 7)));
        assert_eq!(slot.close_round().0, [attempt]);
    }

    #[test]
    fn an_error_that_is_not_an_unusable_response_keeps_the_usage_too() {
        let (slot, pending) = sent();
        pending.answered(None).arrived(&bytes(serde_json::json!({
            "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 },
        })));
        let outcome: Result<CompletionResponse<()>, _> = Err(CompletionError::ProviderError(
            "a body it could not read".into(),
        ));

        let (_, attempt) = slot.awaiting().settle(&outcome).expect("answered");
        assert_eq!(slot.ledger().usage_of(attempt), Some(plain(5, 2)));
    }

    #[test]
    fn an_attempt_whose_body_was_never_read_or_reported_nothing_is_null() {
        let (slot, pending) = sent();
        let _unread = pending.answered(None);
        let (_, unread) = slot
            .awaiting()
            .settle(&unusable_outcome())
            .expect("answered");
        assert_eq!(slot.ledger().usage_of(unread), None, "never read");

        let pending = slot.sending().expect("the retry");
        pending
            .answered(None)
            .arrived(&bytes(serde_json::json!({ "content": [] })));
        let (_, empty) = slot
            .awaiting()
            .settle(&unusable_outcome())
            .expect("answered");
        assert_eq!(slot.ledger().usage_of(empty), None, "no usage in it");
        assert_eq!(slot.ledger().total(&[unread, empty]), None, "not zero");
    }

    #[test]
    fn a_completion_keeps_the_usage_rig_gave_it_over_its_bodys() {
        let (slot, pending) = sent();
        pending.answered(None).arrived(&bytes(serde_json::json!({
            "usage": { "input_tokens": 100, "output_tokens": 100 },
        })));
        let mut given = rig::completion::Usage::new();
        given.input_tokens = 1;
        given.output_tokens = 2;
        given.total_tokens = 3;
        let outcome: Result<CompletionResponse<()>, CompletionError> = Ok(CompletionResponse {
            choice: rig::OneOrMany::one(rig::completion::AssistantContent::text("hi")),
            usage: given,
            raw_response: (),
            message_id: None,
        });

        let (_, attempt) = slot.awaiting().settle(&outcome).expect("answered");
        assert_eq!(slot.ledger().usage_of(attempt), Some(plain(1, 2)));
    }

    #[test]
    fn two_ledgers_never_share_an_id() {
        let (first, second) = (Ledger::default(), Ledger::default());
        assert_eq!(first.mint_attempt(), AttemptId::new(1));
        assert_eq!(first.mint_attempt(), AttemptId::new(2));
        assert_eq!(second.mint_attempt(), AttemptId::new(1));
        assert_eq!(second.mint_call(), CallId::new(1));
    }

    /// An attempt `slot` sent, answered with a body reporting `usage`, and
    /// settled: what the ledger records for it.
    fn spend_through(slot: &CallSlot, usage: Usage) -> AttemptId {
        slot.begin_call();
        slot.candidate("sonnet", MODEL, Some(4096));
        let pending = slot.sending().expect("a call is in flight");
        pending.answered(None).arrived(&bytes(serde_json::json!({
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            },
        })));
        let (_, attempt) = slot
            .awaiting()
            .settle(&unusable_outcome())
            .expect("an answered attempt");
        attempt
    }

    /// One place: a second request waits for it, a waiter whose caller gives
    /// up leaves nothing held and nothing queued, and the next waiter takes
    /// the place the moment it is given back.
    #[tokio::test]
    async fn a_place_bounds_requests_in_flight_and_a_cancelled_wait_leaks_nothing() {
        let ledger = Ledger::new(Events::off(), Some(1), None);
        let held = ledger.place().await.expect("a bounded ledger gives places");
        assert_eq!(ledger.places_held(), 1);

        let mut waiting = Box::pin(ledger.place());
        assert!(
            futures_util::poll!(waiting.as_mut()).is_pending(),
            "the only place is held"
        );
        drop(waiting);
        assert_eq!(ledger.places_held(), 1, "the cancelled wait took nothing");

        let next = tokio::spawn({
            let ledger = ledger.clone();
            async move { ledger.place().await }
        });
        tokio::task::yield_now().await;
        drop(held);
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), next)
            .await
            .expect("the place passes on at once")
            .expect("the waiter ran");
        assert_eq!(ledger.places_held(), 1);
        drop(next);
        assert_eq!(ledger.places_held(), 0);

        assert!(
            Ledger::default().place().await.is_none(),
            "an unbounded ledger hands out none"
        );
    }

    /// A request answered `2xx` keeps its place past the reading of its body,
    /// until its attempt is settled and what it used recorded: a request that
    /// takes the place after it finds the budget that use spent.
    #[tokio::test]
    async fn an_answered_request_keeps_its_place_until_what_it_used_is_recorded() {
        let ledger = Ledger::new(Events::off(), Some(1), Some(19));
        let slot = CallSlot::new(ledger.clone(), Events::off(), None);
        slot.begin_call();
        slot.candidate("sonnet", MODEL, Some(4096));
        let place = ledger.place().await;
        let settling = slot.awaiting();
        let answered = slot.sending().expect("a call is in flight").answered(place);
        answered.arrived(&bytes(serde_json::json!({
            "usage": { "input_tokens": 12, "output_tokens": 7 },
        })));
        drop(answered);
        assert_eq!(ledger.places_held(), 1, "read, and not yet counted");
        assert!(!ledger.budget_spent());

        settling
            .settle(&unusable_outcome())
            .expect("an answered attempt");
        assert_eq!(ledger.places_held(), 0, "given back once counted");
        assert!(ledger.budget_spent());
    }

    /// The budget is spent once the attempts recorded reach it, whichever
    /// agent's slot recorded them.
    #[test]
    fn the_budget_is_spent_once_the_attempts_reach_it() {
        let ledger = Ledger::new(Events::off(), None, Some(30));
        let primary = CallSlot::new(ledger.clone(), Events::off(), None);
        let child = CallSlot::new(ledger.clone(), Events::off().for_agent("c1"), None);
        let out = ledger.budget_watch();
        spend_through(&primary, plain(12, 7));
        assert!(!ledger.budget_spent() && !*out.borrow());
        spend_through(&child, plain(5, 6));
        assert!(ledger.budget_spent(), "19 + 11 is 30");
        assert!(*out.borrow(), "whoever waits hears it");
        assert!(!Ledger::default().budget_spent(), "no budget, never spent");
    }

    /// A child's attempt counts in the round of its parent's it was launched
    /// in while that round runs, a grandchild's in both rounds above it; once
    /// a round has ended, an attempt under it is published apart, naming it.
    #[test]
    fn a_childs_spend_is_added_to_each_round_above_it() {
        let mut stream = crate::events::StreamBuilder::default();
        let mut seen = stream.subscribe(64);
        let events = stream.build();
        let ledger = Ledger::new(events.clone(), None, None);
        let primary = CallSlot::new(ledger.clone(), events.clone(), None);
        primary.open_round(3);
        let child = CallSlot::new(
            ledger.clone(),
            events.for_agent("c1"),
            Some((primary.clone(), primary.current_round())),
        );
        child.open_round(1);
        let grandchild = CallSlot::new(
            ledger.clone(),
            events.for_agent("c2"),
            Some((child.clone(), child.current_round())),
        );

        let own = spend_through(&primary, plain(1, 1));
        let by_child = spend_through(&child, plain(2, 2));
        let by_grandchild = spend_through(&grandchild, plain(3, 3));
        assert_eq!(
            primary.close_round(),
            (vec![own], vec![by_child, by_grandchild])
        );
        assert_eq!(primary.current_round(), Some(3), "the latest round, ended");

        let late = spend_through(&grandchild, plain(4, 4));
        assert_eq!(
            child.close_round(),
            (vec![by_child], vec![by_grandchild, late]),
            "the child's round still ran"
        );
        let usage_events: Vec<_> = std::iter::from_fn(|| seen.try_recv().ok())
            .filter_map(|received| match received {
                crate::harness::event::Received::Event(event)
                    if event.kind() == "model.round.usage" =>
                {
                    Some(event)
                }
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1, "only the round that had ended");
        assert_eq!(
            usage_events[0].subject.as_ref().map(|s| s.as_str()),
            Some("agent/primary")
        );
        assert_eq!(
            usage_events[0].payload,
            Payload::ModelRoundUsage {
                round: 3,
                attempts: vec![late],
                usage: Some(plain(4, 4)),
            }
        );
    }
}
