//! The host's half, driven two ways. Against the payload this build embedded,
//! run on the host as `interpreter_tests.rs` runs it, for what the real
//! interpreter does. Against a fake transport, for what it will not do on
//! request: withhold a reply, answer late, or answer for the wrong id. The e2e
//! pair at the end starts it through podman, in an image with and without the
//! payload.
//!
//! Nothing waits on a wall clock. Where a test needs a wait to give up, time
//! is paused and tokio advances it once nothing else can run; where it needs
//! the reader to have caught up, an inventory round-trip orders it, since
//! replies are read in the order they were written.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

use super::host::{
    Background, ExecId, Interpreter, InterpreterError, Late, Outcome, PRIMARY, Report, Unknown,
};
use super::payload::PAYLOAD;
use super::testing::{ok, spawn, start_on_host, within};

/// What the fake transport says about itself once it closes.
const HUNG_UP: &str = "the fake hung up";

/// The message of the startup error `started` must be.
fn startup_error(started: Result<Interpreter, InterpreterError>) -> String {
    match started {
        Err(InterpreterError::Startup(message)) => message,
        Err(other) => panic!("expected a startup error, got: {other}"),
        Ok(_) => panic!("expected a startup error, but it started"),
    }
}

/// The cause of the `Gone` that `result` must be.
fn gone<T>(result: Result<T, InterpreterError>) -> Arc<str> {
    match result {
        Err(InterpreterError::Gone(cause)) => cause,
        Err(other) => panic!("expected the interpreter gone, got: {other}"),
        Ok(_) => panic!("expected the interpreter gone, but it answered"),
    }
}

// ---------------------------------------------------------------------------- the real payload

async fn run(interpreter: &Interpreter, source: &str) -> Outcome {
    let mut execution = interpreter.submit(source).expect("submitted");
    within(execution.outcome()).await
}

#[tokio::test]
async fn one_plus_one_comes_back_as_its_echo() {
    let interpreter = start_on_host().await;
    assert_eq!(run(&interpreter, "1 + 1").await, ok("2\n"));
}

/// The version a startup line reports is the one the interpreter said it is,
/// and that is the one this build pinned.
#[tokio::test]
async fn the_version_it_greets_with_is_the_pinned_one() {
    let interpreter = start_on_host().await;
    let version = interpreter.version();
    assert!(
        version.starts_with("3.") && PAYLOAD.starts_with(&format!("cpython-{version}+")),
        "greeted with {version:?}; the payload is {PAYLOAD:?}"
    );
}

/// The point of the outcome split: code that raises is a result the model
/// can act on, not a transport failure.
#[tokio::test]
async fn a_raise_is_an_error_result_and_the_interpreter_carries_on() {
    let interpreter = start_on_host().await;
    let Outcome::Error { report, traceback } = run(&interpreter, "print('before')\n1 / 0").await
    else {
        panic!("expected an error result");
    };
    assert_eq!(report.output, "before\n");
    assert!(
        traceback.contains("<execution>") && traceback.contains("ZeroDivisionError"),
        "{traceback}"
    );
    assert_eq!(run(&interpreter, "1 + 1").await, ok("2\n"));
}

/// The rest of a result, as the real interpreter fills it in: output past
/// the bound is counted, and what a task an earlier execution started printed
/// arrives with a later result, billed to the execution that started it.
#[tokio::test]
async fn a_result_counts_what_it_dropped_and_carries_earlier_output() {
    let interpreter = start_on_host().await;
    let Outcome::Ok(flooded) = run(&interpreter, "print('x' * 20000)").await else {
        panic!("expected a clean run");
    };
    assert_eq!(flooded.output, "x".repeat(16 * 1024));
    assert_eq!(flooded.dropped, 20_001 - 16 * 1024);

    let mut starter = interpreter
        .submit(
            "go = asyncio.Event()\n\
             async def later():\n    await go.wait()\n    print('from the starter')\n\
             task = asyncio.create_task(later())",
        )
        .expect("submitted");
    let starter_id = starter.id();
    assert_eq!(within(starter.outcome()).await, ok(""));
    assert_eq!(
        run(&interpreter, "go.set()\nawait task").await,
        Outcome::Ok(Report {
            background: vec![Background {
                id: starter_id,
                output: "from the starter\n".to_string(),
                dropped: 0,
            }],
            ..Report::default()
        })
    );
}

