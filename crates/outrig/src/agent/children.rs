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
//! spent token budget -- and how a release ends a child and everything below
//! it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;

use super::channel::Announcer;
use super::history::Window;
use super::ledger::{CallSlot, Ledger};
use super::orientation::{self, ChannelText, ChildBrief};
use super::resolve::{self, LlmResolveError};
use super::{Agent, AgentError, runner};
use crate::config::{Config, Limits, Secrets};
use crate::events::Events;
use crate::harness::bindings::Brief;
use crate::harness::event::{
    AgentRequestId, ChannelBrief, ChildKind, InputBrief, Payload, RequestEnd, Subject,
};
use crate::harness::lifecycle::Lifecycle;
use crate::harness::{ClosedBy, RequestOutcome};
use crate::python::host::{Channels, ChildLines, Counts, Interpreter};

/// How long a released child's kernel has to stop before it is taken for
/// wedged: its code never yields, and its thread runs on.
const WEDGE_WAIT: Duration = Duration::from_secs(2);

/// How long the interpreter has to give a work child its next submission. It
/// does so on its reader thread, so only an interpreter nothing can run in is
/// slower.
const ASSIGN_WAIT: Duration = Duration::from_secs(10);

/// What a session hands its children's registry: what they are resolved
/// against, the limits they run under, and what their instructions say.
pub(crate) struct Settings {
    pub(crate) config: Arc<Config>,
    pub(crate) secrets: Arc<dyn Secrets>,
    /// The session's agent, whose `[agents.<name>]` limits apply.
    pub(crate) agent: Option<String>,
    pub(crate) limits: Limits,
    pub(crate) workspace: Option<PathBuf>,
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
}

