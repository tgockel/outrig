//! The status line: what the session is doing right now, read from its
//! events.
//!
//! Everything here comes from [`Fact`]s, in the order the session published
//! them, so the line says what the session said and not what the loop
//! guessed. A gap in the events ([`Fact::Missed`]) can hide a change; what it
//! could have hidden is marked unknown, or corrected by the next state the
//! session reports.

use std::time::{Duration, Instant};

use super::facts::{Fact, Phase, cut};

/// How many characters of a retried or moved call's error the line shows.
const ERROR_MAX: usize = 60;

/// How many characters of what the model said the line shows.
const SAID_MAX: usize = 200;

#[derive(Debug)]
pub(super) struct Status {
    phase: Phase,
    model: Option<String>,
    /// The submission running, and since when.
    running: Option<(u64, Instant)>,
    /// A call that failed, until a request settles, another call starts, or
    /// the round moves on.
    failed: Option<Failed>,
    tokens: u64,
    /// The user's messages the agent has yet to take; `None` once a gap in
    /// the events may have hidden one being posted or taken.
    waiting: Option<u64>,
    /// What the last round's code left running.
    still_running: Option<String>,
    /// What the model last said this round, which stands in for "is working"
    /// until it says more.
    said: Option<String>,
    /// Whether the agent has sent the user anything since the round started.
    sent: bool,
}

/// A failed call, as the line says it.
#[derive(Debug)]
enum Failed {
    /// It waits to be tried again on `model` at `at`.
    Retrying {
        model: String,
        at: Instant,
        error: String,
    },
    /// Call `call` moved to another model from `from`.
    Moved {
        from: String,
        error: String,
        call: u64,
    },
}

/// The status line's parts: what is shown, and whether the session is busy,
/// which draws a spinner beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shown {
    pub(super) text: String,
    pub(super) busy: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: Phase::Ready,
            model: None,
            running: None,
            failed: None,
            tokens: 0,
            waiting: Some(0),
            still_running: None,
            said: None,
            sent: false,
        }
    }
}

impl Status {
    /// Read `fact`. Returns whether the line may have changed.
    pub(super) fn apply(&mut self, fact: &Fact, now: Instant) -> bool {
        match fact {
            Fact::Phase(phase) => {
                self.phase = *phase;
                // A submission is running only while the session says so.
                if *phase != Phase::Python {
                    self.running = None;
                }
                if *phase != Phase::Model {
                    self.failed = None;
                }
            }
            Fact::AgentStarted { model } => self.model = Some(model.clone()),
            Fact::RoundStarted { .. } => {
                self.sent = false;
                self.still_running = None;
                self.said = None;
            }
            Fact::Submitted { exec, .. } => {
                self.running = Some((*exec, now));
                self.failed = None;
            }
            Fact::Completed { exec, .. } => {
                if self.running.is_some_and(|(running, _)| running == *exec) {
                    self.running = None;
                }
            }
            Fact::Retry {
                model,
                delay,
                error,
            } => {
                self.failed = Some(Failed::Retrying {
                    model: model.clone(),
                    at: now + *delay,
                    error: first_line(error),
                });
            }
            Fact::Failover {
                from,
                to,
                error,
                call,
            } => {
                self.model = Some(to.clone());
                self.failed = Some(Failed::Moved {
                    from: from.clone(),
                    error: first_line(error),
                    call: *call,
                });
            }
            Fact::Calling { model, call } => {
                self.model = Some(model.clone());
                // The model a call moved to is sent that same call.
                if !matches!(&self.failed, Some(Failed::Moved { call: moved, .. }) if moved == call)
                {
                    self.failed = None;
                }
            }
            Fact::Attempted => self.failed = None,
            Fact::RoundEnded { tokens, .. } => {
                self.tokens += tokens.unwrap_or(0);
                self.failed = None;
                self.said = None;
            }
            Fact::Spent(tokens) => {
                self.tokens += tokens;
                return *tokens > 0;
            }
            Fact::Posted => {
                if let Some(waiting) = &mut self.waiting {
                    *waiting += 1;
                }
            }
            Fact::Taken => {
                if let Some(waiting) = &mut self.waiting {
                    *waiting = waiting.saturating_sub(1);
                }
            }
            Fact::AgentSent => {
                self.sent = true;
                return false;
            }
            Fact::Said(text) => self.said = Some(text.clone()),
            Fact::Missed(_) => self.waiting = None,
            Fact::Exec { .. } | Fact::Note { .. } | Fact::Other { .. } => return false,
        }
        true
    }

