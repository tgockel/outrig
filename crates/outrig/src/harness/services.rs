//! Services: Rust objects an owner binds by name, whose methods agent Python
//! calls through an object generated from their description.
//!
//! A [`Service`] describes its methods -- each by an exact name, with a JSON
//! Schema for its arguments -- and answers a call of one by value: JSON in,
//! JSON out. [`SessionBuilder::serve`](super::SessionBuilder::serve) binds one
//! for the session's life. Every kernel gets an object of a class the
//! interpreter generates from the description, one method per described one,
//! and a call on it crosses the interpreter's pipe as a line of JSON: no
//! process of its own, no proxy, nothing of the service's in the container.
//!
//! The table here holds each service, its current description and the calls in
//! flight. A call is admitted, checked against its method's schema, dispatched
//! to [`Service::call`] on a task of its own with a [`CallContext`], held to a
//! deadline, told to stop through its cancellation token -- at the deadline,
//! when its caller is interrupted, or when the session's drain ends -- and
//! aborted if it has not ended a grace later. Every call is evented as a
//! hosted request is (`service.call.*`), and the report lists each whose
//! outcome came after the close.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinError};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::event::{
    ExecId, InvocationId, Payload, ServiceCallId, ServiceChanges, ServiceKind, ServiceOutcome,
    ServiceReason,
};
use super::lifecycle::Lifecycle;
use super::report::ServiceCall;
use crate::events::Events;
use crate::python::host::Interpreter;
use crate::python::hosted::ToInterpreter;

/// What [`Service::call`] returns: the call's result, a JSON value, or what
/// went wrong.
pub type ServiceFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ServiceError>> + Send + 'a>>;

/// A Rust object agent Python calls by name, bound with
/// [`SessionBuilder::serve`](super::SessionBuilder::serve).
///
/// Binding it is the grant: every method it describes is callable by every
/// agent of the session, each call recorded as events. A service that should do
/// less describes less, and resolves what it acts on from its own
/// configuration rather than trusting an argument to name it. The arguments a
/// call reaches it with have been checked against the method's `params`
/// schema; nothing else about them has.
///
/// A call should return promptly. One that starts long work returns a ticket at
/// once, as a value, and the owner delivers the completion on a channel the
/// agent waits on: a call still running holds its caller's thread, and at the
/// session's close it holds the drain.
pub trait Service: Send + Sync + 'static {
    /// What the service offers: called once as the session starts.
    fn describe(&self) -> ServiceDescription;

    /// Run `method`, one this service described, with `args`, a JSON object
    /// its `params` schema accepted.
    ///
    /// `cx` says whose call it is and until when it may run. When
    /// [`CallContext::cancelled`] resolves -- the deadline passed, the caller
    /// was interrupted, or the session's drain ended -- return promptly, with [`ServiceErrorCategory::Cancelled`] if the work was not
    /// done: a future still running a grace later is dropped at its next await
    /// point, and its call reported `unknown`. Work on a blocking thread is not
    /// stopped by that; bounding it is the service's.
    fn call(&self, method: &str, args: Value, cx: CallContext) -> ServiceFuture<'_>;
}

/// What a service offers: a line saying what it is, and its methods.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ServiceDescription {
    /// What the service is, in a line: what agents are told of it.
    pub description: String,
    /// Its methods, in the order agents are shown them. A name given twice
    /// is the first one's.
    pub methods: Vec<MethodDescription>,
    /// The longest any call of it may run, in place of the session's
    /// [`ServicePolicy::call_deadline`]: for a service whose calls
    /// legitimately run long.
    pub call_deadline: Option<Duration>,
    /// What kind of service it is, which decides the class agents get.
    pub(crate) kind: ServiceKind,
}

impl ServiceDescription {
    /// A service described by `description`, with no methods yet.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            methods: Vec::new(),
            call_deadline: None,
            kind: ServiceKind::Rust,
        }
    }

    /// The description, with `method` added after the others.
    #[must_use]
    pub fn with_method(mut self, method: MethodDescription) -> Self {
        self.methods.push(method);
        self
    }

    /// The description, with `deadline` as its calls' own ceiling.
    #[must_use]
    pub fn with_call_deadline(mut self, deadline: Duration) -> Self {
        self.call_deadline = Some(deadline);
        self
    }
}

/// One method a service offers.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MethodDescription {
    /// Its exact name, any string: agents reach every method by it, and an
    /// attribute stands for it where the name can be one.
    pub name: String,
    /// What it does, for the model reading its docstring.
    pub description: String,
    /// The JSON Schema its arguments object must match, which also gives the
    /// generated method its signature. One that does not compile leaves the
    /// method's arguments unchecked, and says so in `service.bound`.
    pub params: Value,
    /// The JSON Schema of its result, for its docstring; nothing checks it.
    pub result: Option<Value>,
    /// What a presenter of the service's kind may read; the generic one
    /// ignores it.
    pub extras: Map<String, Value>,
}

impl MethodDescription {
    /// A method named `name`, doing what `description` says, whose arguments
    /// match `params`.
    pub fn new(name: impl Into<String>, description: impl Into<String>, params: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            params,
            result: None,
            extras: Map::new(),
        }
    }

    /// The method, with `schema` as its result's.
    #[must_use]
    pub fn with_result(mut self, schema: Value) -> Self {
        self.result = Some(schema);
        self
    }
}

