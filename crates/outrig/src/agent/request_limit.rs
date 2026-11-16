//! An agent's count of the model requests it makes, against a limit of its own.
//!
//! A model request is a logical one: a call the agent's model is asked to answer, named once as
//! it begins ([`CallSlot::begin_call`]), however many requests its retries and its moves to
//! another model send. It is counted once, when a provider first answers it -- a completion, a
//! reply the loop could not use and asked again for, a refusal for what it carried -- so a call
//! every attempt of which failed before an answer, a provider's outage, spends nothing. The
//! count is the agent's own: a child's is not added to its parent's.
//!
//! At the limit, the agent's round ends before its next request ([`RequestLimit::stop`], which
//! the round's hook asks before each model call), and a child's runner ends what the child holds
//! `limit`. Nothing is released or rolled back. The owner -- or, for a child an agent launched,
//! that agent -- resets the count or raises the limit, and the agent goes on.
//!
//! The model is told in its inputs, never on a channel: a warning as the count remaining first
//! falls to each threshold the owner set, and a notice when the count is reset or the limit
//! raised. Each is said once, in the next tool result the model reads or at its next round's
//! opening, whichever comes first (`plan/phase/0003-python/model-policy.md`, "Warnings and
//! notices").
//!
//! [`CallSlot::begin_call`]: super::ledger::CallSlot::begin_call

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::harness::event::Payload;
use crate::harness::{ModelRequests, RoundStop};

/// What an agent's count starts with: its limit, if it has one, and the counts remaining at
/// which its model is warned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LimitConfig {
    pub(crate) limit: Option<u32>,
    pub(crate) warnings: Vec<u32>,
}

/// `value`, a line's count of model requests: a JSON integer from 1 through `u32::MAX`.
pub(crate) fn count_of(value: &Value) -> Option<NonZeroU32> {
    NonZeroU32::new(u32::try_from(value.as_u64()?).ok()?)
}

impl LimitConfig {
    /// What a launch's `model_requests` names -- `{"limit", "warnings"}`, each `null` for the
    /// session's -- over `session`'s, which a launch that names nothing has whole.
    pub(crate) fn of_launch(said: &Value, session: &LimitConfig) -> Result<Self, String> {
        let count = |value: &Value| {
            count_of(value)
                .map(NonZeroU32::get)
                .ok_or_else(|| format!("model_requests takes counts of at least 1, not {value}"))
        };
        if said.is_null() {
            return Ok(session.clone());
        }
        let limit = match &said["limit"] {
            Value::Null => session.limit,
            limit => Some(count(limit)?),
        };
        let warnings = match &said["warnings"] {
            Value::Null => session.warnings.clone(),
            Value::Array(warnings) => warnings.iter().map(count).collect::<Result<_, _>>()?,
            other => return Err(format!("model_requests' warnings are a list, not {other}")),
        };
        Ok(Self { limit, warnings })
    }

    /// What a launch line says of `limit` and `warnings`, either `None` for the session's.
    pub(crate) fn launch_line(limit: Option<NonZeroU32>, warnings: Option<&[NonZeroU32]>) -> Value {
        match (limit, warnings) {
            (None, None) => Value::Null,
            (limit, warnings) => json!({
                "limit": limit.map(NonZeroU32::get),
                "warnings": warnings.map(|w| w.iter().map(|n| n.get()).collect::<Vec<_>>()),
            }),
        }
    }
}

/// One agent's count, its limit, and what its model is still to be told of them. Cheap to
/// clone: clones share one count, which the agent's call slot, its round's hook, its tool, its
/// runner and the registry's record of it all read.
#[derive(Clone, Default)]
pub(crate) struct RequestLimit(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    spent: u32,
    limit: Option<u32>,
    /// The thresholds, highest first, without repeats.
    warnings: Vec<u32>,
    /// The thresholds warned at, or passed, since the count or the limit last changed.
    warned: BTreeSet<u32>,
    /// What the model is to be told and has not been: a warning, or a notice of a change.
    queued: Vec<Notice>,
    /// What a round's opening carried that the round may yet lose: a round that fails before
    /// its code ran keeps nothing of its opening, so the next opening carries it again.
    held: Vec<Notice>,
    /// Whether the agent's arrival at its limit has been recorded since it last changed.
    told: bool,
    /// Where the model's word that the limit cut a round short stands.
    cut: Cut,
}

