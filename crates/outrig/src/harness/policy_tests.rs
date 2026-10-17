//! `[policy]` deciding a session's hosted calls, end to end on the host: rules and defaults, the
//! escalation handler, and the gate that settles a held call against an interrupt and the close.
//! The fixture's `noted` counts its calls and `touch` leaves a file, so a test shows that a call
//! that was refused or cancelled never reached its target.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use tokio::sync::{mpsc, oneshot};

use super::bindings_tests::{Bed, FIXTURE, output, printed, start, stop, submit, taken};
use super::event::{
    DecidedBy, Event, HostedOutcome, HostedReason, Operation, Payload, PolicyLayer, Received,
    RequestId, Subscription,
};
use super::{
    Escalation, EscalationAnswer, EscalationHandler, EscalationResult, Session, SessionBuilder,
    ShutdownReport, Withdrawn,
};
use crate::config::{Config, PolicyAction};
use crate::python::testing::{ended, within};

/// `noted` and `touch` are refused; every other request runs.
const DENY: &str = r#"
[[policy.rules]]
member    = "noted"
operation = "call"
action    = "deny"

[[policy.rules]]
member    = "touch"
operation = "call"
action    = "deny"
"#;

/// Everything but a read is refused by the default.
const DEFAULT_DENY: &str = r#"
[policy]
default = "deny"

[[policy.rules]]
operation = "getattr"
action    = "allow"
"#;

/// A person approves every call of `noted` and of `touch`.
const ESCALATE: &str = r#"
[[policy.rules]]
binding   = "fx"
type      = "outrig_fixture.Root"
member    = "noted"
operation = "call"
action    = "escalate"

[[policy.rules]]
member    = "touch"
operation = "call"
action    = "escalate"
"#;

/// The evaluator judges every call of `noted`, and there is none.
const EVALUATE: &str = r#"
[[policy.rules]]
member    = "noted"
operation = "call"
action    = "evaluate"
"#;

/// Calls `noted`, saying how a refusal ended it, then how many times it ran.
const NOTED: &str = "\
try:
    print('returned', fx.noted(7, 'x'))
except runtime.Denied as e:
    print('denied:', e.reason)
except Exception as e:
    print('raised', type(e).__name__, e)
print(fx.calls)
";

/// What answers escalations in a test: each request the handler is asked about, with where its
/// answer goes, and how many it was asked about.
struct Asks {
    asks: mpsc::UnboundedReceiver<(Escalation, oneshot::Sender<EscalationResult>)>,
    asked: Arc<AtomicUsize>,
}

/// A handler the test answers, request by request, through [`Asks`]; one whose answer the test
/// drops answers `Err`.
fn asked() -> (impl EscalationHandler + 'static, Asks) {
    let (tx, asks) = mpsc::unbounded_channel();
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let handler = move |escalation: Escalation| {
        count.fetch_add(1, Ordering::SeqCst);
        let (answer, answered) = oneshot::channel();
        let _ = tx.send((escalation, answer));
        async move {
            answered
                .await
                .unwrap_or_else(|_| Err("the test dropped its answer".into()))
        }
    };
    (handler, Asks { asks, asked })
}

impl Asks {
    async fn next(&mut self) -> (Escalation, oneshot::Sender<EscalationResult>) {
        within(self.asks.recv())
            .await
            .expect("the handler is asked")
    }

    fn count(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

/// A handler answering every request with what `answer` gives, counting them.
fn answering(
    answer: impl Fn() -> EscalationResult + Send + Sync + 'static,
) -> (impl EscalationHandler + 'static, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let handler = move |_: Escalation| {
        count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(answer())
    };
    (handler, asked)
}

/// A bed whose global config holds the fixture binding and `policy`, and whose repository config
/// is `repo`.
fn bed(policy: &str, repo: &str) -> Bed {
    let bed = Bed::new();
    bed.write(&format!("{FIXTURE}\n{policy}"), repo);
    bed
}

/// A session from `builder`, and its events.
async fn open(mut builder: SessionBuilder) -> (Session, Subscription) {
    let events = builder.subscribe();
    (start(builder).await, events)
}

/// The builder of a session over `bed`'s config.
fn over(bed: &Bed) -> SessionBuilder {
    bed.builder(bed.load())
}

/// The last call of `member` the events hold.
fn call_of(events: &[Arc<Event>], member: &str) -> RequestId {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            Payload::HostedReceived {
                request,
                operation: Operation::Call,
                member: Some(called),
                ..
            } if called == member => Some(*request),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no call of {member}"))
}

/// The kinds of `request`'s events, in order.
fn kinds(events: &[Arc<Event>], request: RequestId) -> Vec<&'static str> {
    events
        .iter()
        .filter(|event| match &event.payload {
            Payload::HostedReceived { request: r, .. }
            | Payload::HostedDecided { request: r, .. }
            | Payload::HostedDispatched { request: r, .. }
            | Payload::HostedEnded { request: r, .. } => *r == request,
            _ => false,
        })
        .map(|event| event.kind())
        .collect()
}

