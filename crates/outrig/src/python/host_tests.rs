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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;

use super::host::{
    Background, ContextChange, Counts, Interpreter, InterpreterError, Late, MESSAGE_MAX, Outcome,
    PRIMARY, Report, USER, Unknown,
};
use super::payload::PAYLOAD;
use super::testing::{
    Fake, HUNG_UP, connect, ok, round_trip, spawn, start_on_host, start_on_host_with, within,
};
use crate::events::{Events, kinds, of_kind, opened, recorded};

/// The message of the startup error `started` must be.
fn startup_error(started: Result<Interpreter, InterpreterError>) -> String {
    match started {
        Err(InterpreterError::Startup(message)) => message,
        Err(other) => panic!("expected a startup error, got: {other}"),
        Ok(_) => panic!("expected a startup error, but it started"),
    }
}

/// The reason of the refusal `result` must be.
fn refused<T>(result: Result<T, InterpreterError>) -> String {
    match result {
        Err(InterpreterError::Refused(reason)) => reason,
        Err(other) => panic!("expected a refusal, got: {other}"),
        Ok(_) => panic!("expected a refusal, but it went through"),
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

/// A listing cut short says so: how many names there are, and how many it
/// left out.
#[tokio::test]
async fn the_inventory_names_what_the_namespace_holds_and_counts_the_rest() {
    let interpreter = start_on_host().await;
    let bind = "answer = 42\nglobals().update({f'v{i:03}': i for i in range(250)})";
    assert_eq!(run(&interpreter, bind).await, ok(""));
    let inventory = within(interpreter.inventory()).await.expect("an inventory");
    assert!(
        inventory
            .globals
            .contains(&("answer".to_string(), "int".to_string())),
        "{inventory:?}"
    );
    assert_eq!(
        (inventory.globals.len(), inventory.total, inventory.more),
        (200, 251, 51)
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
    let message = startup_error(
        within(Interpreter::from_child(
            spawn(None).await,
            crate::events::Events::off(),
        ))
        .await,
    );
    assert!(
        message.contains("usage:") && message.contains("exit status: 2"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------- a fake transport

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
    fake.inventory(&json!(999)).await;
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

// ---------------------------------------------------------------------------- the user channel

/// A post is on the wire as soon as it is made, before anyone awaits it, and
/// behind what was sent before it. Its future is the interpreter's answer: how
/// many then wait, or why it was refused.
#[tokio::test]
async fn a_post_is_queued_when_made_and_answered_with_the_count() {
    let (interpreter, mut fake) = Fake::connected().await;
    let first = interpreter.post("user", "hello").expect("queued");
    let second = interpreter.post("user", "again").expect("queued");

    let request = fake.expect("msg").await;
    assert_eq!(request["agent"], PRIMARY);
    assert_eq!(request["channel"], "user");
    assert_eq!(request["body"], "hello");
    let again = fake.expect("msg").await;
    assert_eq!(again["body"], "again");

    fake.send(json!({"t": "msg", "agent": PRIMARY, "id": request["id"], "pending": 3}))
        .await;
    fake.send(json!({"t": "msg", "agent": PRIMARY, "id": again["id"], "error": "full"}))
        .await;
    assert_eq!(within(first).await.expect("delivered"), 3);
    assert_eq!(refused(within(second).await), "full");
}

/// A message past the bound is refused on the host, and never sent.
#[tokio::test]
async fn a_post_past_the_bound_is_refused_without_being_sent() {
    let (interpreter, mut fake) = Fake::connected().await;
    let reason = refused(
        interpreter
            .post("user", &"x".repeat(MESSAGE_MAX + 1))
            .map(|_| ()),
    );
    assert!(reason.contains("past the"), "{reason}");
    // The next request on the wire is the inventory, not the message.
    round_trip(&interpreter, &mut fake).await;
}

#[tokio::test]
async fn pending_counts_come_back_by_channel() {
    let (interpreter, mut fake) = Fake::connected().await;
    let (pending, ()) = tokio::join!(within(interpreter.pending()), async {
        let request = fake.expect("pending").await;
        let counts = json!({"user": {"pending": 2, "delivered": 5}});
        fake.send(
            json!({"t": "pending", "agent": PRIMARY, "id": request["id"], "channels": counts}),
        )
        .await;
    });
    let pending = pending.expect("answered");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending["user"],
        Counts {
            pending: 2,
            delivered: 5
        }
    );
}

/// What the agent sends the user arrives in the order it was sent. A send on
/// another channel, or one no subscriber is there for, goes nowhere, and is
/// acknowledged at once so the agent is not held back waiting on it. The
/// subscriber hears the interpreter exit as the end of its stream.
#[tokio::test]
async fn sends_reach_the_subscriber_in_order_until_the_interpreter_exits() {
    let (interpreter, mut fake) = Fake::connected().await;
    fake.send(json!({"t": "send", "agent": PRIMARY, "channel": "user", "body": "unheard"}))
        .await;
    // Acknowledged at once: nothing on the host will receive it, and the
    // agent must not wait for that.
    assert_eq!(fake.expect("received").await["channel"], "user");

    let mut sent = interpreter.subscribe();
    for body in ["one", "two"] {
        fake.send(json!({"t": "send", "agent": PRIMARY, "channel": "user", "body": body}))
            .await;
    }
    fake.send(json!({"t": "send", "agent": PRIMARY, "channel": "work", "body": "elsewhere"}))
        .await;
    fake.send(json!({"t": "send", "agent": PRIMARY, "channel": "user", "body": "three"}))
        .await;
    for want in ["one", "two", "three"] {
        assert_eq!(within(sent.recv()).await.as_deref(), Some(want));
    }

    drop(fake);
    assert_eq!(within(sent.recv()).await, None);
    assert_eq!(
        &*gone(interpreter.post("user", "anyone?").map(|_| ())),
        HUNG_UP
    );
    assert!(
        within(interpreter.subscribe().recv()).await.is_none(),
        "a subscriber after the exit hears nothing"
    );
}

/// A message the agent sent is acknowledged when the user receives it, not
/// when it arrives: the acknowledgment is what lets the agent send past its
/// window, so what the host holds is bounded by the user keeping up.
#[tokio::test]
async fn a_send_is_acknowledged_when_the_user_receives_it() {
    let (interpreter, mut fake) = Fake::connected().await;
    let mut sent = interpreter.subscribe();
    for body in ["one", "two"] {
        fake.send(json!({"t": "send", "agent": PRIMARY, "channel": "user", "body": body}))
            .await;
    }
    // Both arrived, and nothing was acknowledged.
    round_trip(&interpreter, &mut fake).await;

    assert_eq!(within(sent.recv()).await.as_deref(), Some("one"));
    let ack = fake.expect("received").await;
    assert_eq!(ack["agent"], PRIMARY);
    assert_eq!(ack["channel"], "user");
    // One receive, one acknowledgment.
    round_trip(&interpreter, &mut fake).await;
}

// ---------------------------------------------------------------------------- the conversation

/// A turn goes out under its own id, with the fields it was given, and nothing
/// answers it. Once the interpreter has gone, nothing more is pushed.
#[tokio::test]
async fn a_turn_is_pushed_under_its_id_and_not_once_the_interpreter_is_gone() {
    let (interpreter, mut fake) = Fake::connected().await;
    interpreter.push_turn(0, json!({"round": 1, "text": "hello"}));
    let turn = fake.expect("turn").await;
    assert_eq!(
        turn,
        json!({"t": "turn", "agent": PRIMARY, "id": 0, "round": 1, "text": "hello"})
    );

    drop(fake);
    let cause = gone(within(interpreter.pending()).await);
    assert_eq!(&*cause, HUNG_UP);
    // Nothing to write to, and nothing panics for want of it.
    interpreter.push_turn(1, json!({"round": 1}));
}

/// What `on_context` registered is handed each promotion and demotion as it
/// arrives, in order and as the agent named it: what a change names is the
/// store's to judge.
#[tokio::test]
async fn promotions_and_demotions_are_handed_over_in_order() {
    let (interpreter, mut fake) = Fake::connected().await;
    let (changed, handed) = recorder();
    interpreter.on_context(changed);
    fake.send(json!({"t": "promote", "agent": PRIMARY, "turns": [2, 0, 1000]}))
        .await;
    fake.send(json!({"t": "demote", "agent": PRIMARY, "turns": [0]}))
        .await;
    fake.send(json!({"t": "promote", "agent": PRIMARY, "turns": [2]}))
        .await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(
        *handed.lock().expect("handed"),
        [
            ContextChange::Promote(vec![2, 0, 1000]),
            ContextChange::Demote(vec![0]),
            ContextChange::Promote(vec![2]),
        ]
    );
}

/// A promotion or demotion made by an execution has been handed over by the
/// time that execution's outcome is here: its line is written ahead of the
/// result, and the host reads in order. That is what lets it reach the round's
/// next model call.
#[tokio::test]
async fn a_change_is_handed_over_before_the_execution_that_made_it_ends() {
    let interpreter = start_on_host().await;
    let (changed, handed) = recorder();
    interpreter.on_context(changed);
    interpreter.push_turn(
        0,
        json!({"round": 1, "prompt": "go", "text": "", "calls": []}),
    );
    let outcome = run(
        &interpreter,
        "runtime.context.promote(runtime.history.turns[0])",
    )
    .await;
    assert_eq!(outcome, ok(""));
    assert_eq!(
        *handed.lock().expect("handed"),
        [ContextChange::Promote(vec![0])]
    );
    let outcome = run(&interpreter, "runtime.context.demote(0)").await;
    assert_eq!(outcome, ok(""));
    assert_eq!(
        handed.lock().expect("handed").last(),
        Some(&ContextChange::Demote(vec![0]))
    );
}

/// Every change a handler was handed, in order.
type Handed = Arc<Mutex<Vec<ContextChange>>>;

/// A context handler that records what it is handed.
fn recorder() -> (impl Fn(ContextChange) + Send + Sync + 'static, Handed) {
    let handed = Handed::default();
    let record = Arc::clone(&handed);
    (
        move |change| record.lock().expect("handed").push(change),
        handed,
    )
}

// ---------------------------------------------------------------------------- the event log

/// A log in a directory of its own, and the directory.
async fn recording() -> (tempfile::TempDir, Events) {
    let dir = tempfile::tempdir().expect("a log dir");
    let events = opened(dir.path()).await;
    (dir, events)
}

/// An execution that runs out of memory is recorded as one, beside the result
/// that says so.
#[tokio::test]
async fn an_execution_that_ran_out_of_memory_is_recorded_as_such() {
    let (dir, events) = recording().await;
    let (interpreter, mut fake) = Fake::connected_with(events.clone()).await;
    let mut execution = interpreter.submit("x = [0] * 10**12").expect("submitted");
    let id = execution.id();
    fake.exec(id).await;
    fake.result(
        id,
        json!({
            "status": "error",
            "error": "Traceback (most recent call last):\n  File \"<agent>\", line 1\nMemoryError\n",
            "raised": "MemoryError",
        }),
    )
    .await;
    within(execution.outcome()).await;
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    assert_eq!(
        kinds(&records),
        ["exec.submitted", "exec.completed", "memory.exhausted"]
    );
    let completed = of_kind(&records, "exec.completed")[0];
    assert_eq!(completed["status"], "error");
    assert!(completed["duration"].as_f64().is_some(), "{completed}");
    assert_eq!(
        *of_kind(&records, "memory.exhausted")[0],
        json!({"execid": id})
    );
}

/// An interpreter that exits while an execution runs: the execution recorded
/// as lost, then the exit and what explains it.
#[tokio::test]
async fn an_interpreter_that_exits_is_recorded_with_what_it_was_running() {
    let (dir, events) = recording().await;
    let (interpreter, mut fake) = Fake::connected_with(events.clone()).await;
    let mut execution = interpreter.submit("work()").expect("submitted");
    fake.exec(execution.id()).await;
    drop(fake);
    within(execution.outcome()).await;
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    assert_eq!(
        kinds(&records),
        ["exec.submitted", "exec.completed", "interpreter.exited"]
    );
    assert_eq!(of_kind(&records, "exec.completed")[0]["status"], "lost");
    assert_eq!(
        *of_kind(&records, "interpreter.exited")[0],
        json!({"cause": HUNG_UP})
    );
    assert!(
        records[2].get("subject").is_none(),
        "the interpreter is no one agent's"
    );
}

/// A message the channel refuses is recorded refused, under the id it was
/// sent under.
#[tokio::test]
async fn a_post_the_channel_refuses_is_recorded_refused() {
    let (dir, events) = recording().await;
    let (interpreter, mut fake) = Fake::connected_with(events.clone()).await;
    let answer = interpreter.post(USER, "hi").expect("queued");
    let request = fake.expect("msg").await;
    fake.send(json!({"t": "msg", "agent": PRIMARY, "id": request["id"], "error": "full"}))
        .await;
    assert_eq!(refused(within(answer).await), "full");
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    assert_eq!(kinds(&records), ["message.sent", "message.refused"]);
    let refusal = of_kind(&records, "message.refused")[0];
    assert_eq!(refusal["message"], request["id"]);
    assert_eq!(refusal["reason"], "full");
}

/// What reaches the interpreter's stderr is recorded: its own diagnostics as
/// such, and output no execution wrote -- which no result carries -- as
/// unattributed, rather than vanishing.
#[tokio::test]
async fn output_no_execution_wrote_is_recorded_unattributed() {
    let (dir, events) = recording().await;
    let interpreter = start_on_host_with(events.clone()).await;
    run(
        &interpreter,
        "import os\nos.write(1, b'RAW-ONE\\n')\nos.write(2, b'outrig-interpreter: a note\\n')",
    )
    .await;
    let path = dir.path().join(crate::events::EVENTS_LOG);
    within(async {
        while !std::fs::read_to_string(&path)
            .is_ok_and(|text| text.contains("RAW-ONE") && text.contains("a note"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    assert!(
        of_kind(&records, "output.unattributed").contains(&&json!({"text": "RAW-ONE"})),
        "{records:#?}"
    );
    assert!(
        of_kind(&records, "interpreter.diagnostic").contains(&&json!({"text": "a note"})),
        "{records:#?}"
    );
}

/// The interpreter says when the agent's code takes a message only once the
/// host asks it to, which only a recording host does.
#[tokio::test]
async fn a_take_is_reported_only_to_a_host_that_observes() {
    for observing in [false, true] {
        let (dir, events) = recording().await;
        let interpreter = if observing {
            start_on_host_with(events.clone()).await
        } else {
            start_on_host().await
        };
        within(interpreter.post(USER, "hi").expect("queued"))
            .await
            .expect("taken");
        run(
            &interpreter,
            "(await runtime.channels['user'].receive()).body",
        )
        .await;
        round_trip_real(&interpreter).await;
        events.close().await.expect("nothing lost");
        let records = if observing {
            recorded(dir.path())
        } else {
            Vec::new()
        };
        assert_eq!(
            of_kind(&records, "message.received").len(),
            usize::from(observing),
            "{records:#?}"
        );
    }
}

/// Wait for the reader to have read every line written before now: an
/// inventory is answered on the agent's loop, after them.
async fn round_trip_real(interpreter: &Interpreter) {
    within(interpreter.inventory()).await.expect("an inventory");
}

// ---------------------------------------------------------------------------- through podman

#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use crate::container::{Container, ContainerLaunchSpec};
    use crate::image::ImageTag;
    use crate::python::payload;
    use crate::python::testing::{ALPINE, PIP_PROBE, pull_alpine};

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
        let interpreter = within(Interpreter::start(&container, Events::off()))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run(&interpreter, "1 + 1").await, ok("2\n"));
        // The memory ceiling, read from what the container sees -- its cgroup
        // and its memory -- and inherited by a program the interpreter starts.
        let ceiling = "import resource, subprocess\n\
                       soft = resource.getrlimit(resource.RLIMIT_DATA)[0]\n\
                       shell = subprocess.run(['sh', '-c', 'ulimit -d'], capture_output=True)\n\
                       print(soft != resource.RLIM_INFINITY, int(shell.stdout) == soft // 1024)";
        assert_eq!(run(&interpreter, ceiling).await, ok("True True\n"));
        drop(interpreter);
        container
            .stop(Duration::from_secs(2))
            .await
            .expect("the container stops");
    }

    /// The payload is read-only in a session, so a plain `pip install` puts a
    /// package in the user site -- which the interpreter reads, so it imports
    /// at once, with no restart and no `--user`.
    #[tokio::test]
    async fn a_plain_pip_install_imports_at_once_in_a_session_container() {
        let container = alpine(true).await;
        let interpreter = within(Interpreter::start(&container, Events::off()))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(run(&interpreter, PIP_PROBE).await, ok(""));
        let installed = "pip('install', '--no-index', wheel('/tmp', 'outrig_probe'))\n\
                         import outrig_probe\n\
                         home = os.path.expanduser('~/.local/')\n\
                         print(outrig_probe.ANSWER, outrig_probe.__file__.startswith(home))";
        assert_eq!(run(&interpreter, installed).await, ok("42 True\n"));
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
        let message = startup_error(within(Interpreter::start(&container, Events::off())).await);
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