#[tokio::test]
async fn the_inventory_names_what_the_namespace_holds() {
    let interpreter = start_on_host().await;
    assert_eq!(run(&interpreter, "answer = 42").await, ok(""));
    let inventory = within(interpreter.inventory()).await.expect("an inventory");
    assert!(
        inventory
            .globals
            .contains(&("answer".to_string(), "int".to_string())),
        "{inventory:?}"
    );
}

/// The confirmed-exit half of `unknown`: the execution is over, and the
/// interpreter with it.
#[tokio::test]
async fn an_interpreter_that_exits_mid_execution_is_an_exit_not_an_error() {
    let interpreter = start_on_host().await;
    let mut execution = interpreter
        .submit("import os\nos._exit(3)")
        .expect("submitted");
    let Outcome::Unknown(Unknown::Exited { id, cause }) = within(execution.outcome()).await else {
        panic!("expected an exit");
    };
    assert_eq!(id, execution.id());
    assert!(cause.contains("exit status: 3"), "{cause}");
    assert_eq!(gone(interpreter.submit("1 + 1")), cause);
}

/// The interpreter's own stderr is what explains a failed start.
#[tokio::test]
async fn an_interpreter_that_cannot_start_says_why() {
    let message = startup_error(within(Interpreter::from_child(spawn(None).await)).await);
    assert!(
        message.contains("usage:") && message.contains("exit status: 2"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------- a fake transport

/// The interpreter's end of a transport, scripted by the test.
struct Fake {
    requests: Lines<BufReader<ReadHalf<DuplexStream>>>,
    replies: WriteHalf<DuplexStream>,
}

type HostEnd = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

impl Fake {
    fn pair() -> (Self, HostEnd) {
        let (host, fake) = tokio::io::duplex(1 << 16);
        let (requests, replies) = tokio::io::split(fake);
        let fake = Self {
            requests: BufReader::new(requests).lines(),
            replies,
        };
        (fake, tokio::io::split(host))
    }

    /// A handle connected to a fake that has greeted.
    async fn connected() -> (Interpreter, Self) {
        let (mut fake, host) = Self::pair();
        fake.send(json!({"t": "ready", "agent": PRIMARY, "version": "3.13"}))
            .await;
        let interpreter = within(connect(host))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        (interpreter, fake)
    }

    async fn send(&mut self, message: Value) {
        self.replies
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("the host reads it");
    }

    async fn next(&mut self) -> Value {
        let line = within(self.requests.next_line())
            .await
            .expect("the host's requests")
            .expect("a request");
        serde_json::from_str(&line).expect("a request is JSON")
    }

    /// Read the next request, which must be execution `id`'s.
    async fn exec(&mut self, id: ExecId) {
        let request = self.next().await;
        assert!(
            request["t"] == "exec" && request["id"] == json!(id),
            "expected execution {id}, got {request}"
        );
    }

    async fn result(&mut self, id: ExecId, fields: Value) {
        let mut result = json!({
            "t": "result", "agent": PRIMARY, "id": id,
            "output": "", "dropped": 0, "error": null, "background": [],
        });
        for (key, value) in fields.as_object().expect("an object") {
            result[key] = value.clone();
        }
        self.send(result).await;
    }

    async fn ok(&mut self, id: ExecId, output: &str) {
        self.result(id, json!({"status": "ok", "output": output}))
            .await;
    }
}

async fn connect((replies, requests): HostEnd) -> Result<Interpreter, InterpreterError> {
    Interpreter::connect(replies, requests, async { HUNG_UP.to_string() }).await
}

/// An inventory, answered by the fake. The next request the fake sees must be
/// it, and once the host has its reply it has read every line before it.
async fn round_trip(interpreter: &Interpreter, fake: &mut Fake) {
    let answer = async {
        let request = fake.next().await;
        assert_eq!(request["t"], "inv", "expected the inventory, got {request}");
        let id = request["id"].clone();
        fake.send(json!({"t": "inv", "agent": PRIMARY, "id": id, "globals": []}))
            .await;
    };
    let (inventory, ()) = tokio::join!(within(interpreter.inventory()), answer);
    inventory.expect("an inventory");
}

/// The acceptance case for `unknown`: an unanswered execution keeps its slot
/// and its id, and the reply, when it comes, is its own.
#[tokio::test(start_paused = true)]
async fn an_unanswered_execution_keeps_its_slot_and_its_late_reply() {
    let (interpreter, mut fake) = Fake::connected().await;

    let mut first = interpreter
        .submit("await never_resolves()")
        .expect("submitted");
    let id = first.id();
    assert!(first.queued(), "a free slot takes the source");
    fake.exec(id).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(60), first.outcome())
            .await
            .is_err(),
        "the fake withheld the reply"
    );
    assert_eq!(
        first.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id })
    );

    // The slot is still the first's. The host refuses without writing, so the
    // next thing on the wire is the inventory.
    let mut second = interpreter.submit("1 + 1").expect("submitted");
    assert!(!second.queued(), "a refused submission never went to run");
    assert_eq!(
        within(second.outcome()).await,
        Outcome::Refused { holder: id }
    );
    round_trip(&interpreter, &mut fake).await;

    // The late reply is a new observation of the first.
    fake.ok(id, "late\n").await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(
        interpreter.take_late(),
        [Late {
            id,
            outcome: ok("late\n")
        }]
    );
    assert_eq!(
        within(second.outcome()).await,
        Outcome::Refused { holder: id }
    );

    // The slot is free. A stray reply for the first id, arriving while the
    // third runs, is not the third's.
    let mut third = interpreter.submit("'three'").expect("submitted");
    assert_ne!(third.id(), id);
    fake.exec(third.id()).await;
    fake.ok(id, "stray\n").await;
    fake.ok(third.id(), "'three'\n").await;
    assert_eq!(within(third.outcome()).await, ok("'three'\n"));
    assert!(interpreter.take_late().is_empty());
}