/// What a call is: whose, which, and until when.
#[derive(Debug, Clone)]
pub struct CallContext {
    call: ServiceCallId,
    service: Arc<str>,
    agent: Arc<str>,
    execution: Option<ExecId>,
    invocation: Option<InvocationId>,
    deadline: std::time::Instant,
    cancellation: CancellationToken,
}

impl CallContext {
    /// The call's id, which every event about it carries.
    pub fn call(&self) -> ServiceCallId {
        self.call
    }

    /// The name the service is bound under.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// The agent whose code made the call, as the interpreter's protocol
    /// names it: `primary`, or a child's id.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// The execution that made the call, as the interpreter said: diagnostic
    /// context, which agent code can change.
    pub fn execution(&self) -> Option<ExecId> {
        self.execution
    }

    /// The skill invocation the call was made inside, as the interpreter said.
    pub fn invocation(&self) -> Option<InvocationId> {
        self.invocation
    }

    /// When the call's deadline passes, and its token is triggered.
    pub fn deadline(&self) -> std::time::Instant {
        self.deadline
    }

    /// Resolves once the call should stop: at its deadline, when its caller
    /// is interrupted, or when the session's drain ends. A `call` selects on
    /// it, and returns [`ServiceErrorCategory::Cancelled`] if its work was not
    /// done; work on a task of its own clones the context and awaits it there.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        self.cancellation.cancelled()
    }

    /// Whether the call should stop, as [`CallContext::cancelled`] says.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// What went wrong in a call, as the service says: agent code gets the
/// category and the message, as written.
///
/// The message is the service's: OutRig forwards it and adds nothing, so a
/// token or a connection detail put in it is put in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServiceError {
    /// What kind of failure it is.
    pub category: ServiceErrorCategory,
    /// What went wrong, for the agent reading it.
    pub message: String,
}

impl ServiceError {
    /// A failure of `category`, saying `message`.
    pub fn new(category: ServiceErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.category.as_str(), self.message)
    }
}

impl std::error::Error for ServiceError {}

/// What kind of failure a [`ServiceError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceErrorCategory {
    /// The arguments were wrong in a way the schema could not say.
    InvalidArguments,
    /// What the call named does not exist.
    NotFound,
    /// The service does not permit it.
    Refused,
    /// Something the service needs is not available now.
    Unavailable,
    /// The work was attempted and failed.
    Failed,
    /// The call was told to stop, and did.
    Cancelled,
    /// Anything else.
    Other,
}

impl ServiceErrorCategory {
    /// Its name, as agent code and the events see it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Other => "other",
        }
    }
}

/// How a session's services are called: their default deadline, the grace a
/// call told to stop is given, the largest result that crosses, and whether
/// the events carry previews of arguments and results.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServicePolicy {
    /// The longest a call may run, unless its service's description sets its
    /// own: ten minutes, as an MCP call's. A call's `timeout=` may lower it
    /// and never raise it.
    pub call_deadline: Duration,
    /// How long a call told to stop has to end before it is aborted and
    /// reported `unknown`: five seconds.
    pub cancel_grace: Duration,
    /// The largest result, in bytes of JSON, that reaches the caller: 16 MiB.
    /// A larger one is refused, its caller told how large it was.
    pub result_max: usize,
    /// Whether `service.call.received` and `service.call.ended` carry
    /// previews of a call's arguments and result, each cut to 1,024
    /// characters: off, since arguments can carry what an event should not.
    pub record_bodies: bool,
}

impl Default for ServicePolicy {
    fn default() -> Self {
        Self {
            call_deadline: Duration::from_secs(crate::config::DEFAULT_MCP_CALL_TIMEOUT_SECS),
            cancel_grace: Duration::from_secs(5),
            result_max: 16 << 20,
            record_bodies: false,
        }
    }
}

/// Refuse a service name the session cannot give: one a binding could not take
/// -- not a Python identifier, a keyword, `runtime`, `asyncio`, `outrig` -- or
/// one a binding `declared`, or another service, takes.
pub(crate) fn check_names(
    declared: &[super::bindings::Declared],
    served: &[(String, Arc<dyn Service>)],
) -> crate::error::Result<()> {
    for (index, (name, _)) in served.iter().enumerate() {
        let refused = |why: String| {
            crate::error::OutrigError::Configuration(format!(
                "the service `{name}` given through `serve` {why}: a service's name is a global in \
                 every kernel, so it names one service"
            ))
        };
        crate::config::check_binding_name(name, None).map_err(|e| {
            refused(format!(
                "cannot take that name, as a binding could not: {e}"
            ))
        })?;
        if declared.iter().any(|binding| binding.name == *name) {
            return Err(refused("is a binding's name already".into()));
        }
        if served[..index].iter().any(|(earlier, _)| earlier == name) {
            return Err(refused("was given through `serve` already".into()));
        }
    }
    Ok(())
}

/// The longest a preview in an event is, in characters, as a child's inputs'.
const PREVIEW_MAX: usize = 1024;

/// What the orientation says of a service: its name and its description.
#[derive(Debug, Clone)]
pub(crate) struct ServiceBrief {
    pub(crate) name: String,
    pub(crate) description: String,
}

/// A session's services and their calls. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Services(Arc<Table>);

struct Table {
    events: Events,
    lifecycle: Arc<Lifecycle>,
    policy: ServicePolicy,
    /// The interpreter's writer, held weakly.
    to_interpreter: ToInterpreter,
    /// The last call id given.
    ids: AtomicU64,
    state: watch::Sender<State>,
}

