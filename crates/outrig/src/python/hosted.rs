//! The relay between agent code's hosted calls and the process holding the session's bindings: it
//! carries `interpreter.py`'s RPyC frames to `binding.py` and back, decides at the session's close
//! which requests never reach the process, and records each request -- its receipt, its dispatch
//! and its outcome -- under a [`RequestId`] it gives out.
//!
//! Lines toward the process wait in one queue, bounded in bytes, which a writer task hands to the
//! supervisor's one-line stdin queue; a request is *forwarded* when the writer takes it. A labeled
//! frame -- a request, or a reply to a callback -- is admitted or refused whole at its first part
//! and queued whole once its last part arrives, so the close's control line never lands inside a
//! forwarded frame. A frame with no label is passed on part by part, for the process to judge. One
//! reader task handles every event line the process wrote before an `rpc` line ahead of it, so a
//! request's outcome is in the stream before its reply reaches the interpreter.
//!
//! What the interpreter sends is agent code's to forge, so the queue counts what each line holds
//! rather than what a label claims: a part's data as its text, and [`LINE`] for the rest of it. A
//! frame reserves what the parts its label declares hold, sent as both programs send them, and one
//! whose parts outrun that -- in bytes or in number -- closes its connection; so does a label
//! declaring a frame past [`FRAME_MAX`]. An agent holds at most [`CONNECTIONS_MAX`] connections to
//! a binding, and the process hears a connection's close once, and only of one it was sent a part
//! on.
//!
//! A request the relay admits is held under the number its label gives it, which the process is
//! sent with the frame's first part; the process ends the request itself when it never serves the
//! frame as that request -- one it reads as something else, or drops unread with its connection --
//! so no request waits on a frame that will never be served. The relay holds at most
//! [`CALLS_MAX`] requests of an agent that have not ended, and keeps [`NAME_MAX`] bytes of a name a
//! label gives.
//!
//! The relay is also the gate for a request the process's policy holds for an answer -- an
//! `escalate`, or an `evaluate` while there is no evaluator. The process asks with a `decision`
//! line and waits; the relay asks the session's escalation handler, and the first of the handler's
//! answer, the caller's interrupt, its connection's close, and the session's close settles the
//! request, under the same lock the close takes. A later answer is recorded as ignored. Every
//! answer goes to the process through the one queue, so an allow settled before the close reaches
//! it ahead of the control line; and the relay records the outcome of a request it ends itself, so
//! nothing the process does afterward -- or fails to do, killed -- changes it.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use super::host::PRIMARY;
use super::supervisor::Process;
use crate::config::PolicyAction;
use crate::events::Events;
use crate::harness::HostedCall;
use crate::harness::escalation::{
    Escalation, EscalationAnswer, EscalationResult, Handler, Withdrawal, Withdrawn,
};
use crate::harness::event::{
    DecidedBy, ExecId, HostedOutcome, HostedReason, Operation, Payload, PolicyLayer, PolicyVersion,
    ReceivedBy, RequestId,
};

/// What the relay sends the interpreter: one protocol line, queued for its writer.
pub(crate) type ToInterpreter = Arc<dyn Fn(Value) + Send + Sync>;

/// Bytes the queue toward the process holds, counted as [`State::queued`] counts them; one frame
/// always fits an empty queue.
const QUEUE_MAX: u64 = 64 << 20;
/// Bytes of data in one RPyC frame, as both programs bound it.
const FRAME_MAX: u64 = 128 << 20;
/// Bytes of frame data in one part, as both programs split a frame.
const PART_MAX: u64 = 512 << 10;
/// Bytes a frame's parts carry beyond the size its label declares: RPyC's header, a length and a
/// flag, before the data, and a newline after it.
const FRAMING: u64 = 6;
/// Bytes charged for each line held, beyond its data's text: the object that holds it.
const LINE: u64 = 1 << 10;
/// Connections one agent may hold to one binding, those a frame is arriving on included: the
/// interpreter's pool opens at most four.
const CONNECTIONS_MAX: usize = 64;
/// Requests one cancel line names: a connection's call, and those its callbacks made on it.
const CANCEL_MAX: usize = 16;
/// Bytes of the interpreter's reason for closing a connection that are passed on to the process.
const REASON_MAX: usize = 1 << 10;
/// Requests of one agent the relay holds that have not ended -- admitted, forwarded or received --
/// past which another is refused: an agent's kernel has a few in flight at once.
const CALLS_MAX: usize = 1024;
/// Bytes of a member or target name the relay keeps from what the interpreter says of a request.
const NAME_MAX: usize = 256;
const NO_ROOM: &str =
    "the bindings process is not reading, and its queue has no room for the frame";
const OUTRUN: &str = "a frame's parts outran the size its label declared";
const STOPPED: &str = "the process holding the bindings has stopped";
const INTERRUPTED: &str = "the call was cancelled before it ran: its caller was interrupted";
const DISCONNECTED: &str = "its connection closed before it ran";
/// How long the relay looks for the exit status of a process whose output closed unasked.
const EXIT_LOOK: Duration = Duration::from_secs(1);

/// An `rpc` line from the interpreter: a part of a frame, whose first part carries the frame's
/// label; a connection's close; or a cancel of requests in flight.
#[derive(Deserialize)]
pub(crate) struct Rpc {
    binding: String,
    id: u64,
    #[serde(default)]
    data: String,
    #[serde(default)]
    more: bool,
    closed: Option<String>,
    cancel: Option<Vec<u64>>,
    request: Option<u64>,
    reply: Option<u64>,
    op: Option<String>,
    member: Option<String>,
    target: Option<String>,
    exec: Option<u64>,
    size: Option<u64>,
}

/// A hosted call the interpreter never sent: woken while it waited for a connection, or refused
/// there once the close reached it.
#[derive(Deserialize)]
pub(crate) struct Notice {
    binding: String,
    exec: Option<u64>,
    operation: String,
    member: Option<String>,
    target: Option<String>,
    outcome: String,
    reason: String,
}

/// What decides a request the policy holds for an answer: the session's escalation handler, if it
/// has one, and the version of the rules in force, which every decision carries.
#[derive(Clone)]
pub(crate) struct Approvals {
    pub(crate) handler: Option<Handler>,
    pub(crate) version: PolicyVersion,
}

