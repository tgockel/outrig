//! What a root is doing, and why it is at rest, as its owner reads it: each case runs a root
//! its owner constructed against the routed mock provider, and reads it through its handle. Each
//! runs three times -- with every event gathered, with no subscription, and with a subscription
//! of one event that is never read -- and reads the same each time: the reads come from the
//! session's registry of its agents, never from the stream. Where the events are gathered, the
//! round and the request each state names are checked against them.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use super::children_tests::{Tree, Watch, data, kind};
use super::instance_tests::ready_then;
use super::mock_http::{Responder, Style, failure, scripted, submit, text_reply};
use super::root_tests::{REVIEWS, agent_of, not_ready, owned_watched, reviewer};
use super::runner;
use crate::harness::event::{AttemptId, Event, RequestEnd};
use crate::harness::{AgentState, RequestHandle, RootError, RootHandle, Status};
use crate::python::testing::{py, within};

/// A session with no primary agent, watched as `watch`, holding a `Reviewer` root whose rounds
/// past its orienting round `rounds` answers, once the root is ready.
async fn ready_root(
    var: &str,
    watch: Watch,
    rounds: Responder,
) -> (Tree, tempfile::TempDir, RootHandle) {
    let (tree, app) = owned_watched(
        var,
        "",
        vec![("ROLE reviewer", ready_then(rounds))],
        watch,
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    (tree, app, root)
}

/// The root's status once `wanted` holds of its state, read change by change from `seq`.
async fn reached(root: &RootHandle, seq: u64, wanted: impl Fn(&AgentState) -> bool) -> Status {
    within(async {
        let mut seq = seq;
        loop {
            let status = root.changed(seq).await;
            if wanted(&status.state) {
                return status;
            }
            seq = status.seq;
        }
    })
    .await
}

/// Whether `request` has not ended.
async fn open(request: &RequestHandle) -> bool {
    tokio::time::timeout(Duration::ZERO, request.outcome())
        .await
        .is_err()
}

/// The reply `request` was answered with, as JSON.
async fn answered(request: &RequestHandle) -> Value {
    let reply = within(request.outcome()).await.expect("answered");
    serde_json::from_str(&reply).expect("a JSON reply")
}

/// The data of `subject`'s events of `what`.
fn of(events: &[Arc<Event>], what: &str, subject: &str) -> Vec<Value> {
    kind(events, what, Some(subject))
        .into_iter()
        .map(data)
        .collect()
}

/// The `model.attempt` of `attempt`, once checked to be the last request a failed round `round`
/// of `subject`'s sent, as its `model.round.failed` says.
fn agrees(events: &[Arc<Event>], subject: &str, round: u32, attempt: Option<AttemptId>) -> Value {
    let attempt = json!(attempt.expect("a request decided it").get());
    let failed = of(events, "model.round.failed", subject);
    assert!(
        failed.iter().any(|ended| ended["round"] == round
            && ended["attempts"].as_array().and_then(|sent| sent.last()) == Some(&attempt)),
        "{attempt} as the last request of round {round}: {failed:?}"
    );
    of(events, "model.attempt", subject)
        .into_iter()
        .find(|sent| sent["attempt_id"] == attempt)
        .expect("the request's model.attempt")
}

/// The source of a round that takes one request on `review`, and keeps it as `d`.
const TAKES: &str = "d = await runtime.channels['review'].receive()";

/// The source of a round that answers the request it took as `d`, then one more.
const ANSWERS_BOTH: &str = "await d.reply(Verdict(path=d.body, ok=True))\n\
                            d = await runtime.channels['review'].receive()\n\
                            await d.reply(Verdict(path=d.body, ok=True))";

/// A round whose code took its request, and whose next call the provider refused for what it
/// carried -- and refused again when sent once more without the results -- leaves the root
/// `refused`, naming the round and the request refused last. It is not opened again: taking the
/// request was the refused round's own doing and no change, so the request stays open and the
/// provider is sent nothing more until a second request arrives; the round that opens then
/// answers both, and the root is idle.
#[tokio::test]
async fn a_refused_round_rests_until_what_the_root_holds_changes() {
    for watch in Watch::ALL {
        refused(watch).await;
    }
}

async fn refused(watch: Watch) {
    let (mut tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_REFUSED_KEY",
        watch,
        scripted(vec![
            submit("c1", &py(TAKES)),
            Style::Messages.empty("refusal"),
            Style::Messages.empty("refusal"),
            submit("c2", &py(ANSWERS_BOTH)),
            text_reply("reviewed both"),
        ]),
    )
    .await;
    let seq = root.status().seq;
    let first = root.request("review", json!("a.py")).expect("sent");
    let stop = within(root.next_stop(seq)).await;
    let AgentState::Refused { round, attempt } = stop.state.clone() else {
        panic!("{watch:?}: {stop:?}");
    };
    assert_eq!(round, 2, "the round after the orienting round");
    // Resting: nothing changes, and nothing more is sent.
    let sent = super::mock_http::drain(&mut tree.requests).len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(root.status(), stop, "{watch:?}");
    assert!(
        open(&first).await,
        "{watch:?}: the refused round's request stays open"
    );
    assert_eq!(
        super::mock_http::drain(&mut tree.requests).len(),
        0,
        "{watch:?}: nothing was sent after the {sent} requests the refusal took"
    );
    let second = root.request("review", json!("b.py")).expect("sent");
    let rested = within(root.next_stop(stop.seq)).await;
    assert!(
        matches!(rested.state, AgentState::Idle { round: later } if later > round),
        "{watch:?}: {rested:?}"
    );
    assert_eq!(answered(&first).await, json!({"path": "a.py", "ok": true}));
    assert_eq!(answered(&second).await, json!({"path": "b.py", "ok": true}));
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    if watch == Watch::Gathered {
        let refused = agrees(&events, &subject, round, attempt);
        assert_eq!(refused["finish_reason"], "refusal", "{refused}");
    }
}

/// A model call that fails in a way that may clear leaves the root `unavailable` while its
/// runner waits to try again: the class names the provider's failure, `since` is the first of
/// the run's failures and stays so, and `next_attempt` is when the runner opens the round again.
/// The request the round opened on stays open, and once the provider recovers the reopened round
/// answers it and the root is idle.
#[tokio::test]
async fn a_failure_that_may_clear_reads_unavailable_until_it_does() {
    for watch in Watch::ALL {
        unavailable(watch).await;
    }
}

async fn unavailable(watch: Watch) {
    // Long enough to read each failure before the runner tries again.
    const WAIT: Duration = Duration::from_millis(400);
    runner::FIRST_WAIT.set(Some(WAIT));
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_UNAVAILABLE_KEY",
        watch,
        scripted(vec![
            failure(503),
            failure(503),
            submit("c1", &py(REVIEWS)),
            text_reply("reviewed"),
        ]),
    )
    .await;
    runner::FIRST_WAIT.set(None);
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let first = within(root.next_stop(seq)).await;
    let AgentState::Unavailable {
        class,
        since,
        next_attempt,
        round,
        attempt,
    } = first.state.clone()
    else {
        panic!("{watch:?}: {first:?}");
    };
    assert_eq!(class, "HTTP 503 Service Unavailable");
    assert_eq!(next_attempt.duration_since(since).ok(), Some(WAIT));
    assert!(open(&request).await, "{watch:?}: the request stays open");
    let second = within(root.next_stop(first.seq)).await;
    let AgentState::Unavailable {
        since: still,
        next_attempt: later,
        round: again,
        attempt: other,
        ..
    } = second.state.clone()
    else {
        panic!("{watch:?}: {second:?}");
    };
    assert_eq!(
        (still, again),
        (since, round),
        "the same run, the same round"
    );
    let after = later.duration_since(since).expect("later");
    assert!(after >= WAIT * 3 && after <= WAIT * 16, "{after:?}");
    assert!(open(&request).await, "{watch:?}: the request stays open");
    let recovered = within(root.next_stop(second.seq)).await;
    assert_eq!(recovered.state, AgentState::Idle { round }, "{watch:?}");
    assert_eq!(
        answered(&request).await,
        json!({"path": "a.py", "ok": true})
    );
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    if watch == Watch::Gathered {
        for attempt in [attempt, other] {
            assert_eq!(agrees(&events, &subject, round, attempt)["error"], class);
        }
    }
}