#[derive(Default)]
struct State {
    /// Each service, in the order the owner bound them.
    served: Vec<Served>,
    /// Each call not yet ended.
    flights: HashMap<ServiceCallId, Flight>,
    /// Set once the report is read: nothing more is published.
    sealed: bool,
    after_close: Vec<ServiceCall>,
}

struct Served {
    name: String,
    service: Arc<dyn Service>,
    current: Arc<Described>,
}

/// One generation of a service's description, its schemas compiled.
struct Described {
    generation: u64,
    description: ServiceDescription,
    /// Each method's compiled schema by exact name, or `None` for one that
    /// did not compile and is called unchecked.
    checks: HashMap<String, Option<jsonschema::Validator>>,
    unchecked: Vec<String>,
}

/// A call not yet ended.
struct Flight {
    /// Why it is to stop, once something has said; the first word holds.
    /// Shared, so it is told with the table unlocked: its run reads it and then
    /// locks the table, and the two are never taken the other way round.
    stop: Arc<watch::Sender<Option<ServiceReason>>>,
    /// Its service's task, once it is dispatched.
    abort: Option<AbortHandle>,
    agent: Arc<str>,
    /// The id the interpreter matches its answer by, which `service.cancel`
    /// names.
    ask: u64,
    service: String,
    method: String,
}

impl Flight {
    /// The report's row for the call `call`, ended as said.
    fn row(
        self,
        call: ServiceCallId,
        outcome: ServiceOutcome,
        category: Option<ServiceErrorCategory>,
        reason: Option<ServiceReason>,
    ) -> ServiceCall {
        ServiceCall {
            call,
            service: self.service,
            method: self.method,
            agent: self.agent.to_string(),
            outcome,
            category,
            reason,
        }
    }
}

/// How a call ended, as its event and its caller are told.
struct End {
    outcome: ServiceOutcome,
    category: Option<ServiceErrorCategory>,
    reason: Option<ServiceReason>,
    message: String,
    path: Option<String>,
    detail: Option<String>,
    bytes: Option<u64>,
    /// The result, as JSON text.
    result: Option<String>,
}

impl End {
    fn new(outcome: ServiceOutcome, message: String) -> Self {
        Self {
            outcome,
            category: None,
            reason: None,
            message,
            path: None,
            detail: None,
            bytes: None,
            result: None,
        }
    }

    fn refused(reason: ServiceReason, message: String) -> Self {
        Self::stopped(ServiceOutcome::Refused, reason, message)
    }

    fn stopped(outcome: ServiceOutcome, reason: ServiceReason, message: String) -> Self {
        Self {
            reason: Some(reason),
            ..Self::new(outcome, message)
        }
    }

    fn raised(category: ServiceErrorCategory, message: String) -> Self {
        Self {
            category: Some(category),
            ..Self::new(ServiceOutcome::Raised, message)
        }
    }
}

/// What a `service.call` line asks, as the interpreter said it.
struct Request {
    service: String,
    method: String,
    generation: u64,
    execution: Option<ExecId>,
    invocation: Option<InvocationId>,
    timeout: Option<f64>,
}

impl Request {
    fn read(line: &Value) -> Self {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        Self {
            service: text(&line["service"]),
            method: text(&line["method"]),
            generation: line["generation"].as_u64().unwrap_or(0),
            execution: line["exec"].as_u64().map(ExecId::new),
            invocation: line["invocation"].as_u64().map(InvocationId::new),
            timeout: line["timeout"].as_f64(),
        }
    }
}

impl Services {
    /// Serve `given` to `interpreter`, each under its name, called under
    /// `policy`: describe each, publish `service.bound`, tell the interpreter
    /// before any kernel opens, and answer each call it sends. Nothing is told
    /// or attached for a session with none.
    pub(crate) fn serve(
        given: Vec<(String, Arc<dyn Service>)>,
        policy: ServicePolicy,
        interpreter: &Interpreter,
        lifecycle: &Arc<Lifecycle>,
    ) -> Self {
        let events = lifecycle.events().clone();
        let served: Vec<Served> = given
            .into_iter()
            .map(|(name, service)| {
                let current = Arc::new(Described::new(service.describe(), 1));
                events.emit(Payload::ServiceBound {
                    service: name.clone(),
                    kind: current.description.kind,
                    methods: current.description.methods.len(),
                    generation: 1,
                    unchecked: current.unchecked.clone(),
                });
                Served {
                    name,
                    service,
                    current,
                }
            })
            .collect();
        let any = !served.is_empty();
        let table = Arc::new(Table {
            events,
            lifecycle: Arc::clone(lifecycle),
            to_interpreter: interpreter.relay_line(),
            ids: AtomicU64::new(0),
            state: watch::Sender::new(State {
                served,
                ..State::default()
            }),
            policy,
        });
        if any {
            let services: Vec<Value> = table
                .state
                .borrow()
                .served
                .iter()
                .map(|served| table.entry(served))
                .collect();
            (table.to_interpreter)(json!({"t": "services", "services": services}));
            // Strong: the interpreter's reader holds the table for as long as it can send a
            // call, so every call it sends is answered.
            let hook = Arc::clone(&table);
            interpreter.attach_services(Arc::new(move |line: Value| hook.line(line)));
        }
        Self(table)
    }

    /// What the orientation lists: each service by name and description, in
    /// the order they were bound.
    pub(crate) fn briefs(&self) -> Vec<ServiceBrief> {
        self.0
            .state
            .borrow()
            .served
            .iter()
            .map(|served| ServiceBrief {
                name: served.name.clone(),
                description: served.current.description.description.clone(),
            })
            .collect()
    }