#[tokio::test]
async fn a_closed_transport_is_an_exit_and_nothing_more_is_sent() {
    let (interpreter, mut fake) = Fake::connected().await;
    let mut execution = interpreter.submit("print('hi')").expect("submitted");
    let id = execution.id();
    fake.exec(id).await;
    drop(fake);

    assert_eq!(
        within(execution.outcome()).await,
        Outcome::Unknown(Unknown::Exited {
            id,
            cause: HUNG_UP.into()
        })
    );
    assert_eq!(&*gone(interpreter.submit("1")), HUNG_UP);
    assert_eq!(&*gone(within(interpreter.inventory()).await), HUNG_UP);
}

/// A result is never lost to a handle nobody reads: dropped before the reply,
/// dropped after it, or given up on after it had already arrived.
#[tokio::test]
async fn a_reply_nobody_read_is_kept_as_late() {
    let (interpreter, mut fake) = Fake::connected().await;

    let before = interpreter.submit("'one'").expect("submitted");
    let one = before.id();
    fake.exec(one).await;
    drop(before);
    fake.ok(one, "'one'\n").await;
    round_trip(&interpreter, &mut fake).await;

    let after = interpreter.submit("'two'").expect("submitted");
    let two = after.id();
    fake.exec(two).await;
    fake.ok(two, "'two'\n").await;
    round_trip(&interpreter, &mut fake).await;
    drop(after);

    assert_eq!(
        interpreter.take_late(),
        [
            Late {
                id: one,
                outcome: ok("'one'\n")
            },
            Late {
                id: two,
                outcome: ok("'two'\n")
            },
        ]
    );
    assert!(interpreter.take_late().is_empty(), "each is reported once");

    let given_up = interpreter.submit("'three'").expect("submitted");
    fake.exec(given_up.id()).await;
    fake.ok(given_up.id(), "'three'\n").await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(given_up.stop_waiting(), ok("'three'\n"));
    assert!(interpreter.take_late().is_empty());
}

/// Each status the interpreter sends, as the host reports it.
#[tokio::test]
async fn the_interpreters_results_decode_into_outcomes() {
    let (interpreter, mut fake) = Fake::connected().await;

    let mut ran = interpreter.submit("x").expect("submitted");
    let id = ran.id();
    fake.exec(id).await;
    fake.result(
        id,
        json!({
            "status": "ok", "output": "x\n", "dropped": 5,
            "background": [{"id": id, "output": "bg", "dropped": 2}],
        }),
    )
    .await;
    assert_eq!(
        within(ran.outcome()).await,
        Outcome::Ok(Report {
            output: "x\n".to_string(),
            dropped: 5,
            background: vec![Background {
                id,
                output: "bg".to_string(),
                dropped: 2
            }],
        })
    );

    let mut raised = interpreter.submit("1 / 0").expect("submitted");
    fake.exec(raised.id()).await;
    fake.result(
        raised.id(),
        json!({"status": "error", "output": "partial", "error": "ZeroDivisionError"}),
    )
    .await;
    assert_eq!(
        within(raised.outcome()).await,
        Outcome::Error {
            report: Report {
                output: "partial".to_string(),
                ..Report::default()
            },
            traceback: "ZeroDivisionError".to_string(),
        }
    );

    // The host gates the slot before sending, so the interpreter refusing is
    // a disagreement; it is still reported as the refusal it is.
    let mut refused = interpreter.submit("2").expect("submitted");
    fake.exec(refused.id()).await;
    fake.result(refused.id(), json!({"status": "refused", "holder": 77}))
        .await;
    let Outcome::Refused { holder } = within(refused.outcome()).await else {
        panic!("expected a refusal");
    };
    assert_eq!(holder.to_string(), "77");
}

