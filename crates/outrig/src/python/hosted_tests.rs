//! Hosted calls through the relay in `hosted.rs`, end to end on the host: an interpreter and the
//! bindings process started by [`Relay`], the fixture library in the process, and the record each
//! request leaves in the session's stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};

use super::host::PRIMARY;
use super::hosted;
use super::relay::{Relay, TIMEOUT, fixture_dir};
use super::testing::{Flag, Start, eventually, py};
use crate::events::StreamBuilder;
use crate::harness::HostedCall;
use crate::harness::event::{
    Event, HostedOutcome, HostedReason, Operation, Payload, Received, ReceivedBy, RequestId,
    Subscription,
};

const FACTORY: &str = "outrig_fixture:make";

/// Python, after [`with_stub`]'s, for frames of a test's own on the pool's one connection to
/// `fx`, `conn`: `forge(seq, label)` sends a read of the root's `public` as request `seq` --
/// labeled as the interpreter labels one, labeled as request `label` when that is a number, or not
/// labeled when it is `False` -- and returns the result its reply will set; `cancel(*seqs)` sends
/// the cancel line an interrupted caller would.
const FORGE: &str = r#"
import rpyc, __main__
from rpyc.core import consts
pool = K._hosted['fx']
conn = pool.connections[0]
root_id = object.__getattribute__(pool._root, "____id_pack__")
def forge(seq, label=True):
    res = rpyc.core.async_.AsyncResult(conn)
    conn._request_callbacks[seq] = res
    boxed = (consts.LABEL_TUPLE,
             ((consts.LABEL_LOCAL_REF, root_id), (consts.LABEL_VALUE, "public")))
    number = seq if label is True else label
    tag = {"request": number, "op": "getattr", "member": "public", "target": None, "exec": None}
    conn._send(
        consts.MSG_REQUEST, seq, (consts.HANDLE_GETATTR, boxed), None if label is False else tag
    )
    return res
def cancel(*seqs):
    __main__._send({"t": "rpc", "agent": "primary", "binding": "fx", "id": conn.number,
                    "cancel": list(seqs)})
"#;

/// Python that binds `K` to `agent`'s kernel and `stub` to its stub for binding `fx`, with the
/// fixture importable, then runs `body`.
fn with_stub_in(agent: &str, body: &str) -> String {
    format!(
        "import __main__, asyncio, os, sys, time\nsys.path.append({:?})\n\
         K = __main__._kernels[{agent:?}]\nstub = K.stub('fx')\n{}",
        fixture_dir(),
        py(body)
    )
}

/// Python for `asyncio` tasks, one per flag in `starts`, each holding a call to the fixture in a
/// worker until `release` is raised, kept alive in `builtins.held`.
fn held_calls(starts: &[Flag], release: &Flag) -> String {
    let tasks: String = starts
        .iter()
        .enumerate()
        .map(|(i, start)| {
            format!(
                "    asyncio.create_task(asyncio.to_thread(\n        \
                 lambda: stub.record(0, 'c{i}', started={}, until={}))),\n",
                start.py(),
                release.py()
            )
        })
        .collect();
    format!("import builtins\nbuiltins.held = [\n{tasks}]")
}

fn with_stub(body: &str) -> String {
    with_stub_in("primary", body)
}

/// Each request's events, by id, in the order they were published.
fn by_request(events: &[Arc<Event>]) -> BTreeMap<RequestId, Vec<Arc<Event>>> {
    let mut requests: BTreeMap<RequestId, Vec<Arc<Event>>> = BTreeMap::new();
    for event in events {
        let id = match &event.payload {
            Payload::HostedReceived { request, .. }
            | Payload::HostedDispatched { request, .. }
            | Payload::HostedEnded { request, .. } => *request,
            _ => continue,
        };
        requests.entry(id).or_default().push(Arc::clone(event));
    }
    requests
}

/// The request's operation and member, from its receipt.
fn asked(events: &[Arc<Event>]) -> (Operation, Option<String>) {
    match &events[0].payload {
        Payload::HostedReceived {
            operation, member, ..
        } => (*operation, member.clone()),
        other => panic!("a request whose first event is {other:?}"),
    }
}

/// The request's outcome and reason, once it has ended.
fn outcome(events: &[Arc<Event>]) -> Option<(HostedOutcome, Option<HostedReason>)> {
    match &events.last()?.payload {
        Payload::HostedEnded {
            outcome, reason, ..
        } => Some((*outcome, *reason)),
        _ => None,
    }
}

/// The outcome and reason of a request that has ended.
fn ended(events: &[Arc<Event>]) -> (HostedOutcome, Option<HostedReason>) {
    outcome(events).unwrap_or_else(|| panic!("a request that has not ended: {events:?}"))
}

/// The kinds of a request's events, in order.
fn shape(events: &[Arc<Event>]) -> Vec<&'static str> {
    events.iter().map(|event| event.kind()).collect()
}

/// Every request recorded among `events`, as `(operation, member, kinds, outcome)`, in the order
/// they were received.
fn summary(
    events: &[Arc<Event>],
) -> Vec<(Operation, Option<String>, Vec<&'static str>, HostedOutcome)> {
    by_request(events)
        .values()
        .map(|events| {
            let (op, member) = asked(events);
            (op, member, shape(events), ended(events).0)
        })
        .collect()
}

/// Wall-clock seconds, as the fixture's `stamp` reports them.
fn seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs_f64()
}

/// Every request is recorded, receipt first: a chain of two reads is two requests of three events
/// each, iteration a request per item, a method call two, a raise names the exception's type
/// while the agent catches the library's own class -- and the dispatch is in the stream before
/// the target runs.
#[test]
fn every_request_is_recorded_receipt_first() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            from outrig_fixture import FixtureError
            chain = stub.a.b
            items = []
            for x in stub.sequence:
                items.append(x)
            print(items)
            print(stub.method(1))
            try:
                stub.raise_fixture()
            except FixtureError as e:
                print('caught', e.status)
            print(stub.stamp())
            "#,
        ),
    );
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("[10, 20, 30]"), "{out}");
    assert_eq!(lines.next(), Some("2"), "{out}");
    assert_eq!(lines.next(), Some("caught 3"), "{out}");
    let stamped: f64 = lines.next().expect("a stamp").parse().expect("seconds");
    let events = relay.hosted_events();
    let requests = summary(&events);
    let full = vec!["hosted.received", "hosted.dispatched", "hosted.ended"];
    let ops: Vec<(Operation, Option<&str>)> = requests
        .iter()
        .map(|(op, member, kinds, _)| {
            assert_eq!(kinds, &full, "{requests:?}");
            (*op, member.as_deref())
        })
        .collect();
    assert_eq!(
        ops,
        [
            (Operation::Getattr, Some("a")),
            (Operation::Getattr, Some("b")),
            (Operation::Getattr, Some("sequence")),
            (Operation::Callattr, Some("__iter__")),
            (Operation::Callattr, Some("__next__")),
            (Operation::Callattr, Some("__next__")),
            (Operation::Callattr, Some("__next__")),
            (Operation::Callattr, Some("__next__")),
            (Operation::Getattr, Some("method")),
            (Operation::Call, None),
            (Operation::Getattr, Some("raise_fixture")),
            (Operation::Call, None),
            (Operation::Getattr, Some("stamp")),
            (Operation::Call, None),
        ],
        "{requests:?}"
    );
    // The iteration's end is a raise too, of `StopIteration`.
    let raised: Vec<(Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::HostedEnded {
                outcome: HostedOutcome::Raised,
                exception,
                traceback,
                ..
            } => Some((exception.clone(), traceback.clone())),
            _ => None,
        })
        .collect();
    let names: Vec<Option<&str>> = raised.iter().map(|(name, _)| name.as_deref()).collect();
    assert_eq!(
        names,
        [
            Some("builtins.StopIteration"),
            Some("outrig_fixture.FixtureError")
        ]
    );
    assert!(
        raised[1]
            .1
            .as_deref()
            .is_some_and(|text| text.contains("raise_fixture")),
        "{raised:?}"
    );
    // The stamp's call was dispatched -- in the stream -- before the fixture read its clock.
    let stamp = by_request(&events)
        .into_values()
        .last()
        .expect("the stamp's call");
    let dispatched = stamp
        .iter()
        .find(|event| event.kind() == "hosted.dispatched")
        .expect("a dispatch");
    assert!(
        seconds(dispatched.time) <= stamped,
        "{dispatched:?} after {stamped}"
    );
}

