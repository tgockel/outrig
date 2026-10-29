//! A session's child agents, as the host keeps them.
//!
//! A child is a kernel of the session's interpreter that an agent's code
//! launched with `runtime.spawn`, driven through rounds by a runner of its own
//! ([`super::runner`]) as the session drives the primary's. What its parent
//! gives it -- a submission, or a request on one of its channels -- moves
//! between the two kernels inside the interpreter, as objects. The host is told
//! each step as a notice, and keeps from them a record of every request made of
//! a child: what decides when a child gets a round, what the session's events
//! say of the request, and what its shutdown report lists.
//!
//! The host decides what the interpreter cannot: whether an agent may launch a
//! child at all -- the session's admission, `subagent-depth-max`,
//! `children-max` -- what model it runs on, when it gets a round, when a request
//! it leaves open ends -- a work child's second round without completing, a
//! spent token budget -- when a round has nothing left to do, and how a release
//! ends a child and everything below it. A release does not drop the round in
//! flight: it ends the child's kernel, which returns the call the round waits
//! on, and the round stops before its next model call; only one still in a
//! model call after [`RELEASE_WAIT`] is dropped.
//!
//! A session a program on the host owns has one more parent: the program,
//! [`OWNER`], whose children -- its agent class instances' -- are root children
//! at depth 1. It asks what a kernel's code asks, over its connection
//! ([`Children::owner_line`]), and hears what a kernel would be told, and what
//! becomes of each request it made, through what [`Children::attach_owner`]
//! returns. Its requests' bodies and replies cross as JSON, and the program
//! decodes each reply itself: its verdict goes back to the child's kernel,
//! which ends the request as its lock orders the verdict, a cancel and the
//! close.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};

use super::channel::Announcer;
use super::history::Window;
use super::ledger::{CallSlot, Ledger};
use super::orientation::{self, ChannelText, ChildBrief};
use super::resolve::{self, LlmResolveError};
use super::round::Quiet;
use super::window::Windows;
use super::{Agent, AgentError, runner};
use crate::config::{Config, Limits, Secrets};
use crate::events::Events;
use crate::harness::bindings::Brief;
use crate::harness::event::{
    AgentInstanceId, AgentRequestId, ChannelBrief, ChildKind, InputBrief, InstanceRelease,
    InvocationEnd, InvocationId, ParamsBy, Payload, RequestEnd, Subject, Usage,
};
use crate::harness::lifecycle::Lifecycle;
use crate::harness::{ClosedBy, RequestOutcome};
use crate::python::host::{Channels, ChildLines, Counts, Interpreter, OWNER};

/// How long a released child's kernel has to stop before it is taken for
/// wedged: its code never yields, and its thread runs on.
const WEDGE_WAIT: Duration = Duration::from_secs(2);

/// How long the interpreter has to give a work child its next submission. It
/// does so on its reader thread, so only an interpreter nothing can run in is
/// slower.
const ASSIGN_WAIT: Duration = Duration::from_secs(10);

/// How long a released child's round has to end on its own before its runner
/// is stopped. A round that ends in time is kept whole: its next model call is
/// not made, since the child is [quiet](Inner::quiet), and what it ran is
/// committed. One still in a model call when the wait is over is dropped, and
/// so is one when the session closes first.
pub(super) const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// What a session hands its children's registry: what they are resolved
/// against, the limits they run under, and what their instructions say.
pub(crate) struct Settings {
    pub(crate) config: Arc<Config>,
    pub(crate) secrets: Arc<dyn Secrets>,
    /// The session's agent, whose `[agents.<name>]` limits apply.
    pub(crate) agent: Option<String>,
    pub(crate) limits: Limits,
    pub(crate) workspace: Option<PathBuf>,
    /// What the session has learned of its models' windows: the primary's
    /// build fills it, and a child on a model already asked about is not asked
    /// again.
    pub(crate) windows: Windows,
    pub(crate) bindings: Vec<Brief>,
    pub(crate) container: String,
}

/// A session's child agents. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Children(Arc<Inner>);

pub(crate) struct Inner {
    /// The interpreter, addressing the primary; each child's handle is made
    /// from it.
    interpreter: Interpreter,
    ledger: Ledger,
    lifecycle: Arc<Lifecycle>,
    settings: Settings,
    state: Mutex<State>,
    /// Where what the program on the host that owns the session hears goes,
    /// while it listens.
    owner: Mutex<Option<mpsc::UnboundedSender<Value>>>,
    /// The program's launches and releases still running, each of which
    /// answers it.
    owner_tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Held from reading a child's life to telling its parent, so the last
    /// told is the latest; never with the state's lock.
    telling: Mutex<()>,
}

#[derive(Default)]
struct State {
    /// Every agent in the session's tree -- the primary, and each child -- by
    /// its agent id.
    agents: HashMap<String, Node>,
    /// Every request made of a child, by id.
    requests: BTreeMap<u64, Record>,
    /// How many children have been launched: the next is `c` and one more.
    launched: u64,
    /// What closed the session's admission, once something has.
    closed: Option<ClosedBy>,
    /// Each request open when admission closed.
    at_close: Vec<u64>,
    /// Who waits for each answer to an `assign`, by its id.
    assigning: HashMap<u64, oneshot::Sender<Option<u64>>>,
    assigns: u64,
    /// Each root child, by id: whether its instance's start, and its release,
    /// have been recorded -- each once, whichever of the program and the
    /// close says the release first.
    owned: HashMap<String, Owned>,
}

#[derive(Default)]
struct Owned {
    /// The instance's id, once its start was recorded.
    instance: Option<u64>,
    started: bool,
    released: bool,
}

struct Node {
    parent: Option<String>,
    depth: u32,
    /// The model it runs on, as named, which a child it launches runs on
    /// unless the launch names another.
    model: String,
    slot: CallSlot,
    /// The children it launched, in order.
    children: Vec<String>,
    child: Option<ChildNode>,
    /// The skill invocations its launch was made inside, innermost first:
    /// what it and every agent below it spend is each one's.
    invocations: Vec<InvocationId>,
}

struct ChildNode {
    life: Life,
    /// Whether it counts against `children-max`: from its launch until its
    /// kernel is gone.
    resident: bool,
    /// Its runner, until a release takes it to wait for.
    runner: Option<JoinHandle<()>>,
    /// Stops its runner, kept for as long as the child is: a release takes
    /// `runner`, and a close that comes during its wait must still stop it.
    abort: Option<AbortHandle>,
    wake: Arc<Notify>,
    /// Every request ever delivered on each of its channels.
    delivered: BTreeMap<String, u64>,
    /// The round running now, if one is.
    round: Option<u32>,
    /// The agent class instance it serves, if it serves one: for a root
    /// child, the program's number for it, which what it is told names.
    instance: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Launching,
    Running,
    Idle,
    Wedged,
    Released,
}

impl Life {
    fn word(self) -> &'static str {
        match self {
            Life::Launching | Life::Idle => "idle",
            Life::Running => "running",
            Life::Wedged => "wedged",
            Life::Released => "released",
        }
    }
}

