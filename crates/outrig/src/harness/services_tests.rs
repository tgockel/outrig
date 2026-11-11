//! Services end to end on the host: a Rust service bound with `serve`, called from agent Python
//! through the object generated from its description -- checked, evented, held to a deadline,
//! cancelled, bounded and reported -- with no bindings process and no RPyC. The tests of a
//! service as an agent's facility, and of what agents are told of one, are `facilities_tests`'.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::bindings_tests::{output, start, stop, taken};
use super::event::{Event, ServiceOutcome, ServiceReason, Subscription};
use super::{
    CallContext, MethodDescription, Service, ServiceDescription, ServiceError,
    ServiceErrorCategory, ServiceFuture, ServicePolicy, Session, SessionBuilder, Verdict,
};
use crate::agent::mock_http::MODEL;
use crate::config::{Binding, Config};
use crate::python::host::Outcome;
use crate::python::testing::{Flag, within};

const VAR: &str = "OUTRIG_TEST_SERVICES_KEY";

/// A credential of the fixture's, which its `panic` method panics with: it must appear in nothing
/// an agent or an owner reads.
const SECRET: &str = "sk-fixture-31f5d0a9c7";

/// Each category a service can raise, by the name agent code reads.
const CATEGORIES: [(ServiceErrorCategory, &str); 7] = [
    (ServiceErrorCategory::InvalidArguments, "invalid_arguments"),
    (ServiceErrorCategory::NotFound, "not_found"),
    (ServiceErrorCategory::Refused, "refused"),
    (ServiceErrorCategory::Unavailable, "unavailable"),
    (ServiceErrorCategory::Failed, "failed"),
    (ServiceErrorCategory::Cancelled, "cancelled"),
    (ServiceErrorCategory::Other, "other"),
];

/// The test service: each method does what its name says, every call is counted and kept, and a
/// call whose cancellation token is triggered says so.
pub(crate) struct Fixture {
    description: Mutex<ServiceDescription>,
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, Value, String)>>,
    running: AtomicUsize,
    fired: Arc<AtomicBool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

/// An object whose properties are `properties`, of which `required` must be given.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// `{secs: number}`, for the methods that wait.
fn seconds() -> Value {
    object(json!({"secs": {"type": "number"}}), &["secs"])
}

impl Fixture {
    pub(crate) fn new() -> Arc<Self> {
        Self::described(Self::methods())
    }

    /// A task's service, as an application binds one: `update_progress(note)` records a note.
    pub(crate) fn task() -> Arc<Self> {
        Self::described(vec![MethodDescription::new(
            "update_progress",
            "Record progress on the task.",
            object(json!({"note": {"type": "string"}}), &["note"]),
        )])
    }

    fn described(methods: Vec<MethodDescription>) -> Arc<Self> {
        let mut description = ServiceDescription::new("The test service.");
        description.methods = methods;
        Arc::new(Self {
            description: Mutex::new(description),
            calls: AtomicUsize::new(0),
            seen: Mutex::default(),
            running: AtomicUsize::new(0),
            fired: Arc::new(AtomicBool::new(false)),
            threads: Mutex::default(),
        })
    }

