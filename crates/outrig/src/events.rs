//! The agent event log: what a session's agents did, one record per line of
//! `<log_dir>/events.jsonl`.
//!
//! One stream rather than a file per subject, because an agent's timeline is a
//! causal chain -- a model call produced this code, which printed this, which
//! sent this message -- and recovering that order by joining files on their
//! timestamps fails exactly when it is needed. Every record is numbered and
//! queued in one step, so the file's order is the order they happened in.
//!
//! The envelope is CloudEvents 1.0 and the payloads are OutRig's. The top
//! level carries the standard context attributes and nothing else, because
//! CloudEvents attribute names are lower-case letters and digits only:
//! everything OutRig-specific is inside `data`. `doc/reference/events.md` is
//! the schema.
//!
//! # Recording is not allowed to change what it records
//!
//! The file is written by a [`LineSink`], and [`Events::emit`] never waits on
//! it: the places that emit include the task reading the interpreter's
//! replies, whose stalling would make healthy Python look stuck to the
//! liveness check, and the path a Ctrl-C takes. Where waiting is harmless --
//! the model loop and the Python tool -- [`Events::ready`] waits for the
//! writer to catch up first, which is how a stalled disk slows the agent down
//! rather than losing its record. The queue holds four times what `ready`
//! waits for, and an event that finds even that full is the sink's to count
//! lost, which [`Events::close`] reports with everything else the file did not
//! get.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rig::completion::{Message, ToolDefinition};
use serde::Serialize;

use crate::error::{IoPathExt, OutrigError, Result};
use crate::line_sink::{self, Labels, LineSink, Loss, Room};
use crate::python::host::{Background, ExecId};

/// The log's name under the session's log directory.
pub(crate) const EVENTS_LOG: &str = "events.jsonl";

/// Who can read it: its owner. It holds message bodies the model may never
/// have printed, which is a different sensitivity from a connection log.
const MODE: u32 = 0o600;

/// How many events may wait for the writer before one that cannot wait is
/// lost. [`Events::ready`] waits at a quarter of this.
const QUEUE: usize = 4 * line_sink::QUEUE;

/// The type prefix: the reverse-DNS name OutRig's labels already use.
const TYPE_PREFIX: &str = "org.outrig.";

/// The `subject` of an event that belongs to an agent: the protocol's id for
/// the one agent a session runs.
pub(crate) const PRIMARY_SUBJECT: &str = "agent/primary";

static EVENT_LABELS: Labels = Labels {
    what: "agent event",
    claimant: "agent",
    warn: |args| tracing::warn!(target: "outrig::events", "{args}"),
    error: |args| tracing::error!(target: "outrig::events", "{args}"),
};

/// Where an agent's events go. Cheap to clone; every clone emits into the one
/// log. [`Events::off`] emits nowhere, and is what a session that did not ask
/// for the log holds.
#[derive(Clone, Default)]
pub(crate) struct Events {
    handle: Option<Arc<Handle>>,
}

struct Handle {
    path: PathBuf,
    /// The session, as a CloudEvents `source`.
    source: String,
    /// How far behind the writer is, without a sender to keep it open.
    room: Room<()>,
    writer: Mutex<Writer>,
}

/// What numbering and queueing share, under one lock.
struct Writer {
    /// The id the last event was given. Ids start at 1.
    last: u64,
    /// The sink, until [`Events::close`] takes it. This is its only sender,
    /// so the last handle dropping lets the writer finish on its own.
    sink: Option<LineSink<()>>,
}

impl Events {
    /// No log: every emit is dropped, and nothing waits.
    pub(crate) fn off() -> Self {
        Self::default()
    }

    /// Whether events go anywhere.
    pub(crate) fn is_on(&self) -> bool {
        self.handle.is_some()
    }

