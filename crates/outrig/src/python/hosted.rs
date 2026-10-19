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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::supervisor::Process;
use crate::events::Events;
use crate::harness::HostedCall;
use crate::harness::event::{
    ExecId, HostedOutcome, HostedReason, Operation, Payload, ReceivedBy, RequestId,
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
    /// Event lines of kinds the relay does not know, for a test that writes its own.
    #[cfg(test)]
    others: Option<mpsc::UnboundedSender<Value>>,
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
    ) -> Self {
        Self(Arc::new(Shared {
            state: watch::Sender::new(State::default()),
            to_interpreter,
            events,
            bindings,
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

    /// Carry `agent`'s lines too: the host has opened a kernel for it. A relay carries no agent's
    /// until told of it, the primary's included.
    pub(crate) fn opened(&self, agent: &str) {
        self.with(|s| s.agents.insert(agent.to_string()));
    }

    /// One of `agent`'s `rpc` lines, for the process.
    pub(crate) fn rpc(&self, agent: &str, mut rpc: Rpc) {
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
                if s.open.remove(&conn) {
                    reason.truncate(floor_char_boundary(&reason, REASON_MAX));
                    let cost = LINE + reason.len() as u64;
                    line["closed"] = reason.into();
                    s.push(line, cost);
                }
                return;
            }
            if let Some(mut seqs) = rpc.cancel.take() {
                // A hint: one with no room is dropped, and its requests run as if their callers
                // had waited.
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

    /// Where event lines of kinds the relay does not know go, for a test.
    #[cfg(test)]
    pub(crate) fn others(&self, others: mpsc::UnboundedSender<Value>) {
        self.with(|s| s.others = Some(others));
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
        let kinds = ["received", "dispatched", "ended", "callback"];
        if !kinds.contains(&line["kind"].as_str().unwrap_or_default()) {
            #[cfg(test)]
            if let Some(others) = &self.0.state.borrow().others {
                let _ = others.send(line);
                return;
            }
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
                        };
                        s.calls.insert(key.clone(), call);
                    }
                    let call = s.calls.get_mut(&key).expect("inserted above");
                    call.sent = true;
                    call.received = true;
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
                s.open.remove(&(agent.to_string(), binding.to_string(), id));
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
        _ => None,
    }
}