impl fmt::Debug for Approvals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Approvals")
            .field("handler", &self.handler.is_some())
            .field("version", &self.version)
            .finish()
    }
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            handler: None,
            version: PolicyVersion::new(""),
        }
    }
}

/// A decision line of the process: request `seq` on connection `conn`, which its policy holds
/// for the owner's answer to the decision the line's `id` names, and how the binding described it.
#[derive(Deserialize)]
struct Ask {
    agent: String,
    binding: String,
    conn: u64,
    seq: u64,
    action: PolicyAction,
    reason: String,
    operation: String,
    member: Option<String>,
    target: Option<String>,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    keywords: BTreeMap<String, String>,
}

/// A request held for an answer: the decision the process waits on, and what tells the handler
/// it stopped waiting.
struct Held {
    decision: u64,
    withdrawal: Withdrawal,
}

/// How the gate settles a held request.
enum Settle {
    Allow,
    /// Refused, recorded with this reason and detail.
    Deny(HostedReason, String),
    /// Never invoked, for this reason: the handler is told why, and the caller raises the text.
    Cancel(Withdrawn, String),
    /// Held once the session had closed, its closing error the detail.
    Refuse(String),
}

/// An event line of the process about request `seq` on connection `conn`.
#[derive(Deserialize)]
struct Record {
    kind: String,
    agent: String,
    binding: String,
    conn: u64,
    seq: u64,
    #[serde(default)]
    operation: String,
    member: Option<String>,
    target: Option<String>,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    keywords: BTreeMap<String, String>,
    parent: Option<u64>,
    ack: Option<u64>,
    #[serde(default)]
    outcome: String,
    reason: Option<String>,
    detail: Option<String>,
    exception: Option<String>,
    traceback: Option<String>,
    #[serde(default)]
    interrupted: bool,
    /// A decision's: the action, what chose it, and the rule's layer and position.
    action: Option<PolicyAction>,
    by: Option<String>,
    layer: Option<PolicyLayer>,
    rule: Option<usize>,
}

/// A connection: agent, binding, and the interpreter's number for it.
type Conn = (String, String, u64);
/// A request: its connection, and its seq there.
type Key = (String, String, u64, u64);

/// A boundary request the relay admitted, or the process received, that has not ended.
struct Call {
    id: RequestId,
    agent: String,
    binding: String,
    exec: Option<ExecId>,
    op: Operation,
    member: Option<String>,
    target: Option<String>,
    /// When the relay first saw it, which its duration counts from.
    since: Instant,
    /// Forwarded to the process, or received by it.
    sent: bool,
    /// Its receipt published by the process.
    received: bool,
    /// The request a callback made it inside, once the process received it.
    parent: Option<RequestId>,
    /// Why its caller stopped waiting -- an interrupt, or its connection's close -- once one
    /// did: a decision it is then held for is cancelled at once, asking nobody.
    withdrawn: Option<Withdrawn>,
    /// Held for an answer.
    held: Option<Held>,
}

/// Lines for the process: a whole frame, or one line of the relay's own.
#[derive(Default)]
struct Item {
    lines: Vec<Value>,
    /// The queue's bytes it holds until the writer takes it.
    cost: u64,
    /// The boundary request it carries.
    request: Option<Key>,
}

/// A frame from the interpreter whose last part has not arrived.
enum Frame {
    /// Admitted: its parts are gathered, with the bytes its label declared still to come.
    Gathering(Item, u64),
    /// Unlabeled: each part is passed on as it comes.
    Passing,
    /// Refused, or its connection closed: its remaining parts are dropped.
    Dropping,
}

#[derive(Default)]
struct State {
    /// The agents the host opened, whose lines are carried.
    agents: HashSet<String>,
    queue: VecDeque<Item>,
    /// Bytes held by lines queued, or reserved by frames still arriving.
    queued: u64,
    frames: HashMap<Conn, Frame>,
    /// The connections the process has been queued a part on and not told closed.
    open: HashSet<Conn>,
    calls: HashMap<Key, Call>,
    /// The session's closing error, once it closed.
    closing: Option<String>,
    /// Set once the session stops the process, whose exit is then expected.
    stopping: bool,
    /// How the process ended, when it ended unasked.
    exited: Option<String>,
    /// Once the process's output has closed and its every line is handled, or the session gave
    /// up waiting for that: what a later request is refused with.
    gone: Option<String>,
    /// Set once the session has reported: nothing is published after.
    sealed: bool,
    hung_up: bool,
    forwarded: u64,
    /// The last request id given out.
    last_request: u64,
    after_close: Vec<HostedCall>,
    /// The escalation handlers asked and not yet done, aborted once the session has reported.
    asking: Vec<AbortHandle>,
}

impl State {
    /// Queue one line for the process, charged `cost`, unless it is gone.
    fn push(&mut self, line: Value, cost: u64) {
        if self.gone.is_none() {
            self.queued += cost;
            self.queue.push_back(Item {
                lines: vec![line],
                cost,
                request: None,
            });
        }
    }

    /// Whether `cost` more bytes fit the queue: anything fits an empty one.
    fn fits(&self, cost: u64) -> bool {
        self.queued == 0 || self.queued + cost <= QUEUE_MAX
    }

    /// How many connections `conn`'s agent holds to its binding: those the process has a part
    /// on, and those a frame is arriving on.
    fn connections(&self, (agent, binding, _): &Conn) -> usize {
        let mine = |conn: &&Conn| &conn.0 == agent && &conn.1 == binding;
        let arriving = self.frames.keys().filter(|conn| !self.open.contains(*conn));
        self.open.iter().filter(mine).count() + arriving.filter(mine).count()
    }

    /// Why another request of `agent`'s is refused, once it holds [`CALLS_MAX`] that have not
    /// ended.
    fn full(&self, agent: &str) -> Option<String> {
        let held = self.calls.keys().filter(|key| key.0 == agent).count();
        (held >= CALLS_MAX)
            .then(|| format!("agent {agent:?} already has {CALLS_MAX} requests in flight"))
    }

    fn next_id(&mut self) -> RequestId {
        self.last_request += 1;
        RequestId::new(self.last_request)
    }

