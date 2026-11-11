//! A root agent the session's owner constructs in a kernel of its own.
//!
//! A Rust owner of a session with no primary agent names a class its application code holds,
//! the arguments to construct it with -- data, and the session's facilities by their bound
//! names -- and a model, and holds a [`RootHandle`]: never the Python object, which lives in the
//! root's kernel as its `self`. The root's rounds open through its own runner -- the orienting
//! round as it launches, then one on each request -- and never through
//! [`Session::round`](super::Session::round).
//!
//! The owner speaks for its roots as a program on the host does for its instances
//! ([`Children::owner_line`]): it asks for a spawn, a release, and each request, as a kernel's
//! code would, and hears what becomes of each through [`Children::attach_owner`]. What differs
//! is that the spawn carries a construct record, which the root's kernel runs, and that the
//! kernel judges each reply against the declared type, as it does for a parent kernel, so the
//! owner hears the accepted reply as JSON.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use super::event::{AgentInstanceId, AgentRequestId};
use crate::agent::children::{Children, channel_texts};
use crate::agent::orientation::ChannelText;

/// How long [`RequestHandle::cancel`] waits for the root's kernel to say how
/// the request ended, as a program's cancel does.
const CANCEL_WAIT: Duration = Duration::from_secs(10);

/// What a root is constructed from: a class of the session's application code, the arguments
/// its constructor is called with, the arguments bound beside `self` in its kernel, and the
/// model it runs on.
///
/// Nothing of the owner's process crosses: the class is named, never sent; each argument is
/// JSON or the bound name of a facility the session gives; and the model is one the
/// session's configuration has.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstructSpec {
    pub(crate) class: String,
    pub(crate) model: String,
    pub(crate) args: Vec<(String, Argument)>,
    pub(crate) aliases: Vec<String>,
}

impl ConstructSpec {
    /// The class `class` names -- `module:qualname`, a subclass of `outrig.Agent` in the
    /// session's application code -- on `model`, a model or alias of the session's
    /// configuration. Its `[agents.<name>]` profiles and `default-agent` never apply.
    pub fn new(class: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            model: model.into(),
            args: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// Call the constructor with `argument` as its keyword argument `name`.
    pub fn arg(mut self, name: impl Into<String>, argument: Argument) -> Self {
        self.args.push((name.into(), argument));
        self
    }

    /// Bind argument `name` as a global of the root's kernel beside `self`: the object the
    /// constructor was given, so `task is self._task` holds there when `__init__` stored it
    /// unchanged. None is bound unless named.
    pub fn alias(mut self, name: impl Into<String>) -> Self {
        self.aliases.push(name.into());
        self
    }

    /// The class it names, `module:qualname`.
    pub fn class(&self) -> &str {
        &self.class
    }

    /// The construct record the root's kernel runs: each argument tagged by what it is.
    fn record(&self) -> Value {
        let args: Vec<Value> = self
            .args
            .iter()
            .map(|(name, argument)| match argument {
                // As text, which keeps a dict's order and an integer's size.
                Argument::Data { json, class } => {
                    json!({"name": name, "data": json.to_string(), "type": class})
                }
                Argument::Facility(facility) => json!({"name": name, "facility": facility.line()}),
                Argument::Facilities(facilities) => {
                    let map: serde_json::Map<String, Value> = facilities
                        .iter()
                        .map(|(key, facility)| (key.clone(), facility.line()))
                        .collect();
                    json!({"name": name, "facilities": map})
                }
            })
            .collect();
        json!({"class": self.class, "args": args, "aliases": self.aliases})
    }
}

/// One argument of a root's constructor.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Argument {
    /// Data, as JSON; with `class`, a `module:qualname` naming a dataclass of the application's,
    /// decoded strictly into it, and refused with the paths of what does not fit.
    Data {
        /// The value.
        json: Value,
        /// The dataclass it decodes into, or `None` for the JSON value as Python reads it.
        class: Option<String>,
    },
    /// A facility the session gives, which the kernel makes a reference to: `outrig.Facility`.
    Facility(Facility),
    /// A dict of facilities, by the keys the constructor reads them under: what an agent
    /// class's `facilities=` takes.
    Facilities(BTreeMap<String, Facility>),
}

impl Argument {
    /// `json`, as Python reads it.
    pub fn data(json: Value) -> Self {
        Self::Data { json, class: None }
    }

