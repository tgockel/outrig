//! The host's half of the protocol `interpreter.py` speaks.
//!
//! [`Interpreter::start`] execs the program inside a session's primary
//! container and waits for its `ready` greeting, so an interpreter that cannot
//! run there is a startup error rather than a hang on the first submission.
//! Two tasks then own the transport between them: a writer, the only thing
//! that touches the exec's stdin, and a reader, the only thing that reads its
//! stdout and the one that correlates each reply to the request that asked for
//! it. They are separate because the interpreter answers some requests from
//! its reader thread -- a refusal, an opened agent's `ready` -- and a host that
//! stopped reading while it wrote could leave both sides blocked on full pipes.
//!
//! # Three outcomes, and two ways of not knowing
//!
//! An execution ends `ok`, `error`, or `unknown`, and nothing is ever rolled
//! back, so `unknown` is never retried here: re-running a submission whose
//! effects are unknown is how one `git push` becomes two. It comes in two kinds
//! that stay apart. [`Unknown::Exited`] means the interpreter is gone, so the
//! execution is over however it ended. [`Unknown::Unresolved`] means the caller
//! stopped waiting: the execution may still be running and it keeps its slot,
//! so a later submission is refused naming it, and a reply that arrives
//! afterwards becomes a [`Late`] record rather than rewriting what the caller
//! was told.
//!
//! # What owns what
//!
//! The [`Child`] here is the host-side `podman exec` client, not the
//! interpreter, which runs under conmon and outlives its client (see
//! [`Container::exec_stdio`]). Dropping every handle closes the interpreter's
//! stdin, which is what makes it exit on the ordinary path; otherwise it ends
//! when the container stops.

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::container::{Container, ExecOptions};
use crate::error::OutrigError;

use super::payload::PAYLOAD_MOUNT;

/// The program the interpreter runs, passed on its command line.
const PROGRAM: &str = include_str!("interpreter.py");

/// What the payload's `python3` is started with, ahead of the agent id. `-I`
/// isolates it: no `PYTHON*` variable, user site, or other configuration the
/// image carries reaches it.
pub(super) const ARGS: [&str; 3] = ["-I", "-c", PROGRAM];

/// The primary agent's id: the program's one argument, and the agent every
/// request here addresses.
pub(super) const PRIMARY: &str = "primary";

/// How long a started interpreter has to greet. CPython starts and compiles
/// the program in well under a second; this only has to be finite.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest reply line read whole. The interpreter bounds everything it
/// sends, and its largest reply -- an inventory of 200 names and types of up
/// to 1000 characters each, JSON-escaped -- stays under 5 MiB. A longer line is
/// something executed code wrote to the protocol descriptor.
const REPLY_LINE_MAX: usize = 16 << 20;

/// The reply buffer's capacity kept between lines, so one large reply does
/// not hold its allocation for the rest of the session.
const REPLY_BUFFER_KEPT: usize = 64 << 10;

/// The longest stderr line logged whole; the rest of a longer one is cut.
const STDERR_LINE_MAX: usize = 4 << 10;

/// How many stderr lines are kept to explain an exit.
const STDERR_TAIL_LINES: usize = 20;

/// How long, once its stdout has closed, the exec client has to exit and its
/// stderr to drain before the exit is described without them.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Prefix of the lines the interpreter itself writes to stderr.
const DIAGNOSTIC: &str = "outrig-interpreter:";

/// Names one submission, for the life of the interpreter. Host-assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ExecId(u64);

impl fmt::Display for ExecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What an execution that ran reported, already bounded by the interpreter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub(crate) struct Report {
    /// What the execution wrote, and a trailing expression's echoed value.
    pub(crate) output: String,
    /// Bytes of output past the interpreter's bound.
    pub(crate) dropped: u64,
    /// What earlier executions wrote since the last result, by execution.
    pub(crate) background: Vec<Background>,
}

/// Output from an earlier execution, delivered with a later result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Background {
    pub(crate) id: ExecId,
    pub(crate) output: String,
    pub(crate) dropped: u64,
}

/// How a submission ended, as far as the host can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Ran to completion.
    Ok(Report),
    /// Raised. Whatever it did before the traceback's last frame happened.
    Error { report: Report, traceback: String },
    /// Not run: `holder` has the agent's one slot. Not an outcome of an
    /// execution, since nothing ran, but it arrives the way one does.
    Refused { holder: ExecId },
    /// The host cannot say, and must not retry.
    Unknown(Unknown),
}