/// Whether the model is still to be told that the limit cut a round short -- ended it before a
/// model request it would have made. A change's notice tells it, as the notice is delivered
/// rather than as it is made, so a change that replaces an earlier notice neither loses the word
/// nor says it twice.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// No round was cut short since the model was last told of one.
    #[default]
    Told,
    /// A round was cut short, and nothing the model kept has said so.
    Untold,
    /// A round's opening said so, and the round has neither kept its opening nor lost it.
    Opening,
}

/// What the model is to be told: a warning, or the notice of an owner's change, which carries
/// the word of a cut when it is delivered.
enum Notice {
    Warning(String),
    Change(String),
}

/// The texts of `notices`, the change's with the word of a cut when `cut`.
fn texts(notices: &[Notice], cut: bool) -> Vec<String> {
    notices
        .iter()
        .map(|notice| match notice {
            Notice::Warning(text) => text.clone(),
            Notice::Change(text) if cut => format!("{text}{CUT}"),
            Notice::Change(text) => text.clone(),
        })
        .collect()
}

/// Whether `notices` hold a change's notice, the one that can tell of a cut.
fn tells_of_a_change(notices: &[Notice]) -> bool {
    notices
        .iter()
        .any(|notice| matches!(notice, Notice::Change(_)))
}

/// The agent's count at its limit, and whether this is the first time it was asked since the
/// agent arrived there: its arrival is recorded once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reached {
    pub(crate) spent: u32,
    pub(crate) limit: u32,
    pub(crate) first: bool,
}

/// How an owner changes an agent's count: a reset sets the count to zero and keeps the limit,
/// or replaces it; a raise replaces the limit and keeps the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Reset(Option<NonZeroU32>),
    Raise(NonZeroU32),
}

impl State {
    fn read(&self) -> ModelRequests {
        ModelRequests {
            spent: self.spent,
            limit: self.limit,
            warnings: self.warnings.clone(),
            warned: self.warned.iter().rev().copied().collect(),
        }
    }

    /// The count and the limit, when the count has reached the limit.
    fn reached(&self) -> Option<(u32, u32)> {
        self.limit
            .filter(|limit| self.spent >= *limit)
            .map(|limit| (self.spent, limit))
    }
}