    fn methods() -> Vec<MethodDescription> {
        vec![
            MethodDescription::new(
                "echo",
                "Return the arguments.",
                object(
                    json!({"x": {"type": "integer", "description": "What comes back."}}),
                    &["x"],
                ),
            ),
            MethodDescription::new(
                "fail",
                "Fail with the category named.",
                object(json!({"category": {"type": "string"}}), &["category"]),
            ),
            MethodDescription::new("sleep", "Sleep, deaf to the token.", seconds()),
            MethodDescription::new("sleep_honoring", "Sleep until told to stop.", seconds()),
            MethodDescription::new("block", "Wait on a blocking thread.", seconds()),
            MethodDescription::new(
                "ask",
                "Ask the user, returning a ticket at once.",
                object(json!({"question": {"type": "string"}}), &["question"]),
            ),
            MethodDescription::new(
                "read-file",
                "Read a file.",
                object(json!({"path": {"type": "string"}}), &["path"]),
            ),
            MethodDescription::new("class", "Named for a keyword.", json!({"type": "object"})),
            MethodDescription::new(
                "odd",
                "Take what no signature holds.",
                json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "integer"}}},
                    {"type": "object", "required": ["b"]},
                ]}),
            ),
            MethodDescription::new("unchecked", "Take anything.", json!({"type": 5})),
            MethodDescription::new("big", "Return 20 MiB.", json!({"type": "object"})),
            MethodDescription::new("panic", "Panic.", json!({"type": "object"})),
            MethodDescription::new(
                "selfish",
                "Take an argument named for the receiver.",
                object(json!({"self": {"type": "integer"}}), &[]),
            ),
        ]
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Each call made of it, as (method, arguments, agent).
    pub(crate) fn seen(&self) -> Vec<(String, Value, String)> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Change the methods it describes from now on.
    fn redescribe(&self, change: impl FnOnce(&mut Vec<MethodDescription>)) {
        change(
            &mut self
                .description
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .methods,
        );
    }

    /// Wait until `count` of its waiting methods have begun.
    async fn running(&self, count: usize) {
        within(async {
            while self.running.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// Wait until a call's cancellation token has been triggered.
    async fn fired(&self) {
        within(async {
            while !self.fired.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// Join the threads `block` started.
    fn join(&self) {
        let threads =
            std::mem::take(&mut *self.threads.lock().unwrap_or_else(PoisonError::into_inner));
        for thread in threads {
            thread.join().expect("the blocking thread ended");
        }
    }
}

/// The category agent code named.
fn category(name: &str) -> ServiceErrorCategory {
    CATEGORIES
        .iter()
        .find(|(_, named)| *named == name)
        .map_or(ServiceErrorCategory::Other, |(category, _)| *category)
}

impl Service for Fixture {
    fn describe(&self) -> ServiceDescription {
        self.description
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn call(&self, method: &str, args: Value, cx: CallContext) -> ServiceFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((method.to_string(), args.clone(), cx.agent().to_string()));
        let (told, fired) = (cx.clone(), Arc::clone(&self.fired));
        tokio::spawn(async move {
            told.cancelled().await;
            fired.store(true, Ordering::SeqCst);
        });
        let secs = Duration::from_secs_f64(args["secs"].as_f64().unwrap_or(0.0));
        let method = method.to_string();
        Box::pin(async move {
            match method.as_str() {
                "panic" => panic!("lost the connection holding {SECRET}"),
                "fail" => Err(ServiceError::new(
                    category(args["category"].as_str().unwrap_or_default()),
                    "failed as asked",
                )),
                "sleep" => {
                    self.running.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(secs).await;
                    Ok(json!("slept"))
                }
                "sleep_honoring" => {
                    self.running.fetch_add(1, Ordering::SeqCst);
                    tokio::select! {
                        () = tokio::time::sleep(secs) => Ok(json!("slept")),
                        () = cx.cancelled() => Err(ServiceError::new(
                            ServiceErrorCategory::Cancelled,
                            "stopped as told",
                        )),
                    }
                }
                "block" => {
                    self.running.fetch_add(1, Ordering::SeqCst);
                    let (done, finished) = tokio::sync::oneshot::channel();
                    let thread = std::thread::spawn(move || {
                        std::thread::sleep(secs);
                        let _ = done.send(());
                    });
                    self.threads
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(thread);
                    let _ = finished.await;
                    Ok(json!("unblocked"))
                }
                "ask" => Ok(json!({"ticket": format!("t-{}", cx.call())})),
                "big" => Ok(Value::String("x".repeat(20 << 20))),
                _ => Ok(args),
            }
        })
    }
}

/// A configuration whose one model is served nowhere, run by a `coding` agent.
fn config() -> Config {
    let cfg = Config::load_from_str(&format!(
        r#"
default-model = "sonnet"

[providers.claude]
style    = "messages"
base-url = "http://127.0.0.1:9"
api-key  = "${{{VAR}}}"

[models.sonnet]
provider   = "claude"
identifier = "{MODEL}"

[agents.coding]
"#
    ))
    .expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

fn builder() -> SessionBuilder {
    SessionBuilder::new(config(), Some("coding"), None)
        .secrets(|var: &str| (var == VAR).then(|| "sk-test".to_string()))
}

/// A session serving `fixture` as `svc` under `policy`, its events subscribed to, with `err`
/// defined in its primary: what a call raised, as its fields.
async fn served(fixture: &Arc<Fixture>, policy: ServicePolicy) -> (Session, Subscription) {
    served_with(
        builder().serve("svc", Arc::clone(fixture) as Arc<dyn Service>),
        policy,
    )
    .await
}

async fn served_with(builder: SessionBuilder, policy: ServicePolicy) -> (Session, Subscription) {
    let mut builder = builder.service_policy(policy);
    let events = builder.subscribe();
    let session = start(builder).await;
    output(&session, ERR).await;
    (session, events)
}

/// What agent code reads of a call that raised `outrig.ServiceError`.
const ERR: &str = r#"
import outrig
def err(f, *args, **kwargs):
    try:
        f(*args, **kwargs)
    except outrig.ServiceError as e:
        print(e.outcome, e.category, e.reason, e.path)
"#;

/// A policy whose calls have `deadline` and a grace of 300 ms.
fn quick(deadline: Duration) -> ServicePolicy {
    ServicePolicy {
        call_deadline: deadline,
        cancel_grace: Duration::from_millis(300),
        ..ServicePolicy::default()
    }
}

/// The `data` of each event of `kind`, in order.
fn of_kind(events: &[Arc<Event>], kind: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind() == kind)
        .map(|event| serde_json::to_value(&event.payload).expect("encodes"))
        .collect()
}

/// The `service.call.*` events of `call`, by kind, in order.
fn call_kinds(events: &[Arc<Event>], call: &Value) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.kind().starts_with("service.call."))
        .filter(|event| serde_json::to_value(&event.payload).expect("encodes")["call"] == *call)
        .map(|event| event.kind().to_string())
        .collect()
}

/// The outcome of `source`, run in `session`'s primary, failing if it did not raise.
async fn raised(session: &Session, source: &str) -> String {
    let mut execution = session
        .interpreter()
        .submit(source)
        .unwrap_or_else(|e| panic!("{e}"));
    match within(execution.outcome()).await {
        Outcome::Error { traceback, .. } => traceback,
        other => panic!("{source:?} did not raise: {other:?}"),
    }
}

/// A call returns its result as Python data, and its events name the service, the method, the
/// agent and the execution, with no previews unless the policy records bodies.
#[tokio::test]
async fn a_call_returns_data_and_is_evented() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, ServicePolicy::default()).await;
    assert_eq!(
        output(&session, "r = svc.echo(x=1)\nprint(r, type(r).__name__)").await,
        "{'x': 1} dict\n"
    );
    let report = stop(session).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let events = taken(&mut events);
    let bound = of_kind(&events, "service.bound");
    assert_eq!(bound.len(), 1, "{bound:?}");
    assert_eq!(
        (
            &bound[0]["service"],
            &bound[0]["kind"],
            &bound[0]["generation"]
        ),
        (&json!("svc"), &json!("rust"), &json!(1))
    );
    assert_eq!(bound[0]["methods"], Fixture::methods().len());
    let received = &of_kind(&events, "service.call.received")[0];
    let ended = &of_kind(&events, "service.call.ended")[0];
    for event in [received, ended] {
        assert_eq!(
            (&event["service"], &event["method"], &event["agent"]),
            (&json!("svc"), &json!("echo"), &json!("primary")),
            "{event}"
        );
        assert!(event["execid"].as_u64().is_some(), "{event}");
    }
    assert_eq!(received["arguments"], Value::Null);
    assert_eq!(received["deadline"], json!(600.0));
    assert_eq!(
        (&ended["outcome"], &ended["body"], &ended["bytes"]),
        (&json!("returned"), &Value::Null, &json!(7))
    );
    assert!(ended["duration"].as_f64().is_some(), "{ended}");
    assert_eq!(
        call_kinds(&events, &received["call"]),
        [
            "service.call.received",
            "service.call.dispatched",
            "service.call.ended"
        ]
    );
    assert!(
        events
            .iter()
            .filter(|event| event.kind().starts_with("service.call."))
            .all(|event| event.subject.as_ref().map(|s| s.as_str()) == Some("agent/primary")),
        "each call event is the calling agent's"
    );

    let recording = ServicePolicy {
        record_bodies: true,
        ..ServicePolicy::default()
    };
    let (session, mut events) = served(&Fixture::new(), recording).await;
    output(&session, "svc.echo(x=12345)").await;
    stop(session).await;
    let events = taken(&mut events);
    assert_eq!(
        of_kind(&events, "service.call.received")[0]["arguments"],
        json!(r#"{"x": 12345}"#),
        "the arguments as the interpreter sent them"
    );
    assert_eq!(
        of_kind(&events, "service.call.ended")[0]["body"],
        json!(r#"{"x":12345}"#)
    );
}

/// Arguments the method's schema refuses never reach the service: the caller learns the path,
/// and no dispatch is published. A method whose schema does not compile is called with its
/// arguments as given, and `service.bound` names it.
#[tokio::test]
async fn the_schema_refuses_before_the_service_is_called() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, ServicePolicy::default()).await;
    assert_eq!(
        output(&session, "err(svc.echo, x='no')").await,
        "refused invalid_arguments invalid_arguments /x\n"
    );
    assert_eq!(fixture.calls(), 0);
    assert_eq!(
        output(&session, "print(svc.unchecked(anything=[1, 'two']))").await,
        "{'anything': [1, 'two']}\n"
    );
    assert_eq!(fixture.seen()[0].1, json!({"anything": [1, "two"]}));
    stop(session).await;
    let events = taken(&mut events);
    assert_eq!(
        of_kind(&events, "service.bound")[0]["unchecked"],
        json!(["unchecked"])
    );
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["reason"], &ended["detail"]),
        (&json!("refused"), &json!("invalid_arguments"), &json!("/x"))
    );
    assert_eq!(
        call_kinds(&events, &ended["call"]),
        ["service.call.received", "service.call.ended"]
    );
}

