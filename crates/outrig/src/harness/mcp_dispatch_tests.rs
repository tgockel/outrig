//! An MCP call's three dispatch states: decided by where in the call it ended, mapped onto the
//! service call's outcomes, raised in agent Python as the state's class, and recorded through the
//! report; the cancel notice the token sends; and a service whose transport failed connected
//! again for later calls, with nothing replayed. Each server is a fake that answers with the
//! JSON a test sets, placed in no container.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::ErrorCode;
use serde_json::{Value, json};

use super::bindings_tests::{output, start, stop, taken};
use super::event::{
    Event, McpDispatchState, ServiceKind, ServiceOutcome, ServiceReason, Subscription,
};
use super::mcp_placement::fake::{Act, Behavior, Toolbox};
use super::mcp_presenter_tests::{calls, printed, text, tool};
use super::mcp_tests::{Fakes, placed_under, policy, position, sidecar};
use super::services::McpEnd;
use super::services_tests::{VAR, builder, of_kind, quick};
use super::{
    CallContext, McpPolicy, McpServiceSpec, MethodDescription, Quiesce, QuiesceOutcome, Service,
    ServiceDescription, ServiceError, ServiceErrorCategory, ServiceFuture, ServicePolicy, Session,
    SessionBuilder, Verdict,
};
use crate::error::McpFailureKind;
use crate::python::host::Outcome;
use crate::python::testing::{Flag, until, within};

/// `mcp(f, ...)` prints how a call of an MCP service's tool ended: `ok` and its text, or the
/// class of the `outrig.McpError` it raised with its outcome, state, failure and reason.
pub(super) const MCP: &str = r#"
import outrig
def mcp(f, *a, **k):
    try:
        r = f(*a, **k)
    except outrig.McpError as e:
        print(type(e).__name__, e.outcome, e.state, e.failure, e.reason)
    else:
        print('ok', r.text)
"#;

/// A server listing a tool taking `{text: string}` for each of `names`.
fn toolbox(names: &[&str]) -> Arc<Toolbox> {
    Toolbox::new(Value::Array(
        names.iter().map(|name| tool(name, text())).collect(),
    ))
}

/// How the service `fs`'s server starts, attempt by attempt, each connect taking the next.
fn connects(toolboxes: &[&Arc<Toolbox>]) -> Vec<Behavior> {
    toolboxes
        .iter()
        .map(|toolbox| Behavior::Toolbox(Arc::clone(toolbox)))
        .collect()
}

/// A session whose MCP service `fs` -- a sidecar of its own -- connects as `attempts` say in
/// turn, its calls under `calls` and its starts under `starts`, built from `builder`; its events
/// subscribed to, `mcp` and `err` defined, and the fakes that placed it.
async fn session_of(
    builder: SessionBuilder,
    attempts: Vec<Behavior>,
    calls: ServicePolicy,
    starts: McpPolicy,
) -> (Session, Subscription, Fakes) {
    let fakes = Fakes::default();
    fakes.of("mcp-fs").script("fs", attempts);
    let builder = builder.mcp("fs", McpServiceSpec::sidecar(sidecar(&[])));
    let (session, events) = placed_under(&fakes, builder, calls, starts).await;
    output(&session, MCP).await;
    (session, events, fakes)
}

/// [`session_of`] with the default builder, calls of five seconds and quick starts.
async fn fs_session(attempts: Vec<Behavior>) -> (Session, Subscription, Fakes) {
    session_of(builder(), attempts, calls(), starts()).await
}

pub(super) fn starts() -> McpPolicy {
    policy(Duration::from_secs(5), Duration::from_secs(10))
}

/// Each `service.call.ended` of `tool`, in order.
fn ended(events: &[Arc<Event>], tool: &str) -> Vec<Value> {
    of_kind(events, "service.call.ended")
        .into_iter()
        .filter(|ended| ended["tool"] == tool)
        .collect()
}

/// `(outcome, state, failure, reason)` of an event.
fn how(ended: &Value) -> (&str, &str, &str, &str) {
    let text = |key: &str| ended[key].as_str().unwrap_or("null");
    (
        text("outcome"),
        text("state"),
        text("failure"),
        text("reason"),
    )
}

