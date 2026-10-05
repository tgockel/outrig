//! One subagent's result inbox, run state, and prompt queue.
//!
//! The inbox is **latest-only, not a queue**: [`SubagentShared::publish`]
//! overwrites the held value and bumps a version counter. A parent that reads
//! after three publishes sees the third and its watermark jumps past the two it
//! missed -- there is no backlog and no ordering to preserve.
//!
//! Reads are edge-triggered on that version, which is what makes "did reading
//! consume it?" a non-question: the parent keeps a watermark, and a second read
//! with nothing new simply blocks.
//!
//! Prompts travel the other way, and those are a queue: every one the parent
//! sends runs, in the order [`SubagentShared::accept`] took it -- see
//! [`crate::subagent::injection`].

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::sync::{Notify, watch};

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
/// idle but not failed. A publish counts only when no steer came after it,
/// since it cannot answer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Idle { published: bool },
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
    /// just that it stopped. A publish clears it, so a cause from before a
    /// report cannot explain a stop after it.
    pub silent_cause: Option<SilentCause>,
}

impl Snapshot {
    /// Whether a parent at `watermark` has something to collect.
    ///
    /// Two ways that happens: a newer version was published, or the subagent
    /// stopped without publishing during its last round -- or without
    /// publishing again after a steer. The second is the error case, and it
    /// stays true until the parent pokes the subagent back into `Running` -- it
    /// is deliberately level-triggered, since nothing about a stalled subagent
    /// changes just by being looked at.
    pub fn readable(&self, watermark: u64) -> bool {
        self.version > watermark
            || (self.version == watermark && self.state == RunState::Idle { published: false })
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
    /// Every prompt the parent has sent that no round has taken up yet, and
    /// whether the round in flight can still take one. See
    /// [`crate::subagent::injection`].
    prompts: Mutex<Prompts>,
    /// Wakes the round driver when a prompt is queued for a round of its own.
    wake: Notify,
}

/// The parent's prompts: steers for the round in flight, and prompts waiting
/// for rounds of their own.
///
/// One lock over all of it is the point. Where a prompt goes is decided against
/// the same state a round closes when its agent loop returns, so no prompt is
/// accepted into a round that has stopped looking. A steer that round never
/// sent joins the same queue the parent's other prompts wait in, so every
/// prompt runs in the order it was accepted.
#[derive(Debug)]
struct Prompts {
    /// Whether the round in flight could still carry a steer to its model.
    /// Set when a round begins, cleared the moment its agent loop returns.
    open: bool,
    /// Steers for the round in flight.
    steers: Vec<String>,
    /// The inbox version when the round in flight last took a steer.
    steered_at: Option<u64>,
    /// How many of `steers` have gone out on a tool result the round's history
    /// keeps. They go out in order, so the rest are still to go.
    delivered: usize,
    /// Prompts waiting for rounds of their own, in the order they were
    /// accepted.
    rounds: VecDeque<String>,
}

/// Where [`SubagentShared::accept`] put a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Into the round in flight, for a tool result to carry to its model.
    Injected,
    /// Behind every prompt accepted before it, as a round of its own.
    Queued,
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
            // Closed until the first round begins, so a send that races the
            // launch queues behind its first prompt instead of steering a
            // round that has not seen its assignment yet.
            prompts: Mutex::new(Prompts {
                open: false,
                steers: Vec::new(),
                steered_at: None,
                delivered: 0,
                rounds: VecDeque::new(),
            }),
            wake: Notify::new(),
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
            snap.silent_cause = None;
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
        let mut prompts = self.lock_prompts();
        prompts.open = true;
        prompts.steered_at = None;
        drop(prompts);
        self.tx.send_modify(|snap| {
            snap.state = RunState::Running;
            snap.silent_cause = None;
        });
    }

    /// End the round, going idle unless another round is already waiting.
    ///
    /// A waiting round was accepted from the parent, which has in effect poked
    /// the subagent already. Going idle in between would hand a parent blocked
    /// on it this round's stop just before that round starts.
    pub fn end_round(&self) {
        // Asked outside `send_modify`, yet still this round's answer: only the
        // round's own task publishes, its agent loop has returned, and it
        // takes no more steers. A publish from before the round's latest
        // steer does not count: a parent that read it and then steered would
        // otherwise wait on the steer for good.
        let published = self.published_this_round() && !self.steered_since_publish();
        // Checked and applied under the prompts lock, which `accept` queues
        // under, so a prompt cannot land between the two.
        let prompts = self.lock_prompts();
        if prompts.rounds.is_empty() {
            self.tx
                .send_modify(|snap| snap.state = RunState::Idle { published });
        }
    }

    /// Whether the round in flight has published yet.
    pub fn published_this_round(&self) -> bool {
        let started_at = *self
            .round_start_version
            .lock()
            .expect("round-version mutex poisoned");
        self.tx.borrow().version > started_at
    }

    /// Whether the parent steered the round in flight after its latest
    /// publish, which that publish therefore cannot answer.
    pub fn steered_since_publish(&self) -> bool {
        self.lock_prompts().steered_at == Some(self.tx.borrow().version)
    }

    /// Take a prompt from the parent: into the round in flight while it could
    /// still carry it to its model, and otherwise into the queue of rounds.
    ///
    /// A queued prompt is a round the subagent is committed to, so a subagent
    /// that went idle reads as `Running` again from this moment. Otherwise a
    /// parent that sends and then waits would be handed the stop of the round
    /// before.
    pub fn accept(&self, prompt: String) -> Accepted {
        let mut prompts = self.lock_prompts();
        if prompts.open {
            prompts.steers.push(prompt);
            prompts.steered_at = Some(self.tx.borrow().version);
            return Accepted::Injected;
        }
        prompts.rounds.push_back(prompt);
        // Under the prompts lock, like `end_round`'s check, so the subagent
        // cannot go idle with this round waiting.
        self.tx.send_if_modified(|snap| {
            let idle = matches!(snap.state, RunState::Idle { .. });
            if idle {
                snap.state = RunState::Running;
            }
            idle
        });
        drop(prompts);
        self.wake.notify_one();
        Accepted::Queued
    }

    /// The prompt of the next round to run, once there is one.
    pub async fn next_round(&self) -> String {
        loop {
            let next = self.lock_prompts().rounds.pop_front();
            if let Some(prompt) = next {
                return prompt;
            }
            // `notify_one` leaves a permit when nothing is waiting yet, so a
            // prompt queued between the check above and this await still
            // wakes it.
            self.wake.notified().await;
        }
    }

    /// The steers queued since the last delivery, in order, counted as
    /// delivered, and the span of the queue they take up: the tool result
    /// about to go back to the model carries them. See
    /// [`crate::llm::InjectionSource`] for who may ask.
    ///
    /// Each goes out once, since rig keeps the result as rewritten. A turn
    /// whose history ends up without the result hands the span to
    /// [`Self::undeliver_injections`].
    pub fn deliver_injections(&self) -> (Vec<String>, Range<usize>) {
        let mut prompts = self.lock_prompts();
        let span = prompts.delivered..prompts.steers.len();
        prompts.delivered = span.end;
        (prompts.steers[span.clone()].to_vec(), span)
    }

    /// Count the steers in `span` as never sent: the history lost the tool
    /// result that carried them.
    ///
    /// Only while nothing has gone out after them, which holds within a turn,
    /// whose undos run newest first.
    pub fn undeliver_injections(&self, span: Range<usize>) {
        let mut prompts = self.lock_prompts();
        if prompts.delivered == span.end {
            prompts.delivered = span.start;
        }
    }

    /// Stop taking steers, since the round's agent loop has returned and no
    /// tool result is left to carry one, and hand back the ones to fold into
    /// the subagent's history.
    ///
    /// Those that went out are in the history already, on the results that
    /// carried them. The rest reached no model:
    /// - With `carry`, they join the queue of rounds as one prompt. That puts
    ///   them behind every prompt queued before them, since nothing was queued
    ///   while this round took steers, and ahead of any queued after.
    /// - Without it, they are handed back.
    ///
    /// From here to the next [`Self::begin_round`], [`Self::accept`] queues
    /// every prompt as a round of its own.
    #[must_use = "a steer dropped here never reaches the model"]
    pub fn close_injections(&self, carry: bool) -> Vec<String> {
        let mut prompts = self.lock_prompts();
        prompts.open = false;
        let delivered = std::mem::take(&mut prompts.delivered);
        let undelivered = std::mem::take(&mut prompts.steers).split_off(delivered);
        if !carry {
            return undelivered;
        }
        if !undelivered.is_empty() {
            prompts.rounds.push_back(undelivered.join("\n\n"));
        }
        Vec::new()
    }

    fn lock_prompts(&self) -> std::sync::MutexGuard<'_, Prompts> {
        self.prompts.lock().expect("prompts mutex poisoned")
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

    /// The next round's prompt, which a test expects to be waiting already.
    fn next_waiting(shared: &SubagentShared) -> String {
        futures_util::FutureExt::now_or_never(shared.next_round()).expect("a round is waiting")
    }

    /// The tool result a steer rides on stays in the history, so the model
    /// goes on seeing the steer without its going out again.
    #[test]
    fn a_steer_is_delivered_once() {
        let shared = SubagentShared::new();
        shared.begin_round();
        assert_eq!(
            shared.accept("stop, wrong module".into()),
            Accepted::Injected
        );
        assert_eq!(shared.deliver_injections().0, ["stop, wrong module"]);
        assert!(
            shared.deliver_injections().0.is_empty(),
            "a steer that went out does not go out again"
        );
        shared.accept("and the tests too".into());
        assert_eq!(shared.deliver_injections().0, ["and the tests too"]);
        assert!(
            shared.close_injections(true).is_empty(),
            "the history holds both, on the results that carried them"
        );
    }

    /// The window #181 was about: the run state still reads `Running` after
    /// the round's last model call, but nothing is left to read a steer. Once
    /// the round closes, a prompt queues as a round of its own.
    #[test]
    fn a_prompt_after_the_round_closes_is_queued_as_a_round() {
        let shared = SubagentShared::new();
        shared.begin_round();
        let _ = shared.close_injections(true);

        assert_eq!(shared.snapshot().state, RunState::Running);
        assert_eq!(shared.accept("too late".into()), Accepted::Queued);
        assert!(
            shared.close_injections(true).is_empty(),
            "a queued prompt must not be a steer as well"
        );
        assert_eq!(next_waiting(&shared), "too late");
    }

    /// A launch queues its first prompt, and a send that races the round's
    /// start queues behind it rather than steering a round that has not seen
    /// its assignment yet.
    #[test]
    fn a_send_before_the_first_round_queues_behind_it() {
        let shared = SubagentShared::new();
        assert_eq!(shared.accept("the assignment".into()), Accepted::Queued);
        assert_eq!(shared.accept("a follow-up".into()), Accepted::Queued);
        assert_eq!(next_waiting(&shared), "the assignment");
        assert_eq!(next_waiting(&shared), "a follow-up");
    }

    /// A tool result carries what was queued when it came back. Whatever
    /// arrived after the round's last one reached no model, so it cannot be
    /// left in history as though it had: it runs as a round of its own.
    #[test]
    fn a_steer_no_tool_result_carried_runs_as_a_round() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.accept("seen".into());
        assert_eq!(shared.deliver_injections().0, ["seen"]);
        shared.accept("unseen".into());
        shared.accept("also unseen".into());

        assert!(
            shared.close_injections(true).is_empty(),
            "the one that went out is in the history already"
        );
        assert_eq!(
            next_waiting(&shared),
            "unseen\n\nalso unseen",
            "the steers one round missed run together, as the next"
        );
    }

    /// A failed round starts nothing on its own, so the steers it never sent
    /// come back to be folded in. One that went out stays where it is, on the
    /// tool result the round's history kept.
    #[test]
    fn a_failed_round_hands_back_the_steers_it_never_sent() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.accept("seen".into());
        let _ = shared.deliver_injections();
        shared.accept("unseen".into());

        assert_eq!(shared.close_injections(false), ["unseen"]);
        assert!(
            futures_util::FutureExt::now_or_never(shared.next_round()).is_none(),
            "nothing may be queued as a round"
        );
    }

    /// A turn whose history ends up without the results its steers went out
    /// on takes those deliveries back, newest first, so the steers count as
    /// never sent and a failed round hands them back with the rest.
    #[test]
    fn a_delivery_the_history_lost_is_taken_back() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.accept("first".into());
        let (_, first) = shared.deliver_injections();
        shared.accept("second".into());
        let (_, second) = shared.deliver_injections();
        shared.accept("unsent".into());

        shared.undeliver_injections(second);
        shared.undeliver_injections(first);
        assert_eq!(
            shared.close_injections(false),
            ["first", "second", "unsent"]
        );
    }

    /// A steer carried out of round A must not overtake B, which was queued
    /// before A even began. Every prompt runs in the order it was accepted,
    /// however many rounds in a row carry a steer.
    #[test]
    fn a_carried_steer_queues_behind_prompts_already_waiting() {
        let shared = SubagentShared::new();
        shared.accept("A".into());
        shared.accept("B".into());

        assert_eq!(next_waiting(&shared), "A");
        shared.begin_round();
        let _ = shared.deliver_injections();
        assert_eq!(shared.accept("C".into()), Accepted::Injected);
        assert!(shared.close_injections(true).is_empty());
        assert_eq!(shared.accept("D".into()), Accepted::Queued);

        assert_eq!(next_waiting(&shared), "B");
        shared.begin_round();
        let _ = shared.deliver_injections();
        shared.accept("E".into());
        let _ = shared.close_injections(true);

        for expected in ["C", "D", "E"] {
            assert_eq!(next_waiting(&shared), expected);
        }
    }

    /// A round already accepted keeps the subagent working: going idle in
    /// between would hand a waiting parent this round's stop just before that
    /// round starts.
    #[test]
    fn a_round_that_ends_with_another_waiting_stays_running() {
        let shared = SubagentShared::new();
        shared.begin_round();
        let _ = shared.close_injections(true);
        shared.accept("next".into());
        shared.end_round();
        assert_eq!(shared.snapshot().state, RunState::Running);
        assert!(!shared.snapshot().readable(0));

        assert_eq!(next_waiting(&shared), "next");
        shared.begin_round();
        let _ = shared.close_injections(true);
        shared.end_round();
        assert_eq!(shared.snapshot().state, RunState::Idle { published: false });
    }

    /// Queuing a round is the poke the stop waits for, so it ends at once: a
    /// parent that sends and then reads is not handed the stale stop.
    #[test]
    fn a_prompt_queued_for_an_idle_subagent_makes_it_running() {
        let shared = SubagentShared::new();
        shared.begin_round();
        let _ = shared.close_injections(true);
        shared.end_round();
        assert!(shared.snapshot().readable(0), "the stop is readable");

        shared.accept("try again".into());
        assert_eq!(shared.snapshot().state, RunState::Running);
        assert!(!shared.snapshot().readable(0), "and no longer, once poked");
    }

    /// Nothing outlives its round among the steers, so one can no longer ride
    /// along with a later round's unrelated work.
    #[test]
    fn a_closed_round_leaves_no_steer_for_the_next() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.accept("steer".into());
        let _ = shared.close_injections(false);

        shared.begin_round();
        assert!(shared.deliver_injections().0.is_empty());
        assert!(shared.close_injections(true).is_empty());
    }

    #[test]
    fn published_this_round_is_per_round() {
        let shared = SubagentShared::new();
        shared.begin_round();
        assert!(!shared.published_this_round());
        shared.publish(Outcome::Result("done".into()));
        assert!(shared.published_this_round());

        shared.begin_round();
        assert!(
            !shared.published_this_round(),
            "an earlier round's publish is not this one's"
        );
    }

    /// A report is no answer to a steer that came after it, and a steer from
    /// an earlier round says nothing about this one.
    #[test]
    fn a_steer_after_a_publish_waits_for_the_next_one() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.publish(Outcome::Result("report".into()));
        assert!(!shared.steered_since_publish());
        shared.accept("follow-up".into());
        assert!(shared.steered_since_publish());
        shared.publish(Outcome::Result("revised".into()));
        assert!(!shared.steered_since_publish());

        shared.accept("late".into());
        let _ = shared.close_injections(true);
        let _ = next_waiting(&shared);
        shared.begin_round();
        assert!(!shared.steered_since_publish(), "a new round starts clean");
    }

    /// A round that took a steer after its report, and ended without
    /// publishing again, reads as stopped: once the parent has collected the
    /// report, which still comes first. The stop's reason is from after the
    /// report, not the truncation the report itself got past.
    #[test]
    fn a_round_steered_after_its_report_and_left_unreported_reads_as_stopped() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.publish(Outcome::Result("report".into()));
        shared.accept("follow-up".into());
        let _ = shared.deliver_injections();
        let _ = shared.close_injections(true);
        shared.end_round();

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.state, RunState::Idle { published: false });
        assert_eq!(snapshot.read(0), Some(Outcome::Result("report".into())));
        assert_eq!(
            snapshot.read(1),
            Some(Outcome::Error(
                "subagent stopped without calling outrig__set_result".into()
            ))
        );
    }
}