/// Every method is reached by its exact name, and by an attribute where its name can be one;
/// two names meeting one attribute leave it to neither. `dir` lists what resolves, and `help`
/// renders each signature from its schema, or `(**arguments)` where none can be.
#[tokio::test]
async fn methods_are_reached_by_exact_name() {
    let fixture = Fixture::new();
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "print(svc.call('read-file', {'path': 'a'}), svc.method('class')())\n\
             print(svc.read_file(path='b'), svc.class_())\n\
             print(sorted(n for n in dir(svc) if n in ('read_file', 'class_', 'echo', 'odd')))",
        )
        .await,
        "{'path': 'a'} {}\n{'path': 'b'} {}\n['class_', 'echo', 'odd', 'read_file']\n"
    );
    let seen: Vec<String> = fixture
        .seen()
        .into_iter()
        .map(|(method, ..)| method)
        .collect();
    assert_eq!(seen, ["read-file", "class", "read-file", "class"]);
    let help = output(&session, "help(svc)").await;
    for said in [
        "echo(self, x: int)",
        "Return the arguments.",
        "x (int, required): What comes back.",
        "odd(self, /, **arguments)",
        "Take what no signature holds.",
        "Its exact name is 'read-file'",
    ] {
        assert!(help.contains(said), "{said}: {help}");
    }
    assert_eq!(
        output(
            &session,
            "import inspect\nprint(inspect.signature(svc.echo))"
        )
        .await,
        "(x: int)\n"
    );
    stop(session).await;

    let mut methods = Fixture::methods();
    methods.push(MethodDescription::new(
        "read_file",
        "Read another way.",
        json!({}),
    ));
    let twins = Fixture::described(methods);
    let (session, _events) = served(&twins, ServicePolicy::default()).await;
    let ambiguous = output(
        &session,
        "try:\n    svc.read_file\nexcept AttributeError as e:\n    print(e)\n\
         print('read_file' in dir(svc), svc.method('read_file')(), svc.method('read-file')('c'))",
    )
    .await;
    assert!(
        ambiguous.contains("'read-file' and 'read_file'")
            && ambiguous.contains(".method('read-file')"),
        "{ambiguous}"
    );
    assert!(
        ambiguous.ends_with("False {} {'path': 'c'}\n"),
        "{ambiguous}"
    );
    stop(session).await;
}

