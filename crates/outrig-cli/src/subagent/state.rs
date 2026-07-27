//! One subagent's result inbox and run state.
//!
//! The inbox is **latest-only, not a queue**: [`SubagentShared::publish`]
//! overwrites the held value and bumps a version counter. A parent that reads
//! after three publishes sees the third and its watermark jumps past the two it
//! missed -- there is no backlog and no ordering to preserve.
//!
//! Reads are edge-triggered on that version, which is what makes "did reading
//! consume it?" a non-question: the parent keeps a watermark, and a second read
//! with nothing new simply blocks.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::sync::watch;

/// What a round published. Exactly one of these; `set_result` takes
/// `{result}` xor `{error}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Result(String),
    Error(String),
}

/// What the parent is told when a report is cut off at the output-token
/// ceiling.
///
/// Shared because two places say it: [`Snapshot::read`], for a round that ends
/// having published nothing, and `set_result` itself, which publishes this
/// verbatim once it stops asking the subagent to retry. Near-duplicate wordings
/// would drift, and the parent cannot tell which path it came down anyway.
pub const TRUNCATED_REPORT: &str = "\
    subagent hit its output token limit while writing its report, so nothing \
    was recorded. Re-ask it for a narrower or shorter report, or raise \
    max-tokens for this agent.";

/// Why a round is heading for -- or ended in -- publishing nothing.
///
/// Recorded as it becomes known so that a silent round can say *why* instead of
/// only that it stopped. Ordering matters: a truncated report is diagnosed
/// mid-round and outranks the early exit that follows from it, since "it could
/// not fit its report" explains "it ran out of tool calls" and not the reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilentCause {
    /// A `set_result` call arrived with `status` and no `body`.
    TruncatedReport,
    /// The agent loop was cut short -- the tool-call budget, or a hook that
    /// stopped it. Carries the reason the loop gave.
    EndedEarly(String),
}

/// Whether the subagent is working.
///
/// `Idle` carries whether the round that just ended published anything,
/// because "went idle without publishing" is the error the parent needs to
/// see, and it is *per round*: a subagent that published and then finished is
/// idle but not failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Idle { published: bool },
}

impl RunState {
    /// The round ended without publishing anything -- the failure the parent
    /// is told about, and its cue to poke the subagent with a new prompt.
    ///
    /// Centralized because three places key off it: whether the subagent is
    /// readable, what a read yields, and what the round driver writes to the
    /// transcript.
    pub fn ended_without_publishing(self) -> bool {
        self == RunState::Idle { published: false }
    }
}

/// A point-in-time view of one subagent, cheap to clone out of the watch
/// channel so no lock is held across an await.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: u64,
    pub state: RunState,
    pub outcome: Option<Outcome>,
    /// Why this round looks like it will publish nothing, when that is known.
    /// Kept so a round that ends up publishing nothing can say *why* instead of
    /// just that it stopped.
    pub silent_cause: Option<SilentCause>,
}

impl Snapshot {
    /// Whether a parent at `watermark` has something to collect.
    ///
    /// Two ways that happens: a newer version was published, or the subagent
    /// stopped without publishing during its last round. The second is the
    /// error case, and it stays true until the parent pokes the subagent back
    /// into `Running` -- it is deliberately level-triggered, since nothing
    /// about a stalled subagent changes just by being looked at.
    pub fn readable(&self, watermark: u64) -> bool {
        self.version > watermark
            || (self.version == watermark && self.state.ended_without_publishing())
    }

    /// What a read at `watermark` yields. `None` when not readable.
    pub fn read(&self, watermark: u64) -> Option<Outcome> {
        if !self.readable(watermark) {
            return None;
        }
        if self.version > watermark {
            // A round may end without publishing while an older result is
            // still uncollected; the unread result is the more useful answer.
            return self.outcome.clone();
        }
        Some(Outcome::Error(match &self.silent_cause {
            Some(SilentCause::TruncatedReport) => TRUNCATED_REPORT.to_string(),
            Some(SilentCause::EndedEarly(reason)) => {
                format!("subagent stopped before reporting: {reason}")
            }
            None => "subagent stopped without calling outrig__set_result".to_string(),
        }))
    }
}