impl RequestLimit {
    /// A count from zero under `config`.
    pub(crate) fn new(config: &LimitConfig) -> Self {
        let mut warnings = config.warnings.clone();
        warnings.sort_unstable_by(|a, b| b.cmp(a));
        warnings.dedup();
        Self(Arc::new(Mutex::new(State {
            limit: config.limit,
            warnings,
            ..State::default()
        })))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A model request was answered for the first time: count it, and queue the warning the
    /// count remaining now calls for, if it calls for one, returning its event. The lowest
    /// threshold it fell to is the one named, and every threshold at or above it is passed. None
    /// is queued once nothing remains, since no request follows to carry it.
    pub(crate) fn answered(&self) -> Option<Payload> {
        let mut state = self.state();
        state.spent = state.spent.saturating_add(1);
        let limit = state.limit?;
        let remaining = limit.saturating_sub(state.spent);
        let threshold = state
            .warnings
            .iter()
            .copied()
            .filter(|threshold| remaining <= *threshold && !state.warned.contains(threshold))
            .min()?;
        let passed: Vec<u32> = state
            .warnings
            .iter()
            .copied()
            .filter(|threshold| *threshold >= remaining)
            .collect();
        state.warned.extend(passed);
        if remaining == 0 {
            return None;
        }
        state
            .queued
            .push(Notice::Warning(warning(remaining, limit)));
        Some(Payload::ModelLimitWarned {
            threshold,
            remaining,
            spent: state.spent,
            limit,
        })
    }

    /// The count and the limit, when the count has reached the limit, and whether the agent's
    /// arrival there is first heard of now: once per arrival, so it is recorded once.
    pub(crate) fn reached(&self) -> Option<Reached> {
        let mut state = self.state();
        let (spent, limit) = state.reached()?;
        let first = !state.told;
        state.told = true;
        Some(Reached {
            spent,
            limit,
            first,
        })
    }

    /// Why the round asking is to end before the model request it would make, when the count
    /// has reached the limit: the round's hook asks before each model call, and a round so
    /// ended is one the limit cut short.
    pub(crate) fn stop(&self) -> Option<RoundStop> {
        let mut state = self.state();
        let (spent, limit) = state.reached()?;
        state.cut = Cut::Untold;
        Some(RoundStop::ModelRequestLimit { spent, limit })
    }

    /// The count, the limit and the thresholds, as the owner reads them.
    pub(crate) fn read(&self) -> ModelRequests {
        self.state().read()
    }

    /// Apply `change`, the owner's, and queue the notice that tells the model of it: what
    /// changed, what remains, and, when the limit cut its last round short and the model has
    /// not been told, that it did. What it has not yet been told is dropped -- a warning, or an
    /// earlier notice an opening held for a round that may yet lose it -- since the notice says
    /// what holds now; the word of a cut is not, as the notice delivered next carries it. Each
    /// threshold below the count now remaining can be warned at again. The change's event, and
    /// the count and limit after.
    pub(crate) fn change(&self, change: Change) -> (Payload, ModelRequests) {
        let mut state = self.state();
        let (spent_before, limit_before) = (state.spent, state.limit);
        match change {
            Change::Reset(limit) => {
                state.spent = 0;
                if let Some(limit) = limit {
                    state.limit = Some(limit.get());
                }
            }
            Change::Raise(limit) => state.limit = Some(limit.get()),
        }
        if let Some(limit) = state.limit {
            let remaining = limit.saturating_sub(state.spent);
            state.warned.retain(|threshold| *threshold >= remaining);
        }
        state.told = false;
        state.queued.clear();
        state.held.clear();
        if let Some(text) = notice(change, limit_before, state.spent, state.limit) {
            state.queued.push(Notice::Change(text));
        }
        let changed = Payload::ModelLimitReset {
            spent_before,
            limit_before,
            spent_after: state.spent,
            limit_after: state.limit,
        };
        (changed, state.read())
    }

    /// What the model is to be told in the result it reads next, taken: said there, it is said.
    /// A result is made only once its round's code ran, so the round keeps its opening, and a
    /// cut the opening told of is not told again.
    pub(crate) fn take_notices(&self) -> Vec<String> {
        let mut state = self.state();
        let queued = std::mem::take(&mut state.queued);
        let tell = state.cut == Cut::Untold && tells_of_a_change(&queued);
        if tell || state.cut == Cut::Opening {
            state.cut = Cut::Told;
        }
        texts(&queued, tell)
    }

    /// What a round's opening is to tell the model: whatever an earlier opening carried that its
    /// round lost, then what is new. Held until [`RequestLimit::opening_read`] says the round
    /// kept its opening; a cut an earlier opening told of, which its round lost, is told again.
    pub(crate) fn for_opening(&self) -> Vec<String> {
        let mut state = self.state();
        let queued = std::mem::take(&mut state.queued);
        state.held.extend(queued);
        if state.cut == Cut::Opening {
            state.cut = Cut::Untold;
        }
        let tell = state.cut == Cut::Untold && tells_of_a_change(&state.held);
        if tell {
            state.cut = Cut::Opening;
        }
        texts(&state.held, tell)
    }

    /// The round kept its opening -- it ended, or ran code -- so what the opening carried was
    /// said.
    pub(crate) fn opening_read(&self) {
        let mut state = self.state();
        state.held.clear();
        if state.cut == Cut::Opening {
            state.cut = Cut::Told;
        }
    }

    /// As if `spent` model requests had been counted, for a test standing in for a count a
    /// resume carries.
    #[cfg(test)]
    pub(crate) fn seed(&self, spent: u32) {
        self.state().spent = spent;
    }
}

/// The warning the model reads once `remaining` of its `limit` remain.
fn warning(remaining: u32, limit: u32) -> String {
    format!(
        "[outrig] {} of your limit of {limit}. When none remain, your round ends before its next \
         model request, and you make none until the limit is reset or raised. Finish what is \
         open and leave the rest in a state you can take up again.",
        remain(remaining)
    )
}

/// `N model requests remain`, in agreement with `N`.
fn remain(remaining: u32) -> String {
    match remaining {
        1 => "1 model request remains".to_string(),
        n => format!("{n} model requests remain"),
    }
}

/// What a change's notice adds when the limit cut the agent's last round short.
const CUT: &str = " Your limit ended your last round before its next model request; nothing it \
                   left running was stopped.";

/// What the model is told of `change`, which took the count to `spent` and the limit from
/// `before` to `limit`. Nothing for an agent with no limit, which a change cannot take away: it
/// was never told of one.
fn notice(change: Change, before: Option<u32>, spent: u32, limit: Option<u32>) -> Option<String> {
    let limit = limit?;
    let text = match change {
        Change::Reset(given) => {
            let set = match given {
                Some(_) if before != Some(limit) => {
                    format!(", and your limit set to {limit}")
                }
                _ => String::new(),
            };
            format!(
                "[outrig] Your model-request count was reset{set}: {} of your limit of {limit}.",
                remain(limit.saturating_sub(spent))
            )
        }
        Change::Raise(_) => {
            let how = match before {
                Some(old) if old > limit => "lowered",
                Some(old) if old == limit => "kept",
                _ => "raised",
            };
            format!(
                "[outrig] Your model-request limit was {how} to {limit}: {} of it.",
                remain(limit.saturating_sub(spent))
            )
        }
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limited(limit: u32, warnings: &[u32]) -> RequestLimit {
        RequestLimit::new(&LimitConfig {
            limit: Some(limit),
            warnings: warnings.to_vec(),
        })
    }

    fn nonzero(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    /// The threshold, the count remaining and the count a warning's event names.
    fn warned(event: Payload) -> (u32, u32, u32) {
        match event {
            Payload::ModelLimitWarned {
                threshold,
                remaining,
                spent,
                ..
            } => (threshold, remaining, spent),
            other => panic!("not a warning: {other:?}"),
        }
    }

    /// Each threshold is warned at once, as the count remaining first falls to it, and names the
    /// lowest it fell to.
    #[test]
    fn each_threshold_is_warned_at_once() {
        let count = limited(20, &[1, 10]);
        let mut said = Vec::new();
        for _ in 0..20 {
            said.extend(count.answered().map(warned));
        }
        assert_eq!(said, [(10, 10, 10), (1, 1, 19)]);
        let read = count.read();
        assert_eq!((read.spent, read.limit), (20, Some(20)));
        assert_eq!(read.warnings, [10, 1]);
        assert_eq!(read.warned, [10, 1]);
        let notices = count.take_notices();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].starts_with("[outrig] 10 model requests remain of your limit of 20."));
        assert!(notices[1].starts_with("[outrig] 1 model request remains of your limit of 20."));
        assert!(count.take_notices().is_empty(), "said once");
    }

    /// A limit below a threshold passes it at the first request, and is warned of once, naming
    /// the lowest threshold passed.
    #[test]
    fn a_limit_below_the_thresholds_is_warned_of_once() {
        let count = limited(3, &[10, 2]);
        let first = warned(count.answered().expect("a warning"));
        assert_eq!(first, (2, 2, 1));
        assert!(count.answered().is_none());
        assert_eq!(count.read().warned, [10, 2]);
    }

    /// Nothing is queued once nothing remains: no request follows to carry it.
    #[test]
    fn nothing_is_said_once_none_remain() {
        let count = limited(1, &[1]);
        assert!(count.answered().is_none());
        assert_eq!(count.read().warned, [1], "passed all the same");
        assert!(count.take_notices().is_empty());
        assert_eq!(
            count.stop(),
            Some(RoundStop::ModelRequestLimit { spent: 1, limit: 1 })
        );
    }

    /// A reset re-arms each threshold below the count now remaining; a raise does too, keeping
    /// the count.
    #[test]
    fn a_reset_or_a_raise_re_arms_what_now_lies_ahead() {
        let count = limited(20, &[10, 1]);
        for _ in 0..18 {
            count.answered();
        }
        assert_eq!(count.read().warned, [10]);
        let (changed, after) = count.change(Change::Reset(None));
        assert!(
            matches!(
                changed,
                Payload::ModelLimitReset {
                    spent_before: 18,
                    spent_after: 0,
                    ..
                }
            ),
            "{changed:?}"
        );
        assert_eq!((after.spent, after.limit), (0, Some(20)));
        assert!(
            after.warned.is_empty(),
            "10 is re-armed, and 1 was never warned"
        );

        let count = limited(20, &[10, 1]);
        for _ in 0..19 {
            count.answered();
        }
        count.take_notices();
        assert_eq!(count.read().warned, [10, 1]);
        let (_, after) = count.change(Change::Raise(nonzero(30)));
        assert_eq!((after.spent, after.limit), (19, Some(30)));
        assert!(after.warned.is_empty(), "11 remain, so both are re-armed");
        let notices = count.take_notices();
        assert_eq!(
            notices,
            ["[outrig] Your model-request limit was raised to 30: 11 model requests remain of it."]
        );
    }

    /// A change after the limit cut a round short says so; a warning not yet told is dropped for
    /// the notice, which says what holds now.
    #[test]
    fn the_notice_says_the_limit_cut_the_last_round() {
        let count = limited(2, &[1]);
        count.answered();
        count.answered();
        assert!(count.stop().is_some(), "the round's next call is stopped");
        assert_eq!(
            count.reached(),
            Some(Reached {
                spent: 2,
                limit: 2,
                first: true
            })
        );
        assert!(
            count.reached().is_some_and(|reached| !reached.first),
            "first once per arrival"
        );
        count.change(Change::Reset(Some(nonzero(5))));
        assert!(count.stop().is_none());
        let notices = count.take_notices();
        assert_eq!(
            notices,
            [
                "[outrig] Your model-request count was reset, and your limit set to 5: 5 model \
                 requests remain of your limit of 5. Your limit ended your last round before its \
                 next model request; nothing it left running was stopped."
            ]
        );
    }

    /// A limit at 1, spent, with the round's next call stopped: a round the limit cut short.
    fn cut_short() -> RequestLimit {
        let count = limited(1, &[]);
        count.answered();
        assert!(count.stop().is_some());
        count
    }

    /// A second change before the model reads the first's notice replaces it, and still tells
    /// of the cut the first told of -- whether the notice waited for a result or an opening
    /// its round lost carried it -- until a notice that tells of it is kept.
    #[test]
    fn successive_changes_keep_an_untold_cut() {
        let count = cut_short();
        count.change(Change::Reset(None));
        count.change(Change::Raise(nonzero(3)));
        let notices = count.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with("[outrig] Your model-request limit was raised to 3"));
        assert!(notices[0].ends_with(CUT), "{notices:?}");
        count.change(Change::Reset(None));
        assert!(
            !count.take_notices()[0].contains(CUT),
            "read, it is not told again"
        );

        let count = cut_short();
        count.change(Change::Reset(None));
        assert!(count.for_opening()[0].ends_with(CUT));
        count.change(Change::Raise(nonzero(4)));
        let opening = count.for_opening();
        assert_eq!(opening.len(), 1, "{opening:?}");
        assert!(opening[0].ends_with(CUT), "the round lost it: {opening:?}");
        count.opening_read();
        count.change(Change::Reset(None));
        assert!(!count.for_opening()[0].contains(CUT), "kept, it was said");
    }