    /// How long a call told to stop has before it is aborted.
    pub(crate) fn grace(&self) -> Duration {
        self.0.policy.cancel_grace
    }

    /// Ask `name`'s service for its description again and, when it changed,
    /// make it the next generation: publish `service.described` and tell the
    /// interpreter. A call made from an older generation's method that the
    /// new one lacks is refused. Whether anything changed; `false` for a name
    /// the session does not serve.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a test calls it; the MCP presenter's relisting will"
        )
    )]
    pub(crate) fn redescribe(&self, name: &str) -> bool {
        let table = &self.0;
        let Some((service, old)) = table.served(name) else {
            return false;
        };
        let next = Described::new(service.describe(), old.generation + 1);
        let changes = changes(&old.description, &next.description);
        if changes == ServiceChanges::default() {
            return false;
        }
        let next = Arc::new(next);
        let mut line = None;
        table.with(|s| {
            if let Some(served) = s.served.iter_mut().find(|served| served.name == name) {
                served.current = Arc::clone(&next);
                line = Some(table.entry(served));
            }
        });
        table.events.emit(Payload::ServiceDescribed {
            service: name.to_string(),
            generation: next.generation,
            changes,
            unchecked: next.unchecked.clone(),
        });
        if let Some(entry) = line {
            (table.to_interpreter)(json!({"t": "service.described", "service": entry}));
        }
        true
    }

    /// Resolves once no call is in flight.
    pub(crate) fn drained(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut state = self.0.state.subscribe();
        async move {
            let _ = state.wait_for(|s| s.flights.is_empty()).await;
        }
    }

    /// The session's drain has ended: tell every call still in flight to
    /// stop, reason `closed`. Each is then given its grace and aborted.
    pub(crate) fn terminate(&self) {
        let calls: Vec<ServiceCallId> = self.0.state.borrow().flights.keys().copied().collect();
        for call in calls {
            self.0.stop(call, ServiceReason::Closed);
        }
    }

    /// Each call whose outcome came after admission closed, in the order they
    /// ended: read last, after which nothing is published. One still in flight
    /// -- its abort not yet seen -- is aborted now and reported `unknown`.
    pub(crate) fn report(&self) -> Vec<ServiceCall> {
        let mut report = Vec::new();
        self.0.with(|s| {
            s.sealed = true;
            for (call, flight) in s.flights.drain() {
                if let Some(abort) = &flight.abort {
                    abort.abort();
                }
                let row = flight.row(
                    call,
                    ServiceOutcome::Unknown,
                    None,
                    Some(ServiceReason::Closed),
                );
                s.after_close.push(row);
            }
            report = s.after_close.clone();
        });
        report
    }
}