/// A write whose server closes its transport on receiving it may have happened: agent code gets
/// `MayHaveDispatched`, its failure the transport; the server got it once; the call is unknown
/// and uncertain, through the report, whose verdict is stopped with unknown; and nothing sends it
/// again.
#[tokio::test]
async fn may_have_dispatched_is_never_replayed() {
    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let (session, mut events, fakes) = fs_session(connects(&[&first])).await;
    assert_eq!(
        output(&session, "mcp(fs.write, text='x')").await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n"
    );
    assert_eq!(first.calls(), ["write"]);
    let uncertain = session.mcp().uncertain();
    assert_eq!(uncertain.len(), 1, "{uncertain:?}");
    assert_eq!(
        (
            uncertain[0].service.as_str(),
            uncertain[0].tool.as_str(),
            uncertain[0].agent.as_str(),
            uncertain[0].failure
        ),
        ("fs", "write", "primary", Some(McpFailureKind::Transport))
    );
    let report = stop(session).await;
    assert_eq!(report.mcp_uncertain, uncertain);
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    assert_eq!(first.calls(), ["write"], "never sent again");
    assert_eq!(fakes.of("mcp-fs").connects(), 1, "nothing reconnected");
    let events = taken(&mut events);
    assert_eq!(
        how(&ended(&events, "write")[0]),
        ("unknown", "may_have_dispatched", "transport", "null")
    );
    let disconnected = of_kind(&events, "mcp.service.disconnected");
    assert_eq!(disconnected.len(), 1, "{disconnected:?}");
    assert_eq!(disconnected[0]["cause"]["kind"], "transport");
    let reported = &of_kind(&events, "session.report")[0];
    assert_eq!(reported["mcp_uncertain"][0]["tool"], "write");
}

/// A call the server never got is known not dispatched, apart from one that may have been: an
/// unknown tool, a call made once admission closed, and one of a paused service each raise
/// their `NotDispatched` class, the server sees nothing, and each ends `refused`,
/// `not_dispatched`, with no `service.call.dispatched` before it.
#[tokio::test]
async fn known_not_dispatched_is_distinct() {
    let server = toolbox(&["write"]);
    let (session, mut events, _fakes) = fs_session(connects(&[&server])).await;
    assert_eq!(
        output(&session, "mcp(fs.call, 'missing', {})").await,
        "UnknownTool refused not_dispatched None unknown_method\n"
    );
    let paused = session
        .mcp()
        .quiesce("fs", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert!(
        matches!(paused, QuiesceOutcome::Quiesced { .. }),
        "{paused:?}"
    );
    assert_eq!(
        output(&session, "mcp(fs.write, text='x')").await,
        "Paused refused not_dispatched None paused\n"
    );
    assert!(session.mcp().resume("fs").await.is_ok());
    let go = Flag::new();
    let mut late = session
        .interpreter()
        .submit(&format!("{}\nmcp(fs.write, text='x')", go.awaited()))
        .unwrap_or_else(|e| panic!("{e}"));
    session.close_admission();
    go.raise();
    match within(late.outcome()).await {
        Outcome::Ok(report) => assert_eq!(
            report.output,
            "AdmissionClosed refused not_dispatched None admission_closed\n"
        ),
        other => panic!("{other:?}"),
    }
    stop(session).await;
    assert!(server.calls().is_empty(), "{:?}", server.calls());
    let events = taken(&mut events);
    assert!(of_kind(&events, "service.call.dispatched").is_empty());
    let states: Vec<_> = of_kind(&events, "service.call.ended")
        .iter()
        .map(|ended| (ended["outcome"].clone(), ended["state"].clone()))
        .collect();
    assert_eq!(
        states,
        vec![(json!("refused"), json!("not_dispatched")); 3],
        "{states:?}"
    );
}

/// A response is a response: an error the server answers raises `ProtocolError`, `raised`, its
/// category from the error's code and its message the server's; a result it marks an error
/// raises `ToolError`, `returned`; both are `response_received`.
#[tokio::test]
async fn a_response_is_a_response() {
    let server = toolbox(&["bad", "gone", "broke", "fails"]);
    server.act(
        "bad",
        Act::Fail(ErrorCode::INVALID_PARAMS.0, "text is not a path"),
    );
    server.act(
        "gone",
        Act::Fail(ErrorCode::METHOD_NOT_FOUND.0, "no such tool"),
    );
    server.act("broke", Act::Fail(-32603, "the disk is full"));
    server.answer(
        "fails",
        json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}),
    );
    let (session, mut events, _fakes) = fs_session(connects(&[&server])).await;
    assert_eq!(
        output(
            &session,
            r#"
for name in ['bad', 'gone', 'broke', 'fails']:
    try:
        fs.call(name, {})
    except outrig.McpError as e:
        print(type(e).__name__, e.outcome, e.category, e.state, e.failure, e.message)
"#
        )
        .await,
        "ProtocolError raised invalid_arguments response_received protocol \
         Mcp error: -32602: text is not a path\n\
         ProtocolError raised not_found response_received protocol Mcp error: -32601: no such tool\n\
         ProtocolError raised failed response_received protocol \
         Mcp error: -32603: the disk is full\n\
         ToolError returned None response_received None the tool reported an error: it failed\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    assert_eq!(
        how(&ended(&events, "bad")[0]),
        ("raised", "response_received", "protocol", "null")
    );
    assert_eq!(
        how(&ended(&events, "fails")[0]),
        ("returned", "response_received", "null", "null")
    );
}