/// Each decision about `request`: the action, what made it, and whether it was ignored.
fn decisions(events: &[Arc<Event>], request: RequestId) -> Vec<(PolicyAction, DecidedBy, bool)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::HostedDecided {
                request: r,
                action,
                by,
                ignored,
                ..
            } if *r == request => Some((*action, *by, *ignored)),
            _ => None,
        })
        .collect()
}

/// The version `session.policy` gave the session's rules.
fn loaded_version(events: &[Arc<Event>]) -> String {
    events
        .iter()
        .find_map(|event| match &event.payload {
            Payload::SessionPolicy { version, .. } => Some(version.to_string()),
            _ => None,
        })
        .expect("session.policy")
}

/// Take events into `seen` until one satisfies `done`.
async fn until(
    events: &mut Subscription,
    seen: &mut Vec<Arc<Event>>,
    done: impl Fn(&Event) -> bool,
) {
    within(async {
        loop {
            if seen.iter().any(|event| done(event)) {
                return;
            }
            match events.recv().await {
                Some(Received::Event(event)) => seen.push(event),
                Some(Received::Missed(n)) => panic!("the subscription missed {n} events"),
                None => panic!("the stream closed"),
            }
        }
    })
    .await;
}

fn rule(layer: PolicyLayer, rule: usize) -> DecidedBy {
    DecidedBy::Rule { layer, rule }
}

/// A rule's deny, the default's, the approver's, and an escalation with no approver each raise
/// `runtime.Denied` in the caller with the reason the request's `refused` outcome records, and
/// leave the target uninvoked: no dispatch, and the counter at zero.
#[tokio::test]
async fn each_refusal_leaves_the_target_uninvoked() {
    let operator = PolicyLayer::Operator;
    for (policy, answer, reason, decided) in [
        (
            DENY,
            None,
            HostedReason::Rule,
            vec![(PolicyAction::Deny, rule(operator, 1), false)],
        ),
        (
            DEFAULT_DENY,
            None,
            HostedReason::Default,
            vec![(
                PolicyAction::Deny,
                DecidedBy::Default { layer: operator },
                false,
            )],
        ),
        (
            ESCALATE,
            Some(EscalationAnswer::Deny("not today".into())),
            HostedReason::Approver,
            vec![
                (PolicyAction::Escalate, rule(operator, 1), false),
                (PolicyAction::Deny, DecidedBy::Approver, false),
            ],
        ),
        (
            ESCALATE,
            None,
            HostedReason::NoApprover,
            vec![
                (PolicyAction::Escalate, rule(operator, 1), false),
                (PolicyAction::Deny, DecidedBy::NoApprover, false),
            ],
        ),
    ] {
        let bed = bed(policy, "");
        let mut builder = over(&bed);
        if let Some(answer) = answer {
            builder = builder.escalation(answering(move || Ok(answer.clone())).0);
        }
        let (session, mut events) = open(builder).await;
        let out = output(&session, NOTED).await;
        let mut lines = out.lines();
        let denied = lines.next().expect("a line");
        assert!(denied.starts_with("denied: "), "{policy}: {out}");
        assert_eq!(lines.next(), Some("0"), "{policy}: the target ran: {out}");
        stop(session).await;
        let events = taken(&mut events);
        let noted = call_of(&events, "noted");
        let mut expected = vec!["hosted.received"];
        expected.extend(decided.iter().map(|_| "hosted.decided"));
        expected.push("hosted.ended");
        assert_eq!(kinds(&events, noted), expected, "{policy}");
        assert_eq!(decisions(&events, noted), decided, "{policy}");
        assert_eq!(
            ended(&events, noted),
            (HostedOutcome::Refused, Some(reason))
        );
        let detail = events.iter().find_map(|event| match &event.payload {
            Payload::HostedEnded {
                request, detail, ..
            } if *request == noted => detail.clone(),
            _ => None,
        });
        assert!(
            denied.contains(detail.as_deref().expect("a refusal's detail")),
            "the caller's reason is the record's: {denied} / {detail:?}"
        );
    }
}