impl Table {
    fn with<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let mut out = None;
        self.state.send_modify(|state| out = Some(change(state)));
        out.expect("the change ran")
    }

    /// `name`'s service and its current description.
    fn served(&self, name: &str) -> Option<(Arc<dyn Service>, Arc<Described>)> {
        let state = self.state.borrow();
        let served = state.served.iter().find(|served| served.name == name)?;
        Some((Arc::clone(&served.service), Arc::clone(&served.current)))
    }

    /// What the interpreter is told of `served`.
    fn entry(&self, served: &Served) -> Value {
        let described = &served.current;
        let methods: Vec<Value> = described
            .description
            .methods
            .iter()
            .map(|method| {
                json!({
                    "name": method.name,
                    "description": method.description,
                    "params": method.params,
                    "result": method.result,
                    "extras": method.extras,
                })
            })
            .collect();
        json!({
            "name": served.name,
            "description": described.description.description,
            "presenter": presenter(described.description.kind),
            "deadline": self.ceiling(&described.description).as_secs_f64(),
            "generation": described.generation,
            "methods": methods,
        })
    }

    /// The longest a call of a service described by `description` may run.
    fn ceiling(&self, description: &ServiceDescription) -> Duration {
        description
            .call_deadline
            .unwrap_or(self.policy.call_deadline)
    }

    /// One line the interpreter sent about a service. Called by the reader of
    /// its output, which must never wait.
    fn line(self: &Arc<Self>, line: Value) {
        match line["t"].as_str() {
            Some("service.call") => self.accept(line),
            Some("service.cancel") => {
                if let (Some(agent), Some(ask)) = (line["agent"].as_str(), line["id"].as_u64()) {
                    let call = self
                        .state
                        .borrow()
                        .flights
                        .iter()
                        .find(|(_, flight)| *flight.agent == *agent && flight.ask == ask)
                        .map(|(call, _)| *call);
                    if let Some(call) = call {
                        self.stop(call, ServiceReason::Interrupted);
                    }
                }
            }
            kind => tracing::warn!("ignored a {kind:?} line from the interpreter"),
        }
    }

    /// A call: recorded at once, so a cancel that follows finds it, and run
    /// on a task of its own.
    fn accept(self: &Arc<Self>, mut line: Value) {
        let (Some(agent), Some(ask)) = (line["agent"].as_str(), line["id"].as_u64()) else {
            tracing::warn!("ignored a service call from the interpreter with no agent or id");
            return;
        };
        let agent: Arc<str> = agent.into();
        let request = Request::read(&line);
        // JSON text, so a value the host's parser would refuse -- nested past its
        // depth, say -- leaves the line readable, and is refused rather than lost.
        let arguments = match line.get_mut("arguments").map(Value::take) {
            Some(Value::String(text)) => text,
            _ => String::new(),
        };
        let call = ServiceCallId::new(self.ids.fetch_add(1, Ordering::Relaxed) + 1);
        let (stop, stopped) = watch::channel(None);
        let stop = Arc::new(stop);
        let sealed = self.with(|s| {
            if s.sealed {
                return true;
            }
            s.flights.insert(
                call,
                Flight {
                    stop,
                    abort: None,
                    agent: Arc::clone(&agent),
                    ask,
                    service: request.service.clone(),
                    method: request.method.clone(),
                },
            );
            false
        });
        if sealed {
            let end = End::refused(
                ServiceReason::AdmissionClosed,
                "the session has stopped".into(),
            );
            (self.to_interpreter)(answer(ask, call, end));
            return;
        }
        tokio::spawn(Arc::clone(self).run(call, agent, ask, request, arguments, stopped));
    }

    /// Tell `call` to stop for `reason`, unless something already has; the
    /// reason that holds.
    fn stop(&self, call: ServiceCallId, reason: ServiceReason) -> Option<ServiceReason> {
        let stop = Arc::clone(&self.state.borrow().flights.get(&call)?.stop);
        stop.send_if_modified(|said| {
            let first = said.is_none();
            if first {
                *said = Some(reason);
            }
            first
        });
        *stop.borrow()
    }

    async fn run(
        self: Arc<Self>,
        call: ServiceCallId,
        agent: Arc<str>,
        ask: u64,
        request: Request,
        arguments: String,
        mut stopped: watch::Receiver<Option<ServiceReason>>,
    ) {
        let since = Instant::now();
        let served = self.served(&request.service);
        let (kind, ceiling) = served.as_ref().map_or(
            (ServiceKind::Rust, self.policy.call_deadline),
            |(_, described)| {
                (
                    described.description.kind,
                    self.ceiling(&described.description),
                )
            },
        );
        let deadline = request
            .timeout
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
            .map_or(ceiling, |timeout| {
                Duration::try_from_secs_f64(timeout).map_or(ceiling, |t| t.min(ceiling))
            });
        let record = self.policy.record_bodies;
        self.events.emit_with(|| Payload::ServiceCallReceived {
            call,
            service: request.service.clone(),
            kind,
            method: request.method.clone(),
            generation: request.generation,
            agent: agent.to_string(),
            execid: request.execution,
            invocation: request.invocation,
            deadline,
            arguments: record.then(|| cut(&arguments)),
        });
        let arguments = serde_json::from_str::<Value>(&arguments);
        let ended = |end: End| self.end(call, &agent, ask, &request, kind, since, end);
        let Some((service, described)) = served else {
            return ended(End::refused(
                ServiceReason::UnknownMethod,
                format!("the session serves no service {:?}", request.service),
            ));
        };
        if self.lifecycle.closed().is_some() {
            return ended(End::refused(
                ServiceReason::AdmissionClosed,
                "the session is closed to new work, so the call was not made".into(),
            ));
        }
        if !described.checks.contains_key(&request.method) {
            let mut message = format!(
                "service {:?} has no method {:?}",
                request.service, request.method
            );
            if request.generation != described.generation {
                message.push_str(&format!(
                    ": it was described at generation {}, and is described at generation {} now",
                    request.generation, described.generation
                ));
            }
            return ended(End::refused(ServiceReason::UnknownMethod, message));
        }
        let arguments = match arguments {
            Ok(arguments) => arguments,
            Err(e) => {
                return ended(End {
                    path: Some(String::new()),
                    detail: Some(String::new()),
                    ..End::refused(
                        ServiceReason::InvalidArguments,
                        format!("the arguments are not JSON the host can read: {e}"),
                    )
                });
            }
        };
        if let Err((path, problem)) = described.check(&request.method, &arguments) {
            let at = match path.as_str() {
                "" => "the arguments".to_string(),
                path => format!("the argument at {path}"),
            };
            return ended(End {
                path: Some(path.clone()),
                detail: Some(path),
                ..End::refused(
                    ServiceReason::InvalidArguments,
                    format!("{at} does not match the method's schema: {problem}"),
                )
            });
        }
        // Copied out first: the guard is not held while `ended` locks the table.
        let told_already = *stopped.borrow();
        if let Some(reason) = told_already {
            return ended(End::stopped(
                ServiceOutcome::Cancelled,
                reason,
                format!(
                    "the call was cancelled before it was made: {}",
                    said(reason)
                ),
            ));
        }
        self.events.emit(Payload::ServiceCallDispatched {
            call,
            service: request.service.clone(),
            method: request.method.clone(),
            agent: agent.to_string(),
        });
        let token = CancellationToken::new();
        let cx = CallContext {
            call,
            service: request.service.as_str().into(),
            agent: Arc::clone(&agent),
            execution: request.execution,
            invocation: request.invocation,
            deadline: (since + deadline).into_std(),
            cancellation: token.clone(),
        };
        let method = request.method.clone();
        let mut task = tokio::spawn(async move { service.call(&method, arguments, cx).await });
        self.with(|s| {
            if let Some(flight) = s.flights.get_mut(&call) {
                flight.abort = Some(task.abort_handle());
            }
        });
        let finished = tokio::select! {
            finished = &mut task => Ok(finished),
            () = tokio::time::sleep_until(since + deadline) => Err(ServiceReason::Deadline),
            reason = told(&mut stopped) => Err(reason),
        };
        let finished = match finished {
            Ok(finished) => finished,
            Err(reason) => {
                let reason = self.stop(call, reason).unwrap_or(reason);
                token.cancel();
                self.events.emit(Payload::ServiceCallCancelSent {
                    call,
                    service: request.service.clone(),
                    agent: agent.to_string(),
                    reason,
                });
                match tokio::time::timeout(self.policy.cancel_grace, &mut task).await {
                    Ok(finished) => finished,
                    Err(_) => {
                        // Dropped at its next await point; one blocking a thread runs on, and
                        // nothing here waits for it.
                        task.abort();
                        return ended(End::stopped(
                            ServiceOutcome::Unknown,
                            reason,
                            format!(
                                "the call was told to stop -- {} -- and had not ended {:?} later, \
                                 so it was abandoned: what it did is unknown",
                                said(reason),
                                self.policy.cancel_grace
                            ),
                        ));
                    }
                }
            }
        };
        ended(self.outcome(finished))
    }

    /// How a call whose service's task finished ended.
    fn outcome(&self, finished: Result<Result<Value, ServiceError>, JoinError>) -> End {
        match finished {
            Ok(Ok(result)) => {
                // Sent as JSON text, which the caller reads: a value its parser
                // refuses then raises there rather than leaving the answer unread.
                let result = result.to_string();
                let bytes = result.len() as u64;
                let max = self.policy.result_max;
                if bytes > max as u64 {
                    let detail = format!("returned {bytes} bytes, past the {max} allowed");
                    End {
                        bytes: Some(bytes),
                        detail: Some(detail),
                        ..End::refused(
                            ServiceReason::ResultTooLarge,
                            format!(
                                "the service returned a result of {bytes} bytes as JSON, past the \
                                 {max} a result may take, so it was not handed over"
                            ),
                        )
                    }
                } else {
                    End {
                        bytes: Some(bytes),
                        result: Some(result),
                        ..End::new(ServiceOutcome::Returned, String::new())
                    }
                }
            }
            Ok(Err(error)) => End::raised(error.category, error.message),
            Err(_) => End::raised(
                ServiceErrorCategory::Failed,
                "the service's call panicked".into(),
            ),
        }
    }

    /// `call` ended as `end` says: publish it, list it when the close has
    /// come, and answer its caller.
    #[allow(clippy::too_many_arguments)]
    fn end(
        &self,
        call: ServiceCallId,
        agent: &str,
        ask: u64,
        request: &Request,
        kind: ServiceKind,
        since: Instant,
        end: End,
    ) {
        let closed = self.lifecycle.closed().is_some();
        let sealed = self.with(|s| {
            // Gone when the report took it already.
            let Some(flight) = s.flights.remove(&call) else {
                return true;
            };
            if closed {
                let row = flight.row(call, end.outcome, end.category, end.reason);
                s.after_close.push(row);
            }
            false
        });
        if !sealed {
            let record = self.policy.record_bodies;
            self.events.emit_with(|| Payload::ServiceCallEnded {
                call,
                service: request.service.clone(),
                kind,
                method: request.method.clone(),
                agent: agent.to_string(),
                execid: request.execution,
                outcome: end.outcome,
                category: end.category,
                reason: end.reason,
                detail: end.detail.clone(),
                bytes: end.bytes,
                duration: since.elapsed(),
                body: end.result.as_deref().filter(|_| record).map(cut),
            });
        }
        (self.to_interpreter)(answer(ask, call, end));
    }
}