/// A round that ends holding nothing leaves the root idle on it, with its facts: the closing
/// text, nothing cut short, and no send. The orienting round is the first so; the history keeps
/// both, the oldest first.
#[tokio::test]
async fn a_round_that_ends_holding_nothing_reads_idle_with_its_facts() {
    for watch in Watch::ALL {
        idle(watch).await;
    }
}

async fn idle(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_IDLE_KEY",
        watch,
        scripted(vec![
            submit("c1", &py(REVIEWS)),
            text_reply("reviewed a.py"),
        ]),
    )
    .await;
    let oriented = within(root.next_stop(0)).await;
    assert_eq!(oriented.state, AgentState::Idle { round: 1 }, "{watch:?}");
    let facts = oriented.last.clone().expect("the orienting round's facts");
    assert_eq!((facts.round, facts.reply.as_str()), (1, "ready"));
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let stop = within(root.next_stop(seq)).await;
    assert_eq!(stop.state, AgentState::Idle { round: 2 }, "{watch:?}");
    answered(&request).await;
    let facts = stop.last.clone().expect("the round's facts");
    assert_eq!(facts.round, 2);
    assert_eq!(facts.reply, "reviewed a.py");
    assert_eq!((&facts.stopped, &facts.reasoning), (&None, &None));
    assert!(facts.sends.is_empty() && !facts.any_send());
    let rounds = root.rounds();
    assert_eq!(
        rounds.iter().map(|facts| facts.round).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(rounds.last(), stop.last.as_ref());
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    if watch == Watch::Gathered {
        let completed = of(&events, "model.round.completed", &subject);
        assert_eq!(completed.last().expect("a round")["round"], 2);
    }
}