/// A call's arguments are previewed as they crossed: plain values whole, by value, a container of
/// them cut to its first 16 items, and keywords by name.
#[test]
fn arguments_are_previewed_as_they_crossed() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(
        1,
        &with_stub("stub.types(1, 'two', (3, 4.5), list(range(20)), flag=True, name=None)"),
    );
    let events = relay.hosted_events();
    let (arguments, keywords) = events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::HostedReceived {
                operation: Operation::Call,
                arguments,
                keywords,
                ..
            } => Some((arguments.clone(), keywords.clone())),
            _ => None,
        })
        .expect("the call");
    assert_eq!(
        arguments,
        [
            "1",
            "'two'",
            "(3, 4.5)",
            "[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, ...]",
        ]
    );
    assert_eq!(
        keywords,
        BTreeMap::from([
            ("flag".to_string(), "True".to_string()),
            ("name".to_string(), "None".to_string()),
        ])
    );
}

/// A callback's invocation and the hosted call it makes carry the outer call's id as parent: the
/// callback runs on the thread waiting for the outer call, and its call goes on that connection.
#[test]
fn a_callback_is_recorded_under_its_call() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            def back():
                return stub.public
            print(stub.call(back))
            "#,
        ),
    );
    assert_eq!(out, "public value\n");
    let events = relay.hosted_events();
    let outer = by_request(&events)
        .into_iter()
        .find(|(_, events)| asked(events).0 == Operation::Call)
        .map(|(id, _)| id)
        .expect("the outer call");
    let callback = events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::HostedCallback { parent, .. } => Some(*parent),
            _ => None,
        })
        .expect("a callback event");
    assert_eq!(callback, outer);
    let nested = events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::HostedReceived { member, parent, .. }
                if member.as_deref() == Some("public") =>
            {
                Some(*parent)
            }
            _ => None,
        })
        .expect("the nested read");
    assert_eq!(nested, Some(outer));
}

/// A refusal by the interception is recorded, bookkeeping included, with no dispatch; a private
/// attribute raises an error of OutRig's that is an `AttributeError` and not the library's.
#[test]
fn a_refusal_by_the_interception_is_recorded() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_stub(
            r#"
            import rpyc
            from rpyc.core import consts
            from outrig_fixture import FixtureError, CommandError
            root = K.hosted('fx')
            conn = object.__getattribute__(root, "____conn__")
            idp = lambda proxy: object.__getattribute__(proxy, "____id_pack__")
            L, V, T = consts.LABEL_LOCAL_REF, consts.LABEL_VALUE, consts.LABEL_TUPLE
            def raw(handler, boxed):
                res = rpyc.core.async_.AsyncResult(conn)
                seq = conn._get_seq_id()
                conn._request_callbacks[seq] = res
                op = {99: "unknown", consts.HANDLE_DEL: "del", consts.HANDLE_INSPECT: "inspect"}
                conn._send(consts.MSG_REQUEST, seq, (handler, boxed),
                           {"request": seq, "op": op[handler]})
                return res.value
            for label, fn in (
                ("unknown", lambda: raw(99, (T, ()))),
                ("inflated", lambda: raw(consts.HANDLE_DEL, (T, ((L, idp(root.nested)), (V, 1000))))),
                ("inspect", lambda: raw(consts.HANDLE_INSPECT, (T, ((V, ("x.Y", 1, 2)),)))),
            ):
                try:
                    fn()
                except Exception as e:
                    print(label, type(e).__name__)
            try:
                stub._private
            except (FixtureError, CommandError):
                print('the library caught it')
            except AttributeError as e:
                print('private', type(e).__name__, isinstance(e, AttributeError))
            "#,
        ),
    );
    assert_eq!(
        out,
        "unknown outrig.Refused\ninflated outrig.Refused\ninspect outrig.Refused\nprivate \
         outrig.Refused True\n"
    );
    let events = relay.hosted_events();
    let refused: Vec<(Operation, Option<String>)> = by_request(&events)
        .values()
        .filter(|events| {
            ended(events) == (HostedOutcome::Refused, Some(HostedReason::Interception))
        })
        .map(|events| {
            assert_eq!(shape(events), ["hosted.received", "hosted.ended"]);
            asked(events)
        })
        .collect();
    assert_eq!(
        refused,
        [
            (Operation::Unknown, None),
            (Operation::Del, None),
            (Operation::Inspect, None),
            (Operation::Getattr, Some("_private".to_string())),
        ]
    );
}

/// Interrupt -- or cancel -- `agent`'s execution while its thread waits 10 s in a hosted call: the
/// call raises there within a second, with `0003-17`'s text, and the binding finishes it, which
/// ends `returned` with the note that its caller was interrupted. The next call returns its own
/// result, and is what lets the first finish: the binding reads it after the cancel line the
/// interrupted call sent ahead of its result, so the note is made before the first can end, where
/// a release from the test could overtake that line.
fn a_woken_call_runs_on(agent: &str, cancel: bool) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    if agent != PRIMARY {
        relay.open(agent);
    }
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        agent,
        1,
        &with_stub_in(
            agent,
            &format!(
                "print(stub.record(10, 'slow', started={}, until={}))",
                started.py(),
                release.py()
            ),
        ),
    );
    started.wait();
    let asked_at = Instant::now();
    if cancel {
        relay.cancel(agent, 1);
    } else {
        relay.interrupt(agent, 1, false);
    }
    let result = relay.result_from(agent, 1, TIMEOUT);
    assert!(
        asked_at.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked_at.elapsed()
    );
    let (raised, verb) = if cancel {
        ("CancelledError", "cancelled")
    } else {
        ("KeyboardInterrupt", "interrupted")
    };
    assert_eq!(result["raised"], raised, "{result}");
    let error = result["error"].as_str().expect("an error");
    assert!(
        error.contains(&format!(
            "{verb} by outrig while waiting for binding 'fx' to answer: the call's outcome on \
             the host is unknown"
        )),
        "{error}"
    );
    let releasing = format!("print(stub.record(0, 'release', started={}))", release.py());
    assert_eq!(
        relay.output_in(agent, 2, &with_stub_in(agent, &releasing)),
        "release\n"
    );
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        by_request(events).values().any(|events| {
            matches!(
                &events.last().expect("an event").payload,
                Payload::HostedEnded {
                    outcome: HostedOutcome::Returned,
                    interrupted: true,
                    ..
                }
            )
        })
    });
    let call = by_request(&events)
        .into_values()
        .find(|events| asked(events).0 == Operation::Call)
        .expect("the slow call");
    assert_eq!(ended(&call).0, HostedOutcome::Returned, "{call:?}");
}

#[test]
fn an_interrupt_ends_the_wait_on_the_primary_and_the_call_runs_on() {
    a_woken_call_runs_on(PRIMARY, false);
}

#[test]
fn an_interrupt_ends_the_wait_on_a_child_and_the_call_runs_on() {
    a_woken_call_runs_on("b", false);
}

#[test]
fn a_cancel_ends_the_wait_and_the_call_runs_on() {
    a_woken_call_runs_on("b", true);
}

/// A call made in a worker is not woken by an interrupt: the worker's frames hold no agent code.
/// A cancel ends the code awaiting it at once, and the call runs on until the host answers; it is
/// recorded `returned`, not interrupted, since nothing gave up on it.
#[test]
fn a_worker_call_is_not_woken_by_an_interrupt() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.submit_in(
        PRIMARY,
        1,
        &with_stub(&format!(
            "print(await asyncio.to_thread(\
             lambda: stub.record(10, 'slow', started={}, until={})))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.interrupt(PRIMARY, 1, false);
    assert!(
        relay
            .try_result_from(PRIMARY, 1, Duration::from_secs(1))
            .is_none(),
        "the interrupt ended an execution awaiting a worker's call"
    );
    let asked_at = Instant::now();
    relay.cancel(PRIMARY, 1);
    let result = relay.result_from(PRIMARY, 1, TIMEOUT);
    assert!(asked_at.elapsed() < Duration::from_secs(1));
    assert_eq!(result["raised"], "CancelledError", "{result}");
    release.raise();
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        by_request(events).values().any(|events| {
            asked(events).0 == Operation::Call
                && outcome(events).map(|end| end.0) == Some(HostedOutcome::Returned)
        })
    });
    let call = by_request(&events)
        .into_values()
        .find(|events| asked(events).0 == Operation::Call)
        .expect("the slow call");
    assert!(
        matches!(
            &call.last().expect("an end").payload,
            Payload::HostedEnded {
                outcome: HostedOutcome::Returned,
                interrupted: false,
                ..
            }
        ),
        "{call:?}"
    );
    assert_eq!(relay.output(2, &with_stub("print(stub.method(1))")), "2\n");
}