impl Described {
    /// `description` as generation `generation`, each method's `params`
    /// compiled; one that does not compile is noted, and called unchecked.
    fn new(description: ServiceDescription, generation: u64) -> Self {
        let mut checks = HashMap::new();
        let mut unchecked = Vec::new();
        for method in &description.methods {
            if checks.contains_key(&method.name) {
                continue;
            }
            // Draft 2020-12 unless the schema says another. Offline, as
            // features an embedder turns on for the crate unify with ours:
            // nothing outside the schema is loaded, and a `$ref` to a file or
            // a URL does not compile.
            let check = match jsonschema::options().offline().build(&method.params) {
                Ok(compiled) => Some(compiled),
                Err(e) => {
                    tracing::debug!("method {:?}'s schema did not compile: {e}", method.name);
                    unchecked.push(method.name.clone());
                    None
                }
            };
            checks.insert(method.name.clone(), check);
        }
        Self {
            generation,
            description,
            checks,
            unchecked,
        }
    }

    /// Whether `arguments` match `method`'s schema; the first failing JSON
    /// path and what failed there, if not.
    fn check(&self, method: &str, arguments: &Value) -> Result<(), (String, String)> {
        if !arguments.is_object() {
            return Err((String::new(), "the arguments are not a JSON object".into()));
        }
        let Some(Some(validator)) = self.checks.get(method) else {
            return Ok(());
        };
        // Masked, so the message names the problem and not the value, which
        // can be as large as the arguments.
        validator.validate(arguments).map_err(|error| {
            (
                error.instance_path().to_string(),
                error.masked().to_string(),
            )
        })
    }
}

/// Which methods differ between two descriptions, by exact name.
#[cfg_attr(not(test), expect(dead_code, reason = "`redescribe` calls it"))]
fn changes(old: &ServiceDescription, new: &ServiceDescription) -> ServiceChanges {
    fn find<'a>(description: &'a ServiceDescription, name: &str) -> Option<&'a MethodDescription> {
        description
            .methods
            .iter()
            .find(|method| method.name == name)
    }
    let mut changes = ServiceChanges::default();
    for method in &new.methods {
        match find(old, &method.name) {
            None => changes.added.push(method.name.clone()),
            Some(was) if was != method => changes.changed.push(method.name.clone()),
            Some(_) => {}
        }
    }
    for method in &old.methods {
        if find(new, &method.name).is_none() {
            changes.removed.push(method.name.clone());
        }
    }
    changes
}