    /// Open `<log_dir>/events.jsonl` for the session `source` names.
    ///
    /// A log that already holds a recording is refused, and left as it is.
    /// Its events carry this `source` with ids from 1, as a second recording's
    /// would -- an agent started again on the same session -- and a reader
    /// that takes `source` and `id` as an event's identity, as CloudEvents
    /// says to, would drop the second's as repeats.
    pub(crate) async fn open(log_dir: &Path, source: String) -> Result<Self> {
        let path = log_dir.join(EVENTS_LOG);
        let sink = LineSink::open(&path, Some(MODE), QUEUE, &EVENT_LABELS).await?;
        // Read under the claim the sink holds, so nothing appends in between.
        // Dropping the sink on the error lets its writer end and the claim go.
        let held = tokio::fs::metadata(&path)
            .await
            .path_ctx("stat", &path)?
            .len();
        if held > 0 {
            return Err(OutrigError::Configuration(format!(
                "the agent event log {} already holds a recording ({held} bytes); a recording \
                 starts a log of its own, so each event's source and id name it alone. Move it \
                 aside, or record in a fresh session",
                path.display()
            )));
        }
        Ok(Self::over(sink, path, source))
    }

    /// Emit into `sink`, which writes `path`.
    fn over(sink: LineSink<()>, path: PathBuf, source: String) -> Self {
        Self {
            handle: Some(Arc::new(Handle {
                path,
                source,
                room: sink.room(),
                writer: Mutex::new(Writer {
                    last: 0,
                    sink: Some(sink),
                }),
            })),
        }
    }

    /// Wait until the writer has caught up to within a quarter of its queue.
    /// Called where pausing is harmless, ahead of what will emit, so a writer
    /// that has fallen behind holds up the agent rather than losing its
    /// record.
    pub(crate) async fn ready(&self) {
        if let Some(handle) = &self.handle {
            handle.room.below(line_sink::QUEUE as u64).await;
        }
    }

    /// Record `event`, now. Never waits: an event there is no room for is
    /// counted lost instead, and one emitted after [`Events::close`] is
    /// dropped, there being no file to report it to.
    pub(crate) fn emit(&self, event: Event<'_>) {
        let Some(handle) = &self.handle else {
            return;
        };
        // Encoded before the lock every emitter shares.
        let data = serde_json::to_vec(&event);
        let mut writer = lock(&handle.writer);
        let Writer { last, sink } = &mut *writer;
        let Some(sink) = sink else {
            tracing::debug!(
                target: "outrig::events",
                "not recorded, the log having closed: {}",
                event.kind()
            );
            return;
        };
        let line = match data {
            Ok(data) => envelope(*last + 1, &handle.source, &event, &data),
            Err(e) => {
                let why = format!("encoding {} failed: {e}", event.kind());
                tracing::warn!(target: "outrig::events", "{why}");
                sink.lose(&(), std::io::Error::other(why));
                return;
            }
        };
        // Numbered and queued under one lock, so an event's id is its place
        // in the file; an id is spent only on an event that was queued.
        if sink.try_enqueue((), line, || {
            format!("more than {QUEUE} events were waiting for the agent event writer")
        }) {
            *last += 1;
        }
    }

    /// Stop taking events, and wait until the file holds every one it was
    /// given -- at most [`line_sink::SHUTDOWN_GRACE`]. What it does not hold by
    /// then is reported, counted, rather than waited for.
    pub(crate) async fn close(&self) -> std::result::Result<(), EventsUnwritten> {
        let Some(handle) = &self.handle else {
            return Ok(());
        };
        let Some(mut sink) = lock(&handle.writer).sink.take() else {
            return Ok(());
        };
        // A writer that had to be stopped holding nothing lost nothing, so
        // only what the sink counted is reported.
        let _ = sink.close().await;
        match sink.take_loss(&()) {
            None => Ok(()),
            Some(Loss {
                records,
                source,
                integrity,
            }) => Err(EventsUnwritten {
                path: handle.path.clone(),
                records,
                first: source.to_string(),
                integrity: integrity.map(|why| why.to_string()),
            }),
        }
    }
}