/// A serialize-lock waiter whose caller is interrupted is cancelled, never invoked.
#[test]
fn an_interrupted_serialize_waiter_is_cancelled() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    let (started, release) = (Flag::new(), Flag::new());
    relay.output_in("b", 1, &with_stub_in("b", "stub.public"));
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            "print(stub.record(0, 'a', started={}, until={}))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.submit_in("b", 2, &with_stub_in("b", "print(stub.noted())"));
    // Its receipt says it reached the binding, where it waits for the turn.
    relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(&event.payload, Payload::HostedReceived { agent, member, .. }
                if agent == "b" && member.as_deref() == Some("noted"))
        })
    });
    std::thread::sleep(Duration::from_millis(200));
    relay.interrupt("b", 2, false);
    assert_eq!(
        relay.result_from("b", 2, TIMEOUT)["raised"],
        "KeyboardInterrupt"
    );
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                Payload::HostedEnded {
                    outcome: HostedOutcome::Cancelled,
                    reason: Some(HostedReason::Interrupted),
                    ..
                }
            )
        })
    });
    let cancelled = by_request(&events)
        .into_values()
        .find(|events| outcome(events).map(|end| end.0) == Some(HostedOutcome::Cancelled))
        .expect("the cancelled call");
    assert_eq!(shape(&cancelled), ["hosted.received", "hosted.ended"]);
    release.raise();
    assert_eq!(relay.result_from("primary", 1, TIMEOUT)["output"], "a\n");
    assert_eq!(relay.output(2, &with_stub("print(stub.calls)")), "0\n");
}

/// A callback's own request, interrupted while it waits for a serialized binding's turn, is
/// cancelled at its depth, and only it: the host calls the callback from a worker thread while the
/// call it is inside holds the turn, so the request waits there. The callback catches the
/// interrupt and returns, and the outer call, which was not cancelled with it, returns too.
#[test]
fn an_interrupted_nested_request_waiting_for_the_turn_is_cancelled_alone() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    relay.submit_in(
        "primary",
        2,
        &with_stub(
            r#"
            def callback():
                try:
                    return stub.noted()
                except KeyboardInterrupt:
                    return "interrupted"
            print(stub.call_in_thread(callback))
            "#,
        ),
    );
    // Its receipt says it reached the binding, where it waits for the turn.
    relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(&event.payload, Payload::HostedReceived { member, .. }
                if member.as_deref() == Some("noted"))
        })
    });
    std::thread::sleep(Duration::from_millis(200));
    relay.interrupt("primary", 2, false);
    assert_eq!(
        relay.result_from("primary", 2, TIMEOUT)["output"],
        "interrupted\n"
    );
    let requests = by_request(&relay.hosted_events());
    let noted = requests
        .values()
        .find(|events| {
            matches!(&events[0].payload, Payload::HostedReceived { member, .. }
                if member.as_deref() == Some("noted"))
        })
        .expect("the callback's request");
    assert_eq!(
        ended(noted),
        (HostedOutcome::Cancelled, Some(HostedReason::Interrupted))
    );
    // Nothing else was cancelled with it: the call it was inside ran to its end.
    let cancelled = requests
        .values()
        .filter(|events| ended(events).0 == HostedOutcome::Cancelled)
        .count();
    assert_eq!(cancelled, 1, "{requests:?}");
    assert_eq!(relay.output(3, &with_stub("print(stub.calls)")), "0\n");
}

/// A call interrupted while it waits for a free connection never reaches the binding, and is
/// recorded `cancelled` from the interpreter's notice: the reader thread's, which wakes the call,
/// or, with `aimed`, the call's own. Then the interrupt is a signal that lands once the call waits,
/// as when the reader thread found the call busy and left the interrupt to the signal, whose
/// handler writes no line and wakes nothing; the wait finds it as it looks again. The signal is
/// aimed and armed from a thread of the execution's own, as the reader thread aims and arms one.
fn a_call_interrupted_waiting_for_a_connection_is_cancelled_by(aimed: bool) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let release = Flag::new();
    let starts: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    relay.output(1, &with_stub(&held_calls(&starts, &release)));
    for start in &starts {
        start.wait();
    }
    if aimed {
        let aim = r#"
            import signal, threading
            main = threading.get_ident()
            def aim():
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    wait = __main__._waits.get(main)
                    if wait is not None and wait.conn is None and wait.waiting:
                        __main__._primary._interrupting = (2, False)
                        signal.pthread_kill(main, signal.SIGINT)
                        return
                    time.sleep(0.01)
            threading.Thread(target=aim, daemon=True).start()
            print(stub.noted())
            "#;
        relay.submit_in("primary", 2, &with_stub(aim));
    } else {
        relay.submit_in("primary", 2, &with_stub("print(stub.noted())"));
        std::thread::sleep(Duration::from_millis(300));
        relay.interrupt("primary", 2, false);
    }
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert_eq!(result["raised"], "KeyboardInterrupt", "{result}");
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                Payload::HostedReceived {
                    by: ReceivedBy::Interpreter,
                    ..
                }
            )
        })
    });
    let never = by_request(&events)
        .into_values()
        .find(|events| {
            matches!(
                &events[0].payload,
                Payload::HostedReceived {
                    by: ReceivedBy::Interpreter,
                    ..
                }
            )
        })
        .expect("the call never sent");
    assert_eq!(
        asked(&never),
        (Operation::Getattr, Some("noted".to_string()))
    );
    assert_eq!(
        ended(&never),
        (HostedOutcome::Cancelled, Some(HostedReason::Interrupted))
    );
    release.raise();
    assert_eq!(relay.output(3, &with_stub("print(stub.calls)")), "0\n");
}

#[test]
fn a_call_interrupted_waiting_for_a_connection_is_cancelled() {
    a_call_interrupted_waiting_for_a_connection_is_cancelled_by(false);
}

#[test]
fn a_call_a_signal_interrupts_waiting_for_a_connection_is_cancelled() {
    a_call_interrupted_waiting_for_a_connection_is_cancelled_by(true);
}

/// A connection to a binding the session does not hold is closed by the relay, saying so; nothing
/// reaches the bindings process.
#[test]
fn a_binding_the_session_does_not_hold_is_refused_by_the_relay() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        "import __main__\nghost = __main__._kernels['primary'].stub('ghost')\n\
         try:\n    ghost.public\nexcept EOFError as e:\n    print(e)",
    );
    assert!(
        out.contains("this session holds no binding \"ghost\""),
        "{out}"
    );
    assert!(relay.hosted_events().is_empty());
}

/// `outrig.bindings[name]` is the calling kernel's own proxy of the binding: a
/// function in a module imported once, called from two kernels, gets each
/// caller's -- not `is`-identical, and both reaching the one object on the
/// host -- whatever the kernel's global of that name holds. A name the session
/// has not bound is a `KeyError`.
#[test]
fn outrig_bindings_is_each_calling_kernels_own_proxy() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        workspace.path().join("shared.py"),
        py(r#"
        import outrig

        seen = []

        def fx():
            return outrig.bindings['fx']
        "#),
    )
    .expect("write");
    let mut relay = Relay::start_with(&Start {
        dir: Some(workspace.path()),
        ..Start::default()
    });
    relay.bind("fx", FACTORY);
    relay.declare_bindings();
    relay.open("b");
    let probe = |agent: &str| {
        py(&format!(
            r#"
            import outrig, shared, __main__
            fx = None
            mine = shared.fx()
            shared.seen.append(mine)
            print(mine is __main__._kernels[{agent:?}].stubs['fx'], mine.ask('which?'))
            print(list(outrig.bindings), 'fx' in outrig.bindings, outrig.bindings)
            try:
                outrig.bindings['missing']
            except KeyError as e:
                print('KeyError', e)
            "#
        ))
    };
    assert_eq!(
        relay.output(1, &probe("primary")),
        "True 1\n['fx'] True <outrig.bindings: fx>\nKeyError 'missing'\n"
    );
    assert_eq!(
        relay.output_in("b", 1, &probe("b")),
        "True 2\n['fx'] True <outrig.bindings: fx>\nKeyError 'missing'\n",
        "the second ticket from the same object"
    );
    assert_eq!(
        relay.output_in("b", 2, "shared.seen[0] is not shared.seen[1]"),
        "True\n"
    );
}