    /// `json`, decoded strictly into the dataclass `class` names (`module:qualname`).
    pub fn data_as(json: Value, class: impl Into<String>) -> Self {
        Self::Data {
            json,
            class: Some(class.into()),
        }
    }
}

/// A facility the session gives, by the name it was bound under. A name the session does not
/// give is refused as the root is constructed, naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Facility {
    /// A service [`SessionBuilder::serve`](super::SessionBuilder::serve) bound.
    Service(String),
    /// A binding the session's configuration or
    /// [`SessionBuilder::bind`](super::SessionBuilder::bind) declares.
    Binding(String),
}

impl Facility {
    /// The service bound as `name`.
    pub fn service(name: impl Into<String>) -> Self {
        Self::Service(name.into())
    }

    /// The binding named `name`.
    pub fn binding(name: impl Into<String>) -> Self {
        Self::Binding(name.into())
    }

    fn line(&self) -> Value {
        match self {
            Self::Service(name) => json!({"kind": "service", "name": name}),
            Self::Binding(name) => json!({"kind": "binding", "name": name}),
        }
    }
}

/// One of a root's request channels -- a method its class marks `@outrig.agent` -- as its
/// kernel declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestChannel {
    /// The method's name.
    pub name: String,
    /// The method's docstring, cleaned.
    pub doc: Option<String>,
    /// The request's type, as Python spells it.
    pub request_type: String,
    /// The reply's type, as Python spells it.
    pub reply_type: String,
    /// The request's JSON Schema, as text.
    pub request: String,
    /// The reply's JSON Schema, as text: what [`RequestHandle::outcome`]'s value fits.
    pub reply: String,
}

impl From<ChannelText> for RequestChannel {
    fn from(channel: ChannelText) -> Self {
        let ChannelText {
            name,
            doc,
            request_type,
            reply_type,
            request,
            reply,
        } = channel;
        Self {
            name,
            doc,
            request_type,
            reply_type,
            request,
            reply,
        }
    }
}

/// One thing wrong with a reply, at a JSON path of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Problem {
    /// Where, as `$.field[0]`.
    pub path: String,
    /// What.
    pub message: String,
}

/// Why a root is not ready, or why a request of it ended without a reply.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum RootError {
    /// The root was not constructed, and no model was called: `code` is `module-missing`,
    /// `import-failed`, `native-extension-unavailable`, `class-missing`,
    /// `not-an-agent-class`, `invalid-argument`, or `raised` for a constructor or
    /// `__outrig_attach__` that raised; `traceback` is the last 16 KiB of it, when there is one.
    #[error("the root was not constructed ({code}): {message}")]
    Construct {
        /// Why, as above.
        code: String,
        /// What was wrong: what was raised, or what was refused, naming it.
        message: String,
        /// The last 16 KiB of the traceback, or empty when there is none.
        traceback: String,
    },
    /// The model named is not one the configuration has.
    #[error("{0}")]
    Model(String),
    /// A limit refused it: `children-max`, or the root's `requests-max`.
    #[error("{0}")]
    Limit(String),
    /// The root was released first.
    #[error("{0}")]
    Released(String),
    /// The session closed first.
    #[error("{0}")]
    Closed(String),
    /// The session's interpreter ended first.
    #[error("{0}")]
    Terminated(String),
    /// The session's token budget was spent.
    #[error("{0}")]
    Budget(String),
    /// The root failed it on purpose, saying why; or, for readiness, its orienting round could
    /// not be had.
    #[error("{0}")]
    Failed(String),
    /// Its replies were refused past the attempt limit: the last one, and what was wrong.
    #[error("{message}")]
    Rejected {
        /// What the root's kernel said of it, the problems listed.
        message: String,
        /// The last reply, as JSON text, `null` when it had no JSON form.
        last: String,
        /// What was wrong with the last reply.
        problems: Vec<Problem>,
    },
    /// It was cancelled.
    #[error("{0}")]
    Cancelled(String),
    /// Anything else, as the engine said it.
    #[error("{0}")]
    Other(String),
}

