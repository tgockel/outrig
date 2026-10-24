//! The session's events, as the screen reads them.
//!
//! The library's events are `#[non_exhaustive]`: this crate can match them but
//! never build one, so nothing that reads them could be tested on a value of
//! its own. Each is read here, once, into a [`Fact`] of this crate's, and the
//! status line and the transcript work on those. A kind this build does not
//! know becomes [`Fact::Other`], shown as its event-log JSON.
//!
//! The screen is the main agent's. A child agent's own work -- its rounds, its
//! code and its model calls -- is left to the event log, as the line mode
//! leaves its code, and what it spent is in the main agent's rounds; its
//! launch and the requests made of it are shown as unfamiliar kinds are.

use std::time::Duration;

use outrig::harness::event::{
    Abandoned, ExecStatus, Payload, ProbeVerdict, Received, Refusal, SessionState, Subject, Usage,
};

use super::super::converse::MAIN_AGENT;

/// How many characters of an unfamiliar event's JSON the detail view shows.
const OTHER_MAX: usize = 400;

/// The channel the agent and the user talk on, and the user's end of it, as
/// the session's events name them.
const USER: &str = "user";

/// What the session is doing, as the status line says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Starting,
    Ready,
    /// A round runs, and its model is being called.
    Model,
    /// A submission runs.
    Python,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fact {
    Phase(Phase),
    AgentStarted {
        model: String,
    },
    /// A model call was made to `model`: the first of a round's, a retry's
    /// failover, or one after it, back at the first.
    Calling {
        model: String,
    },
    RoundStarted {
        round: u32,
    },
    Submitted {
        exec: u64,
        source: String,
    },
    Completed {
        exec: u64,
        status: &'static str,
        duration: Duration,
        output: String,
        dropped: u64,
        error: Option<String>,
        /// Output earlier submissions wrote after they reported: each one's
        /// id, the output, and how much of it was dropped.
        background: Vec<(u64, String, u64)>,
    },
    /// Something that happened to a submission, said in a line.
    Exec {
        exec: u64,
        what: String,
    },
    /// A call failed and is tried again after `delay`. The library logs it
    /// too, and the log line is what the detail view shows.
    Retry {
        model: String,
        delay: Duration,
        error: String,
    },
    /// A call moved to the model `to`; logged, as a retry is.
    Failover {
        to: String,
    },
    /// A request to a provider ended.
    Attempted,
    /// A round ended, having spent `tokens`, its children's included.
    RoundEnded {
        round: u32,
        tokens: Option<u64>,
        error: Option<String>,
    },
    /// What the children a round launched spent after it ended.
    Spent(u64),
    /// The user posted a message to the agent.
    Posted,
    /// The agent took one of the user's messages, or the channel refused it.
    Taken,
    /// The agent sent the user a message.
    AgentSent,
    /// The model said this in a reply, tool calls or not.
    Said(String),
    /// A line about the session, for the conversation or for the detail
    /// view.
    Note {
        text: String,
        inline: bool,
    },
    /// An event of a kind the screen has nothing particular to say of: its
    /// kind, and the child agent it concerns, and its data.
    Other {
        title: String,
        data: String,
    },
    /// This many events were lost before the next one arrived.
    Missed(u64),
}

/// What `received` says, or `None` for one the screen does not show: the
/// conversation as the model was sent it, which `events.jsonl` keeps, and
/// bookkeeping that changes nothing on screen.
pub(super) fn fact(received: &Received) -> Option<Fact> {
    match received {
        Received::Event(event) => match event.subject.as_ref().map(Subject::as_str) {
            Some(child) if child != MAIN_AGENT => match from_payload(&event.payload)? {
                Fact::Other { title, data } => Some(Fact::Other {
                    title: format!("{title} · {child}"),
                    data,
                }),
                _ => None,
            },
            _ => from_payload(&event.payload),
        },
        Received::Missed(missed) => Some(Fact::Missed(*missed)),
        _ => None,
    }
}