    /// Let go of a frame never forwarded, and of the request it carried.
    fn discard(&mut self, item: Item) {
        self.queued -= item.cost;
        if let Some(key) = item.request {
            self.calls.remove(&key);
        }
    }

    /// The next item for the writer, forwarding the request it carries.
    fn take(&mut self) -> Option<Item> {
        if self.hung_up || self.gone.is_some() {
            return None;
        }
        let item = self.queue.pop_front()?;
        self.queued -= item.cost;
        if let Some(call) = item
            .request
            .as_ref()
            .and_then(|key| self.calls.get_mut(key))
        {
            call.sent = true;
            self.forwarded += 1;
        }
        Some(item)
    }
}

/// The session's relay to its bindings process. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Relay(Arc<Shared>);

struct Shared {
    state: watch::Sender<State>,
    to_interpreter: ToInterpreter,
    events: Events,
    bindings: Vec<String>,
    approvals: Approvals,
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay")
            .field("bindings", &self.0.bindings)
            .finish_non_exhaustive()
    }
}

impl Relay {
    pub(crate) fn new(
        to_interpreter: ToInterpreter,
        events: Events,
        bindings: Vec<String>,
        approvals: Approvals,
    ) -> Self {
        let mut state = State::default();
        state.agents.insert(PRIMARY.to_string());
        Self(Arc::new(Shared {
            state: watch::Sender::new(state),
            to_interpreter,
            events,
            bindings,
            approvals,
        }))
    }

    /// Start writing to `process` and reading from it, on the current runtime.
    pub(crate) fn attach(&self, process: &mut Process) {
        tokio::spawn(write(self.clone(), process.sender()));
        tokio::spawn(read(
            self.clone(),
            process.take_rpc(),
            process.take_events(),
            process.pid(),
        ));
    }

