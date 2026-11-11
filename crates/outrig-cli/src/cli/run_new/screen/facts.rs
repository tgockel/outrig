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
//! launch and the requests made of it are shown as unfamiliar kinds are, and
//! a round of its that failed is shown with its error. Its retries and moves
//! to another model are in the library's log lines, as the main agent's are.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use outrig::harness::OpenWait;
use outrig::harness::event::{
    Abandoned, Awaited, ExecStatus, Payload, ProbeVerdict, Received, Refusal, ReturnWhen,
    SessionState, Subject, Usage, WaitEnd,
};

use serde_json::Value;

use super::super::converse::MAIN_AGENT;

/// How many characters of an unfamiliar event's JSON the detail view shows.
const OTHER_MAX: usize = 400;

/// The fields of an unfamiliar event that say what went wrong: shown whole,
/// past [`OTHER_MAX`], since a traceback cut short is no use.
const WHY: &[&str] = &[
    "error",
    "reason",
    "detail",
    "exception",
    "traceback",
    "message",
    "problems",
];

/// The channel the agent and the user talk on, and the user's end of it, as
/// the session's events name them.
const USER: &str = "user";

/// How many of what a wait is on the line names before `and N more`.
const AWAITED_MAX: usize = 2;

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
    /// Model call `call` was made to `model`: the first of a round's, a
    /// retry's failover, which keeps the call's id, or one after it, back at
    /// the first.
    Calling {
        model: String,
        call: u64,
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
        /// id -- `None` for the agent's construction, which is none -- the
        /// output, and how much of it was dropped.
        background: Vec<(Option<u64>, String, u64)>,
    },
    /// Something that happened to a submission, said in a line.
    Exec {
        exec: u64,
        what: String,
    },
    /// Submission `exec`'s code has been in a `runtime.wait` for a second:
    /// what it waits on, as the line says it, and how long its timeout has
    /// left, counted from when the wait began rather than from when it was
    /// told of.
    Awaiting {
        exec: u64,
        wait: u64,
        on: String,
        left: Option<Duration>,
    },
    /// That wait ended: how, as the line says it.
    Awaited {
        exec: u64,
        wait: u64,
        what: String,
    },
    /// A call failed and is tried again after `delay`. The library logs it
    /// too, and the log line is what the detail view shows.
    Retry {
        model: String,
        delay: Duration,
        error: String,
    },
    /// Call `call` failed on the model `from` for `error`, and moved to the
    /// model `to`; logged, as a retry is.
    Failover {
        from: String,
        to: String,
        error: String,
        call: u64,
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
pub(super) fn fact(received: &Received, names: &Names) -> Option<Fact> {
    match received {
        Received::Event(event) => match event.subject.as_ref().map(Subject::as_str) {
            Some(child) if child != MAIN_AGENT => {
                of_child(&names.label(child), from_payload(&event.payload)?)
            }
            _ => from_payload(&event.payload),
        },
        Received::Missed(missed) => Some(Fact::Missed(*missed)),
        _ => None,
    }
}

/// The names the children's launches gave them, by id: `agent.spawned` is the
/// first event under a child's subject, and the only one that carries its name.
#[derive(Default)]
pub(super) struct Names(HashMap<String, String>);

impl Names {
    /// Take the name from `received` when it is a child's launch.
    pub(super) fn observe(&mut self, received: &Received) {
        if let Received::Event(event) = received
            && let Payload::AgentSpawned { name, .. } = &event.payload
            && let Some(subject) = &event.subject
        {
            self.0.insert(subject.as_str().to_string(), name.clone());
        }
    }

    /// How the screen calls the child `subject` names: its name and id, or the
    /// id alone when its launch was not seen, as after a `Received::Missed`.
    fn label(&self, subject: &str) -> String {
        let id = subject.strip_prefix("agent/").unwrap_or(subject);
        match self.0.get(subject) {
            Some(name) if !name.is_empty() => format!("{name} ({id})"),
            _ => id.to_string(),
        }
    }
}

/// What the screen shows of `fact`, which the child agent `child` is the
/// subject of: an unfamiliar kind, and a round that failed, each naming it.
fn of_child(child: &str, fact: Fact) -> Option<Fact> {
    match fact {
        Fact::Other { title, data } => Some(Fact::Other {
            title: format!("{title} · {child}"),
            data,
        }),
        Fact::RoundEnded {
            round,
            error: Some(error),
            ..
        } => Some(Fact::Note {
            text: format!("{child}: round {round}'s model calls failed: {error}"),
            inline: false,
        }),
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
                .map(|earlier| {
                    let id = earlier.id.map(|id| id.get());
                    (id, earlier.output.clone(), earlier.dropped)
                })
                .collect(),
        },
        Payload::ExecWaitStarted {
            execid,
            wait,
            on,
            more,
            return_when,
            timeout,
            elapsed,
            ..
        } => Fact::Awaiting {
            exec: execid.get(),
            wait: *wait,
            on: awaiting(on, *more, *return_when),
            left: timeout.map(|timeout| timeout.saturating_sub(*elapsed)),
        },
        Payload::ExecWaitEnded {
            execid,
            wait,
            how,
            done,
            channels,
            error,
            ..
        } => Fact::Awaited {
            exec: execid.get(),
            wait: *wait,
            what: awaited(*how, done, channels, error.as_deref()),
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
        Payload::ModelFailover {
            from,
            to,
            error,
            call_id,
            ..
        } => Fact::Failover {
            from: from.clone(),
            to: to.clone(),
            error: error.clone(),
            call: call_id.get(),
        },
        // What the model is sent from here on changed, so it is said where
        // the person reads the conversation.
        Payload::ModelWithheld {
            model,
            turn,
            finish_reason,
            ..
        } => Fact::Note {
            text: format!(
                "[outrig] {model} refused a call for what it carried (finish reason \
                 {finish_reason}); it is sent again with a note in place of turn {turn}'s \
                 results, which stay in runtime.history"
            ),
            inline: true,
        },
        Payload::ModelCall(call) => Fact::Calling {
            model: call.budget.model.clone(),
            call: call.call_id.get(),
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
        | Payload::InventoryObserved { .. }
        | Payload::SessionReport { .. } => return None,
        other => Fact::Other {
            title: other.kind().to_string(),
            data: other_data(serde_json::to_value(other).unwrap_or_default()),
        },
    };
    Some(fact)
}

/// An unfamiliar event's `data` as the detail view shows it: its JSON, cut to
/// [`OTHER_MAX`], less the fields that say what went wrong, which follow it
/// whole, one to a line.
fn other_data(mut data: Value) -> String {
    let mut why = String::new();
    if let Some(fields) = data.as_object_mut() {
        for name in WHY {
            let text = match fields.get(*name) {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            fields.remove(*name);
            let _ = write!(why, "\n{name}: {text}");
        }
    }
    cut(&data.to_string(), OTHER_MAX) + &why
}

/// What a usage record says was spent: its total, or the sum of its parts
/// when the provider gave no total.
fn tokens(usage: &Usage) -> u64 {
    usage
        .total_tokens
        .max(usage.input_tokens + usage.output_tokens)
}

/// `text`, cut to `max` characters.
/// What a wait is on, as the line says it: `ci_run, review`, `the first of
/// ci_run, review`, or `ci_run, review and 3 more`.
fn awaiting(on: &[Awaited], more: u64, return_when: ReturnWhen) -> String {
    let names = names_of(on, more);
    if return_when == ReturnWhen::FirstCompleted && (on.len() + more as usize) > 1 {
        format!("the first of {names}")
    } else {
        names
    }
}

/// How a wait ended, as the line says it.
fn awaited(how: WaitEnd, done: &[Awaited], channels: &[String], error: Option<&str>) -> String {
    let done = names_of(done, 0);
    match how {
        WaitEnd::Returned if done.is_empty() => "wait over".to_string(),
        WaitEnd::Returned => format!("wait over: {done} done"),
        WaitEnd::Timeout if done.is_empty() => "wait timed out".to_string(),
        WaitEnd::Timeout => format!("wait timed out; {done} done"),
        WaitEnd::Message => format!("wait ended: a message arrived on {}", channels.join(", ")),
        WaitEnd::Raised => format!("wait ended by {}", error.unwrap_or("an exception")),
        _ => "wait ended".to_string(),
    }
}

/// The first [`AWAITED_MAX`] of `on` by name, and how many more: those past
/// them, and `more`, which the event did not list.
fn names_of(on: &[Awaited], more: u64) -> String {
    let names: Vec<String> = on.iter().take(AWAITED_MAX).map(awaited_name).collect();
    let rest = on.len().saturating_sub(AWAITED_MAX) as u64 + more;
    match (names.is_empty(), rest) {
        (true, 0) => String::new(),
        (true, rest) => format!("{rest} futures"),
        (false, rest) => joined(&names, rest),
    }
}

/// `names`, and how many more there are: `a, b`, or `a, b and 3 more`.
pub(in crate::cli::run_new) fn joined(names: &[String], more: u64) -> String {
    match more {
        0 => names.join(", "),
        more => format!("{} and {more} more", names.join(", ")),
    }
}

/// One thing a wait is on, as the line names it: a task by its name, or by
/// its coroutine when it was left with asyncio's `Task-N`; a handle by its
/// child, or by its request while the child is unknown; a typed call by its
/// function.
fn awaited_name(awaited: &Awaited) -> String {
    let (what, id, child, name) = match awaited {
        Awaited::Task { name, code, .. } => {
            return match code {
                Some(code) if is_default_task_name(name) => code.clone(),
                _ => name.clone(),
            };
        }
        Awaited::Call { function, .. } => {
            return format!("{}()", function.rsplit('.').next().unwrap_or(function));
        }
        Awaited::Skill { skill, .. } => return format!("skill '{skill}'"),
        Awaited::Future { type_name, .. } => return type_name.clone(),
        Awaited::Submission {
            id, child, name, ..
        } => ("submission", id, child, name),
        Awaited::Request {
            id, child, name, ..
        } => ("request", id, child, name),
        _ => return "something".to_string(),
    };
    match (child, name, id) {
        (Some(child), Some(name), _) if !name.is_empty() => format!("{child} '{name}'"),
        (Some(child), _, _) => child.clone(),
        (None, _, Some(id)) => format!("{what} {id}"),
        (None, _, None) => what.to_string(),
    }
}

/// Whether `name` is one asyncio gave a task nobody named: `Task-7`.
fn is_default_task_name(name: &str) -> bool {
    name.strip_prefix("Task-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The main agent's waits among `open`, as the facts that would have set
/// them: what restores the status line after a gap in the events hid one.
pub(super) fn restored(open: &[OpenWait], now: Instant) -> Vec<Fact> {
    open.iter()
        .filter(|wait| wait.agent.as_str() == MAIN_AGENT)
        .map(|wait| Fact::Awaiting {
            exec: wait.execid.get(),
            wait: wait.wait,
            on: awaiting(&wait.on, wait.more, wait.return_when),
            left: wait
                .timeout
                .map(|timeout| (wait.since + timeout).saturating_duration_since(now)),
        })
        .collect()
}

pub(super) fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What says why an unfamiliar event failed is shown whole, however long,
    /// and the rest is cut as before.
    #[test]
    fn an_unfamiliar_events_failure_is_shown_whole() {
        let traceback = format!("Traceback (most recent call last):\n{}", "  x\n".repeat(200));
        let shown = other_data(serde_json::json!({
            "request": 7,
            "outcome": "raised",
            "body": "b".repeat(500),
            "traceback": traceback,
            "problems": ["too short", "not a list"],
            "detail": null,
        }));
        assert!(shown.ends_with(&format!(
            "\ntraceback: {traceback}\nproblems: too short\nnot a list"
        )));
        assert!(!shown.contains("\ndetail:"), "a field that says nothing stays in the JSON");
        let (json, _) = shown.split_once('\n').expect("the fields after the JSON");
        assert!(!json.contains("Traceback"));
        assert!(json.ends_with("..."), "the rest is still cut: {json}");
    }

    /// A child's round that failed is shown with its error and the child's
    /// name; the rest of its rounds are its own.
    #[test]
    fn a_childs_failed_round_is_shown() {
        let ended = |error: Option<&str>| Fact::RoundEnded {
            round: 2,
            tokens: Some(10),
            error: error.map(str::to_string),
        };
        assert_eq!(
            of_child("helper", ended(Some("finish reason content_filter: empty"))),
            Some(Fact::Note {
                text: "helper: round 2's model calls failed: finish reason content_filter: empty"
                    .into(),
                inline: false,
            })
        );
        assert_eq!(of_child("helper", ended(None)), None);
    }

    /// A child is labeled by the name its launch gave it, and by its id alone
    /// when that launch was missed.
    #[test]
    fn a_child_is_labeled_by_its_name() {
        let mut names = Names::default();
        names.0.insert("agent/c3".into(), "city_fact".into());
        names.0.insert("agent/c4".into(), String::new());
        assert_eq!(names.label("agent/c3"), "city_fact (c3)");
        assert_eq!(names.label("agent/c4"), "c4");
        assert_eq!(names.label("agent/c5"), "c5");
    }

    #[test]
    fn text_is_cut_by_characters() {
        assert_eq!(cut("abcdef", 3), "abc...");
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("日本語の", 2), "日本...");
    }

    /// One thing a wait is on, read as the library would read it from the
    /// interpreter's line: the one way this crate can hold one.
    fn one(json: serde_json::Value) -> Awaited {
        serde_json::from_value(json).expect("an awaited")
    }

    /// What a wait is on, as the line names it: a task by its name, a handle
    /// by its child, or by its request while the child is unknown, a typed
    /// call by its function; two names and then a count; and `the first of`
    /// for `FIRST_COMPLETED` when there is more than one.
    #[test]
    fn what_a_wait_awaits_is_named_as_the_line_says_it() {
        let task = one(serde_json::json!({"kind": "task", "name": "ci_run", "code": "watch"}));
        let unnamed = one(serde_json::json!({"kind": "task", "name": "Task-7", "code": "watch_ci"}));
        let bare = one(serde_json::json!({"kind": "task", "name": "Task-8", "code": null}));
        let child = one(serde_json::json!(
            {"kind": "submission", "id": 7, "child": "c1", "name": "reviewer"}
        ));
        let unknown = one(serde_json::json!(
            {"kind": "request", "id": 8, "channel": "fizz", "child": null, "name": null}
        ));
        let call = one(serde_json::json!(
            {"kind": "call", "id": 9, "function": "pkg.mod.review_section", "child": null, "name": null}
        ));
        let skill = one(serde_json::json!({"kind": "skill", "id": 3, "skill": "lint"}));
        let future = one(serde_json::json!({"kind": "future", "type": "Future"}));
        assert_eq!(awaiting(std::slice::from_ref(&task), 0, ReturnWhen::AllCompleted), "ci_run");
        assert_eq!(
            awaiting(&[unnamed, bare], 0, ReturnWhen::AllCompleted),
            "watch_ci, Task-8",
            "a task nobody named reads as its coroutine, when it has one"
        );
        assert_eq!(
            awaiting(&[task.clone(), child], 0, ReturnWhen::FirstCompleted),
            "the first of ci_run, c1 'reviewer'"
        );
        assert_eq!(
            awaiting(std::slice::from_ref(&task), 0, ReturnWhen::FirstCompleted),
            "ci_run",
            "one thing has no first"
        );
        assert_eq!(
            awaiting(&[unknown, call, skill, future], 2, ReturnWhen::AllCompleted),
            "request 8, review_section() and 4 more"
        );
        assert_eq!(awaited(WaitEnd::Returned, std::slice::from_ref(&task), &[], None), "wait over: ci_run done");
        assert_eq!(awaited(WaitEnd::Timeout, &[], &[], None), "wait timed out");
        assert_eq!(
            awaited(WaitEnd::Timeout, &[task], &[], None),
            "wait timed out; ci_run done"
        );
        assert_eq!(
            awaited(WaitEnd::Message, &[], &["user".to_string()], None),
            "wait ended: a message arrived on user"
        );
        assert_eq!(
            awaited(WaitEnd::Raised, &[], &[], Some("CancelledError")),
            "wait ended by CancelledError"
        );
    }
}