impl RootError {
    /// The error the engine names `kind`, saying `message`.
    fn of(kind: &str, message: String) -> Self {
        match kind {
            "model" => Self::Model(message),
            "limit" => Self::Limit(message),
            "released" => Self::Released(message),
            "closed" => Self::Closed(message),
            "terminated" => Self::Terminated(message),
            "budget" => Self::Budget(message),
            "failed" => Self::Failed(message),
            "cancelled" => Self::Cancelled(message),
            _ => Self::Other(message),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Settle `once`, unless it has settled: a root's readiness, or a request's end.
fn settle<T>(once: &watch::Sender<Option<T>>, settled: T) {
    once.send_if_modified(|once| {
        if once.is_some() {
            return false;
        }
        *once = Some(settled);
        true
    });
}

/// What `once` settles as, once it does; `None` if what settles it was dropped first.
async fn settled<T: Clone>(once: &watch::Sender<Option<T>>) -> Option<T> {
    once.subscribe()
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|settled| settled.clone())
}

/// A root's readiness, until it settles.
type Ready = watch::Sender<Option<Result<(), RootError>>>;

/// The session's owner, as its roots' parent: what it numbers, and what it waits to hear.
pub(crate) struct Owner {
    children: Children,
    numbers: AtomicU64,
    state: Mutex<Heard>,
}

#[derive(Default)]
struct Heard {
    /// Each answer to a spawn or a release, by the line's number.
    answers: HashMap<u64, oneshot::Sender<Value>>,
    /// Each root's readiness until it settles, by the owner's number for it: there from before
    /// its spawn is asked for, since its orienting round may end before the answer is read.
    ready: HashMap<u64, Ready>,
    /// Each root launched and not yet released, by its instance id.
    roots: HashMap<AgentInstanceId, Arc<Root>>,
    /// Each request still open, by the owner's number for it.
    asks: HashMap<u64, Arc<Ask>>,
    /// Whether the session has closed.
    closed: bool,
}

impl Owner {
    /// The owner of the session whose children are `children`, which hears them from now on.
    pub(crate) fn attach(children: Children) -> Arc<Self> {
        let mut told = children.attach_owner();
        let owner = Arc::new(Self {
            children,
            numbers: AtomicU64::new(0),
            state: Mutex::default(),
        });
        // Weak, so the registry's channel, which the owner keeps open, ends with the owner.
        let weak = Arc::downgrade(&owner);
        tokio::spawn(async move {
            while let Some(line) = told.recv().await {
                match weak.upgrade() {
                    Some(owner) => owner.heard(line),
                    None => return,
                }
            }
        });
        owner
    }

    fn next(&self) -> u64 {
        self.numbers.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Say `line`, numbered `id`, and wait for the registry's answer to it. A waiter that stops
    /// waiting leaves the answer, whenever it comes, to [`Owner::unheld`].
    async fn asked(&self, id: u64, line: Value) -> Value {
        let (sender, answer) = oneshot::channel();
        lock(&self.state).answers.insert(id, sender);
        let mut awaited = Awaited {
            owner: self,
            answer,
        };
        self.children.owner_line(line);
        (&mut awaited.answer).await.unwrap_or(Value::Null)
    }

    /// Construct the root `spec` describes: the registry's answer, once its kernel has answered.
    pub(crate) async fn construct(self: &Arc<Self>, spec: &ConstructSpec) -> RootHandle {
        let number = self.next();
        let ready = Ready::new(None);
        lock(&self.state).ready.insert(number, ready.clone());
        let name: String = spec
            .class
            .rsplit(['.', ':'])
            .next()
            .unwrap_or_default()
            .chars()
            .take(48)
            .collect();
        let line = json!({
            "t": "agent.spawn", "id": number, "name": name, "kind": "request",
            "model": spec.model, "instance": {"id": number}, "construct": spec.record(),
        });
        let answer = self.asked(number, line).await;
        let root = Arc::new(Root {
            owner: Arc::downgrade(self),
            number,
            child: answer["child"].as_str().map(str::to_string),
            instance: answer["instance"].as_u64().map(AgentInstanceId::new),
            channels: channel_texts(&answer["channels"])
                .into_iter()
                .map(RequestChannel::from)
                .collect(),
            ready,
            releasing: AtomicBool::new(false),
            released: watch::Sender::new(None),
        });
        let mut heard = lock(&self.state);
        match root.instance {
            // The close settled its readiness already, if it came first.
            Some(instance) if !heard.closed => {
                heard.roots.insert(instance, Arc::clone(&root));
            }
            Some(_) => {}
            None => {
                heard.ready.remove(&number);
                drop(heard);
                let error = &answer["error"];
                let message = text(&error["message"]);
                settle(
                    &root.ready,
                    Err(match error["kind"].as_str() {
                        Some("construct") => RootError::Construct {
                            code: text(&error["failure"]["code"]),
                            message,
                            traceback: text(&error["failure"]["traceback"]),
                        },
                        Some(kind) => RootError::of(kind, message),
                        None => RootError::Closed(
                            "the session ended as the root was constructed".into(),
                        ),
                    }),
                );
            }
        }
        RootHandle(root)
    }

    /// The live root whose instance is `id`.
    pub(crate) fn root(&self, id: AgentInstanceId) -> Option<RootHandle> {
        let root = lock(&self.state).roots.get(&id).cloned();
        root.map(RootHandle)
    }

    /// One line the registry told the owner.
    fn heard(&self, line: Value) {
        let kind = line["t"].as_str().unwrap_or_default();
        if let Some(number) = line["ask"].as_u64() {
            let ask = lock(&self.state).asks.get(&number).cloned();
            match (ask, kind) {
                (Some(ask), "agent.request") => {
                    *lock(&ask.id) = line["request"].as_u64().map(AgentRequestId::new);
                }
                (Some(ask), "agent.ended") => self.ended(&ask, &line),
                _ => {}
            }
            return;
        }
        match kind {
            "spawned" | "released" => {
                let id = line["id"].as_u64();
                let waiter = id.and_then(|id| lock(&self.state).answers.remove(&id));
                if let Some(line) = waiter.and_then(|waiter| waiter.send(line).err()) {
                    self.unheld(line);
                }
            }
            "oriented" => {
                let ready = line["instance"]
                    .as_u64()
                    .and_then(|number| lock(&self.state).ready.remove(&number));
                if let Some(ready) = ready {
                    let error = &line["error"];
                    settle(
                        &ready,
                        match error.is_null() {
                            true => Ok(()),
                            false => Err(RootError::of(
                                error["kind"].as_str().unwrap_or_default(),
                                text(&error["message"]),
                            )),
                        },
                    );
                }
            }
            "agent.closed" => self.closed(line["terminated"].as_bool().unwrap_or(false)),
            _ => {}
        }
    }

    /// An answer whose waiter stopped waiting: a root launched for a construction nobody waits
    /// for any more is released, since nothing holds it and nothing could reach it, as with a
    /// spawn a kernel's code gave up on.
    fn unheld(&self, line: Value) {
        if line["t"] != "spawned" {
            return;
        }
        if let Some(id) = line["id"].as_u64() {
            lock(&self.state).ready.remove(&id);
        }
        if let Some(child) = line["child"].as_str() {
            self.children.owner_line(json!({
                "t": "agent.release", "id": self.next(), "children": [child], "how": "explicit",
            }));
        }
    }

    /// How the root's kernel ended request `ask`.
    fn ended(&self, ask: &Ask, line: &Value) {
        let outcome = line["outcome"].as_str().unwrap_or_default();
        let message = line["error"]
            .as_str()
            .map_or_else(|| format!("the request ended {outcome}"), str::to_string);
        let settled = match outcome {
            "accepted" => Ok(text(&line["json"])),
            "rejected" => {
                let last = &line["last"];
                Err(RootError::Rejected {
                    message,
                    last: text(&last["value"]),
                    problems: last["problems"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|problem| Problem {
                            path: text(&problem[0]),
                            message: text(&problem[1]),
                        })
                        .collect(),
                })
            }
            outcome => Err(RootError::of(outcome, message)),
        };
        // The owner's own cancel, and not the host's, which says why.
        let cancelled = outcome == "cancelled" && line["message"].is_null();
        self.end(ask, settled, cancelled);
    }

    /// End `ask` as `settled`, unless it has ended.
    fn end(&self, ask: &Ask, settled: Result<String, RootError>, cancelled: bool) {
        lock(&self.state).asks.remove(&ask.number);
        settle(&ask.outcome, (settled, cancelled));
    }

    /// The session closed, which released every root: each not yet ready never is, and, when
    /// the interpreter died first, each request still open ends here, since nothing else ends it.
    fn closed(&self, terminated: bool) {
        let (ready, asks) = {
            let mut heard = lock(&self.state);
            heard.closed = true;
            let asks: Vec<Arc<Ask>> = match terminated {
                true => heard.asks.values().cloned().collect(),
                false => Vec::new(),
            };
            (std::mem::take(&mut heard.ready), asks)
        };
        for ready in ready.into_values() {
            let message = "the session closed before the root was ready";
            settle(&ready, Err(RootError::Closed(message.into())));
        }
        for ask in asks {
            let message = "the session's interpreter ended before the request was answered";
            self.end(&ask, Err(RootError::Terminated(message.into())), false);
        }
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// The registry's answer to a line the owner said, as its waiter holds it. Dropped unread --
/// its waiter stopped waiting, the answer here already or still to come -- it is the owner's.
struct Awaited<'a> {
    owner: &'a Owner,
    answer: oneshot::Receiver<Value>,
}

impl Drop for Awaited<'_> {
    fn drop(&mut self) {
        // Closed first, so no answer lands between the look and the drop: one here already is
        // taken, and one still to come finds its waiter gone (`Owner::heard`).
        self.answer.close();
        if let Ok(line) = self.answer.try_recv() {
            self.owner.unheld(line);
        }
    }
}

/// What the owner holds of one root it constructed.
struct Root {
    owner: Weak<Owner>,
    number: u64,
    /// The root's agent id, or `None` when it was not launched.
    child: Option<String>,
    instance: Option<AgentInstanceId>,
    channels: Vec<RequestChannel>,
    ready: Ready,
    /// Whether its release has begun, and, once it has ended, that it has: the release is a task
    /// of its own, so a caller that stops waiting leaves it to finish.
    releasing: AtomicBool,
    released: watch::Sender<Option<()>>,
}

impl Root {
    /// Release the root, and record that it was: its readiness, if unsettled, settles
    /// [`RootError::Released`], and the owner holds it no more.
    async fn release(self: Arc<Self>) {
        if let (Some(owner), Some(child), Some(instance)) =
            (self.owner.upgrade(), &self.child, self.instance)
        {
            let id = owner.next();
            let line = json!({
                "t": "agent.release", "id": id, "children": [child], "how": "explicit",
            });
            owner.asked(id, line).await;
            let message = format!("root {child} was released");
            settle(&self.ready, Err(RootError::Released(message)));
            let mut heard = lock(&owner.state);
            heard.roots.remove(&instance);
            heard.ready.remove(&self.number);
        }
        settle(&self.released, ());
    }
}

/// What the session's owner holds of a root it constructed: its readiness, its requests and
/// its release -- never the Python object, which is the root's `self` in its own kernel.
///
/// Cheap to clone; every clone is the same root.
#[derive(Clone)]
pub struct RootHandle(Arc<Root>);

impl std::fmt::Debug for RootHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootHandle")
            .field("child", &self.0.child)
            .field("instance", &self.0.instance)
            .finish_non_exhaustive()
    }
}

impl RootHandle {
    /// The root's instance id, which its events carry; `None` for one that was not launched.
    pub fn id(&self) -> Option<AgentInstanceId> {
        self.0.instance
    }