    /// A change while a round runs on an opening that told of a cut does not tell of it again:
    /// not in the result the round's code reads next, and not at the next opening once the round
    /// kept its own. Had the round lost its opening, the next one tells of it.
    #[test]
    fn a_cut_an_opening_told_is_told_once() {
        let count = cut_short();
        count.change(Change::Reset(None));
        assert!(count.for_opening()[0].ends_with(CUT));
        count.change(Change::Raise(nonzero(3)));
        assert_eq!(
            count.take_notices(),
            ["[outrig] Your model-request limit was raised to 3: 3 model requests remain of it."],
            "the round's code ran, so it kept the opening that said it"
        );
        count.opening_read();
        count.change(Change::Reset(None));
        assert!(!count.for_opening()[0].contains(CUT));

        let count = cut_short();
        count.change(Change::Reset(None));
        assert!(count.for_opening()[0].ends_with(CUT));
        count.change(Change::Raise(nonzero(3)));
        count.opening_read();
        assert!(
            !count.for_opening()[0].contains(CUT),
            "a change while the round's first call was out, and the round kept its opening"
        );

        let count = cut_short();
        count.change(Change::Reset(None));
        assert!(count.for_opening()[0].ends_with(CUT));
        count.change(Change::Raise(nonzero(3)));
        assert!(
            count.for_opening()[0].ends_with(CUT),
            "the same, and the round lost its opening"
        );
    }