    /// How many of the user's messages wait, as the channel said when the
    /// last one was posted: what makes a count unknown after a gap known
    /// again.
    pub(super) fn queued(&mut self, waiting: usize) {
        if self.waiting.is_none() {
            self.waiting = Some(waiting as u64);
        }
    }

    /// The round ended, leaving `still_running`. Whether the agent sent the
    /// user anything during it.
    pub(super) fn ended(&mut self, still_running: Option<String>) -> bool {
        self.still_running = still_running;
        self.sent
    }

    /// Whether the session is working, which the line shows with a spinner.
    pub(super) fn busy(&self) -> bool {
        matches!(self.phase, Phase::Model | Phase::Python)
    }

    pub(super) fn shown(&self, now: Instant) -> Shown {
        let mut parts = Vec::new();
        match self.phase {
            Phase::Starting => parts.push("starting".to_string()),
            Phase::Closing => parts.push("closing".to_string()),
            Phase::Python => match self.running {
                Some((exec, since)) => parts.push(format!(
                    "Python running · execution {exec} · {}",
                    elapsed(now.saturating_duration_since(since))
                )),
                None => parts.push("Python running".to_string()),
            },
            Phase::Model => {
                match &self.failed {
                    Some(Failed::Retrying { model, at, error }) if *at > now => {
                        parts.push(format!(
                            "{model} failed ({error}); retrying in {}",
                            elapsed(at.saturating_duration_since(now))
                        ));
                    }
                    _ => parts.push(self.working()),
                }
                if let Some(Failed::Moved { from, error, .. }) = &self.failed {
                    parts.push(format!("moved from {from} ({error})"));
                }
            }
            Phase::Ready => {
                parts.push("ready".to_string());
                if let Some(running) = &self.still_running {
                    parts.push(format!("still running: {running}"));
                }
            }
        }
        match self.waiting {
            Some(0) => {}
            Some(waiting) => parts.push(format!("{waiting} waiting for the agent")),
            None => parts.push("messages waiting unknown".to_string()),
        }
        if self.tokens > 0 {
            parts.push(format!("{} tokens", count(self.tokens)));
        }
        Shown {
            text: parts.join(" · "),
            busy: self.busy(),
        }
    }

    /// What the model is doing while no retry waits: the latest thing it
    /// said, or that it is working.
    fn working(&self) -> String {
        match (&self.said, &self.model) {
            (Some(said), _) => {
                let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
                cut(&said, SAID_MAX)
            }
            (None, Some(model)) => format!("{model} is working"),
            (None, None) => "the model is working".to_string(),
        }
    }
}

/// The first line of a failed call's `error`, cut to what the line shows.
fn first_line(error: &str) -> String {
    cut(error.lines().next().unwrap_or_default(), ERROR_MAX)
}