/// The two ways of not knowing, which must not be collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unknown {
    /// The interpreter's output closed first: no reply can come, and the
    /// execution is over, however it ended. `cause` says what is known.
    Exited { id: ExecId, cause: Arc<str> },
    /// The caller stopped waiting. The execution may still be running and
    /// holds the slot until its reply arrives, as a [`Late`].
    Unresolved { id: ExecId },
}

/// A result nobody was waiting for when it arrived: an execution recorded
/// [`Unknown::Unresolved`], or one whose [`Execution`] was dropped. A new
/// observation of `id`, never a correction of what its caller was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Late {
    pub(crate) id: ExecId,
    pub(crate) outcome: Outcome,
}

/// What the agent's namespace holds: names and type names, nothing evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inventory {
    pub(crate) globals: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum InterpreterError {
    /// `podman exec` itself could not be started.
    #[error(transparent)]
    Launch(#[from] OutrigError),
    #[error("the Python interpreter did not start: {0}")]
    Startup(String),
    /// The interpreter can no longer be reached; nothing was sent.
    #[error("the Python interpreter is gone: {0}")]
    Gone(Arc<str>),
}

/// A handle on a session's interpreter, addressing its primary agent. Cheap
/// to clone; every clone drives the one interpreter.
#[derive(Clone)]
pub(crate) struct Interpreter {
    table: Arc<Mutex<Table>>,
    /// Whole protocol lines for the writer task. Queued rather than written by
    /// the caller, so a caller cancelled mid-write cannot tear a line.
    lines: mpsc::UnboundedSender<String>,
}

/// One submission's handle. Its id is fixed at submission, which is what a
/// later interrupt or cancellation targets.
pub(crate) struct Execution {
    id: ExecId,
    receiver: oneshot::Receiver<Outcome>,
    /// The outcome once [`Execution::outcome`] has returned it.
    settled: Option<Outcome>,
    table: Arc<Mutex<Table>>,
}

/// The correlation state the reader task shares with every handle.
#[derive(Default)]
struct Table {
    last_id: u64,
    /// The agent's outstanding execution, answered or not. One at a time.
    slot: Option<Slot>,
    inventories: HashMap<ExecId, oneshot::Sender<Inventory>>,
    late: Vec<Late>,
    /// Why nothing more can be sent, once that is so.
    ended: Option<Arc<str>>,
}

struct Slot {
    id: ExecId,
    waiter: oneshot::Sender<Outcome>,
}

/// One protocol line from the interpreter, which names the agent it concerns.
#[derive(Deserialize)]
struct Envelope {
    agent: String,
    #[serde(flatten)]
    reply: Reply,
}

/// Unknown kinds are ignored, as the interpreter ignores kinds it does not
/// know.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Reply {
    Ready,
    Result(WireResult),
    Inv {
        id: ExecId,
        globals: Vec<(String, String)>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct WireResult {
    id: ExecId,
    #[serde(flatten)]
    status: Status,
    #[serde(flatten)]
    report: Report,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Status {
    Ok,
    Error { error: String },
    Refused { holder: ExecId },
}

impl Interpreter {
    /// Start the interpreter in `container` and wait for it to greet.
    ///
    /// The container must have the payload mounted, as every session's
    /// primary does, and its user bootstrapped. Without the payload, podman's
    /// own error is what the startup error carries.
    pub(crate) async fn start(container: &Container) -> Result<Self, InterpreterError> {
        let python = format!("{PAYLOAD_MOUNT}/bin/python3");
        let argv: Vec<String> = [python.as_str()]
            .into_iter()
            .chain(ARGS)
            .chain([PRIMARY])
            .map(String::from)
            .collect();
        let child = container.exec_stdio(&argv, &ExecOptions::new()).await?;
        Self::from_child(child).await
    }

    /// Drive the interpreter `child` runs, over its three piped streams.
    pub(super) async fn from_child(mut child: Child) -> Result<Self, InterpreterError> {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(InterpreterError::Startup(
                "its stdio was not piped".to_string(),
            ));
        };
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&tail)));
        Self::connect(stdout, stdin, exit_cause(child, drain, tail)).await
    }

    /// Wait for the greeting on `replies`, then start the reader and writer
    /// tasks. `ended` is awaited once `replies` closes, or the greeting fails,
    /// and says why.
    pub(super) async fn connect<R, W, E>(
        replies: R,
        requests: W,
        ended: E,
    ) -> Result<Self, InterpreterError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
        E: Future<Output = String> + Send + 'static,
    {
        let mut replies = BufReader::new(replies);
        if let Err(problem) = greeting(&mut replies).await {
            // Closing its stdin is what lets a live interpreter exit, rather
            // than waiting out `ended`'s grace.
            drop(requests);
            let cause = ended.await;
            return Err(InterpreterError::Startup(format!("{problem}; {cause}")));
        }
        let table = Arc::new(Mutex::new(Table::default()));
        let (lines, queued) = mpsc::unbounded_channel();
        tokio::spawn(write_requests(requests, queued, Arc::clone(&table)));
        tokio::spawn(read_replies(replies, Arc::clone(&table), ended));
        Ok(Self { table, lines })
    }

    /// Submit `source` to run in the agent's namespace.
    ///
    /// While another execution is outstanding -- running, or recorded
    /// unresolved -- nothing is sent, and the outcome is already
    /// [`Outcome::Refused`] naming it. An unanswered execution may still be
    /// running, so its slot is not free.
    pub(crate) fn submit(&self, source: &str) -> Result<Execution, InterpreterError> {
        let (waiter, receiver) = oneshot::channel();
        let mut table = lock(&self.table);
        table.open()?;
        let id = table.next_id();
        match &table.slot {
            Some(slot) => {
                let _ = waiter.send(Outcome::Refused { holder: slot.id });
            }
            None => {
                // Queued under the lock, so the wire order is the order the
                // slot was claimed in.
                self.send(json!({"t": "exec", "agent": PRIMARY, "id": id, "src": source}))?;
                table.slot = Some(Slot { id, waiter });
            }
        }
        drop(table);
        Ok(Execution {
            id,
            receiver,
            settled: None,
            table: Arc::clone(&self.table),
        })
    }

    /// List what the agent's namespace holds. Answered on the agent's event
    /// loop, so it waits for as long as that loop is not turning.
    pub(crate) async fn inventory(&self) -> Result<Inventory, InterpreterError> {
        let (waiter, receiver) = oneshot::channel();
        {
            let mut table = lock(&self.table);
            table.open()?;
            let id = table.next_id();
            self.send(json!({"t": "inv", "agent": PRIMARY, "id": id}))?;
            table.inventories.insert(id, waiter);
        }
        receiver
            .await
            .map_err(|_| InterpreterError::Gone(lock(&self.table).cause()))
    }

    /// Results that arrived after their callers stopped waiting, oldest first.
    /// Each is reported once.
    pub(crate) fn take_late(&self) -> Vec<Late> {
        std::mem::take(&mut lock(&self.table).late)
    }

    fn send(&self, message: Value) -> Result<(), InterpreterError> {
        // The writer records why it stopped before it lets go of the queue, and
        // `open` has checked since; only a runtime shutting down gets here.
        self.lines
            .send(format!("{message}\n"))
            .map_err(|_| InterpreterError::Gone("its stdin closed".into()))
    }
}