/// A request the root takes and never answers is cancelled by the host once the warning round
/// has left it so, and the root, holding nothing, is idle.
#[tokio::test]
async fn a_root_whose_request_the_host_cancelled_reads_idle() {
    for watch in Watch::ALL {
        cancelled_by_the_host(watch).await;
    }
}

async fn cancelled_by_the_host(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_HOST_CANCEL_KEY",
        watch,
        scripted(vec![
            submit("c1", &py("d = await runtime.channels['review'].receive()")),
            text_reply("taken"),
        ]),
    )
    .await;
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let stop = within(root.next_stop(seq)).await;
    let AgentState::Idle { round } = stop.state else {
        panic!("{watch:?}: {stop:?}");
    };
    let Err(RootError::Cancelled(why)) = within(request.outcome()).await else {
        panic!("{watch:?}: the request was not cancelled");
    };
    assert!(why.contains("without answering it"), "{why}");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    if watch == Watch::Gathered {
        let completed = of(&events, "model.round.completed", &subject);
        assert_eq!(completed.last().expect("a round")["round"], round);
    }
}

/// A failure that does not clear leaves the root `failed`, with the failure's class and no next
/// attempt: the request it held ends failed, and so does one made later. Nothing changes the
/// state again but a release.
#[tokio::test]
async fn a_failure_that_does_not_clear_reads_failed_until_a_release() {
    for watch in Watch::ALL {
        failed(watch).await;
    }
}