/// A call past its deadline is told to stop, given its grace, and abandoned: `unknown`, with
/// the deadline as its reason. One that honors the token ends `raised`, `cancelled`, within the
/// grace; one blocking a thread is abandoned without OutRig waiting on the thread. A timeout
/// past the deadline, and a call longer than the host reads, raise before anything is sent.
#[tokio::test]
async fn a_call_past_its_deadline_is_abandoned() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(1))).await;
    let started = Instant::now();
    assert_eq!(
        output(&session, "err(svc.sleep, secs=60)").await,
        "unknown None deadline None\n"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(1300) && took < Duration::from_secs(5),
        "{took:?}"
    );
    let started = Instant::now();
    assert_eq!(
        output(&session, "err(svc.sleep_honoring, secs=60)").await,
        "raised cancelled None None\n"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1300),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        output(&session, "err(svc.block, secs=3)").await,
        "unknown None deadline None\n"
    );
    let before = fixture.calls();
    assert_eq!(
        output(
            &session,
            "try:\n    svc.call('echo', {}, timeout=10_000)\nexcept ValueError as e:\n    print(e)",
        )
        .await,
        "timeout=10000 is past the 1 s deadline of service 'svc', which a timeout lowers and \
         never raises\n"
    );
    let long = "try:\n    svc.echo('x' * (17 << 20))\nexcept ValueError as e:\n    print(e)";
    let started = Instant::now();
    let said = output(&session, long).await;
    assert!(
        said.contains("past the 16777216 allowed") && said.contains("bytes as sent"),
        "{said}"
    );
    let worker = format!("await asyncio.to_thread(lambda: exec({long:?}))");
    let said = output(&session, &worker).await;
    assert!(said.contains("past the 16777216 allowed"), "{said}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.calls(), before, "nothing past the bound was sent");
    let report = stop(session).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    fixture.join();
    let events = taken(&mut events);
    let cancels = of_kind(&events, "service.call.cancel.sent");
    assert_eq!(cancels.len(), 3, "{cancels:?}");
    assert!(cancels.iter().all(|cancel| cancel["reason"] == "deadline"));
    assert_eq!(
        of_kind(&events, "service.call.dispatched").len(),
        before,
        "every dispatch was of a call made"
    );
}