/// A stub is bound in a kernel opened after the bindings were declared, and the inventory lists
/// no stub, in it or in the primary.
#[test]
fn a_stub_is_bound_in_a_later_kernel_and_listed_nowhere() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.declare_bindings();
    relay.open("b");
    assert_eq!(
        relay.output_in("b", 1, "print(type(fx).__name__, fx.public)"),
        "HostedRoot public value\n"
    );
    for agent in ["primary", "b"] {
        let inv = relay.inv(agent, 99);
        let names: Vec<&str> = inv["globals"]
            .as_array()
            .expect("globals")
            .iter()
            .filter_map(|entry| entry[0].as_str())
            .collect();
        assert!(!names.contains(&"fx"), "{agent}: {inv}");
    }
    assert_eq!(
        relay.output(1, "fx = None\nprint(type(__import__('__main__')._kernels['primary'].stubs['fx']).__name__)"),
        "HostedRoot\n"
    );
}

/// A request after the close is refused, a fixture method that writes a file leaves none, and a
/// call waiting for a free connection at the close raises at once and is `cancelled`.
#[test]
fn what_the_owner_still_holds_at_the_close_invokes_nothing() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let release = Flag::new();
    let starts: Vec<Flag> = (0..4).map(|_| Flag::new()).collect();
    relay.output(1, &with_stub(&held_calls(&starts, &release)));
    for start in &starts {
        start.wait();
    }
    relay.submit_in("primary", 2, &with_stub("print(stub.noted())"));
    std::thread::sleep(Duration::from_millis(300));
    let closed = Instant::now();
    relay.close_admission();
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert!(
        closed.elapsed() < Duration::from_secs(2),
        "{:?}",
        closed.elapsed()
    );
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("the session is closed to new work")),
        "{result}"
    );
    let marker = Flag::new();
    let out = relay.output(
        3,
        &with_stub(&format!(
            "try:\n    stub.touch({})\nexcept Exception as e:\n    print(type(e).__name__)",
            marker.py()
        )),
    );
    assert_eq!(out, "outrig.Closed\n");
    assert!(!marker.exists(), "the refused call ran");
    release.raise();
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        by_request(events)
            .values()
            .filter(|events| {
                asked(events).0 == Operation::Call
                    && outcome(events).map(|end| end.0) == Some(HostedOutcome::Returned)
            })
            .count()
            >= 4
    });
    // Each call the close kept from running is recorded once: the one woken waiting for a
    // connection, and the one made after.
    let outcomes: Vec<(String, HostedOutcome, Option<HostedReason>)> = by_request(&events)
        .values()
        .filter_map(|events| {
            let member = asked(events).1.filter(|m| m == "noted" || m == "touch")?;
            let (outcome, reason) = outcome(events)?;
            Some((member, outcome, reason))
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            (
                "noted".to_string(),
                HostedOutcome::Cancelled,
                Some(HostedReason::Closed)
            ),
            (
                "touch".to_string(),
                HostedOutcome::Refused,
                Some(HostedReason::AdmissionClosed)
            ),
        ]
    );
}

/// After the close the relay forwards bookkeeping, as its label says; a frame whose label claims
/// bookkeeping and whose request is a read is refused by the binding's flag, admission closed,
/// with no dispatch.
#[test]
fn a_frame_claiming_bookkeeping_after_the_close_is_refused_by_the_binding() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(
        1,
        &with_stub("import builtins\nbuiltins.root = K.hosted('fx')\nroot.public"),
    );
    relay.close_admission();
    let out = relay.output(
        2,
        &with_stub(
            r#"
            import rpyc
            from rpyc.core import consts
            conn = object.__getattribute__(root, "____conn__")
            idp = object.__getattribute__(root, "____id_pack__")
            res = rpyc.core.async_.AsyncResult(conn)
            seq = conn._get_seq_id()
            conn._request_callbacks[seq] = res
            boxed = (consts.LABEL_TUPLE,
                     ((consts.LABEL_LOCAL_REF, idp), (consts.LABEL_VALUE, "public")))
            label = {"request": seq, "op": "getroot"}
            conn._send(consts.MSG_REQUEST, seq, (consts.HANDLE_GETATTR, boxed), label)
            try:
                res.value
            except Exception as e:
                print(type(e).__name__)
            "#,
        ),
    );
    assert_eq!(out, "outrig.Closed\n");
    let events = relay.hosted_events();
    let read = by_request(&events).into_values().last().expect("the read");
    assert_eq!(
        asked(&read),
        (Operation::Getattr, Some("public".to_string()))
    );
    assert!(matches!(
        &read[0].payload,
        Payload::HostedReceived {
            by: ReceivedBy::Binding,
            ..
        }
    ));
    assert_eq!(shape(&read), ["hosted.received", "hosted.ended"]);
    assert_eq!(
        ended(&read),
        (HostedOutcome::Refused, Some(HostedReason::AdmissionClosed))
    );
}

/// With the fixture serialized, a call from kernel B waiting for the turn behind kernel A's call
/// at the close is cancelled once the binding reads its control line, before A's call ends, and
/// A's call drains to `returned`.
#[test]
fn a_serialize_waiter_is_cancelled_when_the_binding_observes_the_close() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.open("b");
    let (started, release) = (Flag::new(), Flag::new());
    relay.output_in("b", 1, &with_stub_in("b", "stub.public"));
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            "print(stub.record(10, 'a', started={}, until={}))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.submit_in("b", 2, &with_stub_in("b", "print(stub.noted())"));
    relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(&event.payload, Payload::HostedReceived { agent, member, .. }
                if agent == "b" && member.as_deref() == Some("noted"))
        })
    });
    std::thread::sleep(Duration::from_millis(200));
    relay.close_admission();
    let result = relay.result_from("b", 2, TIMEOUT);
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("the session is closed to new work")),
        "{result}"
    );
    release.raise();
    assert_eq!(relay.result_from("primary", 1, TIMEOUT)["output"], "a\n");
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        by_request(events)
            .values()
            .any(|events| asked(events).1.as_deref() == Some("record") && events.len() == 3)
    });
    let ends: Vec<(String, HostedOutcome, Option<HostedReason>)> = events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::HostedEnded {
                agent,
                outcome,
                reason,
                ..
            } => Some((agent.clone(), *outcome, *reason)),
            _ => None,
        })
        .collect();
    let cancelled = ends
        .iter()
        .position(|end| {
            end == &(
                "b".into(),
                HostedOutcome::Cancelled,
                Some(HostedReason::Closed),
            )
        })
        .expect("b's call cancelled");
    let drained = ends
        .iter()
        .rposition(|end| end.0 == "primary" && end.1 == HostedOutcome::Returned)
        .expect("a's call returned");
    assert!(cancelled < drained, "{ends:?}");
    // Never dispatched: its target never ran.
    let noted = by_request(&events)
        .into_values()
        .find(|events| asked(events).1.as_deref() == Some("noted"))
        .expect("b's call");
    assert_eq!(shape(&noted), ["hosted.received", "hosted.ended"]);
}