/// The presenter kind -- the Python class -- a service of `kind` is shown as.
fn presenter(kind: ServiceKind) -> &'static str {
    match kind {
        ServiceKind::Rust => "service",
    }
}

/// Resolves with the reason a call is told to stop, once one is said.
async fn told(stopped: &mut watch::Receiver<Option<ServiceReason>>) -> ServiceReason {
    // Copied out, so the guard is not held across the wait below.
    let reason = match stopped.wait_for(Option::is_some).await {
        Ok(reason) => *reason,
        Err(_) => None,
    };
    match reason {
        Some(reason) => reason,
        None => std::future::pending().await,
    }
}

/// What `reason` says, in a message.
fn said(reason: ServiceReason) -> &'static str {
    match reason {
        ServiceReason::Deadline => "its deadline passed",
        ServiceReason::Interrupted => "its caller was interrupted",
        ServiceReason::Closed => "the session's drain ended",
        _ => "it was refused",
    }
}

/// The answer to the call the interpreter asked as `ask`: its result, moved
/// into the line, or why there is none.
fn answer(ask: u64, call: ServiceCallId, end: End) -> Value {
    if end.outcome == ServiceOutcome::Returned {
        return json!({"t": "service", "id": ask, "ok": end.result});
    }
    // A refusal's category is its reason, as agent code reads it.
    let category = match end.outcome {
        ServiceOutcome::Refused => end.reason.map(name),
        _ => end.category.map(|category| category.as_str().to_string()),
    };
    failed(
        ask,
        Some(call),
        Failed {
            outcome: name(end.outcome),
            category,
            reason: end.reason.map(name),
            message: end.message,
            path: end.path,
        },
    )
}

/// What agent code reads of a call that returned no result.
struct Failed {
    outcome: String,
    category: Option<String>,
    reason: Option<String>,
    message: String,
    path: Option<String>,
}

/// The answer to the call asked as `ask`, which ended as `failed` says.
fn failed(ask: u64, call: Option<ServiceCallId>, failed: Failed) -> Value {
    json!({
        "t": "service",
        "id": ask,
        "error": {
            "call": call,
            "outcome": failed.outcome,
            "category": failed.category,
            "reason": failed.reason,
            "message": failed.message,
            "path": failed.path,
        },
    })
}

/// `value`'s name as it serializes: a unit variant's snake-case name.
fn name(value: impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name,
        _ => String::new(),
    }
}

/// The answer to a call the table did not accept: one from an agent whose
/// kernel the host did not open, or one when the session serves no service.
pub(crate) fn unserved(line: &Value, opened: bool) -> Option<Value> {
    if line["t"] != "service.call" {
        return None;
    }
    let ask = line["id"].as_u64()?;
    let (reason, message) = match opened {
        true => (
            name(ServiceReason::UnknownMethod),
            "the session serves no services".to_string(),
        ),
        false => (
            "unknown_agent".to_string(),
            format!(
                "agent {} has no kernel open on the host, so its call was not made",
                line["agent"]
            ),
        ),
    };
    Some(failed(
        ask,
        None,
        Failed {
            outcome: name(ServiceOutcome::Refused),
            category: Some(reason.clone()),
            reason: Some(reason),
            message,
            path: None,
        },
    ))
}