    /// The root's agent id, which its events name as their subject; `None` for one that was not
    /// launched.
    pub fn agent(&self) -> Option<&str> {
        self.0.child.as_deref()
    }

    /// Return once the root's orienting round -- the one it has as it launches, to read its
    /// instructions and prepare in -- has ended. Fails with what its launch met instead: its
    /// construction ([`RootError::Construct`]), an unknown model, `children-max`, the
    /// session's close, its release, or a round it could not have. It settles once: asking
    /// again answers the same at once.
    pub async fn ready(&self) -> Result<(), RootError> {
        settled(&self.0.ready)
            .await
            .unwrap_or_else(|| Err(RootError::Closed("the session ended".into())))
    }

    /// End the root, and each request still open on it with [`RootError::Released`]; its
    /// readiness, if it had not settled, settles so too. Returns once it has ended; a second
    /// call, once the first has. The release runs to its end whether or not anything waits on
    /// it: a call dropped part way leaves it to finish, and a later call waits for it. A root
    /// that was not launched, or that the session's close released, has nothing to end.
    pub async fn release(&self) {
        if !self.0.releasing.swap(true, Ordering::SeqCst) {
            tokio::spawn(Arc::clone(&self.0).release());
        }
        let _ = settled(&self.0.released).await;
    }