async fn failed(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_FAILED_KEY",
        watch,
        scripted(vec![failure(401)]),
    )
    .await;
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let stop = within(root.next_stop(seq)).await;
    let AgentState::Failed {
        class,
        round,
        attempt,
    } = stop.state.clone()
    else {
        panic!("{watch:?}: {stop:?}");
    };
    assert_eq!(class, "HTTP 401 Unauthorized");
    assert_eq!(round, Some(2));
    assert!(matches!(
        within(request.outcome()).await,
        Err(RootError::Failed(_))
    ));
    let later = root.request("review", json!("b.py")).expect("sent");
    assert!(matches!(
        within(later.outcome()).await,
        Err(RootError::Failed(_))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), root.changed(stop.seq))
            .await
            .is_err(),
        "{watch:?}: nothing changes a failed root but a release"
    );
    within(root.release()).await;
    let released = within(root.changed(stop.seq)).await;
    assert_eq!(released.state, AgentState::Released);
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    if watch == Watch::Gathered {
        assert_eq!(agrees(&events, &subject, 2, attempt)["error"], class);
    }
}

/// A release reads `released`; one before the root's first round opened, `cancelled`; and an
/// interpreter that dies under the root, `terminated`, with the request it held listed so by
/// the shutdown report.
#[tokio::test]
async fn a_root_ended_reads_how_it_ended() {
    for watch in Watch::ALL {
        released(watch).await;
        terminated(watch).await;
    }
}

async fn released(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_RELEASED_KEY",
        watch,
        scripted(vec![text_reply("nothing")]),
    )
    .await;
    within(root.release()).await;
    assert_eq!(root.status().state, AgentState::Released, "{watch:?}");
    // A root whose runner is held before its first round is released while launching.
    let hold = Arc::new(Notify::new());
    runner::HOLD.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&hold)));
    let held = within(tree.session.construct(reviewer())).await;
    runner::HOLD.with(|slot| slot.borrow_mut().take());
    let held = held.expect("constructed");
    let launching = held.status();
    assert_eq!(launching.state, AgentState::Launching, "{watch:?}");
    let releasing = tokio::spawn({
        let held = held.clone();
        async move { held.release().await }
    });
    let cancelled = within(held.next_stop(launching.seq)).await;
    assert_eq!(cancelled.state, AgentState::Cancelled, "{watch:?}");
    hold.notify_one();
    within(releasing).await.expect("released");
    assert!(matches!(not_ready(&held).await, RootError::Released(_)));
    tree.end().await;
}

async fn terminated(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_TERMINATED_KEY",
        watch,
        scripted(vec![submit("c1", &py("import os\nos._exit(9)"))]),
    )
    .await;
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let stop = reached(&root, seq, |state| *state == AgentState::Terminated).await;
    assert_eq!(root.status(), stop);
    assert!(matches!(
        within(request.outcome()).await,
        Err(RootError::Terminated(_))
    ));
    let id = request.id().expect("recorded");
    let (report, _, _) = tree.end().await;
    let ended = report
        .requests
        .iter()
        .find(|ended| ended.request == id)
        .unwrap_or_else(|| panic!("{watch:?}: {:?}", report.requests));
    assert_eq!(ended.outcome, RequestEnd::Terminated);
}

/// The read answers at once while the root's kernel spins in `while True: pass`: it is the
/// registry's, and never waits on the interpreter.
#[tokio::test]
async fn the_read_answers_while_the_roots_kernel_spins() {
    for watch in Watch::ALL {
        spinning(watch).await;
    }
}

async fn spinning(watch: Watch) {
    let (tree, _app, root) = ready_root(
        "OUTRIG_TEST_STOP_SPINNING_KEY",
        watch,
        scripted(vec![submit("c1", &py("while True:\n    pass"))]),
    )
    .await;
    let seq = root.status().seq;
    let _request = root.request("review", json!("a.py")).expect("sent");
    let running = reached(&root, seq, |state| {
        matches!(state, AgentState::Running { .. })
    })
    .await;
    // The model's call is answered at once, so its code is spinning by now.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(root.status(), running, "{watch:?}");
    within(root.release()).await;
    assert_eq!(root.status().state, AgentState::Released);
    tree.end().await;
}