/// What the host cannot place is logged and skipped, and correlation carries
/// on: a malformed line, a kind from a newer interpreter, a reply for an agent
/// this host never opened, an inventory nobody asked for.
#[tokio::test]
async fn lines_the_host_cannot_place_are_ignored() {
    let (interpreter, mut fake) = Fake::connected().await;
    let mut execution = interpreter.submit("1").expect("submitted");
    let id = execution.id();
    fake.exec(id).await;

    fake.replies
        .write_all(b"not json\n\n[1]\n")
        .await
        .expect("written");
    fake.send(json!({"t": "from-a-newer-interpreter", "agent": PRIMARY}))
        .await;
    fake.send(json!({"t": "ready", "agent": "other"})).await;
    fake.send(json!({
        "t": "result", "agent": "other", "id": id, "status": "ok", "output": "not ours",
    }))
    .await;
    fake.send(json!({"t": "inv", "agent": PRIMARY, "id": 999, "globals": []}))
        .await;
    fake.ok(id, "1\n").await;

    assert_eq!(within(execution.outcome()).await, ok("1\n"));
}

#[tokio::test(start_paused = true)]
async fn an_interpreter_that_never_greets_is_a_startup_error() {
    let (_fake, host) = Fake::pair();
    let message = startup_error(connect(host).await);
    assert!(
        message.contains("did not report ready within 30s") && message.contains(HUNG_UP),
        "{message}"
    );
}

#[tokio::test]
async fn a_greeting_that_is_not_the_primarys_ready_is_a_startup_error() {
    for greeting in [
        json!({"t": "ready", "agent": "someone-else", "version": "3.13"}),
        json!({"t": "ready", "agent": PRIMARY}),
        json!({"t": "result", "agent": PRIMARY, "id": 1, "status": "ok"}),
    ] {
        let (mut fake, host) = Fake::pair();
        fake.send(greeting.clone()).await;
        let message = startup_error(within(connect(host)).await);
        assert!(message.contains("greeted with"), "{greeting}: {message}");
    }
}

// ---------------------------------------------------------------------------- through podman

#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use crate::container::{Container, ContainerLaunchSpec};
    use crate::image::ImageTag;
    use crate::python::payload;
    use crate::python::testing::{ALPINE, pull_alpine};

    /// A running alpine with its user bootstrapped, and the payload mounted
    /// as every session mounts it when `with_payload`.
    async fn alpine(with_payload: bool) -> Container {
        pull_alpine().await;
        let mut launch = ContainerLaunchSpec::default();
        if with_payload {
            launch
                .mounts
                .push(payload::mount().await.expect("the payload"));
        }
        let mut container = Container::start(&ImageTag::new(ALPINE), launch)
            .await
            .expect("the container starts");
        container
            .bootstrap_user()
            .await
            .expect("the user bootstraps");
        container
    }

    #[tokio::test]
    async fn the_interpreter_starts_in_a_session_container_and_answers() {
        let container = alpine(true).await;
        let interpreter = within(Interpreter::start(&container))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run(&interpreter, "1 + 1").await, ok("2\n"));
        drop(interpreter);
        container
            .stop(Duration::from_secs(2))
            .await
            .expect("the container stops");
    }

    /// podman's own error is the cause, and it arrives when podman gives up
    /// rather than when the ready timeout would.
    #[tokio::test]
    async fn an_image_without_the_payload_is_a_startup_error_naming_the_cause() {
        let container = alpine(false).await;
        let message = startup_error(within(Interpreter::start(&container)).await);
        assert!(
            message.contains(&format!("{}/bin/python3", payload::PAYLOAD_MOUNT))
                && !message.contains("did not report ready"),
            "{message}"
        );
        container
            .stop(Duration::from_secs(2))
            .await
            .expect("the container stops");
    }
}
