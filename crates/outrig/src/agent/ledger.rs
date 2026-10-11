//! A session's model spend, attempt by attempt.
//!
//! Every model call the loop decides to make has a [`CallId`], and every
//! request actually sent for it -- a retry, a move to the next model of a
//! chain -- an [`AttemptId`] of its own. Each attempt is recorded once, as a
//! `model.attempt` event, failed ones included, with what the provider said it
//! used or `null` when it said nothing.
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig::completion::{CompletionError, CompletionResponse};

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
    #[cfg(test)]
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
    /// A ledger recording its attempts in `events`.
    pub(crate) fn new(events: Events) -> Self {
        Self {
            inner: Arc::new(Inner {
                events,
                ..Inner::default()
            }),
        }
    }

    pub(crate) fn events(&self) -> &Events {
        &self.inner.events
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

    /// Record `attempt`, and publish it. Both under the book's lock, so the
    /// order the log holds the records in is the order they were applied in,
    /// and a replay of it applies them the same way.
    fn record(&self, attempt: ModelAttempt) {
        let mut book = self.book();
        book.attempt(attempt.call_id, attempt.attempt_id, attempt.usage);
        self.inner.events.emit(Payload::ModelAttempt(attempt));
    }

    /// Take `usage` as attempt `attempt`'s, arriving after its event: it
    /// replaces a null usage once, and is refused -- and the refusal recorded
    /// -- otherwise. Spend from a later attempt of the same call is that
    /// attempt's, under its own id, never a correction of this one.
    ///
    /// Nothing calls this yet: every provider this loop speaks reports usage
    /// in the response that ends its attempt, a failed request reports none,
    /// and nothing streams. A streamed reply's closing usage, or a provider's
    /// billing callback, is what would.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn late_usage(&self, attempt: AttemptId, usage: Usage) -> Late {
        let mut book = self.book();
        let late = book.late(attempt, usage);
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
/// are made one at a time, so one call is in flight at most.
///
/// [`ChainDeadline`]: super::retry::ChainDeadline
#[derive(Clone, Default)]
pub(crate) struct CallSlot {
    ledger: Ledger,
    state: Arc<Mutex<SlotState>>,
}

#[derive(Default)]
struct SlotState {
    /// The id the round's hook named the next call by, for the chain to take.
    reserved: Option<CallId>,
    call: Option<InFlight>,
    /// The attempts the round in progress has made, failed ones included: its
    /// members, which its total is the sum over.
    round: Vec<AttemptId>,
}

struct InFlight {
    call: CallId,
    settings: Arc<Settings>,
    /// The attempt a `2xx` answered, until the model layer settles it.
    answered: Option<AttemptId>,
    /// The latest attempt the current candidate was sent.
    last: Option<AttemptId>,
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
    pub(crate) fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            state: Arc::default(),
        }
    }

    pub(crate) fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A round begins: the attempts from here on are its.
    pub(crate) fn open_round(&self) {
        self.state().round.clear();
    }

    /// The attempts the round in progress has made.
    pub(crate) fn round_attempts(&self) -> Vec<AttemptId> {
        self.state().round.clone()
    }

    /// The call in flight and the request that answered it, once one has.
    pub(crate) fn answered_by(&self) -> Option<(CallId, AttemptId)> {
        let state = self.state();
        let call = state.call.as_ref()?;
        Some((call.call, call.last?))
    }

    /// Name the next call now, before the chain is asked to make it, so what
    /// is recorded before the call -- its manifest -- names it too.
    pub(crate) fn reserve_call(&self) -> CallId {
        let call = self.ledger.mint_call();
        self.state().reserved = Some(call);
        call
    }

    /// A call begins: the reserved id, or a fresh one for a call the round's
    /// hook never saw.
    pub(crate) fn begin_call(&self) -> CallId {
        let mut state = self.state();
        let call = state
            .reserved
            .take()
            .unwrap_or_else(|| self.ledger.mint_call());
        state.call = Some(InFlight {
            call,
            settings: Arc::default(),
            answered: None,
            last: None,
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
        self.ledger.record(ModelAttempt {
            call_id: call,
            attempt_id: attempt,
            model: settings.model.clone(),
            identifier: settings.identifier.clone(),
            max_tokens: settings.max_tokens,
            error,
            usage,
        });
        let mut state = self.state();
        if let Some(in_flight) = &mut state.call
            && in_flight.call == call
        {
            in_flight.last = Some(attempt);
        }
        state.round.push(attempt);
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
    /// usable, records it.
    pub(crate) fn answered(mut self) {
        self.done = true;
        if let Some(call) = &mut self.slot.state().call
            && call.call == self.call
        {
            call.answered = Some(self.attempt);
        }
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
    /// completion of it, and why not otherwise. The ids, or `None` when no
    /// request was answered -- a failure the HTTP layer already recorded.
    pub(crate) fn settle<R>(
        mut self,
        outcome: &Result<CompletionResponse<R>, CompletionError>,
    ) -> Option<(CallId, AttemptId)> {
        self.done = true;
        let (call, attempt, settings) = {
            let mut state = self.slot.state();
            let in_flight = state.call.as_mut()?;
            let attempt = in_flight.answered.take()?;
            (in_flight.call, attempt, Arc::clone(&in_flight.settings))
        };
        let (error, usage) = match outcome {
            Ok(response) => (None, reported(response.usage)),
            Err(CompletionError::ResponseError(message)) => {
                (Some(super::retry::unusable(message)), None)
            }
            Err(error) => (Some(error.to_string()), None),
        };
        self.slot.record(call, attempt, &settings, error, usage);
        Some((call, attempt))
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
                let attempt = call.answered.take()?;
                Some((call.call, attempt, Arc::clone(&call.settings)))
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

    #[test]
    fn two_ledgers_never_share_an_id() {
        let (first, second) = (Ledger::default(), Ledger::default());
        assert_eq!(first.mint_attempt(), AttemptId::new(1));
        assert_eq!(first.mint_attempt(), AttemptId::new(2));
        assert_eq!(second.mint_attempt(), AttemptId::new(1));
        assert_eq!(second.mint_call(), CallId::new(1));
    }
}