impl Execution {
    pub(crate) fn id(&self) -> ExecId {
        self.id
    }

    /// Wait for the outcome. Cancel-safe, and may be awaited again after a
    /// timeout gave up on it -- or after it returned, for the same outcome.
    pub(crate) async fn outcome(&mut self) -> Outcome {
        if let Some(outcome) = &self.settled {
            return outcome.clone();
        }
        let outcome = match (&mut self.receiver).await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::Unknown(Unknown::Exited {
                id: self.id,
                cause: lock(&self.table).cause(),
            }),
        };
        self.settled = Some(outcome.clone());
        outcome
    }

    /// Stop waiting, recording the execution unresolved: it keeps its slot,
    /// and its reply, should one come, arrives as a [`Late`]. An outcome that
    /// had already arrived is returned instead.
    pub(crate) fn stop_waiting(mut self) -> Outcome {
        if let Some(outcome) = self.settled.take() {
            return outcome;
        }
        // Closed before the check, so a reply lands either here or in `late`
        // and never in a receiver about to be dropped.
        self.receiver.close();
        self.receiver
            .try_recv()
            .unwrap_or(Outcome::Unknown(Unknown::Unresolved { id: self.id }))
    }
}

impl Drop for Execution {
    /// A result that arrived but was never read is kept as a [`Late`], so a
    /// caller cancelled just as its reply landed does not lose it.
    fn drop(&mut self) {
        self.receiver.close();
        if let Ok(outcome @ (Outcome::Ok(_) | Outcome::Error { .. })) = self.receiver.try_recv() {
            lock(&self.table).late.push(Late {
                id: self.id,
                outcome,
            });
        }
    }
}

impl Table {
    fn next_id(&mut self) -> ExecId {
        self.last_id += 1;
        ExecId(self.last_id)
    }

    /// Whether anything more can be sent.
    fn open(&self) -> Result<(), InterpreterError> {
        match &self.ended {
            Some(cause) => Err(InterpreterError::Gone(Arc::clone(cause))),
            None => Ok(()),
        }
    }