struct Record {
    child: String,
    channel: Option<String>,
    state: Req,
    attempts: u32,
    /// The child's rounds that ran while it was open.
    rounds: Vec<u32>,
    /// What a submission's round opens with.
    submission: Option<Submission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Req {
    /// A submission waiting its turn.
    Queued,
    /// A request on a channel the child has not taken.
    Unread,
    /// Taken, at that moment, and not answered.
    Received(Instant),
    Ended(RequestEnd),
}

/// What a submission's round opens with: its inputs, and its result's type.
#[derive(Clone)]
pub(crate) struct Submission {
    pub(crate) inputs: Vec<InputBrief>,
    pub(crate) result_type: String,
    pub(crate) schema: String,
}

/// What a child holds open, as the rule for when it gets a round compares it:
/// the requests waiting unread and those it took and has not answered, by id.
/// For a work child, the submission it works on, or else the next one waiting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) unread: BTreeSet<u64>,
    pub(crate) taken: BTreeSet<u64>,
}

impl Summary {
    pub(crate) fn is_empty(&self) -> bool {
        self.unread.is_empty() && self.taken.is_empty()
    }
}

/// Why a launch was refused, as the spawn's answer names it.
struct Refused {
    kind: &'static str,
    message: String,
}

impl Refused {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Children {
    /// What the session has learned of its models' windows.
    pub(crate) fn windows(&self) -> &Windows {
        &self.0.settings.windows
    }

    /// The registry of a session whose interpreter is `interpreter`, recording
    /// spend in `ledger`. It hears the interpreter's lines about children from
    /// now on, and the session's close once it comes.
    pub(crate) fn new(
        interpreter: Interpreter,
        ledger: Ledger,
        lifecycle: Arc<Lifecycle>,
        settings: Settings,
    ) -> Self {
        let inner = Arc::new(Inner {
            interpreter,
            ledger,
            lifecycle: Arc::clone(&lifecycle),
            settings,
            state: Mutex::default(),
            owner: Mutex::default(),
            owner_tasks: Mutex::default(),
            telling: Mutex::default(),
        });
        let weak = Arc::downgrade(&inner);
        let lines: ChildLines = Arc::new(move |agent: &str, line: Value| {
            if let Some(inner) = weak.upgrade() {
                inner.line(agent, line);
            }
        });
        inner.interpreter.attach_children(lines);
        // The session's `requests-max`, before anything runs: an agent class instance's requests
        // count against it before its child exists to be told it.
        inner.interpreter.tell(json!({
            "t": "limits", "requests_max": inner.settings.limits.requests_max,
        }));
        let weak = Arc::downgrade(&inner);
        lifecycle.on_close(Arc::new(move |by: &ClosedBy| {
            if let Some(inner) = weak.upgrade() {
                inner.close(by);
            }
        }));
        Self(inner)
    }

    /// `agent` is a root of the tree, at `depth`, running on `model` as named
    /// and spending through `slot`: the primary agent, at 1.
    pub(crate) fn root(&self, agent: &str, depth: u32, model: &str, slot: CallSlot) {
        lock(&self.0.state).agents.insert(
            agent.to_string(),
            Node {
                parent: None,
                depth,
                model: model.to_string(),
                slot,
                children: Vec::new(),
                child: None,
                invocations: Vec::new(),
            },
        );
    }

    /// The program on the host that owns the session is a root of the tree,
    /// at depth 0, its children on `default-model` unless a launch names
    /// another: what it is told, and what becomes of each request it makes,
    /// comes out of what this returns, in order, until
    /// [`Children::detach_owner`].
    pub(crate) fn attach_owner(&self) -> mpsc::UnboundedReceiver<Value> {
        let (lines, told) = mpsc::unbounded_channel();
        *lock(&self.0.owner) = Some(lines);
        let model = self.0.settings.config.default_model.clone();
        self.root(OWNER, 0, &model.unwrap_or_default(), CallSlot::default());
        // The interpreter's exit ends what the program still waits on, however admission closed:
        // after an earlier close, nothing else would.
        let (inner, gone) = (Arc::clone(&self.0), self.0.interpreter.gone());
        tokio::spawn(async move {
            gone.await;
            inner.to_owner(json!({"t": "agent.closed", "terminated": true}));
        });
        told
    }

    /// The program has seen the last of the session, once every launch and
    /// release it asked for has answered it: it is told nothing more.
    pub(crate) async fn detach_owner(&self) {
        let tasks = std::mem::take(&mut *lock(&self.0.owner_tasks));
        for task in tasks {
            let _ = task.await;
        }
        lock(&self.0.owner).take();
    }

    /// A line from the program on the host that owns the session: what it asks
    /// as a kernel's code would -- a spawn, a release, what became of an
    /// instance -- or a request it makes of a root child, numbered `ask`, a
    /// cancel of one, or its verdict on a reply, which go on to the
    /// interpreter. A request made once admission has closed ends at once.
    pub(crate) fn owner_line(&self, line: Value) {
        let inner = &self.0;
        match line["t"].as_str().unwrap_or_default() {
            "ask" if inner.closed() => inner.to_owner(json!({
                "t": "agent.ended", "ask": line["ask"], "outcome": "closed",
                "error": "the session is closing, so the request was not sent",
            })),
            "ask" | "withdraw" | "verdict" => inner.interpreter.tell(line),
            // Collected or released once admission has closed, the instance is the close's to
            // release: nothing further, as for a kernel's.
            "agent.release" if inner.closed() => inner.to_owner(json!({
                "t": "released", "agent": OWNER, "id": line["id"], "error": null,
            })),
            "agent.spawn" | "agent.release" | "agent.instance.handle.collected" => {
                inner.line(OWNER, line);
            }
            kind => tracing::warn!("ignored a {kind} line from the program"),
        }
    }

    /// Each request open when admission closed, and how it ended: what the
    /// close or an answer ended it with, or `terminated` for one nothing
    /// ended -- the interpreter died first.
    pub(crate) fn report(&self) -> Vec<RequestOutcome> {
        let state = lock(&self.0.state);
        state
            .at_close
            .iter()
            .filter_map(|id| {
                let record = state.requests.get(id)?;
                Some(RequestOutcome {
                    request: AgentRequestId::new(*id),
                    child: record.child.clone(),
                    channel: record.channel.clone(),
                    outcome: match record.state {
                        Req::Ended(outcome) => outcome,
                        _ => RequestEnd::Terminated,
                    },
                })
            })
            .collect()
    }

    /// Stop every child's runner: the session is ending, and no child makes
    /// another model call.
    pub(crate) fn stop(&self) {
        let runners = lock(&self.0.state).runners();
        for runner in runners {
            runner.abort();
        }
    }
}

impl State {
    fn resident(&self) -> usize {
        self.agents
            .values()
            .filter(|node| node.child.as_ref().is_some_and(|child| child.resident))
            .count()
    }

    fn child(&self, id: &str) -> Option<&ChildNode> {
        self.agents.get(id)?.child.as_ref()
    }

    fn child_mut(&mut self, id: &str) -> Option<&mut ChildNode> {
        self.agents.get_mut(id)?.child.as_mut()
    }

    /// Every child's runner, as what stops it: a released child's too, which
    /// its release is waiting on.
    fn runners(&self) -> Vec<AbortHandle> {
        self.agents
            .values()
            .filter_map(|node| node.child.as_ref()?.abort.clone())
            .collect()
    }