/// An approval runs the call exactly once, and the handler is shown the call -- as a call of the
/// member it was read from, with its arguments -- under the session's policy version. The read
/// that produced the method was not escalated: the handler is asked once.
#[tokio::test]
async fn an_approval_runs_the_call_once() {
    let bed = bed(ESCALATE, "");
    let (handler, mut asks) = asked();
    let (session, mut events) = open(over(&bed).escalation(handler)).await;
    let mut call = submit(&session, NOTED);
    let (escalation, answer) = asks.next().await;
    assert_eq!(escalation.binding, "fx");
    assert_eq!(escalation.operation, Operation::Call);
    assert_eq!(escalation.member.as_deref(), Some("noted"));
    assert_eq!(escalation.target.as_deref(), Some("outrig_fixture.Root"));
    assert_eq!(escalation.arguments, ["7", "'x'"]);
    assert!(
        escalation.reason.contains("operator rule 1"),
        "{}",
        escalation.reason
    );
    answer.send(Ok(EscalationAnswer::Allow)).expect("answered");
    assert_eq!(printed(&mut call).await, "returned 1\n1\n");
    assert_eq!(asks.count(), 1);
    stop(session).await;

    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(escalation.request, noted);
    assert_eq!(
        kinds(&events, noted),
        [
            "hosted.received",
            "hosted.decided",
            "hosted.decided",
            "hosted.dispatched",
            "hosted.ended"
        ]
    );
    assert_eq!(
        decisions(&events, noted),
        [
            (
                PolicyAction::Escalate,
                rule(PolicyLayer::Operator, 1),
                false
            ),
            (PolicyAction::Allow, DecidedBy::Approver, false),
        ]
    );
    // Every decision, and the escalation, carry the version the session started with.
    let version = loaded_version(&events);
    assert_eq!(escalation.policy.as_str(), version);
    for event in &events {
        if let Payload::HostedDecided { version: v, .. } = &event.payload {
            assert_eq!(v.as_str(), version);
        }
    }
}

/// A request a rule denies never reaches the handler.
#[tokio::test]
async fn a_deny_rule_never_reaches_the_handler() {
    let bed = bed(DENY, "");
    let (handler, asked) = answering(|| Ok(EscalationAnswer::Allow));
    let (session, _events) = open(over(&bed).escalation(handler)).await;
    let out = output(&session, NOTED).await;
    assert!(out.starts_with("denied: "), "{out}");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    stop(session).await;
}

/// With no `[policy]` at all, nothing is decided: the session says so once, and each request
/// keeps its three events.
#[tokio::test]
async fn with_no_policy_requests_carry_no_decision() {
    let bed = bed("", "");
    let (session, mut events) = open(over(&bed)).await;
    assert_eq!(output(&session, NOTED).await, "returned 1\n1\n");
    stop(session).await;
    let events = taken(&mut events);
    assert!(
        events.iter().any(|event| matches!(
            event.payload,
            Payload::SessionPolicy {
                configured: false,
                asks: false,
                handler: false,
                ..
            }
        )),
        "no session.policy saying nothing is configured"
    );
    let noted = call_of(&events, "noted");
    assert_eq!(
        kinds(&events, noted),
        ["hosted.received", "hosted.dispatched", "hosted.ended"]
    );
}