    /// Reaching the limit as a round ends on its own cuts nothing, and neither does a change
    /// before the round's next call: the notice claims no cut. A reset drops what an opening held
    /// for a round that failed before its code ran, which no longer holds.
    #[test]
    fn the_notice_claims_no_cut_that_did_not_happen() {
        let count = limited(3, &[1]);
        count.answered();
        count.answered();
        assert_eq!(count.for_opening().len(), 1, "the warning that one remains");
        count.answered();
        assert!(count.reached().is_some(), "at the limit, no round cut");
        count.change(Change::Raise(nonzero(4)));
        assert_eq!(
            count.for_opening(),
            ["[outrig] Your model-request limit was raised to 4: 1 model request remains of it."],
            "the held warning is gone, and no cut is claimed"
        );
    }

    /// An opening holds what it carried until the round keeps it: a round that loses its opening
    /// has the next one carry it again, and a tool result never repeats it.
    #[test]
    fn an_opening_holds_what_it_carried_until_it_is_kept() {
        let count = limited(3, &[2]);
        count.answered();
        let first = count.for_opening();
        assert_eq!(first.len(), 1);
        assert!(count.take_notices().is_empty(), "not said twice");
        assert_eq!(count.for_opening(), first, "the reopen carries it again");
        count.opening_read();
        assert!(count.for_opening().is_empty());
    }