    /// Carry `agent`'s lines too.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the host opens no agent but the primary until `0003-30`"
        )
    )]
    pub(crate) fn opened(&self, agent: &str) {
        self.with(|s| s.agents.insert(agent.to_string()));
    }

    /// One of `agent`'s `rpc` lines, for the process.
    pub(crate) fn rpc(&self, agent: &str, mut rpc: Rpc) {
        // A cancel's numbers, looked up once each under the lock however many the line names.
        let cancelled: HashSet<u64> = rpc.cancel.iter().flatten().copied().collect();
        self.with(|s| {
            if s.sealed {
                return;
            }
            if !s.agents.contains(agent) {
                tracing::warn!(
                    "ignored an rpc line from agent {agent:?}, which the host did not open"
                );
                return;
            }
            let conn = (agent.to_string(), rpc.binding.clone(), rpc.id);
            if !self.0.bindings.contains(&rpc.binding) {
                let reason = format!("this session holds no binding {:?}", rpc.binding);
                (self.0.to_interpreter)(json!({
                    "t": "rpc", "agent": agent, "binding": rpc.binding, "id": rpc.id,
                    "closed": reason,
                }));
                return;
            }
            let mut line =
                json!({"t": "rpc", "binding": rpc.binding, "agent": agent, "id": rpc.id});
            if let Some(mut reason) = rpc.closed.take() {
                if let Some(Frame::Gathering(item, _)) = s.frames.remove(&conn) {
                    s.discard(item);
                }
                self.disconnect(s, &conn);
                if s.open.remove(&conn) {
                    reason.truncate(floor_char_boundary(&reason, REASON_MAX));
                    let cost = LINE + reason.len() as u64;
                    line["closed"] = reason.into();
                    s.push(line, cost);
                }
                return;
            }
            if let Some(mut seqs) = rpc.cancel.take() {
                // At the gate whatever the queue's room: a request held for an answer is cancelled
                // now, and one the process has yet to hold will be the moment it asks.
                let interrupted = |key: &Key| {
                    key.0 == conn.0
                        && key.1 == conn.1
                        && key.2 == conn.2
                        && cancelled.contains(&key.3)
                };
                self.withdraw_where(s, interrupted, Withdrawn::Interrupted);
                // To the process, a hint: one with no room is dropped, and its requests run as if
                // their callers had waited.
                if s.open.contains(&conn) && s.fits(LINE) {
                    seqs.truncate(CANCEL_MAX);
                    line["cancel"] = seqs.into();
                    s.push(line, LINE);
                }
                return;
            }
            let frame = match s.frames.remove(&conn) {
                Some(frame) => frame,
                None if !s.open.contains(&conn) && s.connections(&conn) >= CONNECTIONS_MAX => {
                    // Kept nowhere, so each of its parts is refused alike.
                    let reason = format!(
                        "agent {agent:?} already holds {CONNECTIONS_MAX} connections to binding \
                         {:?}",
                        rpc.binding
                    );
                    self.cut(s, &conn, &reason);
                    return;
                }
                None => self.first(s, &conn, &rpc),
            };
            let cost = rpc.data.len() as u64 + LINE;
            line["data"] = std::mem::take(&mut rpc.data).into();
            line["more"] = rpc.more.into();
            let frame = match frame {
                Frame::Gathering(mut item, left) if cost <= left => {
                    if item.lines.is_empty() && item.request.is_some() {
                        // The number the process ends the request under if it never serves the
                        // frame as that request.
                        line["request"] = rpc.request.into();
                    }
                    item.lines.push(line);
                    Frame::Gathering(item, left - cost)
                }
                Frame::Gathering(item, _) => {
                    s.discard(item);
                    self.cut(s, &conn, OUTRUN);
                    Frame::Dropping
                }
                Frame::Passing if s.fits(cost) => {
                    s.push(line, cost);
                    s.open.insert(conn.clone());
                    Frame::Passing
                }
                Frame::Passing => {
                    self.cut(s, &conn, NO_ROOM);
                    Frame::Dropping
                }
                Frame::Dropping => Frame::Dropping,
            };
            match frame {
                frame if rpc.more => {
                    s.frames.insert(conn, frame);
                }
                Frame::Gathering(item, _) => {
                    s.open.insert(conn);
                    s.queue.push_back(item);
                }
                _ => {}
            }
        });
    }

    /// A hosted call `agent`'s interpreter never sent.
    pub(crate) fn noticed(&self, agent: &str, notice: Notice) {
        self.with(|s| {
            if s.sealed || !s.agents.contains(agent) {
                return;
            }
            let call = Call {
                id: s.next_id(),
                agent: agent.to_string(),
                binding: notice.binding,
                exec: notice.exec.map(ExecId::new),
                op: Operation::named(&notice.operation),
                member: bounded(notice.member.as_deref()),
                target: bounded(notice.target.as_deref()),
                since: Instant::now(),
                sent: false,
                received: false,
                parent: None,
                withdrawn: None,
                held: None,
            };
            self.receipt(&call, ReceivedBy::Interpreter);
            let outcome = outcome(&notice.outcome);
            self.end(s, &call, outcome, reason(&notice.reason), None);
        });
    }

    /// The session closed to new work, with `closing` as its error. Returns at once: every
    /// boundary request not yet forwarded is refused, and so is every later one; the process is
    /// sent the control line behind what was forwarded, and the interpreter is told.
    pub(crate) fn close(&self, closing: &str) {
        let first = self.with(|s| {
            if s.closing.is_some() {
                return false;
            }
            s.closing = Some(closing.to_string());
            self.cancel_where(s, |_| true, Withdrawn::Closed, closing);
            let (mut refused, kept): (VecDeque<Item>, _) = std::mem::take(&mut s.queue)
                .into_iter()
                .partition(|item| item.request.is_some());
            s.queue = kept;
            for frame in s.frames.values_mut() {
                if let Frame::Gathering(item, _) = frame
                    && item.request.is_some()
                {
                    refused.push_back(std::mem::take(item));
                    *frame = Frame::Dropping;
                }
            }
            for item in refused {
                s.queued -= item.cost;
                if let Some((key, call)) = item.request.and_then(|key| s.calls.remove_entry(&key)) {
                    self.refuse(s, &key, &call, HostedReason::AdmissionClosed, closing);
                }
            }
            s.push(json!({"t": "control", "closed": closing}), LINE);
            true
        });
        if first {
            (self.0.to_interpreter)(json!({"t": "close", "reason": closing}));
        }
    }

    /// The session is stopping the process: its exit is expected.
    pub(crate) fn stopping(&self) {
        self.with(|s| s.stopping = true);
    }

    /// Resolves once no request the process was sent is still to end.
    pub(crate) fn drained(&self) -> impl Future<Output = ()> + Send + 'static {
        self.until(|s| s.calls.is_empty())
    }

    /// Resolves once the process's output has closed and every line it wrote is handled.
    pub(crate) fn finished(&self) -> impl Future<Output = ()> + Send + 'static {
        self.until(|s| s.gone.is_some())
    }

    /// The session gave up waiting for the stopped process's output to close -- something the
    /// process started may hold it open -- so the process is taken as gone now: each request it
    /// was sent and never answered is `unknown`, `killed`, and what it writes later is ignored.
    /// Nothing, once it is gone.
    pub(crate) fn abandon(&self) {
        self.with(|s| self.gone(s, None));
    }

    /// Each request that ended after the close, and how, for the session's report, read once the
    /// process is gone. Nothing is published from now on, so no outcome the report leaves out
    /// follows it.
    pub(crate) fn report(&self) -> Vec<HostedCall> {
        self.with(|s| {
            s.sealed = true;
            for handler in s.asking.drain(..) {
                handler.abort();
            }
            s.after_close.clone()
        })
    }

    /// How the process ended, when it ended while the session ran.
    pub(crate) fn exited(&self) -> Option<String> {
        self.0.state.borrow().exited.clone()
    }

    /// How many boundary request frames the writer has forwarded.
    #[cfg(test)]
    pub(crate) fn forwarded(&self) -> u64 {
        self.0.state.borrow().forwarded
    }

    /// The lines queued for the process, and the bytes the queue holds and reserves.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> (Vec<Value>, u64) {
        let s = self.0.state.borrow();
        let lines = s.queue.iter().flat_map(|item| item.lines.clone());
        (lines.collect(), s.queued)
    }

    /// Handle `line` as an event line the process wrote, for a test.
    #[cfg(test)]
    pub(crate) fn wrote(&self, line: Value) {
        self.event(line);
    }

    /// Stop writing to the process, so its stdin can close.
    #[cfg(test)]
    pub(crate) fn hang_up(&self) {
        self.with(|s| s.hung_up = true);
    }

    /// Change the state, waking whoever waits on it.
    fn with<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let mut out = None;
        self.0.state.send_modify(|s| out = Some(change(s)));
        out.expect("send_modify runs the change")
    }

    fn until(&self, done: fn(&State) -> bool) -> impl Future<Output = ()> + Send + 'static {
        let mut state = self.0.state.subscribe();
        async move {
            let _ = state.wait_for(done).await;
        }
    }

    /// Admit the frame whose first part `rpc` is, or refuse it whole. A request is refused once
    /// the process is gone, a boundary one once the session closed or while its agent holds
    /// [`CALLS_MAX`], and one the queue has no room for; a reply with no room, a frame declared
    /// past the bound, and a request numbered as one in flight on its connection close the
    /// connection.
    fn first(&self, s: &mut State, conn: &Conn, rpc: &Rpc) -> Frame {
        if rpc.request.is_none() && rpc.reply.is_none() {
            return match s.gone {
                Some(_) => Frame::Dropping,
                None => Frame::Passing,
            };
        }
        let size = rpc.size.unwrap_or(0);
        if size > FRAME_MAX {
            let reason = format!("a frame of {size} bytes is past the {FRAME_MAX}-byte bound");
            self.cut(s, conn, &reason);
            return Frame::Dropping;
        }
        let cost = held(size);
        let fits = s.fits(cost);
        let mut request = None;
        if let Some(seq) = rpc.request {
            let key = (conn.0.clone(), conn.1.clone(), conn.2, seq);
            if s.calls.contains_key(&key) {
                // Its record would take the place of the one in flight under that number.
                let reason = format!("request {seq} while request {seq} is in flight on it");
                self.cut(s, conn, &reason);
                return Frame::Dropping;
            }
            let op = Operation::named(rpc.op.as_deref().unwrap_or_default());
            let refusal = if let Some(text) = &s.gone {
                Some((HostedReason::Exited, text.clone()))
            } else if let Some(text) = s.closing.as_ref().filter(|_| !op.bookkeeping()) {
                Some((HostedReason::AdmissionClosed, text.clone()))
            } else if let Some(text) = s.full(&conn.0).filter(|_| !op.bookkeeping()) {
                Some((HostedReason::QueueFull, text))
            } else if !fits {
                Some((HostedReason::QueueFull, NO_ROOM.to_string()))
            } else {
                None
            };
            if refusal.is_some() || !op.bookkeeping() {
                let call = Call {
                    id: s.next_id(),
                    agent: conn.0.clone(),
                    binding: conn.1.clone(),
                    exec: rpc.exec.map(ExecId::new),
                    op,
                    member: bounded(rpc.member.as_deref()),
                    target: bounded(rpc.target.as_deref()),
                    since: Instant::now(),
                    sent: false,
                    received: false,
                    parent: None,
                    withdrawn: None,
                    held: None,
                };
                if let Some((reason, error)) = refusal {
                    self.refuse(s, &key, &call, reason, &error);
                    return Frame::Dropping;
                }
                s.calls.insert(key.clone(), call);
                request = Some(key);
            }
        } else if s.gone.is_some() {
            return Frame::Dropping;
        } else if !fits {
            self.cut(s, conn, NO_ROOM);
            return Frame::Dropping;
        }
        s.queued += cost;
        let item = Item {
            lines: Vec::new(),
            cost,
            request,
        };
        Frame::Gathering(item, cost)
    }

    /// Close `conn` on both sides, for `reason`: the process hears of it only if it was sent a
    /// part of the connection's.
    fn cut(&self, s: &mut State, conn: &Conn, reason: &str) {
        let (agent, binding, id) = conn;
        (self.0.to_interpreter)(
            json!({"t": "rpc", "agent": agent, "binding": binding, "id": id, "closed": reason}),
        );
        self.disconnect(s, conn);
        if s.open.remove(conn) {
            let line =
                json!({"t": "rpc", "binding": binding, "agent": agent, "id": id, "closed": reason});
            s.push(line, LINE);
        }
    }

    /// Refuse `call`, which the process never got: recorded from what the interpreter said of
    /// it, and answered in the interpreter, whose caller raises with `error`.
    fn refuse(&self, s: &mut State, key: &Key, call: &Call, reason: HostedReason, error: &str) {
        self.receipt(call, ReceivedBy::Relay);
        self.end(
            s,
            call,
            HostedOutcome::Refused,
            Some(reason),
            Some(error.to_string()),
        );
        let (agent, binding, id, seq) = key;
        (self.0.to_interpreter)(json!({
            "t": "rpc", "agent": agent, "binding": binding, "id": id,
            "refused": seq, "reason": reason, "error": error,
        }));
    }

    /// `call`'s receipt by the relay or the interpreter, from what the interpreter said of it.
    fn receipt(&self, call: &Call, by: ReceivedBy) {
        self.0.events.emit(Payload::HostedReceived {
            request: call.id,
            agent: call.agent.clone(),
            binding: call.binding.clone(),
            execid: call.exec,
            operation: call.op,
            member: call.member.clone(),
            target: call.target.clone(),
            arguments: Vec::new(),
            keywords: BTreeMap::new(),
            parent: None,
            by,
        });
    }

    /// `call`'s outcome, when its target never answered.
    fn end(
        &self,
        s: &mut State,
        call: &Call,
        outcome: HostedOutcome,
        reason: Option<HostedReason>,
        detail: Option<String>,
    ) {
        self.ended(
            s,
            Payload::HostedEnded {
                request: call.id,
                agent: call.agent.clone(),
                binding: call.binding.clone(),
                outcome,
                reason,
                detail,
                exception: None,
                traceback: None,
                interrupted: false,
                duration: call.since.elapsed(),
            },
        );
    }

    /// Publish an outcome, noted for the report when it came after the close.
    fn ended(&self, s: &mut State, end: Payload) {
        if s.closing.is_some()
            && let Payload::HostedEnded {
                request,
                agent,
                binding,
                outcome,
                reason,
                ..
            } = &end
        {
            s.after_close.push(HostedCall {
                request: *request,
                agent: agent.clone(),
                binding: binding.clone(),
                outcome: *outcome,
                reason: *reason,
            });
        }
        self.0.events.emit(end);
    }

    /// An event line of the process: a request's receipt, dispatch, outcome or callback,
    /// recorded under the request's id. Any other kind is a library's own.
    fn event(&self, line: Value) {
        if line["t"] == "decision" {
            self.decision(line);
            return;
        }
        let kinds = ["received", "decided", "dispatched", "ended", "callback"];
        if !kinds.contains(&line["kind"].as_str().unwrap_or_default()) {
            tracing::debug!("ignored an event line of a kind the relay does not know: {line}");
            return;
        }
        let ack = line.get("ack").and_then(Value::as_u64);
        let r: Record = match serde_json::from_value(line) {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!("ignored an event line from the bindings process: {e}");
                // A dispatch it could not record is answered all the same, so its request
                // neither waits for ever nor runs unrecorded.
                if let Some(ack) = ack {
                    let unpublished = json!({"t": "published", "id": ack, "answer": false});
                    self.with(|s| s.push(unpublished, LINE));
                }
                return;
            }
        };
        self.with(|s| {
            // Once the process is taken as gone, nothing it writes counts.
            if s.gone.is_some() || s.sealed {
                return;
            }
            let key = (r.agent.clone(), r.binding.clone(), r.conn, r.seq);
            match r.kind.as_str() {
                "received" => {
                    let parent = r
                        .parent
                        .and_then(|seq| s.calls.get(&(key.0.clone(), key.1.clone(), key.2, seq)))
                        .map(|call| call.id);
                    let op = Operation::named(&r.operation);
                    // One the relay holds nothing of came with no label of its own, and the
                    // process refuses it at once.
                    if !s.calls.contains_key(&key) {
                        let call = Call {
                            id: s.next_id(),
                            agent: r.agent.clone(),
                            binding: r.binding.clone(),
                            exec: None,
                            op,
                            member: r.member.clone(),
                            target: r.target.clone(),
                            since: Instant::now(),
                            sent: true,
                            received: false,
                            parent: None,
                            withdrawn: None,
                            held: None,
                        };
                        s.calls.insert(key.clone(), call);
                    }
                    let call = s.calls.get_mut(&key).expect("inserted above");
                    call.sent = true;
                    call.received = true;
                    call.parent = parent;
                    // Its duration counts from here, as `hosted.ended`'s does.
                    call.since = Instant::now();
                    let received = Payload::HostedReceived {
                        request: call.id,
                        agent: r.agent,
                        binding: r.binding,
                        execid: call.exec,
                        operation: op,
                        member: r.member,
                        target: r.target,
                        arguments: r.arguments,
                        keywords: r.keywords,
                        parent,
                        by: ReceivedBy::Binding,
                    };
                    self.0.events.emit(received);
                }
                "decided" => {
                    let by = match (r.by.as_deref(), r.layer) {
                        (Some("rule"), Some(layer)) => {
                            r.rule.map(|rule| DecidedBy::Rule { layer, rule })
                        }
                        (Some("default"), Some(layer)) => Some(DecidedBy::Default { layer }),
                        _ => None,
                    };
                    if let (Some(call), Some(by), Some(action)) = (s.calls.get(&key), by, r.action)
                    {
                        self.decided(call.id, &key, action, by, r.reason, false);
                    }
                }
                "dispatched" => {
                    let request = s.calls.get(&key).map(|call| call.id);
                    if let Some(request) = request {
                        self.0.events.emit(Payload::HostedDispatched {
                            request,
                            agent: r.agent,
                            binding: r.binding,
                        });
                    }
                    // The process holds the request until the dispatch is in the stream, and
                    // does not run one whose dispatch is not.
                    let answer =
                        json!({"t": "published", "id": r.ack, "answer": request.is_some()});
                    s.push(answer, LINE);
                }
                "ended" => {
                    if let Some(call) = s.calls.remove(&key) {
                        if !call.received {
                            // The process never served its frame as this request: it read
                            // something else there, or dropped the frame unread.
                            self.receipt(&call, ReceivedBy::Relay);
                        }
                        let end = Payload::HostedEnded {
                            request: call.id,
                            agent: r.agent,
                            binding: r.binding,
                            outcome: outcome(&r.outcome),
                            reason: r.reason.as_deref().and_then(reason),
                            detail: r.detail,
                            exception: r.exception,
                            traceback: r.traceback,
                            interrupted: r.interrupted,
                            duration: call.since.elapsed(),
                        };
                        self.ended(s, end);
                    }
                }
                _ => {
                    if let Some(call) = s.calls.get(&key) {
                        let callback = Payload::HostedCallback {
                            parent: call.id,
                            agent: r.agent,
                            binding: r.binding,
                        };
                        self.0.events.emit(callback);
                    }
                }
            }
        });
    }

    /// Publish a decision about `request`, held under `key`, under the rules in force; `ignored`
    /// for an answer that came after the request had stopped waiting for it.
    fn decided(
        &self,
        request: RequestId,
        key: &Key,
        action: PolicyAction,
        by: DecidedBy,
        reason: Option<String>,
        ignored: bool,
    ) {
        self.0.events.emit(Payload::HostedDecided {
            request,
            agent: key.0.clone(),
            binding: key.1.clone(),
            action,
            by,
            reason,
            version: self.0.approvals.version.clone(),
            ignored,
        });
    }

    /// A decision line of the process: a request its policy holds for an answer. Settled at once
    /// when it cannot wait -- the session has closed, its caller was interrupted, or nobody can
    /// answer -- and otherwise held while the session's handler is asked. One the relay holds
    /// nothing of, which only a library's own use of the primitive sends, is denied unrecorded.
    fn decision(&self, line: Value) {
        let Some(id) = line["id"].as_u64() else {
            tracing::warn!("ignored a decision line from the bindings process without an id");
            return;
        };
        let ask = serde_json::from_value::<Ask>(line).ok();
        self.with(|s| {
            if s.gone.is_some() || s.sealed {
                return;
            }
            let found = ask.and_then(|ask| {
                let key = (ask.agent.clone(), ask.binding.clone(), ask.conn, ask.seq);
                s.calls.get(&key).map(|call| (ask, key, call))
            });
            let Some((ask, key, call)) = found else {
                let detail = "the owner holds no request this decision is for";
                let answer = json!({"action": "deny", "reason": "interception", "detail": detail});
                return s.push(json!({"t": "decision", "id": id, "answer": answer}), LINE);
            };
            let (request, execid, parent) = (call.id, call.exec, call.parent);
            if let Some(closing) = s.closing.clone() {
                return self.settle(s, &key, id, Settle::Refuse(closing));
            }
            if let Some(why) = call.withdrawn {
                let cancel = Settle::Cancel(why, withdrawn_text(why).into());
                return self.settle(s, &key, id, cancel);
            }
            let handler = self.0.approvals.handler.clone();
            let mut reason = ask.reason;
            if ask.action == PolicyAction::Evaluate {
                let next = match handler {
                    Some(_) => PolicyAction::Escalate,
                    None => PolicyAction::Deny,
                };
                let why = Some("no evaluator is configured".to_string());
                self.decided(request, &key, next, DecidedBy::NoEvaluator, why, false);
                reason = format!("{reason}, and no evaluator is configured");
            }
            let Some(handler) = handler else {
                let (deny, why) = (PolicyAction::Deny, "no approver is installed");
                self.decided(
                    request,
                    &key,
                    deny,
                    DecidedBy::NoApprover,
                    Some(why.into()),
                    false,
                );
                let detail = format!("{reason}; it was refused, since {why}");
                return self.settle(s, &key, id, Settle::Deny(HostedReason::NoApprover, detail));
            };
            let withdrawal = Withdrawal::default();
            let escalation = Escalation {
                request,
                parent,
                agent: key.0.clone(),
                execid,
                binding: key.1.clone(),
                operation: Operation::named(&ask.operation),
                member: bounded(ask.member.as_deref()),
                target: bounded(ask.target.as_deref()),
                arguments: ask.arguments,
                keywords: ask.keywords,
                reason,
                policy: self.0.approvals.version.clone(),
                withdrawal: withdrawal.clone(),
            };
            // Spawned, not run: the handler's code runs on the runtime, outside this lock, which
            // a handler that closes the session takes.
            let asked = handler.escalate(escalation);
            let asked = tokio::spawn(self.answer(asked, key.clone(), id, request));
            s.asking.retain(|task| !task.is_finished());
            s.asking.push(asked.abort_handle());
            let held = Held {
                decision: id,
                withdrawal,
            };
            if let Some(call) = s.calls.get_mut(&key) {
                call.held = Some(held);
            }
        });
    }

    /// Wait for `asked`, the handler's answer about the request held under `key` for `decision`,
    /// and settle the request with it: an `Err` or a panic denies.
    fn answer(
        &self,
        asked: impl Future<Output = EscalationResult> + Send + 'static,
        key: Key,
        decision: u64,
        request: RequestId,
    ) -> impl Future<Output = ()> + Send + 'static {
        // Weak, so a handler that never answers holds nothing of the session's.
        let relay = Arc::downgrade(&self.0);
        async move {
            let answer = match AssertUnwindSafe(asked).catch_unwind().await {
                Ok(Ok(answer)) => Ok(answer),
                Ok(Err(e)) => Err(format!("the escalation handler failed: {e}")),
                Err(_) => Err("the escalation handler panicked".to_string()),
            };
            if let Some(shared) = relay.upgrade() {
                Relay(shared).answered(&key, decision, request, answer);
            }
        }
    }

    /// The handler's answer for `request`, held under `key` for `decision`: it settles the
    /// request if the request still waits for it, and is recorded as ignored if not. A failure
    /// once the request stopped waiting -- a handler that gave up when told -- is no answer, and
    /// is not recorded.
    fn answered(
        &self,
        key: &Key,
        decision: u64,
        request: RequestId,
        answer: Result<EscalationAnswer, String>,
    ) {
        self.with(|s| {
            if s.sealed {
                return;
            }
            let waiting = s
                .calls
                .get(key)
                .and_then(|call| call.held.as_ref())
                .is_some_and(|held| held.decision == decision);
            let (action, reason) = match answer {
                Ok(EscalationAnswer::Allow) => (PolicyAction::Allow, None),
                Ok(EscalationAnswer::Deny(reason)) => (PolicyAction::Deny, Some(reason)),
                Err(_) if !waiting => return,
                Err(reason) => (PolicyAction::Deny, Some(reason)),
            };
            let by = DecidedBy::Approver;
            self.decided(request, key, action, by, reason.clone(), !waiting);
            if waiting {
                let settle = match reason {
                    None => Settle::Allow,
                    Some(reason) => {
                        let detail = format!("the approver denied it: {reason}");
                        Settle::Deny(HostedReason::Approver, detail)
                    }
                };
                self.settle(s, key, decision, settle);
            }
        });
    }

    /// Settle the request held -- or about to be -- under `key`, answering `decision`. Unless it
    /// is allowed, its outcome is recorded here, since the owner decided it was never invoked,
    /// and what the process writes of it after is ignored.
    fn settle(&self, s: &mut State, key: &Key, decision: u64, settle: Settle) {
        let held = s.calls.get_mut(key).and_then(|call| call.held.take());
        let (answer, ended) = match settle {
            Settle::Allow => (json!({"action": "allow"}), None),
            Settle::Deny(reason, detail) => (
                json!({"action": "deny", "reason": reason, "detail": &detail}),
                Some((HostedOutcome::Refused, reason, Some(detail))),
            ),
            Settle::Cancel(why, detail) => {
                if let Some(held) = &held {
                    held.withdrawal.withdraw(why);
                }
                let reason = cancelled_for(why);
                (
                    json!({"action": "cancel", "reason": reason, "detail": detail}),
                    Some((HostedOutcome::Cancelled, reason, None)),
                )
            }
            Settle::Refuse(closing) => {
                let reason = HostedReason::AdmissionClosed;
                (
                    json!({"action": "refuse", "reason": reason, "detail": &closing}),
                    Some((HostedOutcome::Refused, reason, Some(closing))),
                )
            }
        };
        s.push(
            json!({"t": "decision", "id": decision, "answer": answer}),
            LINE,
        );
        if let Some((outcome, reason, detail)) = ended
            && let Some(call) = s.calls.remove(key)
        {
            self.end(s, &call, outcome, Some(reason), detail);
        }
    }

    /// Cancel every request held for an answer that `which` picks, for `why`, its caller
    /// raising `detail`.
    fn cancel_where(
        &self,
        s: &mut State,
        which: impl Fn(&Key) -> bool,
        why: Withdrawn,
        detail: &str,
    ) {
        let held: Vec<_> = s
            .calls
            .iter()
            .filter(|(key, _)| which(key))
            .filter_map(|(key, call)| Some((key.clone(), call.held.as_ref()?.decision)))
            .collect();
        for (key, decision) in held {
            self.settle(s, &key, decision, Settle::Cancel(why, detail.to_string()));
        }
    }

    /// Cancel every request held for an answer on `conn`, which has closed.
    fn disconnect(&self, s: &mut State, conn: &Conn) {
        let on = |key: &Key| key.0 == conn.0 && key.1 == conn.1 && key.2 == conn.2;
        self.withdraw_where(s, on, Withdrawn::Disconnected);
    }

    /// Note on every request `which` picks that its caller stopped waiting, for `why`, so a
    /// decision it is later held for is cancelled at once; and cancel each already held. The
    /// first reason given stands.
    fn withdraw_where(&self, s: &mut State, which: impl Fn(&Key) -> bool, why: Withdrawn) {
        let picked = s.calls.iter_mut().filter(|(key, _)| which(key));
        for (_, call) in picked {
            call.withdrawn.get_or_insert(why);
        }
        self.cancel_where(s, which, why, withdrawn_text(why));
    }

    /// One of the process's `rpc` lines, for the interpreter, unless the process is taken as gone.
    /// A close it says of a connection is one the relay need not send it.
    fn forward(&self, line: Value) {
        self.with(|s| {
            if s.gone.is_some() {
                return;
            }
            if line.get("closed").is_some()
                && let (Some(agent), Some(binding), Some(id)) = (
                    line["agent"].as_str(),
                    line["binding"].as_str(),
                    line["id"].as_u64(),
                )
            {
                let conn = (agent.to_string(), binding.to_string(), id);
                self.disconnect(s, &conn);
                s.open.remove(&conn);
            }
            (self.0.to_interpreter)(line);
        });
    }

    /// The process's output has closed and its every line is handled, or the session gave up
    /// waiting for that. Each request it was sent and never answered is `unknown` -- `killed` when
    /// the session stopped it, `exited` when it ended unasked -- and one it was never sent is
    /// refused. An exit unasked is published, and the interpreter told. Nothing, once it is gone.
    fn gone(&self, s: &mut State, cause: Option<String>) {
        if s.gone.is_some() {
            return;
        }
        let text = match &cause {
            Some(cause) => format!(
                "the process holding the bindings {} has ended: {cause}",
                self.0.bindings.join(", ")
            ),
            None => STOPPED.to_string(),
        };
        let lost = match cause {
            Some(_) => HostedReason::Exited,
            None => HostedReason::Killed,
        };
        for (key, call) in std::mem::take(&mut s.calls) {
            if !call.sent {
                self.refuse(s, &key, &call, HostedReason::Exited, &text);
                continue;
            }
            if let Some(held) = &call.held {
                // Held for an answer, so never invoked.
                held.withdrawal.withdraw(Withdrawn::Exited);
                let detail = Some(text.clone());
                self.end(
                    s,
                    &call,
                    HostedOutcome::Refused,
                    Some(HostedReason::Exited),
                    detail,
                );
                continue;
            }
            if !call.received {
                self.receipt(&call, ReceivedBy::Relay);
            }
            self.end(s, &call, HostedOutcome::Unknown, Some(lost), None);
        }
        s.queue.clear();
        s.frames.clear();
        s.open.clear();
        s.queued = 0;
        if let Some(cause) = &cause {
            let exited = Payload::BindingExited {
                bindings: self.0.bindings.clone(),
                cause: cause.clone(),
            };
            self.0.events.emit(exited);
            (self.0.to_interpreter)(json!({"t": "unbind", "reason": text}));
        }
        s.exited = cause;
        s.gone = Some(text);
    }
}

