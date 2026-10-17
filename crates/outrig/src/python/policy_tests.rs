//! The policy in the bindings process, through the relay in `hosted.rs`: what a request is matched
//! as however agent code -- or a client written by hand -- makes it, and a request held for an
//! answer while the binding serves everything else.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use super::binding_tests::{lines_of, with_raw, with_root, with_root_in};
use super::relay::{Relay, TIMEOUT};
use super::testing::{Flag, ended};
use crate::config::{Config, Policy};
use crate::harness::escalation::Handler;
use crate::harness::event::{Event, HostedOutcome, HostedReason, Operation, Payload, RequestId};
use crate::harness::{Escalation, EscalationAnswer, EscalationResult};

const FACTORY: &str = "outrig_fixture:make";

/// Decide `relay`'s requests by the `[policy]` in `toml`, asking `handler` about what it holds.
fn govern(relay: &mut Relay, toml: &str, handler: Option<Handler>) {
    let config = Config::load_from_str(toml).expect("the policy parses");
    config.validate(None).expect("the policy validates");
    relay.govern(Policy::of(&config), handler);
}

/// A handler whose requests arrive on the returned channel, each with where its answer goes.
type Asked = (Escalation, oneshot::Sender<EscalationResult>);

fn asking() -> (Handler, mpsc::Receiver<Asked>) {
    let (tx, rx) = mpsc::channel::<Asked>();
    let tx = Mutex::new(tx);
    let handler = move |escalation: Escalation| {
        let (answer, answered) = oneshot::channel();
        let _ = tx.lock().expect("the sender").send((escalation, answer));
        async move {
            answered
                .await
                .unwrap_or_else(|_| Err("the test dropped its answer".into()))
        }
    };
    (Arc::new(handler), rx)
}

/// Each request's receipt, by id: its operation, member and target.
fn received(events: &[Arc<Event>]) -> Vec<(RequestId, Operation, Option<String>, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::HostedReceived {
                request,
                operation,
                member,
                target,
                ..
            } => Some((*request, *operation, member.clone(), target.clone())),
            _ => None,
        })
        .collect()
}

/// The line of `output` that starts with `label`, which there is one of.
fn line<'a>(output: &'a str, label: &str) -> &'a str {
    let lines = lines_of(output, &format!("{label} "));
    assert_eq!(lines.len(), 1, "{label:?} in {output}");
    lines[0]
}

/// A rule on a method's call holds however the call is made. RPyC's `callattr`, which reads and
/// calls in one request; the same function read under another name; a host method passed as an
/// argument -- one the host calls itself, or nested in a container -- or assigned as a value,
/// which the host could call with no request; and Python 2's `oldslicing`, whose fallback calls a
/// second member: each is refused, and the method never runs. A type rule holds of an object that
/// told RPyC another type's name: rules match the type the host names.
#[test]
fn a_call_is_decided_however_it_is_made() {
    let mut relay = Relay::start();
    govern(
        &mut relay,
        r#"
        [[policy.rules]]
        type      = "outrig_fixture.Root"
        member    = "noted"
        operation = "call"
        action    = "deny"

        [[policy.rules]]
        type   = "outrig_fixture.Bag"
        member = "secret"
        action = "deny"

        [[policy.rules]]
        type      = "outrig_fixture.Root"
        operation = "repr"
        action    = "deny"
        "#,
        None,
    );
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_raw(
            r#"
            attempt("call", lambda: root.noted())
            attempt("callattr", lambda: raw(consts.HANDLE_CALLATTR,
                (T, ((L, rid), (V, "noted"), (T, ()), (V, ())))))
            attempt("alias", lambda: root.also_noted())
            attempt("aliased callattr", lambda: raw(consts.HANDLE_CALLATTR,
                (T, ((L, rid), (V, "also_noted"), (T, ()), (V, ())))))
            attempt("property callattr", lambda: raw(consts.HANDLE_CALLATTR,
                (T, ((L, rid), (V, "noted_later"), (T, ()), (V, ())))))
            attempt("property call", lambda: root.noted_later())
            attempt("dunder", lambda: raw(consts.HANDLE_GETATTR,
                (T, ((L, rid), (V, "__repr__"))))())
            attempt("buffiter", lambda: raw(consts.HANDLE_BUFFITER,
                (T, ((L, idp(root.sequence)), (V, 2)))))
            attempt("argument", lambda: root.call(root.noted))
            attempt("nested", lambda: root.shape([{"k": root.noted}]))
            attempt("setattr", lambda: setattr(root, "kept", root.noted))
            attempt("oldslicing", lambda: raw(consts.HANDLE_OLDSLICING,
                (T, ((L, rid), (V, "nope"), (V, "noted"), (V, 0), (V, 1), (V, ())))))
            bag = root.bag
            bag["____id_pack__"] = ("builtins.dict", 1, 2)
            attempt("forged", lambda: root.bag.secret)
            attempt("read", lambda: root.public)
            print("untouched", root.calls, root.kept)
            "#,
        ),
    );
    for label in [
        "call",
        "callattr",
        "alias",
        "aliased callattr",
        "property call",
        "dunder",
        "argument",
        "nested",
        "setattr",
        "forged",
    ] {
        let refused = line(&out, label);
        assert!(refused.contains("refused: Denied"), "{label}: {out}");
    }
    assert!(
        line(&out, "argument").contains("argument 1 passes noted of outrig_fixture.Root"),
        "{out}"
    );
    assert!(
        line(&out, "oldslicing").contains("refused: outrig.Refused oldslicing"),
        "{out}"
    );
    assert!(
        line(&out, "buffiter").contains("refused: outrig.Refused buffiter"),
        "{out}"
    );
    assert!(
        line(&out, "property callattr")
            .contains("refused: outrig.Refused callattr of 'noted_later'"),
        "{out}"
    );
    assert_eq!(line(&out, "read"), "read -> 'public value'");
    assert_eq!(line(&out, "untouched"), "untouched 0 None");

    let events = relay.hosted_events();
    let receipts = received(&events);
    // The alias's call is shown as the member it was read as, and refused as the one it is.
    assert!(
        receipts
            .iter()
            .any(|(_, op, member, target)| *op == Operation::Call
                && member.as_deref() == Some("also_noted")
                && target.as_deref() == Some("outrig_fixture.Root")),
        "{receipts:?}"
    );
    // The forged object's read names its type as the host does.
    let forged = receipts
        .iter()
        .rev()
        .find(|(_, op, member, _)| *op == Operation::Getattr && member.as_deref() == Some("secret"))
        .expect("the read of secret");
    assert_eq!(forged.3.as_deref(), Some("outrig_fixture.Bag"));
    assert_eq!(
        ended(&events, forged.0),
        (HostedOutcome::Refused, Some(HostedReason::Rule))
    );
}