/// Interrupting a pending request withdraws it: the handler is told, the request ends
/// `cancelled`, `interrupted`, and an allow given afterward is recorded as ignored and runs
/// nothing. So for a call made inside a callback: the cancel names that call's own request.
#[tokio::test]
async fn an_interrupted_request_runs_nothing_when_allowed_after() {
    for source in [
        "try:\n    fx.noted()\nexcept KeyboardInterrupt as e:\n    print('interrupted')\n",
        "def back():\n    return fx.noted()\n\
         try:\n    fx.call(back)\nexcept BaseException as e:\n    print('interrupted')\n",
    ] {
        let bed = bed(ESCALATE, "");
        let (handler, mut asks) = asked();
        let (session, mut events) = open(over(&bed).escalation(handler)).await;
        let mut call = submit(&session, source);
        let (escalation, answer) = asks.next().await;
        session.interpreter().interrupt(call.id(), false);
        assert_eq!(within(escalation.withdrawn()).await, Withdrawn::Interrupted);
        assert_eq!(printed(&mut call).await, "interrupted\n", "{source}");
        answer.send(Ok(EscalationAnswer::Allow)).expect("answered");
        let mut seen = Vec::new();
        until(&mut events, &mut seen, |event| {
            matches!(event.payload, Payload::HostedDecided { ignored: true, .. })
        })
        .await;
        assert_eq!(output(&session, "print(fx.calls)").await, "0\n", "{source}");
        stop(session).await;
        seen.extend(taken(&mut events));
        let noted = call_of(&seen, "noted");
        assert_eq!(
            ended(&seen, noted),
            (HostedOutcome::Cancelled, Some(HostedReason::Interrupted))
        );
        assert!(!kinds(&seen, noted).contains(&"hosted.dispatched"));
        assert_eq!(
            decisions(&seen, noted).last(),
            Some(&(PolicyAction::Allow, DecidedBy::Approver, true))
        );
    }
}

/// A caller interrupted before its binding read the request -- the bindings process stopped, so
/// the cancel reaches the owner first -- is cancelled when the request would be held: nobody is
/// asked, and the target never runs.
#[tokio::test]
async fn a_request_interrupted_before_it_is_held_asks_nobody() {
    let bed = bed(ESCALATE, "");
    let (handler, asks) = asked();
    let (session, mut events) = open(over(&bed).escalation(handler)).await;
    let pid = session.bindings().pid().expect("the bindings process");
    let mut call = submit(
        &session,
        "try:\n    fx.noted()\nexcept KeyboardInterrupt as e:\n    print('interrupted')\n",
    );
    // The read of `noted` goes first; stopped once it has, the process holds the call's frame
    // unread.
    let mut seen = Vec::new();
    until(&mut events, &mut seen, |event| {
        matches!(&event.payload, Payload::HostedEnded { .. })
    })
    .await;
    kill(pid, Signal::SIGSTOP).expect("stop the bindings process");
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.interpreter().interrupt(call.id(), false);
    assert_eq!(printed(&mut call).await, "interrupted\n");
    kill(pid, Signal::SIGCONT).expect("continue the bindings process");
    until(&mut events, &mut seen, |event| {
        matches!(
            &event.payload,
            Payload::HostedEnded {
                outcome: HostedOutcome::Cancelled,
                ..
            }
        )
    })
    .await;
    assert_eq!(output(&session, "print(fx.calls)").await, "0\n");
    assert_eq!(
        asks.count(),
        0,
        "nobody is asked about a call its caller gave up"
    );
    stop(session).await;
    let noted = call_of(&seen, "noted");
    assert_eq!(
        ended(&seen, noted),
        (HostedOutcome::Cancelled, Some(HostedReason::Interrupted))
    );
}

/// Calls `touch`, saying how a refusal ended it.
fn touching(path: &Path) -> String {
    format!(
        "try:\n    fx.touch({:?})\n    print('touched')\nexcept Exception as e:\n    \
         print('raised', type(e).__name__)\n",
        path.to_str().expect("a UTF-8 path")
    )
}

/// The report's entry for `request`.
fn reported(report: &ShutdownReport, request: RequestId) -> (HostedOutcome, Option<HostedReason>) {
    let call = report
        .hosted
        .iter()
        .find(|call| call.request == request)
        .unwrap_or_else(|| panic!("{request} is not in the report: {report:?}"));
    (call.outcome, call.reason)
}