struct ChildNode {
    life: Life,
    /// Whether it counts against `children-max`: from its launch until its
    /// kernel is gone.
    resident: bool,
    runner: Option<JoinHandle<()>>,
    wake: Arc<Notify>,
    /// Every request ever delivered on each of its channels.
    delivered: BTreeMap<String, u64>,
    /// The round running now, if one is.
    round: Option<u32>,
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
        });
        let weak = Arc::downgrade(&inner);
        let lines: ChildLines = Arc::new(move |agent: &str, line: Value| {
            if let Some(inner) = weak.upgrade() {
                inner.line(agent, line);
            }
        });
        inner.interpreter.attach_children(lines);
        let weak = Arc::downgrade(&inner);
        lifecycle.on_close(Arc::new(move |by: &ClosedBy| {
            if let Some(inner) = weak.upgrade() {
                inner.close(by);
            }
        }));
        Self(inner)
    }

    /// The primary agent is the tree's root, at depth 1, running on `model`
    /// as named and spending through `slot`.
    pub(crate) fn root(&self, agent: &str, model: &str, slot: CallSlot) {
        lock(&self.0.state).agents.insert(
            agent.to_string(),
            Node {
                parent: None,
                depth: 1,
                model: model.to_string(),
                slot,
                children: Vec::new(),
                child: None,
            },
        );
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
        let runners: Vec<JoinHandle<()>> = lock(&self.0.state)
            .agents
            .values_mut()
            .filter_map(|node| node.child.as_mut()?.runner.take())
            .collect();
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
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Where `agent`'s events go.
    fn events_of(&self, agent: &str) -> Events {
        self.interpreter.events().for_agent(agent)
    }

    /// One line the interpreter sent about children, from `agent`'s kernel.
    fn line(self: &Arc<Self>, agent: &str, line: Value) {
        let kind = line["t"].as_str().unwrap_or_default().to_string();
        match kind.as_str() {
            "agent.spawn" => {
                let inner = Arc::clone(self);
                let agent = agent.to_string();
                tokio::spawn(async move { inner.spawn(agent, line).await });
            }
            "agent.release" => {
                let inner = Arc::clone(self);
                let agent = agent.to_string();
                tokio::spawn(async move { inner.release(agent, line).await });
            }
            "agent.request" => self.requested(agent, &line),
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
            _ => tracing::warn!("ignored a {kind} line from agent {agent:?}"),
        }
    }

    // -------------------------------------------------------------- launching

    /// Answer a spawn `parent`'s code asked for: a child launched, or why not.
    async fn spawn(self: Arc<Self>, parent: String, line: Value) {
        let ask = line["id"].clone();
        let answer = match self.launch(&parent, &line).await {
            Ok(child) => json!({
                "t": "spawned", "agent": parent, "id": ask, "child": child,
                "requests_max": self.settings.limits.requests_max,
            }),
            Err(refused) => json!({
                "t": "spawned", "agent": parent, "id": ask,
                "error": {"kind": refused.kind, "message": refused.message},
            }),
        };
        self.interpreter.tell(answer);
    }

    /// Launch the child `line` asks for under `parent`, and return its id. The
    /// session's admission, the depth and `children-max` decide before
    /// anything is made, and the place it takes is its own from then on;
    /// anything that fails after gives it back, and leaves no child.
    async fn launch(self: &Arc<Self>, parent: &str, line: &Value) -> Result<String, Refused> {
        let limits = self.settings.limits;
        let kind = match line["kind"].as_str() {
            Some("request") => ChildKind::Request,
            _ => ChildKind::Work,
        };
        let name = line["name"].as_str().unwrap_or_default().to_string();
        let (id, model, depth, lineage) = {
            let mut state = self.state();
            if let Some(by) = state.closed.clone().or_else(|| self.lifecycle.closed()) {
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
                        wake: Arc::new(Notify::new()),
                        delivered: BTreeMap::new(),
                        round: None,
                    }),
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
            Ok(()) => Ok(id),
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
    ) -> Result<(), Refused> {
        let settings = &self.settings;
        let mut resolved = resolve::resolve_agent(
            &settings.config,
            settings.agent.as_deref(),
            Some(model),
            &*settings.secrets,
        )
        .map_err(|e| match e {
            AgentError::Resolve(LlmResolveError::UnknownModel { .. }) => {
                Refused::new("model", e.to_string())
            }
            e => Refused::new("other", e.to_string()),
        })?;
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
        handle
            .open_child(
                id,
                json!({
                    "kind": if kind == ChildKind::Request { "request" } else { "work" },
                    "spawn": line["id"],
                    "requests_max": settings.limits.requests_max,
                }),
            )
            .await
            .map_err(|e| Refused::new("other", e.to_string()))?;
        let events = handle.events().clone();
        events.emit(Payload::AgentSpawned {
            name: name.to_string(),
            parent: Subject::agent(parent).to_string(),
            kind,
            model: model.to_string(),
            depth,
            inputs: inputs.clone(),
            channels: channels
                .iter()
                .map(|channel| ChannelBrief {
                    name: channel.name.clone(),
                    request: channel.request_type.clone(),
                    reply: channel.reply_type.clone(),
                })
                .collect(),
        });
        let brief = match kind {
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
        let agent = match Agent::build_child(
            &resolved,
            handle,
            &settings.container,
            preamble,
            slot.clone(),
            announcer,
            Arc::clone(&self.lifecycle),
        ) {
            Ok(agent) => agent,
            Err(e) => {
                let _ = self.interpreter.end(id, WEDGE_WAIT).await;
                return Err(Refused::new("other", e.to_string()));
            }
        };
        let started = {
            let mut state = self.state();
            let Some(node) = state.agents.get_mut(id) else {
                return Err(Refused::new("other", "the child was ended as it launched"));
            };
            node.slot = slot;
            let Some(child) = node.child.as_mut() else {
                return Err(Refused::new("other", "the child was ended as it launched"));
            };
            // Its parent may have been released while this launched it,
            // leaving it to this to end.
            let started = child.life != Life::Released;
            if started {
                child.life = Life::Idle;
                child.runner = Some(tokio::spawn(runner::run(
                    Arc::downgrade(self),
                    id.to_string(),
                    kind,
                    agent,
                    Arc::clone(&child.wake),
                    self.ledger.budget_watch(),
                )));
            }
            started
        };
        if !started {
            self.interpreter.end(id, WEDGE_WAIT).await;
            return Err(Refused::new(
                "released",
                "the agent launching it was released",
            ));
        }
        Ok(())
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
        let state = self.state();
        let mut summary = Summary::default();
        for (id, record) in state.requests.iter().filter(|(_, r)| r.child == child) {
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
    /// it, and so does one made while it runs.
    pub(crate) fn round_opened(&self, child: &str, round: u32) {
        let mut state = self.state();
        if let Some(node) = state.child_mut(child) {
            node.round = Some(round);
            node.life = Life::Running;
        }
        for record in state
            .requests
            .values_mut()
            .filter(|record| record.child == child && !matches!(record.state, Req::Ended(_)))
        {
            // A round that committed nothing leaves its number to the next.
            if record.rounds.last() != Some(&round) {
                record.rounds.push(round);
            }
        }
        drop(state);
        self.tell_life(child);
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
        let state = self.state();
        let Some(node) = state.agents.get(child) else {
            return;
        };
        let (Some(parent), Some(life)) = (&node.parent, node.child.as_ref().map(|c| c.life)) else {
            return;
        };
        let parent_held = state
            .agents
            .get(parent)
            .is_some_and(|node| node.child.as_ref().is_none_or(|c| c.resident));
        if parent_held {
            self.interpreter.tell(json!({
                "t": "child", "agent": parent, "id": 0, "child": child, "state": life.word(),
            }));
        }
    }

    /// End request `id`, open on `child`, `outcome` -- `rejected`, `budget`
    /// or `failed` -- for `message`.
    pub(crate) fn settle(&self, child: &str, id: u64, outcome: &str, message: &str) {
        self.interpreter.tell(json!({
            "t": "settle", "agent": child, "id": id, "outcome": outcome, "message": message,
        }));
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
        let error = self.release_all(&releaser, &ids).await.err();
        self.interpreter.tell(json!({
            "t": "released", "agent": releaser, "id": ask, "error": error,
        }));
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
                if let Some(runner) = child.runner.take() {
                    runner.abort();
                }
                next.extend(node.children.iter().cloned());
                subtree.push((id, launching));
            }
            subtree
        };
        // The deepest first, so each child's kernel ends after those below it.
        // One still launching may have no kernel yet: its launch ends it.
        for (id, _) in subtree.iter().rev().filter(|(_, launching)| !launching) {
            if self.interpreter.end(id, WEDGE_WAIT).await {
                self.gone(id);
            } else {
                let mut state = self.state();
                if let Some(child) = state.child_mut(id) {
                    child.life = Life::Wedged;
                }
                drop(state);
                self.events_of(id).emit(Payload::AgentWedged {});
            }
        }
        let ended: Vec<String> = subtree.into_iter().map(|(id, _)| id).collect();
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
    /// one that died ends none.
    fn close(&self, by: &ClosedBy) {
        let runners: Vec<JoinHandle<()>> = {
            let mut state = self.state();
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(by.clone());
            state.at_close = state
                .requests
                .iter()
                .filter(|(_, record)| !matches!(record.state, Req::Ended(_)))
                .map(|(id, _)| *id)
                .collect();
            state
                .agents
                .values_mut()
                .filter_map(|node| node.child.as_mut()?.runner.take())
                .collect()
        };
        for runner in runners {
            runner.abort();
        }
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