/// The half of a subagent that its own task and its tools write to, shared by
/// `Arc`. The parent's watermark deliberately lives elsewhere (in the
/// registry): it is the *parent's* read position, not the subagent's state.
#[derive(Debug)]
pub struct SubagentShared {
    tx: watch::Sender<Snapshot>,
    /// Version at the current round's start, so `end_round` can tell whether
    /// this round published.
    round_start_version: Mutex<u64>,
    /// Consecutive truncated `set_result` attempts in the current round.
    ///
    /// Deliberately not in [`Snapshot`]: the parent needs the *fact* that the
    /// report did not fit, never the tally. The tally exists only so
    /// `set_result` can stop asking for a retry that keeps failing the same
    /// way.
    truncated_attempts: AtomicU32,
    /// Whether this round already gave up on the subagent's report and told
    /// the parent so.
    ///
    /// A latch, not a counter, and cleared only by [`Self::begin_round`]. It has
    /// to survive the `publish` that giving up performs: without it that
    /// publish reset the tally, so the very next truncated call counted as a
    /// first attempt and the whole three-strike cycle began again -- the loop
    /// slowed to a third of its old rate rather than stopped, re-warning and
    /// re-publishing every three calls.
    gave_up: AtomicBool,
    /// Prompts delivered by the parent mid-round, awaiting injection into the
    /// next model call. See [`crate::subagent::injection`].
    injections: Mutex<Vec<String>>,
}

impl SubagentShared {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(Snapshot {
            version: 0,
            state: RunState::Running,
            outcome: None,
            silent_cause: None,
        });
        Self {
            tx,
            round_start_version: Mutex::new(0),
            truncated_attempts: AtomicU32::new(0),
            gave_up: AtomicBool::new(false),
            injections: Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.tx.subscribe()
    }

    /// Record a round's outcome. Does **not** end the round -- the subagent
    /// keeps working, and may publish again, in which case the later value
    /// wins.
    pub fn publish(&self, outcome: Outcome) {
        self.tx.send_modify(|snap| {
            snap.version += 1;
            snap.outcome = Some(outcome);
        });
    }

    /// Note that a report arrived whole, restoring the truncation retry budget.
    ///
    /// Separate from [`Self::publish`] rather than folded into it, because not
    /// every publish is evidence that the ceiling is survivable: giving up
    /// publishes too, and resetting there re-armed the very loop the give-up
    /// exists to end. Only a body that actually fit says anything about what
    /// the model can produce.
    pub fn note_report_fit(&self) {
        self.truncated_attempts.store(0, Ordering::SeqCst);
    }

    /// Whether this round has already given up on reporting.
    pub fn has_given_up(&self) -> bool {
        self.gave_up.load(Ordering::SeqCst)
    }

    /// Give up on the subagent's report and tell the parent why, once.
    ///
    /// Returns whether this call was the one that gave up, so the caller can
    /// publish and warn exactly once no matter how many more truncated calls
    /// arrive behind it.
    pub fn give_up(&self) -> bool {
        !self.gave_up.swap(true, Ordering::SeqCst)
    }