/// Recording runs no code: an object whose `repr` and `str` write files, returned by one call and
/// passed to another, leaves no file, and the events name it by type and reference.
#[test]
fn recording_runs_no_code() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let base = Flag::new();
    let out = relay.output(
        1,
        &with_stub(&format!(
            "m = stub.marked({})\nprint(stub.takes_marked(m))",
            base.py()
        )),
    );
    assert_eq!(out, "Marked\n");
    let (repr, str_) = (
        std::path::PathBuf::from(format!("{}.repr", base.path().display())),
        std::path::PathBuf::from(format!("{}.str", base.path().display())),
    );
    assert!(
        !repr.exists() && !str_.exists(),
        "recording ran the object's code"
    );
    let events = relay.hosted_events();
    let argument = events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::HostedReceived {
                operation: Operation::Call,
                arguments,
                ..
            } if arguments.iter().any(|a| a.contains("Marked")) => Some(arguments.clone()),
            _ => None,
        })
        .expect("the call that was passed the object");
    assert!(
        argument[0].starts_with("<outrig_fixture.Marked #"),
        "{argument:?}"
    );
}

/// A stopped bindings process stops only the calls waiting on it: plain Python and the user
/// channel go on.
#[test]
fn a_stopped_process_stops_only_its_callers() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    let pid = Pid::from_raw(relay.binding_pid() as i32);
    kill(pid, Signal::SIGSTOP).expect("stop the process");
    relay.submit_in("primary", 2, &with_stub("print(stub.public)"));
    relay.open("b");
    assert_eq!(relay.output_in("b", 1, "print(6 * 7)"), "42\n");
    assert_eq!(relay.msg("b", 2, "user", "hello")["pending"], 1);
    kill(pid, Signal::SIGCONT).expect("continue the process");
    assert_eq!(
        relay.result_from("primary", 2, TIMEOUT)["output"],
        "public value\n"
    );
}

/// The process dying mid-call leaves that call `unknown`; a later use raises naming how the
/// process ended; the stream records the exit; plain Python goes on.
#[test]
fn a_process_that_dies_leaves_its_call_unknown_and_the_session_going() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let started = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_stub(&format!(
            "print(stub.record(60, 'slow', started={}))",
            started.py()
        )),
    );
    started.wait();
    let pid = Pid::from_raw(relay.binding_pid() as i32);
    kill(pid, Signal::SIGKILL).expect("kill the process");
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("outcome on the host is unknown")),
        "{result}"
    );
    let events = relay.hosted_events_until(TIMEOUT, |events| {
        events.iter().any(|event| event.kind() == "binding.exited")
    });
    let exited = events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::BindingExited { cause, .. } => Some(cause.clone()),
            _ => None,
        })
        .expect("the exit");
    assert!(exited.contains("SIGKILL"), "{exited}");
    let slow = by_request(&events)
        .into_values()
        .find(|events| asked(events).0 == Operation::Call)
        .expect("the slow call");
    assert_eq!(
        ended(&slow),
        (HostedOutcome::Unknown, Some(HostedReason::Exited))
    );
    let out = relay.output(
        2,
        &with_stub("try:\n    stub.public\nexcept EOFError as e:\n    print(e)"),
    );
    assert!(out.contains("SIGKILL"), "{out}");
    assert_eq!(relay.output(3, "print(6 * 7)"), "42\n");
}

/// With the bindings process paused and calls sent with big arguments, enough that its pipe is
/// full, the close returns at once and the calls fall in two sets: those forwarded, which the
/// process receives and finishes once it runs again, and those it never received, which raise at
/// once. The process receives exactly what the relay forwarded.
#[test]
fn a_forwarded_request_is_the_bindings_to_finish() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("noted = stub.noted"));
    let pid = Pid::from_raw(relay.binding_pid() as i32);
    let before = relay.forwarded();
    kill(pid, Signal::SIGSTOP).expect("stop the process");
    relay.submit_in(
        "primary",
        2,
        &with_stub(
            r#"
            big = b'x' * 300_000
            calls = [asyncio.create_task(asyncio.to_thread(noted, big)) for _ in range(4)]
            done = await asyncio.gather(*calls, return_exceptions=True)
            print(sorted(type(r).__name__ for r in done))
            "#,
        ),
    );
    eventually(
        || (relay.forwarded() >= before + 2).then_some(()),
        || "two calls forwarded".to_string(),
    );
    // Long enough for every call to be sent.
    std::thread::sleep(Duration::from_millis(500));
    let closed = Instant::now();
    relay.close_admission();
    assert!(closed.elapsed() < Duration::from_millis(500));
    let forwarded = relay.forwarded() - before;
    assert!(forwarded < 4, "every call was forwarded");
    kill(pid, Signal::SIGCONT).expect("continue the process");
    let result = relay.result_from("primary", 2, TIMEOUT);
    let printed = result["output"].as_str().expect("output").to_string();
    assert_eq!(
        printed.matches("'int'").count() as u64,
        forwarded,
        "{printed}"
    );
    assert_eq!(
        printed.matches("'outrig.Closed'").count() as u64,
        4 - forwarded,
        "{printed}"
    );
    let events = relay.hosted_events();
    let calls: Vec<(ReceivedBy, HostedOutcome)> = by_request(&events)
        .values()
        .filter(|events| asked(events).0 == Operation::Call)
        .map(|events| match &events[0].payload {
            Payload::HostedReceived { by, .. } => (*by, ended(events).0),
            other => panic!("a request whose first event is {other:?}"),
        })
        .collect();
    let received = calls
        .iter()
        .filter(|call| **call == (ReceivedBy::Binding, HostedOutcome::Returned))
        .count() as u64;
    assert_eq!(received, forwarded, "{calls:?}");
    // The rest the relay refused; none is recorded as never sent.
    let refused = calls
        .iter()
        .filter(|call| **call == (ReceivedBy::Relay, HostedOutcome::Refused))
        .count() as u64;
    assert_eq!(refused, 4 - forwarded, "{calls:?}");
    assert_eq!(calls.len(), 4, "{calls:?}");
}

/// A request forwarded before the close may start after it: the process reads it ahead of its
/// control line and runs it, and it is never `cancelled` or `refused`.
#[test]
fn a_forwarded_request_may_start_after_the_close_returns() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("touch = stub.touch"));
    let pid = Pid::from_raw(relay.binding_pid() as i32);
    let marker = Flag::new();
    kill(pid, Signal::SIGSTOP).expect("stop the process");
    let before = relay.forwarded();
    relay.submit_in("primary", 2, &with_stub(&format!("touch({})", marker.py())));
    eventually(
        || (relay.forwarded() > before).then_some(()),
        || "the call forwarded".to_string(),
    );
    let closed = Instant::now();
    relay.close_admission();
    assert!(closed.elapsed() < Duration::from_millis(500));
    kill(pid, Signal::SIGCONT).expect("continue the process");
    let result = relay.result_from("primary", 2, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert!(marker.exists(), "the forwarded call did not run");
    let events = relay.hosted_events();
    let call = by_request(&events)
        .into_values()
        .find(|events| asked(events).0 == Operation::Call)
        .expect("the call");
    assert_eq!(
        shape(&call),
        ["hosted.received", "hosted.dispatched", "hosted.ended"]
    );
    assert_eq!(ended(&call).0, HostedOutcome::Returned, "{call:?}");
}