fn from_payload(payload: &Payload) -> Option<Fact> {
    let fact = match payload {
        Payload::SessionState { state, .. } => Fact::Phase(match state {
            SessionState::Starting => Phase::Starting,
            SessionState::Idle => Phase::Ready,
            SessionState::RoundRunning => Phase::Model,
            SessionState::Executing => Phase::Python,
            _ => Phase::Closing,
        }),
        Payload::AgentStarted { model, .. } => Fact::AgentStarted {
            model: model.clone(),
        },
        Payload::RoundStarted { round, .. } => Fact::RoundStarted { round: *round },
        Payload::ExecSubmitted { execid, source, .. } => Fact::Submitted {
            exec: execid.get(),
            source: source.clone(),
        },
        Payload::ExecCompleted {
            execid,
            status,
            duration,
            output,
            dropped,
            error,
            background,
            ..
        } => Fact::Completed {
            exec: execid.get(),
            status: match status {
                ExecStatus::Ok => "ok",
                ExecStatus::Error => "error",
                ExecStatus::Refused => "refused",
                ExecStatus::Lost => "lost",
                _ => "ended",
            },
            duration: *duration,
            output: output.clone(),
            dropped: *dropped,
            error: error.clone(),
            background: background
                .iter()
                .map(|earlier| (earlier.id.get(), earlier.output.clone(), earlier.dropped))
                .collect(),
        },
        Payload::ExecRefused {
            execid,
            holder,
            reason,
            ..
        } => Fact::Exec {
            exec: execid.get(),
            what: match (reason, holder) {
                (Refusal::Held, Some(holder)) => {
                    format!("refused: execution {holder} holds the interpreter")
                }
                (Refusal::Closed, _) => "refused: the session is closed to new work".to_string(),
                _ => "refused".to_string(),
            },
        },
        Payload::MemoryExhausted { execid, .. } => Fact::Exec {
            exec: execid.get(),
            what: "ran out of memory".to_string(),
        },
        Payload::ExecCancelSent { execid, .. } => Fact::Exec {
            exec: execid.get(),
            what: "cancelled".to_string(),
        },
        Payload::ExecInterruptSent {
            execid, runaway, ..
        } => Fact::Exec {
            exec: execid.get(),
            what: if *runaway {
                "interrupted: it kept a CPU busy without letting its loop turn".to_string()
            } else {
                "interrupted".to_string()
            },
        },
        Payload::ExecProbeFailed {
            execid, verdict, ..
        } => Fact::Exec {
            exec: execid.get(),
            what: format!(
                "its event loop was found {}",
                match verdict {
                    ProbeVerdict::Blocked => "blocked",
                    ProbeVerdict::Spinning => "spinning",
                    ProbeVerdict::Starved => "starved",
                    _ => "stuck",
                }
            ),
        },
        Payload::ExecAbandoned { execid, why, .. } => Fact::Exec {
            exec: execid.get(),
            what: match why {
                Abandoned::User => "no longer waited for: interrupted twice".to_string(),
                Abandoned::Runaway => "no longer waited for: a runaway".to_string(),
                _ => "no longer waited for".to_string(),
            },
        },
        Payload::ToolResultTruncated {
            execid, size, kept, ..
        } => Fact::Exec {
            exec: execid.get(),
            what: format!("the model was sent {kept} of its result's {size} bytes"),
        },
        Payload::ModelRetry {
            model,
            delay,
            error,
            ..
        } => Fact::Retry {
            model: model.clone(),
            delay: *delay,
            error: error.clone(),
        },
        Payload::ModelFailover { to, .. } => Fact::Failover { to: to.clone() },
        Payload::ModelCall(call) => Fact::Calling {
            model: call.budget.model.clone(),
        },
        Payload::ModelAttempt(_) => Fact::Attempted,
        Payload::ModelRoundCompleted {
            round,
            usage,
            total,
            ..
        }
        | Payload::ModelRoundDropped {
            round,
            usage,
            total,
            ..
        } => Fact::RoundEnded {
            round: *round,
            tokens: total.as_ref().or(usage.as_ref()).map(tokens),
            error: None,
        },
        Payload::ModelRoundFailed {
            round,
            error,
            usage,
            total,
            ..
        } => Fact::RoundEnded {
            round: *round,
            tokens: total.as_ref().or(usage.as_ref()).map(tokens),
            error: Some(error.clone()),
        },
        Payload::ModelRoundUsage { usage, .. } => Fact::Spent(usage.as_ref().map_or(0, tokens)),
        Payload::MessageSent {
            from,
            channel,
            body,
            ..
        } => {
            if from == USER {
                Fact::Posted
            } else if channel == USER {
                Fact::AgentSent
            } else {
                Fact::Note {
                    text: format!("{from} sent on {channel}: {body}"),
                    inline: false,
                }
            }
        }
        Payload::MessageReceived { from, .. } | Payload::MessageRefused { from, .. }
            if from == USER =>
        {
            Fact::Taken
        }
        Payload::MessageReceived { .. } => return None,
        Payload::InterpreterExited {
            cause, expected, ..
        } => Fact::Note {
            text: if *expected {
                format!("the Python interpreter stopped: {cause}")
            } else {
                format!("the Python interpreter exited: {cause}")
            },
            inline: false,
        },
        Payload::InterpreterDiagnostic { text, .. } => Fact::Note {
            text: format!("the interpreter says: {text}"),
            inline: false,
        },
        Payload::OutputUnattributed { text, .. } => Fact::Note {
            text: format!("output no submission claimed:\n{text}"),
            inline: false,
        },
        // The approver said so as the session started.
        Payload::BindingRefused {
            binding, reason, ..
        } => Fact::Note {
            text: format!("binding {binding} did not start: {reason}"),
            inline: false,
        },
        Payload::BindingExited {
            bindings, cause, ..
        } => Fact::Note {
            text: format!(
                "[outrig] the bindings process exited ({cause}); the session goes on without {}",
                bindings.join(", ")
            ),
            inline: true,
        },
        // What discovery left out or warned of, as the session started.
        Payload::SkillDiscovered { diagnostics, .. } if !diagnostics.is_empty() => Fact::Note {
            text: diagnostics
                .iter()
                .map(|diagnostic| format!("[outrig] {}", diagnostic.message))
                .collect::<Vec<_>>()
                .join("\n"),
            inline: true,
        },
        Payload::SkillDiscovered { .. } => return None,
        Payload::ModelText { text, .. } => Fact::Said(text.clone()),
        Payload::ModelInstructions { .. }
        | Payload::TurnCommitted { .. }
        | Payload::ModelUsageReplaced { .. }
        | Payload::ModelUsageRefused { .. }
        | Payload::InventoryObserved { .. }
        | Payload::SessionReport { .. } => return None,
        other => Fact::Other {
            title: other.kind().to_string(),
            data: cut(&serde_json::to_string(other).unwrap_or_default(), OTHER_MAX),
        },
    };
    Some(fact)
}

/// What a usage record says was spent: its total, or the sum of its parts
/// when the provider gave no total.
fn tokens(usage: &Usage) -> u64 {
    usage
        .total_tokens
        .max(usage.input_tokens + usage.output_tokens)
}

/// `text`, cut to `max` characters.
pub(super) fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_by_characters() {
        assert_eq!(cut("abcdef", 3), "abc...");
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("日本語の", 2), "日本...");
    }
}