/// One record: the CloudEvents context attributes, then `data`.
fn envelope(id: u64, source: &str, event: &Event<'_>, data: &[u8]) -> Vec<u8> {
    #[derive(Serialize)]
    struct Context<'a> {
        specversion: &'static str,
        id: String,
        source: &'a str,
        #[serde(rename = "type")]
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<&'static str>,
        time: String,
        datacontenttype: &'static str,
    }
    let context = Context {
        specversion: "1.0",
        id: id.to_string(),
        source,
        kind: format!("{TYPE_PREFIX}{}", event.kind()),
        subject: event.subject(),
        time: jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string(),
        datacontenttype: "application/json",
    };
    let mut line = serde_json::to_vec(&context).expect("the context attributes are plain strings");
    // `}` replaced by `data` and a closing brace: the payload was encoded once,
    // ahead of the lock this runs under.
    line.pop();
    line.reserve(data.len() + 10);
    line.extend_from_slice(b",\"data\":");
    line.extend_from_slice(data);
    line.extend_from_slice(b"}\n");
    line
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Events the log could not keep: how many, why the first was lost, and --
/// when the writer could not prove its rollback -- why the file may hold a
/// partial record.
#[derive(Debug, thiserror::Error)]
#[error(
    "{records} agent event(s) could not be written to {}; the first was lost because: {first}{}",
    path.display(),
    match integrity {
        Some(why) => format!(
            "\n  and the log may hold a partial record that could not be removed: {why}"
        ),
        None => String::new(),
    }
)]
pub(crate) struct EventsUnwritten {
    pub(crate) path: PathBuf,
    pub(crate) records: u64,
    /// Why the first was lost.
    pub(crate) first: String,
    pub(crate) integrity: Option<String>,
}

// ---------------------------------------------------------------------------
// The catalog. Each event's `data` is the variant's fields, as they are
// named here; `doc/reference/events.md` says what each means and which of
// the three categories it belongs to.

/// Something an agent did, or something done to it.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Event<'a> {
    // Model view: what a model call was presented, or produced.
    ModelInstructions {
        model: &'a str,
        preamble: &'a str,
        tools: &'a [ToolDefinition],
        max_tokens: Option<u32>,
    },
    TurnCommitted {
        turn: usize,
        round: u32,
        incomplete: bool,
        messages: &'a [Message],
    },
    ModelCall(ModelCall<'a>),
    ExecSubmitted {
        execid: ExecId,
        source: &'a str,
    },

    // Execution diagnostics: the runtime's own state.
    AgentStarted {
        model: &'a str,
        python: &'a str,
        container: &'a str,
        tool_call_max: usize,
        tool_result_max: usize,
    },
    AgentStopped {},
    RoundStarted {
        round: u32,
    },
    ExecRefused {
        execid: ExecId,
        holder: ExecId,
    },
    ExecCompleted {
        execid: ExecId,
        status: &'static str,
        duration: f64,
        output: &'a str,
        dropped: u64,
        error: Option<&'a str>,
        background: &'a [Background],
    },
    MemoryExhausted {
        execid: ExecId,
    },
    ExecCancelSent {
        execid: ExecId,
    },
    ExecInterruptSent {
        execid: ExecId,
        runaway: bool,
    },
    ExecProbeFailed {
        execid: ExecId,
        verdict: &'static str,
    },
    ExecAbandoned {
        execid: ExecId,
        why: &'static str,
    },
    InventoryObserved {
        execid: ExecId,
        names: Vec<Held<'a>>,
        total: usize,
        more: usize,
    },
    ToolResultTruncated {
        execid: ExecId,
        size: usize,
        max: usize,
        kept: usize,
    },
    ContextPromoted {
        turns: &'a [u64],
    },
    ContextDemoted {
        turns: &'a [u64],
    },
    OutputUnattributed {
        text: &'a str,
    },
    InterpreterDiagnostic {
        text: &'a str,
    },
    InterpreterExited {
        cause: &'a str,
    },

    // Integration audit: what crossed to a provider, or across a channel.
    ModelRoundCompleted {
        round: u32,
        stopped: Option<&'a str>,
        usage: Usage,
        calls: Vec<CallUsage>,
        input_tokens_max: u64,
    },
    ModelRoundFailed {
        round: u32,
        error: &'a str,
        calls: Vec<CallUsage>,
    },
    ModelRoundDropped {
        round: u32,
        calls: Vec<CallUsage>,
    },
    #[cfg_attr(not(test), expect(dead_code, reason = "0003-15 emits it"))]
    ModelRetry {
        attempt: u32,
        delay: f64,
        error: &'a str,
    },
    #[cfg_attr(not(test), expect(dead_code, reason = "0003-15 emits it"))]
    ModelFailover {
        from: &'a str,
        to: &'a str,
        error: &'a str,
    },
    MessageSent {
        message: ExecId,
        channel: &'a str,
        from: &'a str,
        to: &'a str,
        body: &'a str,
    },
    MessageRefused {
        message: ExecId,
        channel: &'a str,
        from: &'a str,
        to: &'a str,
        reason: &'a str,
    },
    MessageReceived {
        message: ExecId,
        channel: &'a str,
        from: &'a str,
        to: &'a str,
    },
}

