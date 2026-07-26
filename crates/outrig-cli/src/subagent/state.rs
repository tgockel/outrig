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

use tokio::sync::watch;

/// What a round published. Exactly one of these; `set_result` takes
/// `{result}` xor `{error}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Result(String),
    Error(String),
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
    /// Whether this round tried to report and was cut off part-way. Kept so a
    /// round that ends up publishing nothing can say *why* instead of just
    /// that it stopped.
    pub truncated_attempt: bool,
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
        Some(Outcome::Error(if self.truncated_attempt {
            "subagent hit its output token limit while writing its report, so \
             nothing was recorded. Re-ask it for a narrower or shorter report, \
             or raise max-tokens for this agent."
                .to_string()
        } else {
            "subagent stopped without calling outrig__set_result".to_string()
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
            truncated_attempt: false,
        });
        Self {
            tx,
            round_start_version: Mutex::new(0),
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

    /// Record that a report was cut off part-way. Per round, so a later
    /// successful round does not inherit an earlier round's explanation.
    pub fn note_truncated_attempt(&self) {
        self.tx.send_modify(|snap| snap.truncated_attempt = true);
    }

    pub fn begin_round(&self) {
        let version = self.tx.borrow().version;
        *self
            .round_start_version
            .lock()
            .expect("round-version mutex poisoned") = version;
        self.tx.send_modify(|snap| {
            snap.state = RunState::Running;
            snap.truncated_attempt = false;
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
            truncated_attempt: false,
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

        let Some(Outcome::Error(message)) = shared.snapshot().read(0) else {
            panic!("a silent round should read as an error");
        };
        assert!(message.contains("output token limit"), "got: {message}");
    }

    #[test]
    fn a_later_round_does_not_inherit_an_earlier_truncation() {
        let shared = SubagentShared::new();
        shared.begin_round();
        shared.note_truncated_attempt();
        shared.end_round();

        shared.begin_round();
        shared.end_round();
        let Some(Outcome::Error(message)) = shared.snapshot().read(0) else {
            panic!("a silent round should read as an error");
        };
        assert!(!message.contains("output token limit"), "got: {message}");
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