    /// A launch names a limit, thresholds, both or neither, the rest the session's; a count of
    /// 0, or one that is no count, is refused.
    #[test]
    fn a_launch_names_what_it_changes() {
        let session = LimitConfig {
            limit: Some(10),
            warnings: vec![3],
        };
        let of = |said: Value| LimitConfig::of_launch(&said, &session);
        assert_eq!(of(Value::Null), Ok(session.clone()));
        assert_eq!(
            of(json!({"limit": 2, "warnings": null})),
            Ok(LimitConfig {
                limit: Some(2),
                warnings: vec![3]
            })
        );
        assert_eq!(
            of(json!({"limit": 2, "warnings": [1]})),
            Ok(LimitConfig {
                limit: Some(2),
                warnings: vec![1]
            })
        );
        for bad in [
            json!({"limit": 0}),
            json!({"limit": "2"}),
            json!({"limit": 2, "warnings": [0]}),
            json!({"limit": 2, "warnings": 1}),
        ] {
            assert!(of(bad.clone()).is_err(), "{bad}");
        }
        let line = LimitConfig::launch_line(Some(nonzero(4)), None);
        assert_eq!(
            of(line),
            Ok(LimitConfig {
                limit: Some(4),
                warnings: vec![3]
            })
        );
        assert_eq!(LimitConfig::launch_line(None, None), Value::Null);
    }

    /// An agent with no limit counts, and is never stopped or told anything.
    #[test]
    fn no_limit_counts_and_says_nothing() {
        let count = RequestLimit::default();
        assert!(count.answered().is_none());
        assert!(count.stop().is_none());
        assert_eq!(count.read().spent, 1);
        let (_, after) = count.change(Change::Reset(None));
        assert_eq!(after.spent, 0);
        assert!(count.take_notices().is_empty());
    }
}