impl Event<'_> {
    /// The event's type, after `org.outrig.`.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Event::ModelInstructions { .. } => "model.instructions",
            Event::TurnCommitted { .. } => "turn.committed",
            Event::ModelCall(_) => "model.call",
            Event::ExecSubmitted { .. } => "exec.submitted",
            Event::AgentStarted { .. } => "agent.started",
            Event::AgentStopped {} => "agent.stopped",
            Event::RoundStarted { .. } => "round.started",
            Event::ExecRefused { .. } => "exec.refused",
            Event::ExecCompleted { .. } => "exec.completed",
            Event::MemoryExhausted { .. } => "memory.exhausted",
            Event::ExecCancelSent { .. } => "exec.cancel.sent",
            Event::ExecInterruptSent { .. } => "exec.interrupt.sent",
            Event::ExecProbeFailed { .. } => "exec.probe.failed",
            Event::ExecAbandoned { .. } => "exec.abandoned",
            Event::InventoryObserved { .. } => "inventory.observed",
            Event::ToolResultTruncated { .. } => "tool.result.truncated",
            Event::ContextPromoted { .. } => "context.promoted",
            Event::ContextDemoted { .. } => "context.demoted",
            Event::OutputUnattributed { .. } => "output.unattributed",
            Event::InterpreterDiagnostic { .. } => "interpreter.diagnostic",
            Event::InterpreterExited { .. } => "interpreter.exited",
            Event::ModelRoundCompleted { .. } => "model.round.completed",
            Event::ModelRoundFailed { .. } => "model.round.failed",
            Event::ModelRoundDropped { .. } => "model.round.dropped",
            Event::ModelRetry { .. } => "model.retry",
            Event::ModelFailover { .. } => "model.failover",
            Event::MessageSent { .. } => "message.sent",
            Event::MessageRefused { .. } => "message.refused",
            Event::MessageReceived { .. } => "message.received",
        }
    }

    /// The agent the event belongs to, or `None` for one that belongs to the
    /// interpreter every agent shares.
    fn subject(&self) -> Option<&'static str> {
        match self {
            Event::OutputUnattributed { .. }
            | Event::InterpreterDiagnostic { .. }
            | Event::InterpreterExited { .. } => None,
            _ => Some(PRIMARY_SUBJECT),
        }
    }
}

/// One model call's manifest: what it was sent of the conversation, and why.
#[derive(Debug, Serialize)]
pub(crate) struct ModelCall<'a> {
    pub(crate) call: u64,
    pub(crate) round: u32,
    pub(crate) budget: CallBudget<'a>,
    pub(crate) estimate: u64,
    pub(crate) carried: Vec<Chosen>,
    pub(crate) evicted: Vec<Chosen>,
    pub(crate) opening: Option<&'a Message>,
    pub(crate) adjacent: Vec<Repeat>,
}