/// A subscriber that takes nothing while a call runs, as more events are published than it
/// holds, resumes to a counted gap that holds the call's receipt and dispatch, and then sees the
/// call's `returned`.
#[test]
fn a_stalled_subscriber_loses_the_dispatch_and_sees_returned() {
    let mut relay = Relay::start_with_capacity(8);
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.output(
        1,
        &with_stub(&format!(
            "import builtins\nbuiltins.slow = asyncio.create_task(asyncio.to_thread(\
             lambda: stub.record(10, 'slow', started={}, until={})))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.output(2, &with_stub("for _ in range(10):\n    stub.public"));
    release.raise();
    let (missed, events) = relay.hosted_events_with_gaps(TIMEOUT, |events| {
        events.iter().any(|event| {
            matches!(&event.payload, Payload::HostedEnded { duration, .. }
                if *duration >= Duration::from_millis(1))
                && by_request(events).values().any(|request| {
                    request.len() == 1 && ended(request).0 == HostedOutcome::Returned
                })
        })
    });
    assert!(missed > 0, "nothing was missed");
    let slow = by_request(&events)
        .into_values()
        .find(|request| request.len() == 1)
        .expect("a call seen only by its end");
    // Invoked, as its outcome says: the file it made exists.
    assert_eq!(ended(&slow).0, HostedOutcome::Returned);
    assert!(started.exists());
}

/// A request numbered past what the owner reads -- below zero, or past 64 bits -- closes its
/// connection before anything is recorded or waited for: with the binding serialized, nothing
/// holds its turn, and the next call returns at once.
#[test]
fn a_request_numbered_past_64_bits_closes_its_connection_unrecorded() {
    let mut relay = Relay::start();
    relay.bind_serialized("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    for (id, seq) in [(2, "-1"), (4, "2 ** 64")] {
        let out = relay.output(
            id,
            &with_stub(&format!(
                "{FORGE}\ntry:\n    forge({seq}, label=False).value\nexcept EOFError as e:\n    \
                 print(e)"
            )),
        );
        assert_eq!(
            out,
            format!(
                "the host closed this connection: a request numbered {}, not an integer below \
                 2**64\n",
                if seq == "-1" {
                    "-1"
                } else {
                    "18446744073709551616"
                }
            )
        );
        assert_eq!(
            relay.output(id + 1, &with_stub("print(stub.public)")),
            "public value\n"
        );
    }
    let events = relay.hosted_events();
    let reads = summary(&events)
        .into_iter()
        .filter(|(_, member, ..)| member.as_deref() == Some("public"))
        .map(|(.., outcome)| outcome)
        .collect::<Vec<_>>();
    assert_eq!(reads, [HostedOutcome::Returned; 3]);
}

/// A request sent under the number of one its connection is serving -- from inside that one's
/// callback -- closes the connection unserved: two requests never share one record.
#[test]
fn a_request_numbered_as_one_in_flight_closes_its_connection() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    let out = relay.output(
        2,
        &with_stub(&format!(
            r#"
            {FORGE}
            def again():
                forge(max(conn._request_callbacks), label=False)
                deadline = time.monotonic() + 10
                while not conn._channel.closed and time.monotonic() < deadline:
                    time.sleep(0.01)
                return 1
            try:
                stub.call(again)
            except EOFError as e:
                print(e)
            "#,
            FORGE = py(FORGE).replace('\n', "\n            ")
        )),
    );
    assert!(
        out.starts_with("the host closed this connection: request ")
            && out.ends_with(" is served on this connection\n"),
        "{out}"
    );
    let events = relay.hosted_events();
    for (id, request) in by_request(&events) {
        let receipts = shape(&request)
            .iter()
            .filter(|kind| **kind == "hosted.received")
            .count();
        assert_eq!(receipts, 1, "request {id:?}: {request:?}");
    }
}

/// A cancel the binding reads before its connection's thread has taken the request it names --
/// that thread still busy with the call before it -- is kept for the request's receipt: the
/// request is cancelled, never dispatched.
#[test]
fn a_cancel_read_ahead_of_its_request_cancels_it() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.output(
        1,
        &with_stub(&format!(
            "import builtins\nbuiltins.held = asyncio.create_task(asyncio.to_thread(\
             lambda: stub.record(0, 'hold', started={}, until={})))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    relay.output(
        2,
        &with_stub(&format!(
            "{FORGE}\nimport builtins\nseq = conn._get_seq_id()\n\
             builtins.forged = forge(seq)\ncancel(seq)"
        )),
    );
    release.raise();
    let out = relay.output(
        3,
        &with_stub(
            "import builtins\nawait builtins.held\ntry:\n    builtins.forged.value\n\
             except Exception as e:\n    print(type(e).__name__, e)",
        ),
    );
    assert!(
        out.starts_with("EOFError the call was cancelled before it ran"),
        "{out}"
    );
    let events = relay.hosted_events();
    let forged = by_request(&events)
        .into_values()
        .find(|request| asked(request).1.as_deref() == Some("public"))
        .expect("the forged read");
    assert_eq!(shape(&forged), ["hosted.received", "hosted.ended"]);
    assert_eq!(
        ended(&forged),
        (HostedOutcome::Cancelled, Some(HostedReason::Interrupted))
    );
}

/// Cancels kept ahead of their requests past what an interpreter sends -- 32 requests that never
/// arrive, named while a frame waits -- close the connection, so what is kept stays bounded.
#[test]
fn cancels_kept_past_the_bound_close_their_connection() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.output(
        1,
        &with_stub(&format!(
            "import builtins\nbuiltins.held = asyncio.create_task(asyncio.to_thread(\
             lambda: stub.record(0, 'hold', started={}, until={})))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    let out = relay.output(
        2,
        &with_stub(&format!(
            "{FORGE}\nimport builtins\nforge(conn._get_seq_id(), label=False)\n\
             cancel(*range(10 ** 6, 10 ** 6 + 16))\ncancel(*range(10 ** 6 + 16, 10 ** 6 + 32))\n\
             try:\n    await builtins.held\nexcept EOFError as e:\n    print(e)"
        )),
    );
    release.raise();
    assert_eq!(
        out,
        "the host closed this connection: more than 16 requests cancelled before they arrived\n"
    );
}

/// A cancel naming a request that has ended, or none, read while no frame waits, is kept
/// nowhere: forty of them leave the connection open, and its next call returns.
#[test]
fn cancels_of_requests_that_ended_are_kept_nowhere() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    let out = relay.output(
        2,
        &with_stub(&format!(
            "{FORGE}\nfor seq in range(40):\n    cancel(seq, seq + 1000)\n\
             print(stub.public, len(pool.connections), conn._channel.closed)"
        )),
    );
    assert_eq!(out, "public value 1 False\n");
}

/// A frame the relay holds a request for that the binding reads as something else -- a request
/// under another number, a number past 64 bits, or a reply -- closes its connection, and the
/// request ends `refused` by the interception, received by the relay: nothing is left for the
/// drain to wait on, and the next call returns.
#[test]
fn a_frame_unlike_its_label_ends_its_request_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    let reply = r#"
def reply(label):
    tag = {"request": label, "op": "getattr", "member": "public"}
    conn._send(consts.MSG_REPLY, label, (consts.LABEL_VALUE, None), tag)
    deadline = time.monotonic() + 10
    while not conn._channel.closed and time.monotonic() < deadline:
        time.sleep(0.01)
    raise EOFError(conn._channel.reason)
"#;
    let cases = [
        (
            "forge(5000, label=6000).value",
            "request 5000 arrived labeled as request 6000",
        ),
        (
            "forge(-1, label=6001).value",
            "a request numbered -1, not an integer below 2**64",
        ),
        (
            "reply(6002)",
            "an undecodable frame: ValueError: a reply labeled as request 6002",
        ),
    ];
    for (id, (send, reason)) in (2..).step_by(2).zip(cases) {
        let out = relay.output(
            id,
            &with_stub(&format!(
                "{FORGE}\n{reply}\ntry:\n    {send}\nexcept EOFError as e:\n    print(e)"
            )),
        );
        assert_eq!(out, format!("the host closed this connection: {reason}\n"));
        assert_eq!(
            relay.output(id + 1, &with_stub("print(stub.public)")),
            "public value\n"
        );
    }
    let events = relay.hosted_events();
    let refused: Vec<String> = by_request(&events)
        .into_values()
        .filter(|request| ended(request).0 == HostedOutcome::Refused)
        .map(|request| {
            assert_eq!(shape(&request), ["hosted.received", "hosted.ended"]);
            let (
                Payload::HostedReceived { by, member, .. },
                Payload::HostedEnded { reason, detail, .. },
            ) = (&request[0].payload, &request[1].payload)
            else {
                panic!("{request:?}");
            };
            assert_eq!(
                (*by, member.as_deref(), *reason),
                (
                    ReceivedBy::Relay,
                    Some("public"),
                    Some(HostedReason::Interception)
                )
            );
            detail.clone().expect("what refused it")
        })
        .collect();
    assert_eq!(refused, cases.map(|(_, reason)| reason.to_string()));
}

/// Frames the relay holds requests for, left unread when their connection closes -- held behind a
/// call still running, or arriving once it has closed -- end their requests `cancelled`,
/// `disconnected`, received by the relay: nothing is left for the drain to wait on.
#[test]
fn labeled_frames_dropped_unread_end_their_requests_cancelled() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let (started, release) = (Flag::new(), Flag::new());
    relay.output(
        1,
        &with_stub(&format!(
            "import builtins\nbuiltins.held = asyncio.create_task(asyncio.to_thread(\
             lambda: stub.record(0, 'hold', started={}, until={})))",
            started.py(),
            release.py()
        )),
    );
    started.wait();
    let out = relay.output(
        2,
        &with_stub(&format!(
            r#"
            {FORGE}
            import base64, builtins, struct
            forge(5000)
            forge(5001)
            cancel(*range(10 ** 6, 10 ** 6 + 16))
            cancel(*range(10 ** 6 + 16, 10 ** 6 + 32))
            try:
                await builtins.held
            except EOFError as e:
                print(e)
            part = struct.pack("!LB", 1, 0) + b"x\n"
            _ = __main__._send({{"t": "rpc", "agent": "primary", "binding": "fx",
                                "id": conn.number, "request": 5002, "op": "getattr",
                                "member": "public", "size": 1,
                                "data": base64.b64encode(part).decode()}})
            "#,
            FORGE = py(FORGE).replace('\n', "\n            ")
        )),
    );
    release.raise();
    let closed = "more than 16 requests cancelled before they arrived";
    assert_eq!(out, format!("the host closed this connection: {closed}\n"));
    let events = relay.hosted_events();
    let dropped: Vec<String> = by_request(&events)
        .into_values()
        .filter(|request| asked(request).1.as_deref() == Some("public"))
        .map(|request| {
            assert_eq!(shape(&request), ["hosted.received", "hosted.ended"]);
            let (Payload::HostedReceived { by, .. }, Payload::HostedEnded { reason, detail, .. }) =
                (&request[0].payload, &request[1].payload)
            else {
                panic!("{request:?}");
            };
            assert_eq!(
                (*by, ended(&request).0, *reason),
                (
                    ReceivedBy::Relay,
                    HostedOutcome::Cancelled,
                    Some(HostedReason::Disconnected)
                )
            );
            detail.clone().expect("why its connection closed")
        })
        .collect();
    assert_eq!(dropped, [closed, closed, "this connection has closed"]);
}

/// A boundary request whose frame carries no label of the relay's is refused by the binding, and
/// recorded: run, it would be recorded under whatever request the relay holds by its number.
#[test]
fn a_boundary_request_without_the_relays_label_is_refused() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.output(1, &with_stub("stub.public"));
    let out = relay.output(
        2,
        &with_stub(&format!(
            "{FORGE}\ntry:\n    forge(5000, label=False).value\nexcept Exception as e:\n    \
             print(type(e).__name__, str(e).split('\\n')[0])"
        )),
    );
    assert_eq!(
        out,
        "outrig.Refused request 5000 came without the owner's label\n"
    );
    let events = relay.hosted_events();
    let forged = by_request(&events)
        .into_values()
        .find(|request| ended(request).0 == HostedOutcome::Refused)
        .expect("the forged read");
    assert_eq!(shape(&forged), ["hosted.received", "hosted.ended"]);
    assert_eq!(
        asked(&forged),
        (Operation::Getattr, Some("public".to_string()))
    );
    assert_eq!(
        ended(&forged),
        (HostedOutcome::Refused, Some(HostedReason::Interception))
    );
}