/// `text`, JSON, cut to [`PREVIEW_MAX`] characters, saying how many more it
/// had: what an event shows of a call's arguments or result.
fn cut(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_MAX) {
        Some((at, _)) => {
            let more = text[at..].chars().count();
            format!("{}... [{more} more characters]", &text[..at])
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How every outcome a call can end with reaches agent code: the fields of the
    /// `outrig.ServiceError` it raises, or its result for a call that returned. A refusal's
    /// category is its reason; a raised call's is its service's, by name.
    #[test]
    fn each_outcome_reaches_agent_code_as_its_fields() {
        let call = ServiceCallId::new(7);
        for outcome in [
            ServiceOutcome::Returned,
            ServiceOutcome::Raised,
            ServiceOutcome::Refused,
            ServiceOutcome::Cancelled,
            ServiceOutcome::Unknown,
        ] {
            // Exhaustive, so an outcome added here is mapped here too.
            let (end, fields) = match outcome {
                ServiceOutcome::Returned => {
                    let end = End {
                        result: Some(r#"{"x":1}"#.into()),
                        ..End::new(outcome, String::new())
                    };
                    (end, json!({"t": "service", "id": 3, "ok": r#"{"x":1}"#}))
                }
                ServiceOutcome::Raised => (
                    End::raised(ServiceErrorCategory::NotFound, "no such".into()),
                    json!({"outcome": "raised", "category": "not_found", "reason": null}),
                ),
                ServiceOutcome::Refused => (
                    End {
                        path: Some("/x".into()),
                        ..End::refused(ServiceReason::InvalidArguments, "bad".into())
                    },
                    json!({"outcome": "refused", "category": "invalid_arguments",
                           "reason": "invalid_arguments", "path": "/x"}),
                ),
                ServiceOutcome::Cancelled => (
                    End::stopped(outcome, ServiceReason::Interrupted, "stopped".into()),
                    json!({"outcome": "cancelled", "category": null, "reason": "interrupted"}),
                ),
                ServiceOutcome::Unknown => (
                    End::stopped(outcome, ServiceReason::Deadline, "abandoned".into()),
                    json!({"outcome": "unknown", "category": null, "reason": "deadline"}),
                ),
            };
            let answer = answer(3, call, end);
            if outcome == ServiceOutcome::Returned {
                assert_eq!(answer, fields);
                continue;
            }
            let error = &answer["error"];
            assert_eq!(error["call"], 7, "{answer}");
            for (field, value) in fields.as_object().expect("fields") {
                assert_eq!(&error[field], value, "{outcome:?}'s {field}: {answer}");
            }
        }
        for category in [
            ServiceErrorCategory::InvalidArguments,
            ServiceErrorCategory::NotFound,
            ServiceErrorCategory::Refused,
            ServiceErrorCategory::Unavailable,
            ServiceErrorCategory::Failed,
            ServiceErrorCategory::Cancelled,
            ServiceErrorCategory::Other,
        ] {
            assert_eq!(name(category), category.as_str());
            let answer = answer(3, call, End::raised(category, String::new()));
            assert_eq!(answer["error"]["category"], category.as_str());
        }
        for reason in [
            ServiceReason::AdmissionClosed,
            ServiceReason::UnknownMethod,
            ServiceReason::InvalidArguments,
            ServiceReason::ResultTooLarge,
            ServiceReason::Deadline,
            ServiceReason::Interrupted,
            ServiceReason::Closed,
        ] {
            let answer = answer(3, call, End::refused(reason, String::new()));
            assert_eq!(answer["error"]["category"], answer["error"]["reason"]);
            assert!(!name(reason).is_empty());
        }
    }

    /// A schema's first failing JSON path is what a refusal names, and one that does not compile
    /// leaves its method unchecked; a `$ref` to anything outside the schema does not compile,
    /// though the crate's features would load it.
    /// Numbers are compared exactly: an integer past 2^53 against an integer bound, and a decimal
    /// against `multipleOf`, as an `f64` comparison would not.
    #[test]
    fn a_schema_is_compiled_and_checked() {
        // The tests turn on `resolve-file`, as an embedder may, so the crate
        // loads this file by default; the check must not. Under `/tmp`: the
        // crate reads a `file:` URI's path undecoded, and `TMPDIR` may hold
        // a space.
        let dir = tempfile::tempdir_in("/tmp").expect("a temporary directory");
        let file = dir.path().join("integer.json");
        std::fs::write(&file, r#"{"type": "integer"}"#).expect("written");
        let far = json!({"$ref": format!("file://{}", file.display())});
        assert!(jsonschema::validator_for(&far).is_ok(), "loaded by default");
        let described = Described::new(
            ServiceDescription::new("s")
                .with_method(MethodDescription::new(
                    "m",
                    "",
                    json!({"type": "object", "properties": {"a": {"type": "object",
                        "properties": {"b": {"type": "integer"}}}}, "required": ["a"]}),
                ))
                .with_method(MethodDescription::new("bad", "", json!({"type": 5})))
                .with_method(MethodDescription::new("far", "", far))
                .with_method(MethodDescription::new(
                    "big",
                    "",
                    json!({"type": "object", "properties": {
                        "n": {"type": "integer", "maximum": 9_007_199_254_740_992_u64},
                        "m": {"type": "integer", "minimum": -9_007_199_254_740_992_i64},
                        "k": {"multipleOf": 2},
                        "p": {"multipleOf": 0.1},
                        "c": {"multipleOf": 0.01}}}),
                )),
            1,
        );
        assert_eq!(described.unchecked, ["bad", "far"]);
        assert_eq!(described.check("m", &json!({"a": {"b": 1}})), Ok(()));
        let refused = described.check("m", &json!({"a": {"b": "a long secret value"}}));
        let (path, problem) = refused.expect_err("a string is no integer");
        assert_eq!(path, "/a/b");
        assert!(
            !problem.contains("secret"),
            "the value is not echoed: {problem}"
        );
        assert_eq!(
            described.check("m", &json!({})).map_err(|(path, _)| path),
            Err(String::new())
        );
        assert_eq!(described.check("bad", &json!({"anything": 1})), Ok(()));
        assert!(
            described.check("bad", &json!([1])).is_err(),
            "not an object"
        );
        let big = |text: &str| -> Value { serde_json::from_str(text).expect("JSON") };
        for (arguments, valid) in [
            (r#"{"n": 9007199254740992}"#, true),
            (r#"{"n": 9007199254740993}"#, false),
            (r#"{"m": -9007199254740993}"#, false),
            (r#"{"k": 9007199254740993}"#, false),
            (r#"{"p": 0.3}"#, true),
            (r#"{"p": 0.35}"#, false),
            (r#"{"c": 19.99}"#, true),
        ] {
            assert_eq!(
                described.check("big", &big(arguments)).is_ok(),
                valid,
                "{arguments}"
            );
        }
    }

    #[test]
    fn a_preview_is_cut_and_says_how_much_more() {
        assert_eq!(cut(r#"{"x":1}"#), r#"{"x":1}"#);
        let long = cut(&"é".repeat(2000));
        assert!(long.ends_with("... [976 more characters]"), "{long}");
        assert_eq!(
            long.chars().count(),
            1024 + "... [976 more characters]".len()
        );
    }
}