/// An interrupt of the primary during a call raises in agent code, tells the service to stop,
/// and leaves the kernel's loop free; a call in a `to_thread` worker is not woken, and ends at
/// its deadline.
#[tokio::test]
async fn an_interrupt_cancels_the_call() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(60))).await;
    let mut call = session
        .interpreter()
        .submit("svc.sleep(secs=60)")
        .unwrap_or_else(|e| panic!("{e}"));
    fixture.running(1).await;
    session.interpreter().interrupt(call.id(), false);
    match within(call.outcome()).await {
        Outcome::Error { traceback, .. } => {
            assert!(traceback.contains("KeyboardInterrupt"), "{traceback}");
        }
        other => panic!("the call was not interrupted: {other:?}"),
    }
    fixture.fired().await;
    assert_eq!(output(&session, "print('free')").await, "free\n");
    // Ended mid-session, after its grace, so the report -- which lists outcomes after the close
    // -- is clean.
    within(session.services().drained()).await;
    let report = stop(session).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let events = taken(&mut events);
    let cancels = of_kind(&events, "service.call.cancel.sent");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["reason"], "interrupted");
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["reason"]),
        (&json!("unknown"), &json!("interrupted"))
    );

    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(2))).await;
    let mut call = session
        .interpreter()
        .submit("await asyncio.to_thread(lambda: err(svc.sleep, secs=60))")
        .unwrap_or_else(|e| panic!("{e}"));
    fixture.running(1).await;
    session.interpreter().interrupt(call.id(), false);
    let _ = within(call.outcome()).await;
    fixture.fired().await;
    stop(session).await;
    let events = taken(&mut events);
    let cancels = of_kind(&events, "service.call.cancel.sent");
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(cancels[0]["reason"], "deadline", "the worker was not woken");
}

/// At the close, a call in flight drains to the deadline, is then told to stop, and given its
/// grace: one that honors the token ends `raised`, and one that does not is abandoned,
/// `unknown`, and listed so; a call made after the close is refused, its service never asked.
#[tokio::test]
async fn the_close_drains_then_abandons() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(60))).await;
    let _call = session
        .interpreter()
        .submit("svc.sleep_honoring(secs=60)")
        .unwrap_or_else(|e| panic!("{e}"));
    fixture.running(1).await;
    let report = within(session.shutdown(Duration::from_millis(300))).await;
    assert_eq!(report.services.len(), 1, "{report:?}");
    let call = &report.services[0];
    assert_eq!(
        (call.outcome, call.category, call.reason),
        (
            ServiceOutcome::Raised,
            Some(ServiceErrorCategory::Cancelled),
            None
        )
    );
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let events = taken(&mut events);
    assert_eq!(
        of_kind(&events, "service.call.cancel.sent")[0]["reason"],
        "closed"
    );

    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(60))).await;
    let _call = session
        .interpreter()
        .submit("svc.sleep(secs=60)")
        .unwrap_or_else(|e| panic!("{e}"));
    fixture.running(1).await;
    let started = Instant::now();
    let report = within(session.shutdown(Duration::from_millis(300))).await;
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.services.len(), 1, "{report:?}");
    let call = &report.services[0];
    assert_eq!(
        (
            call.outcome,
            call.reason,
            call.method.as_str(),
            call.agent.as_str()
        ),
        (
            ServiceOutcome::Unknown,
            Some(ServiceReason::Closed),
            "sleep",
            "primary"
        )
    );
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    let events = taken(&mut events);
    let reported = of_kind(&events, "session.report");
    assert_eq!(reported[0]["services"][0]["outcome"], "unknown");
    assert_eq!(reported[0]["verdict"], "stopped_with_unknown");

    let fixture = Fixture::new();
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    let go = Flag::new();
    let mut late = session
        .interpreter()
        .submit(&format!("{}\nerr(svc.echo, x=1)", go.awaited()))
        .unwrap_or_else(|e| panic!("{e}"));
    session.close_admission();
    go.raise();
    match within(late.outcome()).await {
        Outcome::Ok(report) => assert_eq!(
            report.output,
            "refused admission_closed admission_closed None\n"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.calls(), 0);
    let report = stop(session).await;
    assert_eq!(report.services.len(), 1, "{report:?}");
    assert_eq!(
        report.services[0].reason,
        Some(ServiceReason::AdmissionClosed)
    );
}