    /// The root's request channels, as its class declares them: empty for a class with none.
    pub fn channels(&self) -> &[RequestChannel] {
        &self.0.channels
    }

    /// Send `body` on the root's channel `channel` as one request, and return its handle at
    /// once. The body is the request as JSON, decoded strictly in the root's kernel into the
    /// channel's request type; a body that does not decode ends the request
    /// [`RootError::Failed`]. Refused, with nothing sent, on a channel the class does not
    /// declare, for a root not launched or released, once the session has closed, and past the
    /// session's `requests-max` open on the root.
    pub fn request(&self, channel: &str, body: Value) -> Result<RequestHandle, RootError> {
        let root = &self.0;
        let (Some(owner), Some(child), Some(instance)) =
            (root.owner.upgrade(), &root.child, root.instance)
        else {
            return Err(RootError::Other(
                "the root was not launched, so it takes no request".into(),
            ));
        };
        if !root
            .channels
            .iter()
            .any(|declared| declared.name == channel)
        {
            let names: Vec<&str> = root.channels.iter().map(|c| c.name.as_str()).collect();
            return Err(RootError::Other(format!(
                "root {child} has no request channel {channel:?}; it has {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            )));
        }
        let requests_max = owner.children.requests_max() as usize;
        let ask = Arc::new(Ask {
            number: owner.next(),
            root: root.number,
            id: Mutex::default(),
            outcome: watch::Sender::new(None),
        });
        {
            let mut heard = lock(&owner.state);
            if heard.closed {
                return Err(RootError::Closed(
                    "the session is closing, so the request was not sent".into(),
                ));
            }
            if !heard.roots.contains_key(&instance) {
                return Err(RootError::Released(format!(
                    "root {child} was released, so it takes no request"
                )));
            }
            let open = heard.asks.values().filter(|ask| ask.root == root.number);
            if open.count() >= requests_max {
                return Err(RootError::Limit(format!(
                    "root {child} already holds {requests_max} unsettled requests, its \
                     requests-max, so nothing was sent; cancel one, or wait for an answer"
                )));
            }
            heard.asks.insert(ask.number, Arc::clone(&ask));
        }
        owner.children.owner_line(json!({
            "t": "ask", "agent": child, "ask": ask.number, "channel": channel,
            "body": body.to_string(),
        }));
        Ok(RequestHandle {
            owner: Arc::downgrade(&owner),
            child: child.clone(),
            ask,
        })
    }
}

/// One request the owner made of a root.
struct Ask {
    number: u64,
    /// The owner's number for the root it was made of.
    root: u64,
    /// The session's id for it, once the root's kernel has it.
    id: Mutex<Option<AgentRequestId>>,
    /// How it ended, and whether the owner's cancel ended it.
    outcome: watch::Sender<Option<(Result<String, RootError>, bool)>>,
}

/// A request the owner made of a root: one terminal outcome, which is the reply as JSON text,
/// fit to the channel's reply schema, or why there is none.
#[derive(Clone)]
pub struct RequestHandle {
    owner: Weak<Owner>,
    child: String,
    ask: Arc<Ask>,
}

impl std::fmt::Debug for RequestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestHandle")
            .field("child", &self.child)
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl RequestHandle {
    /// The session's id for the request, which its events carry; `None` until the root's kernel
    /// has it.
    pub fn id(&self) -> Option<AgentRequestId> {
        *lock(&self.ask.id)
    }

    /// Wait for the request to end: the accepted reply, as JSON text the root's kernel checked
    /// against the channel's reply type, or why it ended without one. Text, so an integer
    /// keeps its size however large: deserialize it with `serde_json::from_str` into a type of
    /// your own. A reply that does not fit is sent back to the root with what is wrong, up to
    /// its attempts; one past them ends [`RootError::Rejected`]. Dropping the wait cancels
    /// nothing.
    pub async fn outcome(&self) -> Result<String, RootError> {
        match settled(&self.ask.outcome).await {
            Some((settled, _)) => settled,
            None => Err(RootError::Closed("the session ended".into())),
        }
    }

    /// Cancel the request, and return whether that ended it: one the root has not taken never
    /// reaches it, and one it took ends so that its answer raises. The root's kernel may have
    /// ended it otherwise first, which this waits up to ten seconds to hear.
    pub async fn cancel(&self) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        if self.ask.outcome.borrow().is_some() {
            return false;
        }
        owner.children.owner_line(json!({
            "t": "withdraw", "agent": self.child, "ask": self.ask.number,
        }));
        let ended = tokio::time::timeout(CANCEL_WAIT, settled(&self.ask.outcome)).await;
        matches!(ended, Ok(Some((_, true))))
    }
}