/// A rule's wildcards are matched without backtracking: a name of thousands of characters that a
/// rule of many `*` does not match is decided at once, and one it does is refused.
#[test]
fn a_wildcard_rule_decides_a_long_name_at_once() {
    let mut relay = Relay::start();
    govern(
        &mut relay,
        "[[policy.rules]]\nmember = \"*a*a*a*a*a*a*a*a*b\"\naction = \"deny\"\n",
        None,
    );
    relay.bind("fx", FACTORY);
    let started = std::time::Instant::now();
    let out = relay.output(
        1,
        &with_raw(
            r#"
            attempt("unmatched", lambda: hasattr(root, "a" * 5000))
            attempt("matched", lambda: getattr(root, "a" * 5000 + "b"))
            "#,
        ),
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(line(&out, "unmatched"), "unmatched -> False");
    assert!(line(&out, "matched").contains("refused: Denied"), "{out}");
}

/// While one kernel's request waits for an answer, another kernel's request to the same binding
/// returns: the held request holds its connection's serving thread and no turn.
#[test]
fn a_held_request_blocks_only_its_caller() {
    let mut relay = Relay::start();
    let (handler, asks) = asking();
    govern(
        &mut relay,
        "[[policy.rules]]\nmember = \"noted\"\noperation = \"call\"\naction = \"escalate\"\n",
        Some(handler),
    );
    relay.bind("fx", FACTORY);
    relay.open("b");
    relay.submit_in("primary", 1, &with_root("print(root.noted())"));
    let (escalation, answer) = asks.recv_timeout(TIMEOUT).expect("the handler is asked");
    assert_eq!(escalation.member.as_deref(), Some("noted"));
    assert_eq!(
        relay.output_in("b", 2, &with_root_in("b", "print(root.method(1))")),
        "2\n"
    );
    assert!(
        relay
            .try_result_from("primary", 1, Duration::from_millis(300))
            .is_none(),
        "the held request waits for its answer"
    );
    answer
        .send(Ok(EscalationAnswer::Allow))
        .expect("the handler waits");
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["output"], "1\n", "{result}");
}

/// With rules in force, admission is still decided in one place. A request made after the close
/// is refused before it reaches the bindings process, so no rule decides it and its target never
/// runs; and a frame the relay forwards after the close as bookkeeping, whose request is a read
/// the rules allow, is refused by the process's closed flag before any rule is asked.
#[test]
fn admission_is_decided_in_one_place_with_rules_in_force() {
    let mut relay = Relay::start();
    govern(
        &mut relay,
        "[[policy.rules]]\nmember = \"*\"\naction = \"allow\"\n",
        None,
    );
    relay.bind("fx", FACTORY);
    relay.output(
        1,
        &with_root("import builtins\nbuiltins.root = root\nroot.public"),
    );
    relay.close_admission();
    let marker = Flag::new();
    let out = relay.output(
        2,
        &with_root(&format!(
            "try:\n    root.touch({})\nexcept Exception as e:\n    print(type(e).__name__)",
            marker.py()
        )),
    );
    assert_eq!(out, "outrig.Closed\n");
    assert!(!marker.exists(), "the refused call ran");
    let out = relay.output(
        3,
        &with_raw(
            r#"
            res = rpyc.core.async_.AsyncResult(conn)
            seq = conn._get_seq_id()
            conn._request_callbacks[seq] = res
            boxed = (T, ((L, rid), (V, "public")))
            conn._send(consts.MSG_REQUEST, seq, (consts.HANDLE_GETATTR, boxed),
                       {"request": seq, "op": "getroot"})
            try:
                res.value
            except Exception as e:
                print(type(e).__name__)
            "#,
        ),
    );
    assert_eq!(out, "outrig.Closed\n");
    let events = relay.hosted_events();
    assert!(
        !events
            .iter()
            .skip_while(|event| !matches!(&event.payload,
                Payload::HostedReceived { member: Some(m), .. } if m == "touch"))
            .any(|event| matches!(event.payload, Payload::HostedDecided { .. })),
        "a request after the close was decided by a rule"
    );
    let (read, ..) = received(&events)
        .into_iter()
        .rev()
        .find(|(_, op, member, _)| *op == Operation::Getattr && member.as_deref() == Some("public"))
        .expect("the forged read");
    assert_eq!(
        ended(&events, read),
        (HostedOutcome::Refused, Some(HostedReason::AdmissionClosed))
    );
}