/// A result larger than the policy's bound is not handed over: the caller is told its size,
/// and the event records that the service returned it.
#[tokio::test]
async fn a_result_past_the_bound_is_refused() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, ServicePolicy::default()).await;
    let said = output(
        &session,
        "try:\n    svc.big()\nexcept outrig.ServiceError as e:\n    print(e.outcome, e.category, e)",
    )
    .await;
    assert!(
        said.starts_with("refused result_too_large ")
            && said.contains("20971522 bytes")
            && said.contains("16777216"),
        "{said}"
    );
    stop(session).await;
    let events = taken(&mut events);
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["reason"], &ended["bytes"]),
        (
            &json!("refused"),
            &json!("result_too_large"),
            &json!(20_971_522)
        )
    );
    assert!(
        ended["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("returned 20971522 bytes")),
        "{ended}"
    );

    let roomy = ServicePolicy {
        result_max: 32 << 20,
        ..ServicePolicy::default()
    };
    let (session, _events) = served(&Fixture::new(), roomy).await;
    assert_eq!(
        output(&session, "print(len(svc.big()))").await,
        "20971520\n"
    );
    stop(session).await;
}

/// The fixture's credential appears in nothing agent code or the owner reads: not the object's
/// `repr` or `help`, not an exception, not an event, not the report.
#[tokio::test]
async fn internals_never_cross() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, quick(Duration::from_secs(1))).await;
    let mut read = String::new();
    for source in [
        "print(repr(svc), svc.methods)",
        "help(svc)",
        "help(svc.echo)",
        "err(svc.fail, category='failed')",
        "err(svc.echo, x='no')",
        "err(svc.sleep, secs=60)",
        "try:\n    svc.fail(category='refused')\nexcept outrig.ServiceError as e:\n    print(repr(e), str(e))",
        "err(svc.panic)",
        "print(runtime.bindings['svc'])",
    ] {
        read.push_str(&output(&session, source).await);
    }
    let report = stop(session).await;
    let events = taken(&mut events);
    assert!(read.contains("failed as asked"), "{read}");
    assert!(read.contains("raised failed None None"), "{read}");
    assert!(!read.contains(SECRET), "{read}");
    for event in &events {
        let text = serde_json::to_string(&event.payload).expect("encodes");
        assert!(!text.contains(SECRET), "{text}");
    }
    let report = serde_json::to_string(&report).expect("encodes");
    assert!(!report.contains(SECRET), "{report}");
}

/// `runtime.bindings` lists a service with its kind and its class; `outrig.bindings` reaches
/// the kernel's own object; a global is bound where the name is free, and not where a builtin
/// holds it.
#[tokio::test]
async fn a_service_is_listed_and_resolved() {
    let fixture = Fixture::new();
    let builder = builder()
        .serve("svc", Arc::clone(&fixture) as Arc<dyn Service>)
        .serve("print", Arc::clone(&fixture) as Arc<dyn Service>);
    let (session, _events) = served_with(builder, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "import builtins\nb = runtime.bindings['svc']\n\
             print(b.kind, b.type, b.description)\n\
             print(outrig.bindings['svc'] is svc, type(svc).__qualname__, \
             sorted(outrig.bindings))\n\
             print(print is builtins.print, type(outrig.bindings['print']).__name__)",
        )
        .await,
        "service outrig.services.svc The test service.\nTrue svc ['print', 'svc']\nTrue print\n"
    );
    stop(session).await;
}