    /// Why a waiter was let go unanswered. The reader records why before it
    /// lets any go, so the fallback is a reader that stopped without finishing:
    /// a panic, or a runtime shutting down.
    fn cause(&self) -> Arc<str> {
        self.ended
            .clone()
            .unwrap_or_else(|| "the host stopped reading its replies".into())
    }

    /// Settle the outstanding execution with its result. A result for any
    /// other id is not this execution's, whatever is running now.
    fn settle(&mut self, result: WireResult) {
        let Some(slot) = self.slot.take_if(|slot| slot.id == result.id) else {
            tracing::warn!(
                "ignored a result for execution {}, which is not outstanding",
                result.id
            );
            return;
        };
        let outcome = match result.status {
            Status::Ok => Outcome::Ok(result.report),
            Status::Error { error } => Outcome::Error {
                report: result.report,
                traceback: error,
            },
            Status::Refused { holder } => {
                // The host claims the slot before it sends, so the two disagree.
                tracing::warn!(
                    "the interpreter refused execution {}: it says {holder} holds the slot",
                    result.id
                );
                Outcome::Refused { holder }
            }
        };
        if let Err(outcome) = slot.waiter.send(outcome) {
            self.late.push(Late {
                id: slot.id,
                outcome,
            });
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read the interpreter's first line, which must be the primary's `ready`.
async fn greeting<R: AsyncBufRead + Unpin>(replies: &mut R) -> Result<(), String> {
    let mut line = Vec::new();
    let read =
        tokio::time::timeout(READY_TIMEOUT, next_line(replies, &mut line, REPLY_LINE_MAX)).await;
    match read {
        Err(_) => Err(format!("it did not report ready within {READY_TIMEOUT:?}")),
        Ok(Err(e)) => Err(format!("its greeting could not be read: {e}")),
        Ok(Ok(None)) => Err("its output closed before it reported ready".to_string()),
        Ok(Ok(Some(cut))) => match serde_json::from_slice(&line) {
            Ok(Envelope {
                agent,
                reply: Reply::Ready,
            }) if cut == 0 && agent == PRIMARY => Ok(()),
            _ => {
                line.truncate(200);
                Err(format!(
                    "it greeted with {:?} rather than the primary's ready",
                    String::from_utf8_lossy(&line)
                ))
            }
        },
    }
}

/// Write each queued line to the interpreter's stdin until every handle is
/// gone, then close it -- which is the interpreter's signal to exit. A write
/// that fails is recorded as why nothing more can be sent.
async fn write_requests<W: AsyncWrite + Unpin>(
    mut requests: W,
    mut queued: mpsc::UnboundedReceiver<String>,
    table: Arc<Mutex<Table>>,
) {
    while let Some(line) = queued.recv().await {
        let written = async {
            requests.write_all(line.as_bytes()).await?;
            requests.flush().await
        };
        if let Err(e) = written.await {
            tracing::warn!("writing to the Python interpreter failed: {e}");
            lock(&table)
                .ended
                .get_or_insert_with(|| format!("writing to its stdin failed: {e}").into());
            return;
        }
    }
    let _ = requests.shutdown().await;
}

/// Read and correlate replies until the interpreter's output closes, then
/// settle what is still outstanding as [`Unknown::Exited`].
async fn read_replies<R, E>(mut replies: R, table: Arc<Mutex<Table>>, ended: E)
where
    R: AsyncBufRead + Unpin,
    E: Future<Output = String>,
{
    let mut line = Vec::new();
    loop {
        match next_line(&mut replies, &mut line, REPLY_LINE_MAX).await {
            Ok(Some(0)) => dispatch(&table, &line),
            Ok(Some(cut)) => tracing::warn!(
                "ignored a line of more than {REPLY_LINE_MAX} bytes from the Python interpreter \
                 ({cut} bytes over)"
            ),
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("reading from the Python interpreter failed: {e}");
                break;
            }
        }
        // Emptied first: `shrink_to` keeps at least the length, and a large
        // reply's buffer would otherwise be held while the reader waits.
        line.clear();
        line.shrink_to(REPLY_BUFFER_KEPT);
    }
    // Nothing new is sent from here on; the full account follows once the
    // client has exited.
    lock(&table).ended = Some("its output closed".into());
    let cause: Arc<str> = ended.await.into();
    let mut table = lock(&table);
    table.ended = Some(Arc::clone(&cause));
    if let Some(slot) = table.slot.take() {
        // An exit is not a reply, so a caller no longer waiting gets no `Late`.
        let _ = slot
            .waiter
            .send(Outcome::Unknown(Unknown::Exited { id: slot.id, cause }));
    }
    // Dropping the waiters fails each inventory with the cause just recorded.
    table.inventories.clear();
}

fn dispatch(table: &Mutex<Table>, line: &[u8]) {
    let Envelope { agent, reply } = match serde_json::from_slice(line) {
        Ok(envelope) => envelope,
        Err(e) => {
            tracing::warn!("ignored an unreadable line from the Python interpreter: {e}");
            return;
        }
    };
    if agent != PRIMARY {
        tracing::warn!("ignored a message for agent {agent:?}, which this host did not open");
        return;
    }
    match reply {
        Reply::Result(result) => lock(table).settle(result),
        Reply::Inv { id, globals } => match lock(table).inventories.remove(&id) {
            Some(waiter) => {
                let _ = waiter.send(Inventory { globals });
            }
            None => tracing::warn!("ignored inventory {id}, which nothing asked for"),
        },
        Reply::Ready => tracing::warn!("ignored a second ready from the primary"),
        Reply::Other => tracing::debug!(
            "ignored a message of a kind this host does not know: {}",
            String::from_utf8_lossy(line)
        ),
    }
}

/// Log the interpreter's stderr line by line, keeping the last few to explain
/// an exit. Its own diagnostics are warnings. Everything else is output no
/// execution can be billed for -- `os.write(1, ...)`, `os.system` -- and is
/// logged at debug so an agent's stray output does not reach the terminal.
async fn drain_stderr<R: AsyncRead + Unpin>(stderr: R, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut stderr = BufReader::new(stderr);
    let mut line = Vec::new();
    while let Ok(Some(cut)) = next_line(&mut stderr, &mut line, STDERR_LINE_MAX).await {
        let mut text = String::from_utf8_lossy(&line).into_owned();
        if cut > 0 {
            let _ = write!(text, " [{cut} more bytes cut]");
        }
        if text.starts_with(DIAGNOSTIC) {
            tracing::warn!("{text}");
        } else {
            tracing::debug!("python: {text}");
        }
        let mut tail = lock(&tail);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(text);
    }
}

/// Describe how the exec ended, once its stdout has closed: the client's exit
/// status, which `podman exec` takes from the interpreter, and the last of its
/// stderr.
async fn exit_cause(
    mut child: Child,
    drain: JoinHandle<()>,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> String {
    // One deadline for both: the client's exit is what ends the drain.
    let deadline = tokio::time::Instant::now() + EXIT_GRACE;
    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => format!("its process exited ({status})"),
        Ok(Err(e)) => format!("its exit status could not be read: {e}"),
        Err(_) => {
            let _ = child.start_kill();
            format!("its output closed, but the exec client had not exited after {EXIT_GRACE:?}")
        }
    };
    let _ = tokio::time::timeout_at(deadline, drain).await;
    let mut tail = lock(&tail);
    if tail.is_empty() {
        status
    } else {
        format!("{status}; stderr:\n{}", tail.make_contiguous().join("\n"))
    }
}

/// Read one line into `line`, without its newline, keeping at most `max`
/// bytes and discarding the rest of a longer one. Returns how many bytes were
/// discarded, or `None` at end of stream. A final line without a newline
/// counts.
async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<usize>> {
    line.clear();
    let mut cut = 0;
    let mut read_any = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(read_any.then_some(cut));
        }
        read_any = true;
        let newline = buf.iter().position(|&b| b == b'\n');
        let chunk = &buf[..newline.unwrap_or(buf.len())];
        let keep = chunk.len().min(max - line.len());
        line.extend_from_slice(&chunk[..keep]);
        cut += chunk.len() - keep;
        let used = newline.map_or(buf.len(), |at| at + 1);
        reader.consume(used);
        if newline.is_some() {
            return Ok(Some(cut));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line past the bound is cut, and the stream stays in step: the next
    /// line starts where the long one's newline was.
    #[tokio::test]
    async fn a_long_line_is_cut_and_the_next_is_whole() {
        let mut reader = BufReader::with_capacity(3, &b"abcdefgh\nxy\n\nlast"[..]);
        let mut line = Vec::new();
        let mut lines = Vec::new();
        while let Some(cut) = next_line(&mut reader, &mut line, 4).await.unwrap() {
            lines.push((String::from_utf8(line.clone()).unwrap(), cut));
        }
        assert_eq!(
            lines,
            [
                ("abcd".to_string(), 4),
                ("xy".to_string(), 0),
                (String::new(), 0),
                ("last".to_string(), 0),
            ]
        );
    }
}