/// A close while a request waits for its approval cancels it: the handler sees the withdrawal,
/// the caller gets the closing error, a late allow is recorded and changes nothing, and the
/// report lists the request `cancelled`, never invoked -- still so when the bindings process is
/// killed afterward, since the gate recorded it.
#[tokio::test]
async fn a_close_cancels_a_pending_request() {
    let bed = bed(ESCALATE, "");
    let marker = bed.records.join("touched");
    let (handler, mut asks) = asked();
    let (session, mut events) = open(over(&bed).escalation(handler)).await;
    let pid = session.bindings().pid().expect("the bindings process");
    let mut call = submit(&session, &touching(&marker));
    let (escalation, answer) = asks.next().await;
    session.close_admission();
    assert_eq!(within(escalation.withdrawn()).await, Withdrawn::Closed);
    let out = printed(&mut call).await;
    assert!(
        out.starts_with("raised ") && out.contains("Closed"),
        "{out}"
    );
    let _ = answer.send(Ok(EscalationAnswer::Allow));
    let mut seen = Vec::new();
    until(&mut events, &mut seen, |event| {
        matches!(event.payload, Payload::HostedDecided { ignored: true, .. })
    })
    .await;
    kill(pid, Signal::SIGKILL).expect("kill the bindings process");
    let report = stop(session).await;
    seen.extend(taken(&mut events));
    assert!(!marker.exists(), "the cancelled call ran");
    let touched = call_of(&seen, "touch");
    assert_eq!(
        reported(&report, touched),
        (HostedOutcome::Cancelled, Some(HostedReason::Closed))
    );
    assert!(!kinds(&seen, touched).contains(&"hosted.dispatched"));
}

/// An allow and the close, raced with each order forced, settle the request once, and whether
/// the target ran agrees with how it is recorded. An allow that won before the close reaches the
/// bindings process after its closed flag -- stopped until both were sent -- and the call runs.
#[tokio::test]
async fn an_allow_and_the_close_settle_once_in_either_order() {
    for allow_first in [true, false] {
        let bed = bed(ESCALATE, "");
        let marker = bed.records.join("touched");
        let (handler, mut asks) = asked();
        let (session, mut events) = open(over(&bed).escalation(handler)).await;
        let pid = session.bindings().pid().expect("the bindings process");
        let mut call = submit(&session, &touching(&marker));
        let (escalation, answer) = asks.next().await;
        let mut seen = Vec::new();
        if allow_first {
            kill(pid, Signal::SIGSTOP).expect("stop the bindings process");
            answer.send(Ok(EscalationAnswer::Allow)).expect("answered");
            until(&mut events, &mut seen, |event| {
                matches!(
                    event.payload,
                    Payload::HostedDecided {
                        by: DecidedBy::Approver,
                        ..
                    }
                )
            })
            .await;
            session.close_admission();
            kill(pid, Signal::SIGCONT).expect("continue the bindings process");
            assert_eq!(printed(&mut call).await, "touched\n");
            assert!(
                escalation.is_withdrawn().is_none(),
                "an allowed request is not withdrawn"
            );
        } else {
            session.close_admission();
            assert_eq!(within(escalation.withdrawn()).await, Withdrawn::Closed);
            let _ = answer.send(Ok(EscalationAnswer::Allow));
            assert!(printed(&mut call).await.contains("Closed"));
        }
        let report = stop(session).await;
        seen.extend(taken(&mut events));
        let touched = call_of(&seen, "touch");
        let outcome = reported(&report, touched);
        if allow_first {
            assert_eq!(outcome, (HostedOutcome::Returned, None));
            assert!(marker.exists());
        } else {
            assert_eq!(
                outcome,
                (HostedOutcome::Cancelled, Some(HostedReason::Closed))
            );
            assert!(!marker.exists());
        }
        let endings = seen
            .iter()
            .filter(|event| {
                matches!(&event.payload,
                Payload::HostedEnded { request, .. } if *request == touched)
            })
            .count();
        assert_eq!(endings, 1, "one outcome");
    }
}

/// A repository's rule restricts the operator's and cannot loosen it: the stricter action wins,
/// so an operator `escalate` beside a repo `evaluate` escalates; and a repo `deny` refuses what
/// the operator's rules allow.
#[tokio::test]
async fn a_repo_rule_restricts_and_the_stricter_action_wins() {
    let repo_evaluates =
        "[[policy.rules]]\nmember = \"noted\"\noperation = \"call\"\naction = \"evaluate\"\n";
    let bed_a = bed(ESCALATE, repo_evaluates);
    let (handler, asked) = answering(|| Ok(EscalationAnswer::Allow));
    let (session_a, mut events) = open(over(&bed_a).escalation(handler)).await;
    assert_eq!(output(&session_a, NOTED).await, "returned 1\n1\n");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    stop(session_a).await;
    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(
        decisions(&events, noted)[0],
        (
            PolicyAction::Escalate,
            rule(PolicyLayer::Operator, 1),
            false
        )
    );

    let repo_denies =
        "[[policy.rules]]\nmember = \"noted\"\noperation = \"call\"\naction = \"deny\"\n";
    let bed_b = bed("", repo_denies);
    let (session_b, mut events) = open(over(&bed_b)).await;
    assert!(output(&session_b, NOTED).await.starts_with("denied: "));
    stop(session_b).await;
    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(
        decisions(&events, noted),
        [(PolicyAction::Deny, rule(PolicyLayer::Repo, 1), false)]
    );
}