/// A call past its deadline sends its server the cancel notice and returns at once, before the
/// table's grace: `MayHaveDispatched`, `timeout`, its cancel `deadline`, and one ending, an
/// answer that comes later discarded. A per-service deadline bounds that service alone.
#[tokio::test]
async fn the_notice_on_the_token() {
    let server = toolbox(&["stall", "late"]);
    server.act("stall", Act::Stall);
    server.act("late", Act::After(Duration::from_millis(1500)));
    let other = toolbox(&["stall", "late"]);
    other.act("stall", Act::Stall);
    other.act("late", Act::After(Duration::from_millis(1500)));
    let fakes = Fakes::default();
    fakes.of("mcp-fs").script("fs", connects(&[&server]));
    fakes.of("mcp-tight").script("tight", connects(&[&other]));
    let builder = builder()
        .mcp("fs", McpServiceSpec::sidecar(sidecar(&[])))
        .mcp(
            "tight",
            McpServiceSpec::sidecar(sidecar(&[])).with_call_timeout_secs(1),
        );
    let grace = ServicePolicy {
        cancel_grace: Duration::from_secs(5),
        ..quick(Duration::from_secs(10))
    };
    let (session, mut events) = placed_under(&fakes, builder, grace, starts()).await;
    output(&session, MCP).await;
    assert_eq!(
        output(
            &session,
            r#"
import time
t = time.monotonic()
mcp(fs.call, 'stall', {}, timeout=1)
took = time.monotonic() - t
print(0.9 < took < 3)
mcp(fs.call, 'late', {}, timeout=0.5)
mcp(tight.call, 'stall', {})
mcp(fs.call, 'late', {})
"#
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched timeout deadline\n\
         True\n\
         MayHaveDispatched unknown may_have_dispatched timeout deadline\n\
         MayHaveDispatched unknown may_have_dispatched timeout deadline\n\
         ok \n"
    );
    until("both cancels", || server.cancels().len() == 2).await;
    assert_eq!(server.cancels(), ["stall", "late"]);
    assert_eq!(other.cancels(), ["stall"]);
    // Long enough for the first `late` to have answered, unread.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop(session).await;
    let events = taken(&mut events);
    let cancels = of_kind(&events, "service.call.cancel.sent");
    assert!(
        cancels.iter().all(|cancel| cancel["reason"] == "deadline"),
        "{cancels:?}"
    );
    assert_eq!(cancels.len(), 3, "{cancels:?}");
    let lates = ended(&events, "late");
    assert_eq!(lates.len(), 2, "one ending each: {lates:?}");
    assert_eq!(lates[1]["outcome"], "returned");
}

/// An interrupt of the primary during a call raises in agent code, sends the server the notice,
/// and ends the call `unknown`, `canceled`; the kernel's loop is free afterward. A call in a
/// `to_thread` worker is not woken, and ends at its deadline.
#[tokio::test]
async fn an_interrupt_cancels() {
    let server = toolbox(&["stall"]);
    server.act("stall", Act::Stall);
    let (session, mut events, _fakes) = fs_session(connects(&[&server])).await;
    let mut call = session
        .interpreter()
        .submit("fs.call('stall', {})")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the call", || server.calls().len() == 1).await;
    session.interpreter().interrupt(call.id(), false);
    match within(call.outcome()).await {
        Outcome::Error { traceback, .. } => {
            assert!(traceback.contains("KeyboardInterrupt"), "{traceback}");
        }
        other => panic!("the call was not interrupted: {other:?}"),
    }
    until("the cancel", || server.cancels().len() == 1).await;
    assert_eq!(output(&session, "print('free')").await, "free\n");

    let mut worker = session
        .interpreter()
        .submit("await asyncio.to_thread(lambda: mcp(fs.call, 'stall', {}, timeout=1))")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the second call", || server.calls().len() == 2).await;
    session.interpreter().interrupt(worker.id(), false);
    let _ = within(worker.outcome()).await;
    until("both cancels", || server.cancels().len() == 2).await;
    stop(session).await;
    let events = taken(&mut events);
    let reasons: Vec<_> = of_kind(&events, "service.call.cancel.sent")
        .iter()
        .map(|cancel| cancel["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("interrupted"), json!("deadline")]);
    let stalls = ended(&events, "stall");
    assert_eq!(
        how(&stalls[0]),
        ("unknown", "may_have_dispatched", "canceled", "interrupted")
    );
    assert_eq!(
        how(&stalls[1]),
        ("unknown", "may_have_dispatched", "timeout", "deadline")
    );
}

/// After the transport fails, the next call connects the service again -- a second connect --
/// lists its tools, announcing the change, and is dispatched once; the call the failure ended is
/// uncertain and never sent again.
#[tokio::test]
async fn the_next_call_reconnects() {
    let first = toolbox(&["write", "echo"]);
    first.act("write", Act::HangUp);
    let second = toolbox(&["write", "echo", "extra"]);
    let (session, mut events, fakes) = fs_session(connects(&[&first, &second])).await;
    assert_eq!(
        output(
            &session,
            "mcp(fs.write, text='x')\nmcp(fs.echo, text='back')\nprint(fs.listing, 'extra' in fs.tools)"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\nok back\n2 True\n"
    );
    assert_eq!(fakes.of("mcp-fs").connects(), 2);
    assert_eq!(first.calls(), ["write"]);
    assert_eq!(second.calls(), ["echo"]);
    let uncertain = session.mcp().uncertain();
    assert_eq!(uncertain.len(), 1, "{uncertain:?}");
    assert_eq!(uncertain[0].tool, "write");
    stop(session).await;
    let events = taken(&mut events);
    let reconnected = &of_kind(&events, "mcp.service.reconnected")[0];
    assert_eq!(
        (&reconnected["attempts"], &reconnected["listing"]),
        (&json!(1), &json!(2))
    );
    assert!(
        position(&events, "mcp.service.disconnected", "fs")
            < position(&events, "mcp.service.reconnected", "fs")
    );
    assert!(
        position(&events, "mcp.service.reconnected", "fs")
            < position(&events, "service.described", "fs")
    );
    let described = &of_kind(&events, "service.described")[0];
    assert_eq!(described["changes"]["added"], json!(["extra"]));
    let echoes: Vec<_> = of_kind(&events, "service.call.dispatched")
        .into_iter()
        .filter(|dispatched| dispatched["method"] == "echo")
        .collect();
    assert_eq!(echoes.len(), 1, "{echoes:?}");
}

/// `reconnect()` connects a disconnected service without a call and says what its relist found;
/// on a connected one it does nothing and returns `None`. One that fails refuses its call
/// `ServiceUnavailable`, never sent, and the next call tries again.
#[tokio::test]
async fn reconnect_is_asked_for_or_retried() {
    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let second = toolbox(&["write", "extra"]);
    let (session, _events, fakes) = fs_session(connects(&[&first, &second])).await;
    assert_eq!(
        output(
            &session,
            "mcp(fs.write, text='x')\nr = fs.reconnect()\nprint(r.added, r.listing)\n\
             print(fs.reconnect())"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n('extra',) 2\nNone\n"
    );
    assert_eq!(fakes.of("mcp-fs").connects(), 2);
    stop(session).await;

    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let third = toolbox(&["write"]);
    let attempts = vec![
        Behavior::Toolbox(Arc::clone(&first)),
        Behavior::Refuse,
        Behavior::Toolbox(Arc::clone(&third)),
    ];
    let (session, _events, fakes) = fs_session(attempts).await;
    assert_eq!(
        output(
            &session,
            "mcp(fs.write, text='x')\nmcp(fs.write, text='y')\nmcp(fs.write, text='z')"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n\
         ServiceUnavailable refused not_dispatched None unavailable\n\
         ok z\n"
    );
    assert_eq!(fakes.of("mcp-fs").connects(), 3);
    assert_eq!(third.calls(), ["write"]);
    stop(session).await;
}

/// The reconnect bound: with one reconnect allowed, a second disconnection leaves the service
/// unavailable, its calls refused and nothing connected. After admission closes, a disconnected
/// service is not connected again.
#[tokio::test]
async fn reconnects_are_bounded_and_end_at_the_close() {
    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let second = toolbox(&["write"]);
    second.act("write", Act::HangUp);
    let (session, _events, fakes) = session_of(
        builder(),
        connects(&[&first, &second]),
        calls(),
        starts().with_reconnect_max(1),
    )
    .await;
    assert_eq!(
        output(
            &session,
            "mcp(fs.write, text='x')\nmcp(fs.write, text='y')\nmcp(fs.write, text='z')"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n\
         MayHaveDispatched unknown may_have_dispatched transport None\n\
         ServiceUnavailable refused not_dispatched None unavailable\n"
    );
    assert_eq!(fakes.of("mcp-fs").connects(), 2);
    stop(session).await;

    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let (session, _events, fakes) = fs_session(connects(&[&first])).await;
    output(&session, "mcp(fs.write, text='x')").await;
    let go = Flag::new();
    let mut late = session
        .interpreter()
        .submit(&format!(
            "{}\ntry:\n    fs.reconnect()\nexcept outrig.McpError as e:\n    \
             print(type(e).__name__, e.reason)",
            go.awaited()
        ))
        .unwrap_or_else(|e| panic!("{e}"));
    session.close_admission();
    go.raise();
    match within(late.outcome()).await {
        Outcome::Ok(report) => assert_eq!(report.output, "AdmissionClosed admission_closed\n"),
        other => panic!("{other:?}"),
    }
    assert_eq!(fakes.of("mcp-fs").connects(), 1);
    stop(session).await;
}

/// A call that connects its service again is checked again against what the server lists then,
/// not what the table checked it against: arguments the new schema refuses are
/// `InvalidArguments`, naming the failing path, and a tool it no longer lists is `UnknownTool`,
/// each refused and never sent.
#[tokio::test]
async fn a_reconnected_call_is_checked_again() {
    let number = json!({"type": "object", "properties": {"text": {"type": "integer"}}});
    let first = toolbox(&["write", "echo"]);
    first.act("write", Act::HangUp);
    let second = Toolbox::new(json!([tool("write", text()), tool("echo", number)]));
    second.act("write", Act::HangUp);
    let third = toolbox(&["echo"]);
    let (session, _events, fakes) = fs_session(connects(&[&first, &second, &third])).await;
    assert_eq!(
        output(
            &session,
            "mcp(fs.write, text='x')\n\
             try:\n    fs.echo(text='a')\n\
             except outrig.InvalidArguments as e:\n    \
             print(type(e).__name__, e.outcome, e.state, e.reason, e.path)\n\
             mcp(fs.write, text='y')\n\
             mcp(fs.write, text='z')"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched transport None\n\
         InvalidArguments refused not_dispatched invalid_arguments /text\n\
         MayHaveDispatched unknown may_have_dispatched transport None\n\
         UnknownTool refused not_dispatched None unknown_method\n"
    );
    assert_eq!(fakes.of("mcp-fs").connects(), 3);
    assert_eq!(second.calls(), ["write"], "the echo was not sent");
    assert!(third.calls().is_empty(), "nor was the last write");
    stop(session).await;
}

/// A reconnect is bounded by the connect budget, readying what its server ran in included, even
/// asked for with no call's deadline: past it the service is `ServiceUnavailable`.
#[tokio::test]
async fn a_reconnect_ends_within_its_budget() {
    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let budget = Duration::from_secs(1);
    let (session, _events, fakes) = session_of(
        builder(),
        connects(&[&first]),
        calls(),
        policy(budget, budget),
    )
    .await;
    output(&session, "mcp(fs.write, text='x')").await;
    fakes.of("mcp-fs").slow_recover(Duration::from_secs(3600));
    let began = std::time::Instant::now();
    assert_eq!(
        output(
            &session,
            "try:\n    fs.reconnect()\nexcept outrig.McpError as e:\n    \
             print(type(e).__name__, e.reason, 'connect budget' in str(e))"
        )
        .await,
        "ServiceUnavailable unavailable True\n"
    );
    assert!(began.elapsed() < budget * 3, "{:?}", began.elapsed());
    assert_eq!(
        fakes.of("mcp-fs").connects(),
        1,
        "nothing connected past it"
    );
    stop(session).await;
}

/// A reconnect a call makes covers the listing changes its server announced before it, as a
/// relist does: the service is no longer stale.
#[tokio::test]
async fn a_call_s_reconnect_is_a_relist() {
    let first = toolbox(&["write", "echo"]);
    first.act("write", Act::HangUp);
    let second = toolbox(&["write", "echo"]);
    let (session, _events, _fakes) = fs_session(connects(&[&first, &second])).await;
    first.list_changed().await;
    assert_eq!(
        printed(&session, "print(fs.stale)", "True\n").await,
        "True\n"
    );
    assert_eq!(
        output(&session, "mcp(fs.write, text='x')\nmcp(fs.echo, text='a')").await,
        "MayHaveDispatched unknown may_have_dispatched transport None\nok a\n"
    );
    assert_eq!(
        printed(&session, "print(fs.stale)", "False\n").await,
        "False\n"
    );
    stop(session).await;
}

/// A reconnect starts the server with the values resolved at the start: the secrets are not asked
/// again, and the second connect is given what the first was.
#[tokio::test]
async fn values_are_not_resolved_again() {
    let asked = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&asked);
    let secrets = move |var: &str| {
        if var == VAR {
            *counter.lock().unwrap() += 1;
        }
        (var == VAR).then(|| "sk-test".to_string())
    };
    let first = toolbox(&["write"]);
    first.act("write", Act::HangUp);
    let second = toolbox(&["write"]);
    let fakes = Fakes::default();
    fakes
        .of("mcp-fs")
        .script("fs", connects(&[&first, &second]));
    let reference = format!("${{{VAR}}}");
    let builder = builder().secrets(secrets).mcp(
        "fs",
        McpServiceSpec::sidecar(sidecar(&[("TOKEN", reference.as_str())])),
    );
    let (session, _events) = placed_under(&fakes, builder, calls(), starts()).await;
    output(&session, MCP).await;
    // The model's key names the variable too.
    let at_start = *asked.lock().unwrap();
    assert_eq!(
        output(&session, "mcp(fs.write, text='x')\nmcp(fs.write, text='y')").await,
        "MayHaveDispatched unknown may_have_dispatched transport None\nok y\n"
    );
    assert_eq!(
        *asked.lock().unwrap(),
        at_start,
        "not asked by the reconnect"
    );
    let envs = fakes.of("mcp-fs").envs();
    assert_eq!(envs.len(), 2);
    assert!(
        envs.iter().all(|(_, env)| env["TOKEN"] == "sk-test"),
        "{envs:?}"
    );
    stop(session).await;
}

/// At the close a call in flight drains: one its server answers within the deadline is a
/// response; one it never answers is sent the notice at terminating and ends `unknown`, may have
/// dispatched, reason `closed`, in `services` and `mcp_uncertain`, the verdict stopped with
/// unknown.
#[tokio::test]
async fn the_close_drains_then_cancels() {
    let server = toolbox(&["late"]);
    server.act("late", Act::After(Duration::from_millis(300)));
    let (session, mut events, _fakes) = fs_session(connects(&[&server])).await;
    let _call = session
        .interpreter()
        .submit("fs.call('late', {})")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the call", || server.calls().len() == 1).await;
    let report = within(session.shutdown(Duration::from_secs(3))).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let events = taken(&mut events);
    assert_eq!(
        how(&ended(&events, "late")[0]),
        ("returned", "response_received", "null", "null")
    );

    let server = toolbox(&["stall"]);
    server.act("stall", Act::Stall);
    let (session, mut events, _fakes) = fs_session(connects(&[&server])).await;
    let _call = session
        .interpreter()
        .submit("fs.call('stall', {})")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the call", || server.calls().len() == 1).await;
    let report = within(session.shutdown(Duration::from_millis(300))).await;
    until("the cancel", || server.cancels() == ["stall"]).await;
    assert_eq!(report.services.len(), 1, "{report:?}");
    assert_eq!(
        (report.services[0].outcome, report.services[0].reason),
        (ServiceOutcome::Unknown, Some(ServiceReason::Closed))
    );
    assert_eq!(report.mcp_uncertain.len(), 1, "{report:?}");
    assert_eq!(
        report.mcp_uncertain[0].failure,
        Some(McpFailureKind::Canceled)
    );
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    let events = taken(&mut events);
    assert_eq!(
        how(&ended(&events, "stall")[0]),
        ("unknown", "may_have_dispatched", "canceled", "closed")
    );
}

/// A stop sends the call in flight the notice; it ends `unknown`, reason `stopped`, and is
/// uncertain.
#[tokio::test]
async fn a_stop_sends_the_notice() {
    let server = toolbox(&["stall"]);
    server.act("stall", Act::Stall);
    let (session, _events, _fakes) = fs_session(connects(&[&server])).await;
    let mut call = session
        .interpreter()
        .submit("mcp(fs.call, 'stall', {})")
        .unwrap_or_else(|e| panic!("{e}"));
    until("the call", || server.calls().len() == 1).await;
    session
        .mcp()
        .quiesce("fs", Quiesce::Stop, Duration::ZERO)
        .await;
    match within(call.outcome()).await {
        Outcome::Ok(report) => assert_eq!(
            report.output,
            "MayHaveDispatched unknown may_have_dispatched canceled stopped\n"
        ),
        other => panic!("{other:?}"),
    }
    until("the cancel", || server.cancels() == ["stall"]).await;
    let uncertain = session.mcp().uncertain();
    assert_eq!(
        (uncertain[0].tool.as_str(), uncertain[0].reason),
        ("stall", Some(ServiceReason::Stopped))
    );
    stop(session).await;
}

/// A service that says its call may have dispatched, of a kind this build does not classify,
/// reaches agent code as `MayHaveDispatched` with that kind; a Rust service that says it does not
/// know whether its work was done ends `unknown`, never `raised`.
struct Uncertain {
    kind: ServiceKind,
}

impl Service for Uncertain {
    fn describe(&self) -> ServiceDescription {
        ServiceDescription::new("Uncertain.")
            .of_kind(self.kind)
            .with_method(MethodDescription::new("go", "", json!({"type": "object"})))
    }

    fn call(&self, _method: &str, _args: Value, _cx: CallContext) -> ServiceFuture<'_> {
        let mcp = self.kind == ServiceKind::Mcp;
        Box::pin(async move {
            let error = ServiceError::new(ServiceErrorCategory::Unknown, "it fell behind");
            Err(match mcp {
                true => error.of_mcp(McpEnd::Sent {
                    state: McpDispatchState::MayHaveDispatched,
                    failure: Some(McpFailureKind::Other),
                }),
                false => error,
            })
        })
    }
}

#[tokio::test]
async fn an_unclassified_failure_keeps_its_state() {
    let mut builder = builder()
        .serve(
            "mcpish",
            Arc::new(Uncertain {
                kind: ServiceKind::Mcp,
            }) as Arc<dyn Service>,
        )
        .serve(
            "rusty",
            Arc::new(Uncertain {
                kind: ServiceKind::Rust,
            }) as Arc<dyn Service>,
        );
    let mut events = builder.subscribe();
    let session = start(builder).await;
    output(&session, MCP).await;
    assert_eq!(
        output(
            &session,
            "mcp(mcpish.go)\ntry:\n    rusty.go()\nexcept outrig.ServiceError as e:\n    \
             print(type(e).__name__, e.outcome, e.category, e.reason)"
        )
        .await,
        "MayHaveDispatched unknown may_have_dispatched other None\n\
         ServiceError unknown None None\n"
    );
    let uncertain = session.mcp().uncertain();
    assert_eq!(uncertain.len(), 1, "an MCP call only: {uncertain:?}");
    stop(session).await;
    let events = taken(&mut events);
    let outcomes: Vec<_> = of_kind(&events, "service.call.ended")
        .iter()
        .map(|ended| (ended["service"].clone(), ended["outcome"].clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (json!("mcpish"), json!("unknown")),
            (json!("rusty"), json!("unknown"))
        ]
    );
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::path::Path;

    use super::*;
    use crate::SidecarSpec;
    use crate::config::{ImageConfig, SidecarWorkspaceAccess, Workspace};
    use crate::harness::DEFAULT_DRAIN;
    use crate::process::Cmd;

    /// Against podman: the `mcp-fs` fixture's filesystem server in a sidecar of its own, its
    /// container killed with `podman kill`. The next call may have dispatched -- the fixture has
    /// no slow tool to kill it during, so the kill lands first, and a failed send proves no more
    /// than a failed wait -- and the call after it connects through a new container, listing the
    /// same tools.
    #[tokio::test]
    async fn a_killed_sidecar_is_connected_again() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-fs");
        let build = ImageConfig::from_dockerfile("Dockerfile", ".");
        let tag = crate::image::ensure_image(&build, &fixture, false)
            .await
            .unwrap_or_else(|e| panic!("ensure the mcp-fs fixture: {e}"))
            .tag;
        let workspace = tempfile::tempdir().expect("a workspace");
        std::fs::write(workspace.path().join("hello.txt"), "hi").expect("write");
        let logs = tempfile::tempdir().expect("a log dir");
        let mut config = super::super::services_tests::config();
        config.workspace = Workspace::new(workspace.path(), "/workspace");
        config.images.insert("fs".to_string(), build);
        let spec =
            super::super::container_spec(&config, "fs", Some(&tag), &fixture, logs.path().into())
                .await
                .expect("the session's container spec");
        let serve = vec![
            "mcp-server-filesystem".to_string(),
            "/workspace".to_string(),
        ];
        let mut builder = SessionBuilder::new(config, Some("coding"), None)
            .secrets(|var: &str| (var == VAR).then(|| "sk-test".to_string()))
            .container(spec.spec)
            .mcp(
                "fs",
                McpServiceSpec::sidecar(
                    SidecarSpec::from_image("fs", tag.as_str())
                        .with_workspace_access(SidecarWorkspaceAccess::Ro)
                        .with_server("fs", serve),
                ),
            );
        let mut events = builder.subscribe();
        let session = tokio::time::timeout(Duration::from_secs(300), builder.start())
            .await
            .expect("the session started in time")
            .unwrap_or_else(|e| panic!("the session did not start: {e}"));
        output(&session, MCP).await;
        let before = output(&session, "print(sorted(fs.tools))").await;
        let sidecar = format!("{}-mcp-fs", session.container_name());
        let killed = crate::process::try_capture(Cmd::new("podman").args(["kill", &sidecar]))
            .await
            .expect("podman runs");
        assert!(killed.status.success(), "podman kill {sidecar}: {killed:?}");
        assert_eq!(
            output(&session, "mcp(fs.call, 'list_allowed_directories', {})").await,
            "MayHaveDispatched unknown may_have_dispatched transport None\n"
        );
        assert_eq!(
            output(
                &session,
                "r = fs.call('read_text_file', {'path': '/workspace/hello.txt'})\nprint(r.text)"
            )
            .await,
            "hi\n"
        );
        assert_eq!(output(&session, "print(sorted(fs.tools))").await, before);
        let report =
            tokio::time::timeout(Duration::from_secs(120), session.shutdown(DEFAULT_DRAIN))
                .await
                .expect("the session stopped in time");
        assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
        assert_eq!(report.mcp_uncertain.len(), 1, "{report:?}");
        assert!(report.mcp_services[0].proven, "{report:?}");
        let events = taken(&mut events);
        assert_eq!(of_kind(&events, "mcp.service.reconnected").len(), 1);
    }
}
