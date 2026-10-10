//! What a session's shutdown reports: whether everything it started is
//! proven stopped, how each execution running at the close ended, and how its
//! events were delivered.

use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

use super::ClosedBy;
use super::event::ExecId;

/// What [`Session::shutdown`](super::Session::shutdown) found. Read it before
/// deciding anything about the work the session did: it is the first point at
/// which nothing the session started is still running, if
/// [`ShutdownReport::verdict`] says so.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ShutdownReport {
    /// What closed admission. It is closed, whatever closed it.
    pub closed_by: ClosedBy,
    /// Whether what the session started is proven stopped.
    pub stopped: Stopped,
    /// Each execution running when admission closed, and how it ended.
    pub executions: Vec<ExecutionOutcome>,
    pub events: EventDelivery,
}

impl ShutdownReport {
    /// What the report adds up to.
    pub fn verdict(&self) -> Verdict {
        verdict(&self.stopped, &self.executions)
    }
}

pub(crate) fn verdict(stopped: &Stopped, executions: &[ExecutionOutcome]) -> Verdict {
    match stopped {
        Stopped::NotProven { .. } => Verdict::NotProvenStopped,
        Stopped::Proven
            if executions
                .iter()
                .any(|execution| execution.status == ExecutionStatus::Unknown) =>
        {
            Verdict::StoppedWithUnknown
        }
        Stopped::Proven => Verdict::Clean,
    }
}

/// Whether what the session started is proven stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Stopped {
    /// Its container was stopped, which took the interpreter and every kernel
    /// with it.
    Proven,
    /// Something could not be confirmed stopped, for `reason`: something the
    /// session started may still be acting.
    #[non_exhaustive]
    NotProven { reason: String },
}

/// One execution running when admission closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ExecutionOutcome {
    #[serde(rename = "execid")]
    pub id: ExecId,
    pub status: ExecutionStatus,
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExecutionStatus {
    /// Its result arrived: it ran to completion.
    Ok,
    /// Its result arrived: it raised.
    Error,
    /// No result arrived. It was cut off, by the container stopping or the
    /// interpreter exiting, and what it did may or may not have happened.
    Unknown,
}

/// What a report adds up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// Everything stopped, and every outcome is known: the stop is clean.
    Clean,
    /// Everything stopped, and some execution was cut off. Its outcome is
    /// unknown, never failed: its effect may stand.
    StoppedWithUnknown,
    /// Something could not be confirmed stopped. The stop is not clean, and
    /// what the session ran on should not be reused.
    NotProvenStopped,
}

/// How the session's events reached its subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventDelivery {
    /// The id of the last event published.
    pub last_sequence: u64,
    /// How many events each subscription lost, in the order they were made.
    pub missed: Vec<u64>,
    /// What `events.jsonl` did not get, when the session recorded one and it
    /// missed anything.
    pub log: Option<LogLoss>,
}

/// Events the session's log did not get.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogLoss {
    pub path: PathBuf,
    pub records: u64,
    /// Why the first was lost.
    pub first: String,
    /// When the writer could not prove its rollback: why the file may hold a
    /// partial record.
    pub integrity: Option<String>,
}

impl fmt::Display for LogLoss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} agent event(s) could not be written to {}; the first was lost because: {}",
            self.records,
            self.path.display(),
            self.first
        )?;
        if let Some(why) = &self.integrity {
            write!(
                f,
                "\n  and the log may hold a partial record that could not be removed: {why}"
            )?;
        }
        Ok(())
    }
}