/// A service whose description changes is told to the interpreter as the next generation: the
/// object it is reached by gains the new method, and a method held from the old description is
/// refused for the one it lost, its service never asked.
#[tokio::test]
async fn a_description_changes_under_the_session() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, ServicePolicy::default()).await;
    output(&session, "held = svc.echo").await;
    fixture.redescribe(|methods| {
        methods.retain(|method| method.name != "echo");
        methods.push(MethodDescription::new("new", "Newly here.", json!({})));
    });
    assert!(session.services().redescribe("svc"));
    assert!(
        !session.services().redescribe("svc"),
        "nothing changed since"
    );
    assert_eq!(
        output(
            &session,
            "print(svc.new())\nerr(held, x=1)\nprint(hasattr(svc, 'echo'), 'new' in svc.methods)",
        )
        .await,
        "{}\nrefused unknown_method unknown_method None\nFalse True\n"
    );
    assert_eq!(
        fixture
            .seen()
            .iter()
            .map(|(method, ..)| method.as_str())
            .collect::<Vec<_>>(),
        ["new"]
    );
    stop(session).await;
    let events = taken(&mut events);
    let described = &of_kind(&events, "service.described")[0];
    assert_eq!(described["generation"], 2);
    assert_eq!(
        described["changes"],
        json!({"added": ["new"], "removed": ["echo"], "changed": []})
    );
}

/// A description of 500 methods makes its class and a kernel's object of it quickly: the time
/// is printed for the record.
#[tokio::test]
async fn five_hundred_methods_are_cheap() {
    let methods = (0..500)
        .map(|i| {
            MethodDescription::new(
                format!("m{i}"),
                format!("Method {i}."),
                object(
                    json!({"a": {"type": "integer"}, "b": {"type": "string", "default": "x"}}),
                    &["a"],
                ),
            )
        })
        .collect();
    let fixture = Fixture::described(methods);
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    let timed = output(
        &session,
        "import time, outrig._services as S\n\
         entry = {'name': 'many', 'description': 'Many.', 'presenter': 'service', \
         'generation': 1, 'deadline': 600.0, 'methods': [\
         {'name': m.name, 'description': m.description, 'params': m.params, 'result': None} \
         for m in svc.methods.values()]}\n\
         start = time.perf_counter()\n\
         obj = S._made(S.generated(entry), 'many', None)\n\
         took = time.perf_counter() - start\n\
         print(len(obj.methods), svc.m499(a=1), round(took * 1000, 1))",
    )
    .await;
    let mut said = timed.split_whitespace();
    assert_eq!(said.next(), Some("500"), "{timed}");
    let ms: f64 = timed
        .split_whitespace()
        .last()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("{timed}"));
    eprintln!("a 500-method service's class and object: {ms} ms");
    assert!(ms < 2000.0, "{timed}");
    stop(session).await;
}

/// Each category a service raises reaches agent code by its name, as `raised`.
#[tokio::test]
async fn each_category_reaches_agent_code_by_name() {
    let fixture = Fixture::new();
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    let source: String = CATEGORIES
        .iter()
        .map(|(_, name)| format!("err(svc.fail, category={name:?})\n"))
        .collect();
    let expected: String = CATEGORIES
        .iter()
        .map(|(_, name)| format!("raised {name} None None\n"))
        .collect();
    assert_eq!(output(&session, &source).await, expected);
    stop(session).await;
}

/// A call naming an agent whose kernel the host did not open is answered at once, refused, and
/// reaches no service.
#[tokio::test]
async fn a_call_from_an_agent_the_host_did_not_open_is_refused() {
    let fixture = Fixture::new();
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "import __main__\n\
             a = __main__._ask_host({'t': 'service.call', 'agent': 'ghost', 'service': 'svc', \
             'method': 'echo', 'generation': 1, 'arguments': {'x': 1}})\n\
             print(a['error']['outcome'], a['error']['category'])",
        )
        .await,
        "refused unknown_agent\n"
    );
    assert_eq!(fixture.calls(), 0);
    let raised = raised(&session, "svc.call('echo', {'x': 1}, timeout=-1)").await;
    assert!(
        raised.contains("ValueError: timeout is a positive number"),
        "{raised}"
    );
    stop(session).await;
}

/// A service's name follows a binding's rule, and names one thing.
#[test]
fn a_service_name_is_checked_as_a_bindings() {
    let fixture: Arc<dyn Service> = Fixture::new();
    let refused = |builder: SessionBuilder| match builder.check() {
        Err(e) => e.to_string(),
        Ok(()) => panic!("the start was not refused"),
    };
    let said = refused(builder().serve("runtime", Arc::clone(&fixture)));
    assert!(said.contains("the service `runtime`"), "{said}");
    let said = refused(builder().serve("not a name", Arc::clone(&fixture)));
    assert!(said.contains("cannot take that name"), "{said}");
    let said = refused(
        builder()
            .serve("svc", Arc::clone(&fixture))
            .serve("svc", Arc::clone(&fixture)),
    );
    assert!(said.contains("given through `serve` already"), "{said}");
    let said = refused(
        builder()
            .bind("svc", Binding::new("os:getcwd", "A binding."))
            .serve("svc", Arc::clone(&fixture)),
    );
    assert!(said.contains("is a binding's name already"), "{said}");
    builder()
        .serve("svc", fixture)
        .check()
        .expect("a free name is served");
}