// ---------------------------------------------------------------------------- the relay alone

/// A relay for binding `fx` attached to no process, so what it queues stays queued; the lines it
/// sends the interpreter, kept; and a subscription to what it records.
struct Bare {
    relay: hosted::Relay,
    told: Arc<Mutex<Vec<Value>>>,
    events: Subscription,
}

impl Bare {
    fn new() -> Self {
        let told = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&told);
        let to_interpreter: hosted::ToInterpreter =
            Arc::new(move |line| kept.lock().expect("told lock").push(line));
        let mut stream = StreamBuilder::default();
        let events = stream.subscribe(1024);
        let relay = hosted::Relay::new(to_interpreter, stream.build(), vec!["fx".to_string()]);
        Self {
            relay,
            told,
            events,
        }
    }

    /// The primary agent's `rpc` line on connection `id` to `fx`, with `fields`.
    fn rpc(&self, id: u64, fields: Value) {
        let mut line = json!({"binding": "fx", "id": id});
        line.as_object_mut()
            .expect("an object")
            .extend(fields.as_object().expect("fields").clone());
        let rpc = serde_json::from_value(line).expect("an rpc line");
        self.relay.rpc(PRIMARY, rpc);
    }

    /// An event line of the process about request `seq` on the primary's connection 1, with
    /// `fields`.
    fn wrote(&self, kind: &str, seq: Value, fields: Value) {
        let mut line =
            json!({"t": "event", "kind": kind, "binding": "fx", "agent": PRIMARY, "conn": 1});
        line["seq"] = seq;
        line.as_object_mut()
            .expect("an object")
            .extend(fields.as_object().expect("fields").clone());
        self.relay.wrote(line);
    }

    /// The lines sent the interpreter since last asked.
    fn told(&self) -> Vec<Value> {
        std::mem::take(&mut *self.told.lock().expect("told lock"))
    }

    /// What the relay recorded since last asked.
    fn recorded(&mut self) -> Vec<Payload> {
        let mut recorded = Vec::new();
        while let Ok(received) = self.events.try_recv() {
            if let Received::Event(event) = received {
                recorded.push(event.payload.clone());
            }
        }
        recorded
    }
}

/// A label declaring a frame past the bound closes its connection before anything is held, and
/// the frame's later parts are dropped.
#[test]
fn a_frame_declared_past_the_bound_is_never_held() {
    let mut bare = Bare::new();
    let size = (128u64 << 20) + 1;
    bare.rpc(
        1,
        json!({"request": 1, "op": "getattr", "size": size, "data": "AAAA", "more": true}),
    );
    bare.rpc(1, json!({"data": "AAAA", "more": true}));
    bare.rpc(1, json!({"data": "AAAA"}));
    assert_eq!(bare.relay.queued(), (Vec::new(), 0));
    assert_eq!(
        bare.told(),
        [
            json!({"t": "rpc", "agent": PRIMARY, "binding": "fx", "id": 1,
            "closed": "a frame of 134217729 bytes is past the 134217728-byte bound"})
        ]
    );
    assert!(bare.recorded().is_empty());
}

/// A frame's reservation counts its parts' lines as well as their data, so parts that carry
/// nothing use it up: the second empty part of a 10-byte request closes its connection, and
/// nothing stays held.
#[test]
fn empty_parts_use_up_a_frames_reservation() {
    let mut bare = Bare::new();
    bare.rpc(
        1,
        json!({"request": 1, "op": "getattr", "size": 10, "data": "", "more": true}),
    );
    assert!(bare.relay.queued().1 > 0, "nothing reserved");
    bare.rpc(1, json!({"data": "", "more": true}));
    assert_eq!(bare.relay.queued(), (Vec::new(), 0));
    assert_eq!(
        bare.told(),
        [
            json!({"t": "rpc", "agent": PRIMARY, "binding": "fx", "id": 1,
            "closed": "a frame's parts outran the size its label declared"})
        ]
    );
    assert!(bare.recorded().is_empty());
}

/// An agent holds at most 64 connections to a binding: a part opening one more closes it alone,
/// and once one of the 64 closes, a new one opens.
#[test]
fn an_agent_holds_at_most_64_connections_to_a_binding() {
    let bare = Bare::new();
    for id in 1..=64 {
        bare.rpc(id, json!({"data": "AAAA"}));
    }
    assert_eq!(bare.relay.queued().0.len(), 64);
    assert!(bare.told().is_empty());
    bare.rpc(65, json!({"data": "AAAA"}));
    assert_eq!(bare.relay.queued().0.len(), 64);
    assert_eq!(
        bare.told(),
        [
            json!({"t": "rpc", "agent": PRIMARY, "binding": "fx", "id": 65,
            "closed": "agent \"primary\" already holds 64 connections to binding \"fx\""})
        ]
    );
    bare.rpc(1, json!({"closed": "done"}));
    bare.rpc(65, json!({"data": "AAAA"}));
    let (lines, _) = bare.relay.queued();
    assert_eq!(lines.len(), 66);
    assert_eq!(lines[64]["closed"], "done");
    assert_eq!(lines[65]["id"], 65);
    assert!(bare.told().is_empty());
}