    /// Round `round` of `child`'s opens, unless the child was released, which
    /// is decided here, under the lock a release marks it under: a runner that
    /// found it live a moment ago must not make it live again. Whether it
    /// opened.
    fn open_round(&mut self, child: &str, round: u32) -> bool {
        let Some(node) = self.child_mut(child) else {
            return false;
        };
        if matches!(node.life, Life::Released | Life::Wedged) {
            return false;
        }
        node.round = Some(round);
        node.life = Life::Running;
        for record in self
            .requests
            .values_mut()
            .filter(|record| record.child == child && !matches!(record.state, Req::Ended(_)))
        {
            // A round that committed nothing leaves its number to the next.
            if record.rounds.last() != Some(&round) {
                record.rounds.push(round);
            }
        }
        true
    }

    /// Whether `child` was released: its release has begun, or it is wedged.
    fn released(&self, child: &str) -> bool {
        self.child(child)
            .is_some_and(|node| matches!(node.life, Life::Released | Life::Wedged))
    }

    /// What `child` holds open: see [`Inner::summary`].
    fn summary(&self, child: &str, kind: ChildKind) -> Summary {
        let mut summary = Summary::default();
        for (id, record) in self.requests.iter().filter(|(_, r)| r.child == child) {
            match record.state {
                Req::Unread => {
                    summary.unread.insert(*id);
                }
                Req::Received(_) => {
                    summary.taken.insert(*id);
                }
                Req::Queued if kind == ChildKind::Work => {
                    summary.unread.insert(*id);
                }
                Req::Queued | Req::Ended(_) => {}
            }
        }
        if kind == ChildKind::Work {
            if summary.taken.is_empty() {
                summary.unread = summary.unread.iter().take(1).copied().collect();
            } else {
                summary.unread.clear();
            }
        }
        summary
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Where `agent`'s events go.
    fn events_of(&self, agent: &str) -> Events {
        self.interpreter.events().for_agent(agent)
    }

    /// Tell the program on the host that owns the session `line`, while it
    /// listens.
    fn to_owner(&self, line: Value) {
        if let Some(owner) = &*lock(&self.owner) {
            let _ = owner.send(line);
        }
    }

    /// Hand `line` to `agent`'s code: a kernel's, or the program on the
    /// host's.
    fn answer(&self, agent: &str, line: Value) {
        match agent {
            OWNER => self.to_owner(line),
            _ => self.interpreter.tell(line),
        }
    }

    /// One line the interpreter sent about children, from `agent`'s kernel;
    /// or one the program on the host sent, from [`OWNER`].
    fn line(self: &Arc<Self>, agent: &str, line: Value) {
        // A step of a request the program made of a root child, which it hears.
        if !line["ask"].is_null() {
            self.to_owner(line.clone());
        }
        let kind = line["t"].as_str().unwrap_or_default().to_string();
        match kind.as_str() {
            "agent.spawn" | "agent.release" => {
                let (inner, parent) = (Arc::clone(self), agent.to_string());
                let spawning = kind == "agent.spawn";
                let task = tokio::spawn(async move {
                    match spawning {
                        true => inner.spawn(parent, line).await,
                        false => inner.release(parent, line).await,
                    }
                });
                // Each answers the program, which hears them all before it stops listening.
                if agent == OWNER {
                    let mut tasks = lock(&self.owner_tasks);
                    tasks.retain(|task| !task.is_finished());
                    tasks.push(task);
                }
            }
            "agent.request" => self.requested(agent, &line),
            "agent.call" => self.called(agent, &line),
            "agent.call.ended" => self.call_ended(&line),
            "agent.instance.collected"
            | "agent.instance.released"
            | "agent.instance.handle.collected" => self.instance(agent, &kind, &line),
            "agent.invocation" => self.invoked(agent, &line),
            "agent.invocation.ended" => self.invocation_ended(agent, &line),
            "agent.received" => self.received(&line),
            "agent.answered" => self.answered(&line),
            "agent.ended" => self.ended(&line),
            "agent.refused" => self.refused(&line),
            "agent.assigned" => {
                if let Some(id) = line["id"].as_u64()
                    && let Some(waiter) = self.state().assigning.remove(&id)
                {
                    let _ = waiter.send(line["request"].as_u64());
                }
            }
            "agent.progress" => self.progress(agent, &line),
            "agent.gone" => self.gone(agent),
            // A reply to a request the program made, which the program judges.
            "agent.reply" => {}
            _ => tracing::warn!("ignored a {kind} line from agent {agent:?}"),
        }
    }

    // -------------------------------------------------------------- launching

    /// Answer a spawn `parent`'s code asked for: a child launched, or why not.
    async fn spawn(self: Arc<Self>, parent: String, line: Value) {
        let ask = line["id"].clone();
        let answer = match self.launch(&parent, &line).await {
            Ok((child, instance)) => json!({
                "t": "spawned", "agent": parent, "id": ask, "child": child, "instance": instance,
                "requests_max": self.settings.limits.requests_max,
            }),
            Err(refused) => json!({
                "t": "spawned", "agent": parent, "id": ask,
                "error": {"kind": refused.kind, "message": refused.message},
            }),
        };
        self.answer(&parent, answer);
    }

    /// Launch the child `line` asks for under `parent`, and return its id, and
    /// a root child's instance id, which its kernel gave it. The
    /// session's admission, the depth and `children-max` decide before
    /// anything is made, and the place it takes is its own from then on;
    /// anything that fails after gives it back, and leaves no child.
    async fn launch(
        self: &Arc<Self>,
        parent: &str,
        line: &Value,
    ) -> Result<(String, Option<u64>), Refused> {
        let limits = self.settings.limits;
        let kind = match line["kind"].as_str() {
            Some("request") => ChildKind::Request,
            _ => ChildKind::Work,
        };
        let name = line["name"].as_str().unwrap_or_default().to_string();
        let invocations = line["invocations"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_u64)
                    .map(InvocationId::new)
                    .collect()
            })
            .unwrap_or_default();
        // Read before the state's lock: reading it takes the host's table lock, which the host
        // holds as it hands this registry a line, which takes the state's.
        let closing = self.lifecycle.closed();
        let (id, model, depth, lineage) = {
            let mut state = self.state();
            if let Some(by) = state.closed.clone().or(closing) {
                return Err(Refused::new(
                    "closed",
                    format!("the session is closing -- {by} -- so no child is launched"),
                ));
            }
            let Some(node) = state.agents.get(parent) else {
                return Err(Refused::new(
                    "other",
                    format!("agent {parent:?} is not one that may launch a child"),
                ));
            };
            // Decided under the lock a release marks its subtree under: a
            // child launched after that would be missed by the release, and
            // outlive the agent that launched it.
            if node
                .child
                .as_ref()
                .is_some_and(|c| matches!(c.life, Life::Released | Life::Wedged))
            {
                return Err(Refused::new(
                    "released",
                    format!("agent {parent:?} was released, so it launches no child"),
                ));
            }
            if node.depth >= limits.depth_max {
                return Err(Refused::new(
                    "limit",
                    format!(
                        "subagent-depth-max is {}, and this agent is at depth {}, so it cannot \
                         launch a child",
                        limits.depth_max, node.depth
                    ),
                ));
            }
            if state.resident() >= limits.children_max as usize {
                return Err(Refused::new(
                    "limit",
                    format!(
                        "children-max ({}) children are resident in this session; release one \
                         before launching another",
                        limits.children_max
                    ),
                ));
            }
            let model = line["model"]
                .as_str()
                .map_or_else(|| node.model.clone(), str::to_string);
            // The round of the parent's it is launched in, whose tree what it
            // spends is added to.
            let lineage = (node.slot.clone(), node.slot.current_round());
            let depth = node.depth + 1;
            state.launched += 1;
            let id = format!("c{}", state.launched);
            state.agents.insert(
                id.clone(),
                Node {
                    parent: Some(parent.to_string()),
                    depth,
                    model: model.clone(),
                    slot: CallSlot::default(),
                    children: Vec::new(),
                    child: Some(ChildNode {
                        life: Life::Launching,
                        resident: true,
                        runner: None,
                        abort: None,
                        wake: Arc::new(Notify::new()),
                        delivered: BTreeMap::new(),
                        round: None,
                        instance: line["instance"]["id"].as_u64(),
                    }),
                    invocations,
                },
            );
            if let Some(node) = state.agents.get_mut(parent) {
                node.children.push(id.clone());
            }
            (id, model, depth, lineage)
        };
        match self
            .make(&id, &name, kind, &model, depth, lineage, parent, line)
            .await
        {
            Ok(instance) => Ok((id, instance)),
            Err(refused) => {
                let mut state = self.state();
                state.agents.remove(&id);
                if let Some(node) = state.agents.get_mut(parent) {
                    node.children.retain(|child| *child != id);
                }
                Err(refused)
            }
        }
    }