/// Arguments nested deeper than the host's parser reads leave the line readable: the call is
/// refused at once, as arguments that do not match, and its service never sees it.
#[tokio::test]
async fn arguments_the_host_cannot_read_are_refused_at_once() {
    let fixture = Fixture::new();
    let (session, mut events) = served(&fixture, ServicePolicy::default()).await;
    let started = Instant::now();
    assert_eq!(
        output(
            &session,
            "deep = 1\nfor _ in range(200):\n    deep = [deep]\n\
             err(svc.call, 'unchecked', {'a': deep}, timeout=1)",
        )
        .await,
        "refused invalid_arguments invalid_arguments \n"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.calls(), 0);
    assert_eq!(output(&session, "print(svc.echo(x=1))").await, "{'x': 1}\n");
    stop(session).await;
    let events = taken(&mut events);
    let ended = &of_kind(&events, "service.call.ended")[0];
    assert_eq!(
        (&ended["outcome"], &ended["reason"]),
        (&json!("refused"), &json!("invalid_arguments"))
    );
    assert_eq!(
        call_kinds(&events, &ended["call"]),
        ["service.call.received", "service.call.ended"]
    );
}

/// A schema whose `$ref`s chain further than the renderer follows still makes a class, and the
/// session's other services are bound beside it.
#[tokio::test]
async fn a_long_reference_chain_leaves_every_service_bound() {
    let mut defs = serde_json::Map::new();
    for i in 0..1100 {
        defs.insert(
            format!("d{i}"),
            json!({"$ref": format!("#/$defs/d{}", i + 1)}),
        );
    }
    defs.insert("d1100".into(), json!({"type": "integer"}));
    let chain = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/d0"}},
        "$defs": defs,
    });
    let long = Fixture::described(vec![
        MethodDescription::new("m", "Follow the chain.", chain.clone()).with_result(chain),
    ]);
    let builder = builder()
        .serve("chain", long as Arc<dyn Service>)
        .serve("svc", Fixture::new() as Arc<dyn Service>);
    let (session, _events) = served_with(builder, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "import inspect\nprint(chain.m(x=1), svc.echo(x=2), inspect.signature(chain.m))",
        )
        .await,
        "{'x': 1} {'x': 2} (*, x=...)\n"
    );
    stop(session).await;
}

/// Definitions that branch to one another, each an `anyOf` of three `$ref`s to the next, are
/// 3^20 paths in under 2 KiB; the renderer gives up on the type well inside the step's time,
/// rather than holding the reader of the host's lines while it walks them.
#[tokio::test]
async fn a_branching_reference_graph_renders_at_once() {
    let mut defs = serde_json::Map::new();
    for i in 0..20 {
        let next = json!({"$ref": format!("#/$defs/d{}", i + 1)});
        defs.insert(format!("d{i}"), json!({"anyOf": [next, next, next]}));
    }
    defs.insert("d20".into(), json!({"type": "integer"}));
    let graph = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/d0"}},
        "$defs": defs,
    });
    let branching = Fixture::described(vec![
        MethodDescription::new("m", "Walk the graph.", graph.clone()).with_result(graph),
    ]);
    let builder = builder().serve("graph", branching as Arc<dyn Service>);
    let (session, _events) = served_with(builder, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "import inspect\nprint(graph.m(x=1), inspect.signature(graph.m))",
        )
        .await,
        "{'x': 1} (*, x=...)\n"
    );
    stop(session).await;
}

/// A method whose schema names an argument `self` takes it as an argument, by its attribute
/// and through `method`, its receiver being positional only.
#[tokio::test]
async fn an_argument_named_self_is_an_argument() {
    let fixture = Fixture::new();
    let (session, _events) = served(&fixture, ServicePolicy::default()).await;
    assert_eq!(
        output(
            &session,
            "import inspect\nprint(svc.selfish(self=1), svc.method('selfish')(**{'self': 2}), \
             inspect.signature(svc.selfish))",
        )
        .await,
        "{'self': 1} {'self': 2} (**arguments)\n"
    );
    stop(session).await;
}