/// Hand the queue's lines to the process's stdin, an item at a time.
async fn write(relay: Relay, stdin: mpsc::Sender<Value>) {
    let mut state = relay.0.state.subscribe();
    while state
        .wait_for(|s| !s.queue.is_empty() || s.hung_up || s.gone.is_some())
        .await
        .is_ok()
    {
        let Some(item) = relay.with(State::take) else {
            return;
        };
        for line in item.lines {
            if stdin.send(line).await.is_err() {
                return;
            }
        }
    }
}

/// Handle every line the process writes, and then its end.
async fn read(
    relay: Relay,
    mut rpc: mpsc::Receiver<Value>,
    mut events: mpsc::Receiver<Value>,
    pid: Pid,
) {
    loop {
        tokio::select! {
            Some(line) = events.recv() => relay.event(line),
            Some(line) = rpc.recv() => {
                // Every event line the process wrote before this one is handled first.
                while let Ok(event) = events.try_recv() {
                    relay.event(event);
                }
                relay.forward(line);
            }
            else => break,
        }
    }
    let stopping = relay.0.state.borrow().stopping;
    let cause = if stopping {
        None
    } else {
        Some(ended(pid).await)
    };
    relay.with(|s| relay.gone(s, cause));
}

/// How the process ended, read without reaping it, so that its stop still can; looked for for
/// [`EXIT_LOOK`], since its output can close while it lives on.
async fn ended(pid: Pid) -> String {
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    let deadline = Instant::now() + EXIT_LOOK;
    loop {
        match waitid(Id::Pid(pid), flags) {
            Ok(WaitStatus::Exited(_, code)) => return format!("it exited with status {code}"),
            Ok(WaitStatus::Signaled(_, signal, _)) => return format!("it was killed by {signal}"),
            Ok(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(_) => return "its output closed, and it had not exited".to_string(),
            Err(e) => return format!("its output closed, and its exit could not be read: {e}"),
        }
    }
}

/// What a frame of `size` bytes holds in the queue, sent as both programs send one: in parts of
/// [`PART_MAX`] bytes but the last, each line its data's base64 text and [`LINE`].
fn held(size: u64) -> u64 {
    let bytes = size + FRAMING;
    let line = |data: u64| data.div_ceil(3) * 4 + LINE;
    let last = match bytes % PART_MAX {
        0 => 0,
        last => line(last),
    };
    bytes / PART_MAX * line(PART_MAX) + last
}

/// `name` cut to [`NAME_MAX`] bytes: what the interpreter says of a request is its to make long.
fn bounded(name: Option<&str>) -> Option<String> {
    name.map(|name| name[..floor_char_boundary(name, NAME_MAX)].to_string())
}

/// The largest index at most `index` that starts a character of `text`.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn outcome(name: &str) -> HostedOutcome {
    match name {
        "returned" => HostedOutcome::Returned,
        "raised" => HostedOutcome::Raised,
        "refused" => HostedOutcome::Refused,
        "cancelled" => HostedOutcome::Cancelled,
        _ => HostedOutcome::Unknown,
    }
}

fn reason(name: &str) -> Option<HostedReason> {
    match name {
        "interception" => Some(HostedReason::Interception),
        "admission_closed" => Some(HostedReason::AdmissionClosed),
        "interrupted" => Some(HostedReason::Interrupted),
        "closed" => Some(HostedReason::Closed),
        "disconnected" => Some(HostedReason::Disconnected),
        "rule" => Some(HostedReason::Rule),
        "default" => Some(HostedReason::Default),
        "approver" => Some(HostedReason::Approver),
        "no_approver" => Some(HostedReason::NoApprover),
        _ => None,
    }
}

/// What the caller of a request cancelled for `why` raises, when the session's close is not it.
fn withdrawn_text(why: Withdrawn) -> &'static str {
    match why {
        Withdrawn::Interrupted => INTERRUPTED,
        _ => DISCONNECTED,
    }
}

/// The reason a held request cancelled because of `why` ends with.
fn cancelled_for(why: Withdrawn) -> HostedReason {
    match why {
        Withdrawn::Interrupted => HostedReason::Interrupted,
        Withdrawn::Closed => HostedReason::Closed,
        Withdrawn::Disconnected => HostedReason::Disconnected,
        Withdrawn::Exited => HostedReason::Exited,
    }
}