    /// Record that a report was cut off part-way, and return how many
    /// consecutive times that has now happened this round.
    ///
    /// The count is what lets `set_result` stop asking: the first failure is
    /// worth a retry, the third is the same failure three times. Per round, so a
    /// later successful round does not inherit an earlier round's explanation.
    pub fn note_truncated_attempt(&self) -> u32 {
        self.tx
            .send_modify(|snap| snap.silent_cause = Some(SilentCause::TruncatedReport));
        self.truncated_attempts.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Record that the agent loop was cut short, so a round that publishes
    /// nothing can name the reason it stopped.
    ///
    /// Does not displace a truncated report: running out of tool calls is what
    /// *follows* from a report that would not fit, and the report is the cause
    /// worth reporting.
    pub fn note_ended_early(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.tx.send_modify(|snap| {
            if snap.silent_cause.is_none() {
                snap.silent_cause = Some(SilentCause::EndedEarly(reason));
            }
        });
    }

    pub fn begin_round(&self) {
        let version = self.tx.borrow().version;
        *self
            .round_start_version
            .lock()
            .expect("round-version mutex poisoned") = version;
        self.truncated_attempts.store(0, Ordering::SeqCst);
        self.gave_up.store(false, Ordering::SeqCst);
        self.tx.send_modify(|snap| {
            snap.state = RunState::Running;
            snap.silent_cause = None;
        });
    }

    pub fn end_round(&self) {
        let started_at = *self
            .round_start_version
            .lock()
            .expect("round-version mutex poisoned");
        self.tx.send_modify(|snap| {
            snap.state = RunState::Idle {
                published: snap.version > started_at,
            };
        });
    }

    /// Queue a prompt for injection into the round in flight.
    pub fn queue_injection(&self, prompt: String) {
        self.injections
            .lock()
            .expect("injection mutex poisoned")
            .push(prompt);
    }

    /// Every injection queued so far, in order. Read (not drained) on each
    /// model call, because `RequestPatch` is per-turn and non-sticky: a steer
    /// dropped after one turn would vanish from the next one.
    pub fn injections(&self) -> Vec<String> {
        self.injections
            .lock()
            .expect("injection mutex poisoned")
            .clone()
    }

    /// Hand back the injections so the round's caller can fold them into the
    /// subagent's own history, and clear them for the next round.
    pub fn take_injections(&self) -> Vec<String> {
        std::mem::take(&mut *self.injections.lock().expect("injection mutex poisoned"))
    }
}

impl Default for SubagentShared {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(version: u64, state: RunState, outcome: Option<Outcome>) -> Snapshot {
        Snapshot {
            version,
            state,
            outcome,
            silent_cause: None,
        }
    }

    /// The error text a silent round reads as, or a panic -- every test below
    /// that asks "what is the parent told?" wants the same unwrapping.
    fn silent_message(shared: &SubagentShared) -> String {
        match shared.snapshot().read(0) {
            Some(Outcome::Error(message)) => message,
            other => panic!("a silent round should read as an error, got: {other:?}"),
        }
    }

    #[test]
    fn newer_version_is_readable() {
        let s = snap(1, RunState::Running, Some(Outcome::Result("a".into())));
        assert!(s.readable(0));
        assert_eq!(s.read(0), Some(Outcome::Result("a".into())));
    }

    #[test]
    fn running_with_nothing_new_is_not_readable() {
        let s = snap(1, RunState::Running, Some(Outcome::Result("a".into())));
        assert!(!s.readable(1));
        assert_eq!(s.read(1), None);
    }

    /// The point of tracking `published` per round: a subagent that answered
    /// and then finished is idle, not failed, so a second read blocks instead
    /// of reporting an error.
    #[test]
    fn idle_after_publishing_is_not_readable_once_collected() {
        let s = snap(
            1,
            RunState::Idle { published: true },
            Some(Outcome::Result("a".into())),
        );
        assert!(!s.readable(1));
    }

    #[test]
    fn idle_without_publishing_is_readable_as_an_error() {
        let s = snap(0, RunState::Idle { published: false }, None);
        assert!(s.readable(0));
        assert!(matches!(s.read(0), Some(Outcome::Error(_))));
    }

    /// Latest-only: three publishes then one read yields the third, and the
    /// watermark jumps past the two that were never collected.
    #[test]
    fn publishing_thrice_then_reading_yields_only_the_latest() {
        let shared = SubagentShared::new();
        shared.begin_round();
        for value in ["one", "two", "three"] {
            shared.publish(Outcome::Result(value.to_string()));
        }
        let s = shared.snapshot();
        assert_eq!(s.version, 3);
        assert_eq!(s.read(0), Some(Outcome::Result("three".into())));
        assert!(!s.readable(3), "watermark jumps to 3, nothing left to read");
    }

    /// A round cut off mid-report publishes nothing, so without this the
    /// parent would be told only that the subagent "stopped" -- true, but with
    /// no hint that the fix is a shorter report or a bigger ceiling.
    #[test]
    fn a_truncated_round_explains_itself_to_the_parent() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.end_round();

        assert!(
            silent_message(&shared).contains("output token limit"),
            "got: {}",
            silent_message(&shared)
        );
    }

    #[test]
    fn a_later_round_does_not_inherit_an_earlier_truncation() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.end_round();