    /// Make child `id`: resolve its model, open its kernel, build it, and
    /// start its runner.
    #[allow(clippy::too_many_arguments)]
    async fn make(
        self: &Arc<Self>,
        id: &str,
        name: &str,
        kind: ChildKind,
        model: &str,
        depth: u32,
        lineage: (CallSlot, Option<u32>),
        parent: &str,
        line: &Value,
    ) -> Result<Option<u64>, Refused> {
        let settings = &self.settings;
        // A root child with no model named, in a configuration with no
        // `default-model`, has none, which resolving says.
        let mut resolved = resolve::resolve_agent(
            &settings.config,
            settings.agent.as_deref(),
            Some(model).filter(|model| !model.is_empty()),
            &*settings.secrets,
        )
        .map_err(|e| match e {
            AgentError::Resolve(LlmResolveError::UnknownModel { .. }) => {
                Refused::new("model", e.to_string())
            }
            // A root child's, whose program named none.
            AgentError::Resolve(LlmResolveError::MissingModel) => Refused::new(
                "model",
                "no model was named with model=, and the configuration sets no default-model",
            ),
            e => Refused::new("other", e.to_string()),
        })?;
        settings.windows.fill(&mut resolved.candidates).await;
        // The tool limits come with the session's agent; its preamble does not
        // (0003-30's fork 1): what a child needs, its parent's prompt says.
        resolved.preamble = None;
        let inputs = briefs(&line["manifest"]);
        let channels: Vec<ChannelText> = line["channels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|channel| ChannelText {
                name: text(&channel["name"]),
                doc: channel["doc"].as_str().map(str::to_string),
                request_type: text(&channel["request_type"]),
                reply_type: text(&channel["reply_type"]),
                request: text(&channel["request"]),
                reply: text(&channel["reply"]),
            })
            .collect();
        let handle = self.interpreter.for_agent(id);
        let minted = handle
            .open_child(
                id,
                json!({
                    "kind": if kind == ChildKind::Request { "request" } else { "work" },
                    "spawn": line["id"],
                    "requests_max": settings.limits.requests_max,
                    // A root child's, which the program described.
                    "root": line["root"],
                }),
            )
            .await
            // A root child's kernel opens only while the session does.
            .map_err(|e| {
                Refused::new(
                    if self.closed() { "closed" } else { "other" },
                    e.to_string(),
                )
            })?;
        let events = handle.events().clone();
        let briefed: Vec<ChannelBrief> = channels
            .iter()
            .map(|channel| ChannelBrief {
                name: channel.name.clone(),
                request: channel.request_type.clone(),
                reply: channel.reply_type.clone(),
            })
            .collect();
        events.emit(Payload::AgentSpawned {
            name: name.to_string(),
            parent: Subject::agent(parent).to_string(),
            kind,
            model: model.to_string(),
            depth,
            inputs: inputs.clone(),
            channels: briefed.clone(),
        });
        // An agent class instance's child, which has a round of its own as it launches: a root
        // child's instance has the id its kernel gave it, which no other has.
        let instance = minted.or(line["instance"]["id"].as_u64());
        let brief = match kind {
            ChildKind::Request if instance.is_some() => ChildBrief::Instance(&channels),
            ChildKind::Request => ChildBrief::Request(&channels),
            _ => ChildBrief::Work,
        };
        let preamble = orientation::child_preamble(
            settings.workspace.as_deref(),
            &settings.bindings,
            Window::DEFAULT,
            line["prompt"].as_str().unwrap_or_default(),
            &inputs,
            &brief,
        );
        let slot = CallSlot::new(self.ledger.clone(), events, Some(lineage));
        let weak = Arc::downgrade(self);
        let child = id.to_string();
        let announcer = Announcer::of_requests(Arc::new(move || {
            weak.upgrade()
                .map(|inner| inner.counts(&child))
                .unwrap_or_default()
        }));
        let weak = Arc::downgrade(self);
        let child = id.to_string();
        let quiet: Quiet =
            Arc::new(move || weak.upgrade().and_then(|inner| inner.quiet(&child, kind)));
        let agent = match Agent::build_child(
            &resolved,
            handle,
            &settings.container,
            preamble,
            slot.clone(),
            announcer,
            quiet,
            Arc::clone(&self.lifecycle),
        ) {
            Ok(agent) => agent,
            Err(e) => {
                let _ = self.interpreter.end(id, WEDGE_WAIT).await;
                return Err(Refused::new("other", e.to_string()));
            }
        };
        let refused = {
            let mut state = self.state();
            let state = &mut *state;
            let Some(node) = state.agents.get_mut(id) else {
                return Err(Refused::new("other", "the child was ended as it launched"));
            };
            node.slot = slot;
            let Some(child) = node.child.as_mut() else {
                return Err(Refused::new("other", "the child was ended as it launched"));
            };
            // A root child's instance the close has said is released never starts.
            let owned = (parent == OWNER).then(|| state.owned.entry(id.to_string()).or_default());
            // Its parent may have been released while this launched it,
            // leaving it to this to end.
            if child.life == Life::Released {
                Some(Refused::new(
                    "released",
                    "the agent launching it was released",
                ))
            } else if owned.as_ref().is_some_and(|owned| owned.released) {
                Some(Refused::new(
                    "closed",
                    "the session closed as the child launched",
                ))
            } else {
                if let Some(owned) = owned {
                    owned.started = true;
                    owned.instance = instance;
                }
                if let Some(instance) = instance {
                    // Once nothing can refuse the launch, and before the runner can say the
                    // child is ready.
                    self.events_of(id).emit(Payload::AgentInstanceStarted {
                        instance: AgentInstanceId::new(instance),
                        parent: Subject::agent(parent).to_string(),
                        module: text(&line["instance"]["module"]),
                        qualname: text(&line["instance"]["qualname"]),
                        digest: line["instance"]["digest"].as_str().map(str::to_string),
                        inputs,
                        channels: briefed,
                    });
                }
                child.life = Life::Idle;
                let runner = tokio::spawn(runner::run(
                    Arc::downgrade(self),
                    id.to_string(),
                    kind,
                    agent,
                    Arc::clone(&child.wake),
                    self.ledger.budget_watch(),
                    instance,
                ));
                child.abort = Some(runner.abort_handle());
                child.runner = Some(runner);
                None
            }
        };
        if let Some(refused) = refused {
            self.interpreter.end(id, WEDGE_WAIT).await;
            return Err(refused);
        }
        Ok(minted)
    }

    // -------------------------------------------------------------- requests

    fn requested(&self, sender: &str, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        let child = text(&line["child"]);
        let channel = line["channel"].as_str().map(str::to_string);
        let submission = channel.is_none().then(|| Submission {
            inputs: briefs(&line["manifest"]),
            result_type: text(&line["result"]["type"]),
            schema: text(&line["result"]["schema"]),
        });
        let mut state = self.state();
        // A round running now is one it spans.
        let rounds = state
            .child(&child)
            .and_then(|node| node.round)
            .into_iter()
            .collect();
        state.requests.insert(
            id,
            Record {
                child: child.clone(),
                channel: channel.clone(),
                state: if submission.is_some() {
                    Req::Queued
                } else {
                    Req::Unread
                },
                attempts: 0,
                rounds,
                submission,
            },
        );
        if let Some(node) = state.child_mut(&child) {
            if let Some(channel) = &channel {
                *node.delivered.entry(channel.clone()).or_default() += 1;
            }
            node.wake.notify_one();
        }
        drop(state);
        self.events_of(&child).emit(Payload::AgentRequestSent {
            request: AgentRequestId::new(id),
            channel,
            sender: Subject::agent(sender).to_string(),
            body: line["body"].as_str().unwrap_or_default().to_string(),
            attempts_max: line["attempts"].as_u64().unwrap_or_default() as u32,
        });
    }

    /// A typed call `caller`'s code made -- `@outrig.agent`'s -- whose child
    /// the line names: recorded under the child, with the declaration's name
    /// and digest, ahead of its submission's events.
    fn called(&self, caller: &str, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        self.events_of(&text(&line["child"]))
            .emit(Payload::AgentCallStarted {
                request: AgentRequestId::new(id),
                parent: Subject::agent(caller).to_string(),
                module: text(&line["module"]),
                qualname: text(&line["qualname"]),
                digest: line["digest"].as_str().map(str::to_string),
                invocation: line["invocation"].as_u64().map(InvocationId::new),
            });
    }

    /// The call ended and its child was released: how it ended, what every
    /// model request the child made used, and what it was accepted with.
    fn call_ended(&self, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        let child = text(&line["child"]);
        let attempts = self
            .state()
            .agents
            .get(&child)
            .map(|node| node.slot.attempts())
            .unwrap_or_default();
        self.events_of(&child).emit(Payload::AgentCallSettled {
            request: AgentRequestId::new(id),
            outcome: outcome(line["outcome"].as_str().unwrap_or_default()),
            usage: self.ledger.total(&attempts),
            invocation: line["invocation"].as_u64().map(InvocationId::new),
            value: line["value"].as_str().map(str::to_string),
        });
    }

    /// What an agent class's instance says of itself, `kind`: it was collected,
    /// one of its handles was, or it released its child -- which settled what
    /// the release or the close ended on it, and spent what every model
    /// request the child made used. Recorded under the child.
    fn instance(&self, agent: &str, kind: &str, line: &Value) {
        let Some(id) = line["instance"].as_u64() else {
            return;
        };
        let (instance, child) = (AgentInstanceId::new(id), text(&line["child"]));
        // A root child's release, which the program and the close may each say: recorded once,
        // and not for an instance that never started, which the close kept from starting.
        if agent == OWNER && kind == "agent.instance.released" {
            let mut state = self.state();
            let owned = state.owned.entry(child.clone()).or_default();
            if std::mem::replace(&mut owned.released, true) || !owned.started {
                return;
            }
        }
        let payload = match kind {
            "agent.instance.collected" => Payload::AgentInstanceCollected { instance },
            "agent.instance.handle.collected" => Payload::AgentInstanceHandleCollected {
                instance,
                request: AgentRequestId::new(line["request"].as_u64().unwrap_or_default()),
            },
            _ => {
                let (requests, attempts) = {
                    let state = self.state();
                    let requests = state
                        .requests
                        .iter()
                        .filter(|(_, record)| {
                            record.child == child
                                && matches!(
                                    record.state,
                                    Req::Ended(RequestEnd::Released | RequestEnd::Closed)
                                )
                        })
                        .map(|(id, _)| AgentRequestId::new(*id))
                        .collect();
                    let attempts = state
                        .agents
                        .get(&child)
                        .map(|node| node.slot.attempts())
                        .unwrap_or_default();
                    (requests, attempts)
                };
                Payload::AgentInstanceReleased {
                    instance,
                    how: match line["how"].as_str() {
                        Some("async_with") => InstanceRelease::AsyncWith,
                        Some("collected") => InstanceRelease::Collected,
                        Some("closing") => InstanceRelease::Closing,
                        _ => InstanceRelease::Explicit,
                    },
                    requests,
                    usage: self.ledger.total(&attempts),
                }
            }
        };
        self.events_of(&child).emit(payload);
    }

    /// `agent`'s code is calling a skill's entry through
    /// `outrig.skills.invoke`.
    fn invoked(&self, agent: &str, line: &Value) {
        let Some(id) = line["invocation"].as_u64() else {
            return;
        };
        let call = line["call"].as_u64().map(AgentRequestId::new);
        let bindings = line["bindings"]
            .as_array()
            .map(|names| names.iter().map(text).collect())
            .unwrap_or_default();
        self.events_of(agent).emit(Payload::SkillInvocationStarted {
            invocation: InvocationId::new(id),
            skill: text(&line["skill"]),
            source: text(&line["source"]),
            digest: text(&line["digest"]),
            params: line["params"].as_str().map(str::to_string),
            by: if call.is_some() {
                ParamsBy::Parser
            } else {
                ParamsBy::Caller
            },
            call,
            bindings,
        });
    }

    /// The entry an invocation of `agent`'s called has ended: how, and what
    /// the children made inside the invocation spent.
    fn invocation_ended(&self, agent: &str, line: &Value) {
        let Some(id) = line["invocation"].as_u64() else {
            return;
        };
        let invocation = InvocationId::new(id);
        let outcome = match line["outcome"].as_str() {
            Some("returned") => InvocationEnd::Returned,
            Some("raised") => InvocationEnd::Raised,
            _ => InvocationEnd::Cancelled,
        };
        let duration = line["duration"]
            .as_f64()
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .unwrap_or_default();
        self.events_of(agent)
            .emit(Payload::SkillInvocationFinished {
                invocation,
                outcome,
                duration,
                result: line["result"].as_str().map(str::to_string),
                exception: line["exception"].as_str().map(str::to_string),
                usage: self.spent_inside(invocation),
            });
    }

    /// What every child launched inside `invocation` has spent, and every
    /// agent below them, each attempt once: a child keeps its node, and its
    /// slot every attempt it made, once it is released.
    fn spent_inside(&self, invocation: InvocationId) -> Option<Usage> {
        let attempts = {
            let state = self.state();
            let mut attempts = Vec::new();
            let mut todo: Vec<&str> = state
                .agents
                .iter()
                .filter(|(_, node)| node.invocations.contains(&invocation))
                .map(|(id, _)| id.as_str())
                .collect();
            while let Some(id) = todo.pop() {
                if let Some(node) = state.agents.get(id) {
                    attempts.extend(node.slot.attempts());
                    todo.extend(node.children.iter().map(String::as_str));
                }
            }
            attempts
        };
        self.ledger.total(&attempts)
    }

    fn received(&self, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        let mut state = self.state();
        let Some(record) = state.requests.get_mut(&id) else {
            return;
        };
        if matches!(record.state, Req::Queued | Req::Unread) {
            record.state = Req::Received(Instant::now());
        }
        let child = record.child.clone();
        let round = state.child(&child).and_then(|node| node.round);
        if let Some(node) = state.child(&child) {
            node.wake.notify_one();
        }
        drop(state);
        self.events_of(&child).emit(Payload::AgentRequestReceived {
            request: AgentRequestId::new(id),
            round,
        });
    }

    fn answered(&self, line: &Value) {
        let (Some(id), Some(attempt)) = (line["request"].as_u64(), line["attempt"].as_u64()) else {
            return;
        };
        let child = {
            let mut state = self.state();
            let Some(record) = state.requests.get_mut(&id) else {
                return;
            };
            record.attempts = attempt as u32;
            record.child.clone()
        };
        let events = self.events_of(&child);
        let request = AgentRequestId::new(id);
        events.emit(Payload::AgentRequestReplied {
            request,
            attempt: attempt as u32,
            value: line["value"].as_str().unwrap_or_default().to_string(),
        });
        let problems: Vec<String> = line["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        if !problems.is_empty() {
            events.emit(Payload::AgentRequestInvalid {
                request,
                attempt: attempt as u32,
                problems,
            });
        }
    }

    fn ended(&self, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        let outcome = outcome(line["outcome"].as_str().unwrap_or_default());
        let (child, attempts, rounds) = {
            let mut state = self.state();
            let Some(record) = state.requests.get_mut(&id) else {
                return;
            };
            record.state = Req::Ended(outcome);
            let ended = (record.child.clone(), record.attempts, record.rounds.clone());
            if let Some(node) = state.child(&ended.0) {
                node.wake.notify_one();
            }
            ended
        };
        let events = self.events_of(&child);
        let request = AgentRequestId::new(id);
        match outcome {
            RequestEnd::Cancelled => events.emit(Payload::AgentRequestCancelled {
                request,
                received: line["received"].as_bool().unwrap_or_default(),
            }),
            RequestEnd::Failed => events.emit(Payload::AgentRequestFailed {
                request,
                message: line["message"].as_str().unwrap_or_default().to_string(),
            }),
            _ => {}
        }
        events.emit(Payload::AgentRequestSettled {
            request,
            outcome,
            attempts,
            rounds,
            cause: line["cause"].as_str().map(str::to_string),
        });
    }

    fn refused(&self, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        let Some(child) = self.state().requests.get(&id).map(|r| r.child.clone()) else {
            return;
        };
        self.events_of(&child).emit(Payload::AgentRequestRefused {
            request: AgentRequestId::new(id),
            ended: outcome(line["ended"].as_str().unwrap_or_default()),
        });
    }

    /// Progress a work child sent on the submission it works on, which its
    /// sender reads.
    fn progress(&self, child: &str, line: &Value) {
        let Some(id) = line["request"].as_u64() else {
            return;
        };
        self.events_of(child)
            .emit_with(|| Payload::AgentRequestProgress {
                request: AgentRequestId::new(id),
                body: line["body"].as_str().unwrap_or_default().to_string(),
            });
    }

    // -------------------------------------------------------------- what the runner asks

    /// What `child` holds open, as the rule for its rounds compares it. A
    /// work child holds one submission at a time: the one it took, or else
    /// the next waiting.
    pub(crate) fn summary(&self, child: &str, kind: ChildKind) -> Summary {
        self.state().summary(child, kind)
    }

    /// Why `child`'s round has nothing left to do, if it has none: a work
    /// child's last submission was accepted and no other waits, or the child
    /// was released. The round ends before its next model call, which
    /// would only speak after the work. The acceptance is named first: a typed
    /// call releases its child as the answer is accepted, and what the round
    /// records is that it was done. A request child is quiet only once
    /// released: what it holds open is what its channels' requests come to,
    /// and it answers them as it chooses.
    fn quiet(&self, child: &str, kind: ChildKind) -> Option<String> {
        let state = self.state();
        if kind == ChildKind::Work && state.summary(child, kind).is_empty() {
            // The latest, so an older acceptance does not vouch for a
            // submission that was rejected or cancelled after it.
            if let Some((id, record)) = state.requests.iter().rev().find(|(_, r)| r.child == child)
                && record.state == Req::Ended(RequestEnd::Accepted)
            {
                return Some(format!(
                    "submission {id} was accepted and nothing else waits"
                ));
            }
        }
        state
            .released(child)
            .then(|| "the child was released".to_string())
    }

    /// Whether `child` was released: its runner has no round to open.
    pub(crate) fn released(&self, child: &str) -> bool {
        self.state().released(child)
    }

    /// The requests waiting unread on each of `child`'s channels, and every
    /// one ever delivered there: what its announcer counts.
    fn counts(&self, child: &str) -> Channels {
        let state = self.state();
        let mut channels: Channels = state
            .child(child)
            .map(|node| {
                node.delivered
                    .iter()
                    .map(|(channel, delivered)| {
                        (
                            channel.clone(),
                            Counts {
                                pending: 0,
                                delivered: *delivered,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for record in state
            .requests
            .values()
            .filter(|record| record.child == child && record.state == Req::Unread)
        {
            if let Some(channel) = &record.channel
                && let Some(counts) = channels.get_mut(channel)
            {
                counts.pending += 1;
            }
        }
        channels
    }

    /// Each request `child` took and has not answered: its id, its channel,
    /// and how long ago it was taken.
    pub(crate) fn taken(&self, child: &str) -> Vec<(u64, String, Duration)> {
        self.state()
            .requests
            .iter()
            .filter(|(_, record)| record.child == child)
            .filter_map(|(id, record)| match (record.state, &record.channel) {
                (Req::Received(at), Some(channel)) => Some((*id, channel.clone(), at.elapsed())),
                _ => None,
            })
            .collect()
    }

    /// What `child`'s open requests on its channels come to, in the
    /// announcer's words.
    pub(crate) fn unread_text(&self, child: &str) -> String {
        super::channel::announcement(&self.counts(child), super::channel::Noun::Request)
            .unwrap_or_default()
    }

    /// The submission request `id` is.
    pub(crate) fn submission(&self, id: u64) -> Option<Submission> {
        self.state().requests.get(&id)?.submission.clone()
    }

    /// Every request open on `child`.
    pub(crate) fn open(&self, child: &str) -> Vec<u64> {
        self.state()
            .requests
            .iter()
            .filter(|(_, record)| record.child == child && !matches!(record.state, Req::Ended(_)))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Give work child `child` the next submission waiting for it; which it
    /// was, or `None` when none was.
    pub(crate) async fn assign(&self, child: &str) -> Option<u64> {
        let (waiter, answer) = oneshot::channel();
        let id = {
            let mut state = self.state();
            state.assigns += 1;
            let id = state.assigns;
            state.assigning.insert(id, waiter);
            id
        };
        self.interpreter
            .tell(json!({"t": "assign", "agent": child, "id": id}));
        let assigned = tokio::time::timeout(ASSIGN_WAIT, answer).await;
        self.state().assigning.remove(&id);
        assigned.ok().and_then(Result::ok).flatten()
    }

    /// Round `round` of `child`'s opens: every request open on it now spans
    /// it, and so does one made while it runs. Whether it did: a released
    /// child's does not, and its runner is over.
    pub(crate) fn round_opened(&self, child: &str, round: u32) -> bool {
        let opened = self.state().open_round(child, round);
        if opened {
            self.tell_life(child);
        }
        opened
    }

    /// `child`'s round has ended. Whether it is idle is said only once its
    /// runner waits, so a round that follows at once is never seen apart from
    /// the one before.
    pub(crate) fn round_ended(&self, child: &str) {
        if let Some(node) = self.state().child_mut(child) {
            node.round = None;
        }
    }

    /// `child`'s runner waits for a change: it is idle.
    pub(crate) fn idle(&self, child: &str) {
        let mut state = self.state();
        let Some(node) = state.child_mut(child) else {
            return;
        };
        if node.life != Life::Running {
            return;
        }
        node.life = Life::Idle;
        drop(state);
        self.tell_life(child);
    }

    /// Tell `child`'s parent what it is doing now, so its handles' status
    /// says so without asking.
    fn tell_life(&self, child: &str) {
        let _telling = lock(&self.telling);
        let line = {
            let state = self.state();
            let Some(node) = state.agents.get(child) else {
                return;
            };
            let (Some(parent), Some(life)) = (&node.parent, node.child.as_ref().map(|c| c.life))
            else {
                return;
            };
            let parent_held = state
                .agents
                .get(parent)
                .is_some_and(|node| node.child.as_ref().is_none_or(|c| c.resident));
            if parent == OWNER {
                let instance = node.child.as_ref().and_then(|c| c.instance);
                self.to_owner(json!({"t": "child", "instance": instance, "state": life.word()}));
                return;
            }
            if !parent_held {
                return;
            }
            json!({"t": "child", "agent": parent, "id": 0, "child": child, "state": life.word()})
        };
        // Not under the state's lock: the host hands this registry lines with its table's lock
        // held, which telling takes.
        self.interpreter.tell(line);
    }

    /// End request `id`, open on `child`, `outcome` -- `rejected`, `budget`
    /// or `failed` -- for `message`. `cause`, when there is one, is how a
    /// model call of the child's failed and ended a round: the parent's
    /// exception carries it as its `__cause__`, and the event the end is
    /// recorded in.
    pub(crate) fn settle(
        &self,
        child: &str,
        id: u64,
        outcome: &str,
        message: &str,
        cause: Option<&str>,
    ) {
        self.interpreter.tell(json!({
            "t": "settle", "agent": child, "id": id, "outcome": outcome, "message": message,
            "cause": cause,
        }));
    }

    /// The orienting round of `child`, agent class instance `instance`'s,
    /// has ended; or, with `error`, it cannot be had -- `budget` or `failed`,
    /// and why. The instance's `ready()` hears so from its parent's kernel.
    pub(crate) fn oriented(&self, child: &str, instance: u64, error: Option<(&str, &str)>) {
        let state = self.state();
        let node = state.agents.get(child);
        let parent = node.and_then(|node| node.parent.clone());
        // A root child's instance at the session's close never becomes ready: under the lock
        // the close is told under, so the program hears one of the two.
        if parent.as_deref() == Some(OWNER) && state.closed.is_some() {
            return;
        }
        self.events_of(child).emit(Payload::AgentInstanceReady {
            instance: AgentInstanceId::new(instance),
            error: error.map(|(_, why)| why.to_string()),
        });
        let error = error.map(|(kind, message)| json!({"kind": kind, "message": message}));
        match parent.as_deref() {
            Some(OWNER) => {
                // Named by the program's number for the instance.
                let key = node.and_then(|node| node.child.as_ref()?.instance);
                self.to_owner(json!({"t": "oriented", "instance": key, "error": error}));
            }
            Some(parent) => {
                let line =
                    json!({"t": "oriented", "agent": parent, "id": instance, "error": error});
                drop(state);
                self.interpreter.tell(line);
            }
            None => {}
        }
    }

    pub(crate) fn closed(&self) -> bool {
        self.state().closed.is_some() || self.lifecycle.closed().is_some()
    }

    /// The session's token budget, once it is spent.
    pub(crate) fn spent_budget(&self) -> Option<u64> {
        self.ledger.budget().filter(|_| self.ledger.budget_spent())
    }

    // -------------------------------------------------------------- releasing

    /// Answer a release `releaser`'s code asked for: every child it names
    /// released, with everything below each, or none of them and why.
    async fn release(self: Arc<Self>, releaser: String, line: Value) {
        let ask = line["id"].clone();
        let ids: Vec<String> = line["children"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        // The program's instances' releases are recorded here, as they are made: a collection
        // first, and once released, `how`.
        let how = (releaser == OWNER).then(|| text(&line["how"]));
        if how.as_deref() == Some("collected") {
            self.owned_said(&ids, "agent.instance.collected", "collected");
        }
        let error = self.release_all(&releaser, &ids).await.err();
        if let Some(how) = how.filter(|_| error.is_none()) {
            self.owned_said(&ids, "agent.instance.released", &how);
        }
        self.answer(
            &releaser,
            json!({"t": "released", "agent": releaser, "id": ask, "error": error}),
        );
    }

    /// Record `kind` -- a collection, or a release `how` -- of each of root children `ids`'
    /// instances, as [`Inner::instance`] records what a kernel says of one.
    fn owned_said(&self, ids: &[String], kind: &str, how: &str) {
        for child in ids {
            let instance = self
                .state()
                .owned
                .get(child)
                .and_then(|owned| owned.instance);
            let line = json!({"instance": instance, "child": child, "how": how});
            self.instance(OWNER, kind, &line);
        }
    }

    async fn release_all(self: &Arc<Self>, releaser: &str, ids: &[String]) -> Result<(), String> {
        let subtree = {
            let mut state = self.state();
            let mut named = HashSet::new();
            for id in ids {
                if !named.insert(id) {
                    return Err(format!(
                        "child {id:?} was named more than once in this release"
                    ));
                }
                let held = state.agents.get(id).is_some_and(|node| {
                    node.parent.as_deref() == Some(releaser)
                        && node
                            .child
                            .as_ref()
                            .is_some_and(|c| !matches!(c.life, Life::Released | Life::Wedged))
                });
                if !held {
                    let live: Vec<&str> = state
                        .agents
                        .iter()
                        .filter(|(_, node)| {
                            node.parent.as_deref() == Some(releaser)
                                && node.child.as_ref().is_some_and(|c| {
                                    !matches!(c.life, Life::Released | Life::Wedged)
                                })
                        })
                        .map(|(id, _)| id.as_str())
                        .collect();
                    return Err(format!(
                        "{id:?} is not a child this agent launched and still holds; it holds {}; \
                         nothing was released",
                        if live.is_empty() {
                            "none".to_string()
                        } else {
                            live.join(", ")
                        }
                    ));
                }
            }
            // Each named child and everything below it, every child after its
            // parent, and whether it is still launching.
            let mut subtree = Vec::new();
            let mut next: Vec<String> = ids.to_vec();
            while let Some(id) = next.pop() {
                let Some(node) = state.agents.get_mut(&id) else {
                    continue;
                };
                let Some(child) = node.child.as_mut() else {
                    continue;
                };
                if matches!(child.life, Life::Released | Life::Wedged) {
                    continue;
                }
                let launching = child.life == Life::Launching;
                child.life = Life::Released;
                // Woken to see it, and not stopped: a round in flight is
                // given until its next model call to end (see below).
                child.wake.notify_one();
                let runner = child.runner.take();
                next.extend(node.children.iter().cloned());
                subtree.push((id, launching, runner));
            }
            subtree
        };
        // The deepest first, so each child's kernel ends after those below it.
        // One still launching may have no kernel yet: its launch ends it.
        let ended: Vec<String> = subtree.iter().map(|(id, ..)| id.clone()).collect();
        for (id, launching, runner) in subtree.into_iter().rev() {
            if launching {
                continue;
            }
            // Ending the kernel returns the call its round waits on, so the
            // round stops before its next model call and its runner returns.
            let (stopped, ()) =
                tokio::join!(self.interpreter.end(&id, WEDGE_WAIT), wind_down(runner));
            if stopped {
                self.gone(&id);
            } else {
                let mut state = self.state();
                if let Some(child) = state.child_mut(&id) {
                    child.life = Life::Wedged;
                }
                drop(state);
                self.events_of(&id).emit(Payload::AgentWedged {});
            }
        }
        let requests: Vec<AgentRequestId> = {
            let state = self.state();
            state
                .requests
                .iter()
                .filter(|(_, record)| {
                    ended.contains(&record.child)
                        && record.state == Req::Ended(RequestEnd::Released)
                })
                .map(|(id, _)| AgentRequestId::new(*id))
                .collect()
        };
        for id in &ended {
            self.tell_life(id);
        }
        self.events_of(releaser).emit(Payload::AgentReleased {
            released: ids.to_vec(),
            ended,
            requests,
        });
        Ok(())
    }

    /// `child`'s kernel is gone: it no longer counts against `children-max`.
    fn gone(&self, child: &str) {
        let mut state = self.state();
        let Some(node) = state.child_mut(child) else {
            return;
        };
        if !node.resident {
            return;
        }
        node.resident = false;
        if node.life == Life::Wedged {
            node.life = Life::Released;
        }
        drop(state);
        self.events_of(child).emit(Payload::AgentStopped {});
    }

    // -------------------------------------------------------------- closing

    /// Admission has closed: no child makes another model call, and each
    /// request open now is what the report accounts for. The interpreter ends
    /// every one of them with the closing error, as it hears of the close;
    /// one that died ends none, so the program on the host ends its own.
    fn close(&self, by: &ClosedBy) {
        let runners = {
            let mut state = self.state();
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(by.clone());
            let terminated = matches!(by, ClosedBy::InterpreterExited { .. });
            self.to_owner(json!({"t": "agent.closed", "terminated": terminated}));
            state.at_close = state
                .requests
                .iter()
                .filter(|(_, record)| !matches!(record.state, Req::Ended(_)))
                .map(|(id, _)| *id)
                .collect();
            state.runners()
        };
        for runner in runners {
            runner.abort();
        }
    }
}

/// Give a released child's runner [`RELEASE_WAIT`] to end its round and
/// return, and stop it if it has not. The round it was in is dropped then,
/// which is the one thing left to end a model call in flight; the stop is
/// awaited, so the drop's record precedes the release's.
async fn wind_down(runner: Option<JoinHandle<()>>) {
    let Some(mut runner) = runner else {
        return;
    };
    if tokio::time::timeout(RELEASE_WAIT, &mut runner)
        .await
        .is_err()
    {
        runner.abort();
        let _ = runner.await;
    }
}

/// How a request ended, as the interpreter names it.
fn outcome(name: &str) -> RequestEnd {
    match name {
        "accepted" => RequestEnd::Accepted,
        "rejected" => RequestEnd::Rejected,
        "failed" => RequestEnd::Failed,
        "cancelled" => RequestEnd::Cancelled,
        "released" => RequestEnd::Released,
        "budget" => RequestEnd::Budget,
        "closed" => RequestEnd::Closed,
        _ => RequestEnd::Terminated,
    }
}

/// A manifest the interpreter sent: each input's name, type and preview.
fn briefs(manifest: &Value) -> Vec<InputBrief> {
    manifest
        .as_array()
        .into_iter()
        .flatten()
        .map(|input| InputBrief {
            name: text(&input["name"]),
            type_name: text(&input["type"]),
            preview: text(&input["preview"]),
        })
        .collect()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// The registry a runner holds: dropped with the session, which ends it.
pub(crate) type Registry = Weak<Inner>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry holding one child `c1`, in `life`, with one request open on it.
    fn state_of(life: Life) -> State {
        let mut state = State::default();
        state.agents.insert(
            "c1".to_string(),
            Node {
                parent: Some("primary".to_string()),
                depth: 2,
                model: "sonnet".to_string(),
                slot: CallSlot::default(),
                children: Vec::new(),
                child: Some(ChildNode {
                    life,
                    resident: true,
                    runner: None,
                    abort: None,
                    wake: Arc::new(Notify::new()),
                    delivered: BTreeMap::new(),
                    round: None,
                    instance: None,
                }),
                invocations: Vec::new(),
            },
        );
        state.requests.insert(
            1,
            Record {
                child: "c1".to_string(),
                channel: Some("fizz".to_string()),
                state: Req::Unread,
                attempts: 0,
                rounds: Vec::new(),
                submission: None,
            },
        );
        state
    }

    /// A runner that read its child as live, and then lost the race to its
    /// release, does not make the child live again by opening a round: the
    /// child stays released, no round is recorded for it, and the request open
    /// on it does not span one. A live child's round opens as it always did.
    #[test]
    fn a_released_childs_round_does_not_open() {
        for life in [Life::Released, Life::Wedged] {
            let mut state = state_of(life);
            assert!(!state.open_round("c1", 3), "{life:?}");
            let child = state.child("c1").expect("the child");
            assert_eq!((child.life, child.round), (life, None));
            assert!(state.requests[&1].rounds.is_empty());
        }
        let mut state = state_of(Life::Idle);
        assert!(state.open_round("c1", 3));
        let child = state.child("c1").expect("the child");
        assert_eq!((child.life, child.round), (Life::Running, Some(3)));
        assert_eq!(state.requests[&1].rounds, [3]);
    }
}