/// A `Config` given to the builder with no layer kept -- one a program built -- is the
/// operator's: its `[policy]` decides as a global one does.
#[tokio::test]
async fn a_built_config_is_the_operators() {
    let bed = bed(DENY, "");
    let text = std::fs::read_to_string(&bed.global).expect("the global config");
    let built = Config::load_from_str(&text).expect("parses");
    let (session, mut events) = open(bed.builder(built)).await;
    assert!(output(&session, NOTED).await.starts_with("denied: "));
    stop(session).await;
    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(
        decisions(&events, noted),
        [(PolicyAction::Deny, rule(PolicyLayer::Operator, 1), false)]
    );
}

/// `evaluate`, with no evaluator, escalates when a handler is installed and is denied when none
/// is; it never allows by itself.
#[tokio::test]
async fn evaluate_with_no_evaluator_escalates_or_denies() {
    let bed_a = bed(EVALUATE, "");
    let (handler, asked) = answering(|| Ok(EscalationAnswer::Allow));
    let (session, mut events) = open(over(&bed_a).escalation(handler)).await;
    assert_eq!(output(&session, NOTED).await, "returned 1\n1\n");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    stop(session).await;
    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(
        decisions(&events, noted),
        [
            (
                PolicyAction::Evaluate,
                rule(PolicyLayer::Operator, 1),
                false
            ),
            (PolicyAction::Escalate, DecidedBy::NoEvaluator, false),
            (PolicyAction::Allow, DecidedBy::Approver, false),
        ]
    );

    let bed_b = bed(EVALUATE, "");
    let (session, mut events) = open(over(&bed_b)).await;
    assert!(output(&session, NOTED).await.starts_with("denied: "));
    stop(session).await;
    let events = taken(&mut events);
    let noted = call_of(&events, "noted");
    assert_eq!(
        decisions(&events, noted),
        [
            (
                PolicyAction::Evaluate,
                rule(PolicyLayer::Operator, 1),
                false
            ),
            (PolicyAction::Deny, DecidedBy::NoEvaluator, false),
            (PolicyAction::Deny, DecidedBy::NoApprover, false),
        ]
    );
    assert_eq!(
        ended(&events, noted),
        (HostedOutcome::Refused, Some(HostedReason::NoApprover))
    );
}

/// A handler that fails, one that panics, and one dropped without answering each deny: the
/// target never runs, and the request is refused by the approver.
#[tokio::test]
async fn a_handler_that_fails_or_is_dropped_denies() {
    for case in ["fails", "panics", "dropped"] {
        let bed = bed(ESCALATE, "");
        let (handler, mut asks) = asked();
        let failing = move |escalation: Escalation| async move {
            if case == "panics" {
                panic!("the handler broke on purpose");
            }
            drop(escalation);
            Err::<EscalationAnswer, _>("the handler broke on purpose".into())
        };
        let (session, mut events) = match case {
            "dropped" => open(over(&bed).escalation(handler)).await,
            _ => open(over(&bed).escalation(failing)).await,
        };
        let mut call = submit(&session, NOTED);
        if case == "dropped" {
            let (_, answer) = asks.next().await;
            drop(answer);
        }
        let out = printed(&mut call).await;
        assert!(
            out.starts_with("denied: ") && out.ends_with("\n0\n"),
            "{case}: {out}"
        );
        stop(session).await;
        let events = taken(&mut events);
        let noted = call_of(&events, "noted");
        assert_eq!(
            ended(&events, noted),
            (HostedOutcome::Refused, Some(HostedReason::Approver)),
            "{case}"
        );
    }
}