/// The process hears of a connection's close once, and only of one it was sent a part on, with
/// the interpreter's reason cut to 1 KiB; a cancel reaches only an open connection, naming 16
/// requests at most.
#[test]
fn closes_and_cancels_reach_only_an_open_connection() {
    let bare = Bare::new();
    bare.rpc(9, json!({"closed": "never opened"}));
    bare.rpc(9, json!({"cancel": [1]}));
    assert_eq!(bare.relay.queued(), (Vec::new(), 0));
    bare.rpc(1, json!({"data": "AAAA"}));
    bare.rpc(1, json!({"cancel": (1..=40).collect::<Vec<u64>>()}));
    bare.rpc(1, json!({"closed": "x".repeat(5000)}));
    bare.rpc(1, json!({"closed": "again"}));
    let (lines, queued) = bare.relay.queued();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[1]["cancel"], json!((1..=16).collect::<Vec<u64>>()));
    assert_eq!(lines[2]["closed"].as_str().map(str::len), Some(1024));
    // Each line is charged: the part's data and a line, a line, a line and the reason.
    assert_eq!(queued, (4 + 1024) + 1024 + (1024 + 1024));
}

/// A labeled request under the number of one in flight on its connection closes the connection,
/// and the first keeps its record.
#[test]
fn a_labeled_request_numbered_as_one_in_flight_closes_its_connection() {
    let mut bare = Bare::new();
    let read = json!({"request": 3, "op": "getattr", "size": 1, "data": "AAAA"});
    bare.rpc(1, read.clone());
    bare.rpc(1, read);
    assert_eq!(
        bare.told(),
        [
            json!({"t": "rpc", "agent": PRIMARY, "binding": "fx", "id": 1,
            "closed": "request 3 while request 3 is in flight on it"})
        ]
    );
    bare.wrote("received", json!(3), json!({"operation": "getattr"}));
    bare.wrote("ended", json!(3), json!({"outcome": "returned"}));
    let recorded = bare.recorded();
    let [
        Payload::HostedReceived { request, .. },
        Payload::HostedEnded {
            request: ended,
            outcome: HostedOutcome::Returned,
            ..
        },
    ] = recorded.as_slice()
    else {
        panic!("{recorded:?}");
    };
    assert_eq!(request, ended);
}

/// A dispatch the relay cannot record -- of a request it never saw received, or on a line it
/// cannot read -- is answered unpublished, so the process neither waits for ever nor runs it
/// unrecorded; one it records is answered published.
#[test]
fn a_dispatch_the_relay_cannot_record_is_answered_unpublished() {
    let mut bare = Bare::new();
    bare.wrote("dispatched", json!(3), json!({"ack": 1}));
    bare.wrote("dispatched", json!(-1), json!({"ack": 2}));
    bare.wrote("received", json!(4), json!({"operation": "getattr"}));
    bare.wrote("dispatched", json!(4), json!({"ack": 3}));
    let published = |id: u64, answer: bool| json!({"t": "published", "id": id, "answer": answer});
    assert_eq!(
        bare.relay.queued().0,
        [published(1, false), published(2, false), published(3, true)]
    );
    let recorded = bare.recorded();
    assert!(
        matches!(
            recorded.as_slice(),
            [
                Payload::HostedReceived { .. },
                Payload::HostedDispatched { .. }
            ]
        ),
        "{recorded:?}"
    );
}

/// A process the session gave up waiting on is gone at once: the call it was serving is
/// `unknown`, `killed`, in the report, and nothing it writes later -- nor anything after the
/// report -- is recorded.
#[test]
fn an_abandoned_process_records_nothing_more() {
    let mut bare = Bare::new();
    bare.wrote("received", json!(4), json!({"operation": "call"}));
    bare.relay.close("the session closed");
    bare.relay.stopping();
    bare.relay.abandon();
    bare.relay.abandon();
    bare.wrote("ended", json!(4), json!({"outcome": "returned"}));
    bare.wrote("received", json!(5), json!({"operation": "call"}));
    let report = bare.relay.report();
    let notice = json!({"binding": "fx", "exec": null, "operation": "call", "member": null,
        "target": null, "outcome": "refused", "reason": "admission_closed"});
    bare.relay
        .noticed(PRIMARY, serde_json::from_value(notice).expect("a notice"));
    let recorded = bare.recorded();
    let [
        Payload::HostedReceived { request, .. },
        Payload::HostedEnded {
            request: ended,
            outcome: HostedOutcome::Unknown,
            reason: Some(HostedReason::Killed),
            ..
        },
    ] = recorded.as_slice()
    else {
        panic!("{recorded:?}");
    };
    assert_eq!(request, ended);
    assert_eq!(
        report,
        [HostedCall {
            request: *request,
            agent: PRIMARY.to_string(),
            binding: "fx".to_string(),
            outcome: HostedOutcome::Unknown,
            reason: Some(HostedReason::Killed),
        }]
    );
}

/// A request the relay holds is sent the process under the number it is held by, on its frame's
/// first part, and bookkeeping, held nowhere, is not; one the process ends without having received
/// it -- a frame it never served as that request -- is received by the relay, so its record is
/// whole, and nothing stays held.
#[test]
fn a_request_the_process_never_served_is_received_by_the_relay() {
    let mut bare = Bare::new();
    bare.rpc(
        1,
        json!({"request": 3, "op": "getattr", "member": "public", "size": 1, "data": "AAAA"}),
    );
    bare.rpc(
        1,
        json!({"request": 4, "op": "del", "size": 1, "data": "AAAA"}),
    );
    let (lines, _) = bare.relay.queued();
    assert_eq!(lines[0]["request"], 3, "{lines:?}");
    assert!(lines[1].get("request").is_none(), "{lines:?}");
    let detail = "this connection has closed";
    bare.wrote(
        "ended",
        json!(3),
        json!({"outcome": "cancelled", "reason": "disconnected", "detail": detail}),
    );
    let recorded = bare.recorded();
    let [
        Payload::HostedReceived {
            request,
            member,
            by: ReceivedBy::Relay,
            ..
        },
        Payload::HostedEnded {
            request: ended,
            outcome: HostedOutcome::Cancelled,
            reason: Some(HostedReason::Disconnected),
            detail: Some(said),
            ..
        },
    ] = recorded.as_slice()
    else {
        panic!("{recorded:?}");
    };
    assert_eq!(
        (request, member.as_deref(), said.as_str()),
        (ended, Some("public"), detail)
    );
    assert!(bare.relay.drained().now_or_never().is_some());
}

/// An agent holds at most 1,024 requests that have not ended: one more is refused whole, recorded
/// with its name cut to 256 bytes, and held nowhere; once one ends, another is admitted.
#[test]
fn an_agent_holds_at_most_1024_requests_in_flight() {
    let mut bare = Bare::new();
    let read = |seq: u64, member: &str| json!({"request": seq, "op": "getattr", "member": member, "size": 1, "data": "AAAA"});
    for seq in 1..=1024 {
        bare.rpc(1, read(seq, "public"));
    }
    assert!(bare.told().is_empty());
    bare.rpc(1, read(1025, &"m".repeat(5000)));
    let error = "agent \"primary\" already has 1024 requests in flight";
    assert_eq!(
        bare.told(),
        [
            json!({"t": "rpc", "agent": PRIMARY, "binding": "fx", "id": 1, "refused": 1025,
            "reason": "queue_full", "error": error})
        ]
    );
    assert_eq!(bare.relay.queued().0.len(), 1024);
    let recorded = bare.recorded();
    let [
        Payload::HostedReceived {
            member: Some(member),
            by: ReceivedBy::Relay,
            ..
        },
        Payload::HostedEnded {
            outcome: HostedOutcome::Refused,
            reason: Some(HostedReason::QueueFull),
            detail: Some(detail),
            ..
        },
    ] = recorded.as_slice()
    else {
        panic!("{recorded:?}");
    };
    assert_eq!((member.len(), detail.as_str()), (256, error));
    bare.wrote("ended", json!(1), json!({"outcome": "returned"}));
    bare.rpc(1, read(1025, "public"));
    assert!(bare.told().is_empty());
}