/// What a call was held to, in tokens.
#[derive(Debug, Serialize)]
pub(crate) struct CallBudget<'a> {
    pub(crate) model: &'a str,
    pub(crate) window: u32,
    pub(crate) window_assumed: bool,
    pub(crate) reserve: u32,
    pub(crate) overhead: u64,
}

/// A turn a call carried or left out, and the reason it was chosen.
#[derive(Debug, Serialize)]
pub(crate) struct Chosen {
    pub(crate) turn: usize,
    pub(crate) why: &'static str,
}

/// Where one role follows itself in what a call was sent.
#[derive(Debug, Serialize)]
pub(crate) struct Repeat {
    pub(crate) turn: Option<usize>,
    pub(crate) role: &'static str,
}

/// A name the agent's namespace holds, and its value's type.
#[derive(Debug, Serialize)]
pub(crate) struct Held<'a> {
    pub(crate) name: &'a str,
    #[serde(rename = "type")]
    pub(crate) kind: &'a str,
}

/// Tokens, as the provider reported them. Zeros mean it reported none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Usage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) reasoning_tokens: u64,
}

impl From<rig::completion::Usage> for Usage {
    fn from(usage: rig::completion::Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

/// One model call's tokens, by its place in the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct CallUsage {
    pub(crate) index: usize,
    pub(crate) usage: Usage,
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) use self::testing::*;

/// Opening a log, and reading one back, for tests across the crate.
#[cfg(test)]
mod testing {
    use std::path::Path;

    use serde_json::Value;

    use super::Events;

    /// The `source` of a test's log.
    pub(crate) const TEST_SOURCE: &str = "/outrig/session/test";

    /// A log in `dir`, as `PythonAgent::start` opens one.
    pub(crate) async fn opened(dir: &Path) -> Events {
        Events::open(dir, TEST_SOURCE.to_string())
            .await
            .expect("open the log")
    }

    /// Wait until nothing holds the lock on `log_dir`'s closed log, for a test
    /// that opens it again.
    ///
    /// Closing the log does not release its lock at once when another test
    /// is starting a process. `flock` belongs to the open file description,
    /// and a process being spawned holds a copy of the descriptor table it
    /// was cloned with until it execs, so the lock outlives the writer by
    /// that long: a fraction of a millisecond, often enough for a reopen to
    /// be refused as "already owned" rather than for what it holds.
    pub(crate) async fn released(log_dir: &Path) {
        let path = log_dir.join(super::EVENTS_LOG);
        tokio::task::spawn_blocking(move || {
            let log = std::fs::File::open(&path).expect("open the log");
            nix::fcntl::Flock::lock(log, nix::fcntl::FlockArg::LockExclusive)
                .map_err(|(_, errno)| errno)
                .expect("lock the log");
        })
        .await
        .expect("wait for the log's lock");
    }

    /// Every record in `log_dir`'s `events.jsonl`, in order, each checked to
    /// parse.
    pub(crate) fn recorded(log_dir: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(log_dir.join(super::EVENTS_LOG)).expect("read events");
        text.lines()
            .map(|line| serde_json::from_str(line).expect("every line is a whole record"))
            .collect()
    }

    /// The type of each record, without the prefix.
    pub(crate) fn kinds(records: &[Value]) -> Vec<String> {
        records
            .iter()
            .map(|record| {
                record["type"]
                    .as_str()
                    .and_then(|kind| kind.strip_prefix(super::TYPE_PREFIX))
                    .expect("an OutRig type")
                    .to_string()
            })
            .collect()
    }

    /// The `data` of each record of type `kind`, in order.
    pub(crate) fn of_kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
        let kind = format!("{}{kind}", super::TYPE_PREFIX);
        records
            .iter()
            .filter(|record| record["type"] == kind.as_str())
            .map(|record| &record["data"])
            .collect()
    }
}