/// `duration` as the screen shows it: `3.2s`, then `1m05s`.
pub(super) fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{}.{}s", seconds, duration.subsec_millis() / 100)
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// `tokens` as the line shows it: `950`, `12.3k`, `1.2M`.
fn count(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(facts: &[Fact], now: Instant) -> Status {
        let mut status = Status::default();
        for fact in facts {
            status.apply(fact, now);
        }
        status
    }

    #[test]
    fn a_running_submission_shows_its_id_and_how_long_it_has_run() {
        let start = Instant::now();
        let status = after(
            &[
                Fact::Phase(Phase::Model),
                Fact::Submitted {
                    exec: 7,
                    source: "time.sleep(60)".into(),
                },
                Fact::Phase(Phase::Python),
            ],
            start,
        );
        let shown = status.shown(start + Duration::from_millis(3_250));
        assert_eq!(shown.text, "Python running · execution 7 · 3.2s");
        assert!(shown.busy);
        assert_eq!(
            status.shown(start + Duration::from_secs(65)).text,
            "Python running · execution 7 · 1m05s"
        );
    }

    /// The submission is cleared by its completion, and by any state other
    /// than running Python -- which is what corrects the line after a gap
    /// hid the completion.
    #[test]
    fn a_submission_stops_showing_when_it_completes_or_the_state_moves_on() {
        let now = Instant::now();
        let submitted = Fact::Submitted {
            exec: 3,
            source: String::new(),
        };
        let mut status = after(&[submitted.clone(), Fact::Phase(Phase::Python)], now);
        status.apply(&Fact::Missed(4), now);
        status.apply(&Fact::Phase(Phase::Model), now);
        assert_eq!(status.running, None);

        let mut status = after(&[submitted], now);
        status.apply(
            &Fact::Completed {
                exec: 3,
                status: "ok",
                duration: Duration::ZERO,
                output: String::new(),
                dropped: 0,
                error: None,
                background: Vec::new(),
            },
            now,
        );
        assert_eq!(status.running, None);
    }

    #[test]
    fn a_retry_counts_down_and_clears_with_the_next_attempt() {
        let now = Instant::now();
        let mut status = after(
            &[
                Fact::AgentStarted {
                    model: "sonnet".into(),
                },
                Fact::Phase(Phase::Model),
                Fact::Retry {
                    model: "sonnet".into(),
                    delay: Duration::from_secs(12),
                    error: "429 Too Many Requests\nbody".into(),
                },
            ],
            now,
        );
        assert_eq!(
            status.shown(now + Duration::from_secs(2)).text,
            "sonnet failed (429 Too Many Requests); retrying in 10.0s"
        );
        // Past the delay, the call is being made again.
        assert_eq!(
            status.shown(now + Duration::from_secs(13)).text,
            "sonnet is working"
        );
        status.apply(&Fact::Attempted, now);
        assert_eq!(status.shown(now).text, "sonnet is working");
    }

    /// What the model said stands in for "is working", each message replaces
    /// the last, and the round's start and end clear it.
    #[test]
    fn the_latest_thing_the_model_said_replaces_is_working() {
        let now = Instant::now();
        let mut status = after(
            &[
                Fact::AgentStarted {
                    model: "sonnet".into(),
                },
                Fact::Phase(Phase::Model),
            ],
            now,
        );
        assert!(status.apply(&Fact::Said("Reading\n  the   code".into()), now));
        assert_eq!(status.shown(now).text, "Reading the code");
        status.apply(&Fact::Said("Now testing".into()), now);
        assert_eq!(status.shown(now).text, "Now testing");
        // It outlasts the call it came from, and the Python that call ran.
        status.apply(&Fact::Phase(Phase::Python), now);
        status.apply(&Fact::Phase(Phase::Model), now);
        status.apply(
            &Fact::Calling {
                model: "sonnet".into(),
                call: 2,
            },
            now,
        );
        assert_eq!(status.shown(now).text, "Now testing");
        status.apply(&Fact::RoundStarted { round: 2 }, now);
        assert_eq!(status.shown(now).text, "sonnet is working");
    }

    /// A failover names the model now called, and the one it moved from and
    /// why, while that model is sent the call and until it answers; the next
    /// call, made to the first model again, names that one.
    #[test]
    fn the_line_names_the_model_each_call_is_made_to() {
        let now = Instant::now();
        let failover = Fact::Failover {
            from: "a".into(),
            to: "b".into(),
            error: "ResponseError: finish reason content_filter: Response contained no message\n..."
                .into(),
            call: 1,
        };
        let moved = "b is working · moved from a (ResponseError: finish reason content_filter: \
                     Response contai...)";
        let mut status = after(
            &[
                Fact::AgentStarted { model: "a".into() },
                Fact::Phase(Phase::Model),
                Fact::Calling {
                    model: "a".into(),
                    call: 1,
                },
                failover.clone(),
            ],
            now,
        );
        assert_eq!(status.shown(now).text, moved);
        // The chain sends the call it moved to the next model.
        status.apply(
            &Fact::Calling {
                model: "b".into(),
                call: 1,
            },
            now,
        );
        assert_eq!(status.shown(now).text, moved);
        status.apply(&Fact::Attempted, now);
        assert_eq!(status.shown(now).text, "b is working");
        status.apply(&failover, now);
        status.apply(
            &Fact::Calling {
                model: "a".into(),
                call: 2,
            },
            now,
        );
        assert_eq!(status.shown(now).text, "a is working");
    }

    /// The count of the user's messages waiting rises as they are posted and
    /// falls as the agent takes them; the agent's own sends do not touch it.
    /// After a gap it is unknown until the channel next says.
    #[test]
    fn the_waiting_count_follows_the_users_messages_only() {
        let now = Instant::now();
        let mut status = after(
            &[
                Fact::Phase(Phase::Model),
                Fact::Posted,
                Fact::Posted,
                Fact::AgentSent,
                Fact::Taken,
            ],
            now,
        );
        assert_eq!(
            status.shown(now).text,
            "the model is working · 1 waiting for the agent"
        );
        status.apply(&Fact::Missed(2), now);
        assert_eq!(
            status.shown(now).text,
            "the model is working · messages waiting unknown"
        );
        status.queued(2);
        assert_eq!(
            status.shown(now).text,
            "the model is working · 2 waiting for the agent"
        );
        status.queued(5);
        assert_eq!(status.waiting, Some(2), "a known count is not overwritten");
    }

    #[test]
    fn a_ready_session_names_what_still_runs_and_the_tokens_spent() {
        let now = Instant::now();
        let mut status = after(
            &[
                Fact::RoundStarted { round: 1 },
                Fact::RoundEnded {
                    round: 1,
                    tokens: Some(12_000),
                    error: None,
                },
                Fact::Phase(Phase::Ready),
                Fact::Spent(345),
            ],
            now,
        );
        assert!(!status.ended(Some("ci_run".into())), "nothing was sent");
        let shown = status.shown(now);
        assert_eq!(shown.text, "ready · still running: ci_run · 12.3k tokens");
        assert!(!shown.busy);

        status.apply(&Fact::RoundStarted { round: 2 }, now);
        status.apply(&Fact::AgentSent, now);
        assert!(status.ended(None), "the agent sent something");
        assert_eq!(status.shown(now).text, "ready · 12.3k tokens");
    }

    /// Facts that leave the line as it was say so, so the screen draws no
    /// frame for them.
    #[test]
    fn only_facts_the_line_shows_change_it() {
        let now = Instant::now();
        let mut status = Status::default();
        assert!(status.apply(&Fact::Phase(Phase::Model), now));
        assert!(status.apply(&Fact::Posted, now));
        assert!(!status.apply(&Fact::AgentSent, now));
        let other = Fact::Other {
            title: "hosted.ended".into(),
            data: String::new(),
        };
        assert!(!status.apply(&other, now));
        assert!(!status.apply(&Fact::Spent(0), now));
    }

    #[test]
    fn counts_and_durations_read_short() {
        assert_eq!(count(950), "950");
        assert_eq!(count(12_345), "12.3k");
        assert_eq!(count(1_250_000), "1.2M");
        assert_eq!(elapsed(Duration::from_millis(999)), "0.9s");
        assert_eq!(elapsed(Duration::from_secs(600)), "10m00s");
    }
}