        shared.begin_round();
        shared.end_round();
        assert!(
            !silent_message(&shared).contains("output token limit"),
            "got: {}",
            silent_message(&shared)
        );
    }

    /// Without this the parent is told the subagent "stopped without calling
    /// outrig__set_result", which names the symptom and hides the cause -- the
    /// loop was cut short before it ever got the chance.
    #[test]
    fn an_early_exit_names_the_reason_it_stopped() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_ended_early("tool-call iteration max (50) reached");
        shared.end_round();

        let message = silent_message(&shared);
        assert!(
            message.contains("tool-call iteration max (50)"),
            "got: {message}"
        );
    }

    /// Both causes land in the same round constantly: a report that will not
    /// fit is retried until the budget is gone. The truncation is the one worth
    /// telling the parent, since the exhaustion follows from it.
    #[test]
    fn a_truncated_report_outranks_the_early_exit_it_causes() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.note_ended_early("tool-call iteration max (50) reached");
        shared.end_round();

        let message = silent_message(&shared);
        assert!(message.contains("output token limit"), "got: {message}");
        assert!(
            !message.contains("tool-call iteration max"),
            "got: {message}"
        );
    }

    /// The retry budget keys off *consecutive* failures, so the count has to
    /// climb across attempts and start over each round.
    #[test]
    fn truncated_attempts_count_up_and_reset_per_round() {
        let shared = SubagentShared::new();
        shared.begin_round();
        assert_eq!(shared.note_truncated_attempt(), 1);
        assert_eq!(shared.note_truncated_attempt(), 2);
        shared.end_round();

        shared.begin_round();
        assert_eq!(
            shared.note_truncated_attempt(),
            1,
            "a fresh round gets a fresh retry budget"
        );
    }

    /// A subagent that reports, then truncates while revising, has proven it
    /// can produce a body that fits -- so the budget starts over rather than
    /// counting the earlier failures against it.
    #[test]
    fn a_report_that_fit_resets_the_truncation_budget() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.note_truncated_attempt();
        shared.note_report_fit();

        assert_eq!(shared.note_truncated_attempt(), 1);
    }

    /// The reset deliberately does *not* ride on `publish`: giving up publishes
    /// too, and resetting there re-armed the retry cycle the give-up exists to
    /// end.
    #[test]
    fn publishing_alone_does_not_reset_the_truncation_budget() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.note_truncated_attempt();
        shared.publish(Outcome::Error("gave up".into()));

        assert_eq!(
            shared.note_truncated_attempt(),
            3,
            "a publish that is not evidence of a body that fit must not restore the budget"
        );
    }

    /// The latch is per round and one-shot: it tells the first caller it won,
    /// every later one that the round has already given up, and resets when the
    /// parent sends new work.
    #[test]
    fn giving_up_latches_once_per_round() {
        let shared = SubagentShared::new();
        shared.begin_round();
        assert!(!shared.has_given_up());
        assert!(shared.give_up(), "the first caller gives up");
        assert!(!shared.give_up(), "later callers do not publish again");
        assert!(shared.has_given_up());

        shared.end_round();
        shared.begin_round();
        assert!(
            !shared.has_given_up(),
            "new work from the parent deserves a fresh attempt"
        );
    }

    #[test]
    fn round_that_published_nothing_ends_idle_unpublished() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.end_round();
        assert_eq!(shared.snapshot().state, RunState::Idle { published: false });
    }

    #[test]
    fn round_that_published_ends_idle_published() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.publish(Outcome::Result("done".into()));
        shared.end_round();
        assert_eq!(shared.snapshot().state, RunState::Idle { published: true });
    }

    /// A second round that publishes nothing is a failure even though an
    /// earlier round did publish -- `published` is per round, not cumulative.
    #[test]
    fn second_silent_round_is_a_failure_despite_an_earlier_publish() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.publish(Outcome::Result("first".into()));
        shared.end_round();

        shared.begin_round();
        shared.end_round();
        assert_eq!(shared.snapshot().state, RunState::Idle { published: false });
    }

    #[test]
    fn injections_survive_repeated_reads_until_taken() {
        let shared = SubagentShared::new();
        shared.queue_injection("stop, wrong module".into());
        assert_eq!(shared.injections().len(), 1);
        assert_eq!(shared.injections().len(), 1, "reading does not drain");
        assert_eq!(shared.take_injections().len(), 1);
        assert!(shared.injections().is_empty());
    }
}
