//! Requests across external channels end to end (`0003-52`): an inbound request the owner pushes
//! is a request of its holder's, answered with `reply` or `fail` and held to the rule every
//! request is held to, and its every end is told the backend through `settled`; an outbound
//! request the holder makes goes to the backend as a send and settles as the owner says, or as
//! the backend answers its withdrawal. Each agent's model is the routed mock; the backend is
//! `external_tests`'s, which records what it is given and answers as the test says.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::children_tests::{
    Tree, Watch, after_result, data_of, exec_outputs, kind, serving, tree_watched,
};
use super::external_tests::{
    Answer, Backend, constructed, from_launch, holder, holding, inbox, openings, ready, running,
    until,
};
use super::mock_http;
use super::mock_http::{RecordedRequest, Responder, submit, text_reply};
use super::root_tests::agent_of;
use crate::harness::event::{AgentRequestId, RequestEnd};
use crate::harness::{
    AgentState, CancelledBy, ChannelBackend, ChannelDefinition, ChannelKind, ChannelOperation,
    Contract, DeliverError, Delivered, DeliveryId, Direction, Inbound, OutboundState, RosterState,
    Settle, SettleError, Settled, Settlement, Verdict, WithdrawAnswer, WithdrawEnd,
};
use crate::python::testing::{Flag, py, within};

/// `questions`: an inbound request channel, text both ways.
pub(super) fn questions() -> ChannelDefinition {
    ChannelDefinition::new("questions")
        .kind(ChannelKind::Request(Direction::Inbound))
        .description("Questions the task's user asks.")
}

/// `asks`: an outbound request channel, text both ways.
pub(super) fn asks() -> ChannelDefinition {
    ChannelDefinition::new("asks")
        .kind(ChannelKind::Request(Direction::Outbound))
        .description("Permission you ask for.")
}

/// Push request `item` of `body` from `sender` on `channel`, and the request's id.
pub(super) async fn ask(
    tree_channel: &crate::harness::Channel,
    item: &str,
    sender: &str,
    body: Value,
) -> u64 {
    let delivered = within(tree_channel.deliver(Inbound::request(item, sender, body)))
        .await
        .expect("queued");
    let Delivered::Queued {
        delivery: DeliveryId::Request(request),
    } = delivered
    else {
        panic!("{delivered:?}");
    };
    request.get()
}

/// Each outbound request outstanding on `tree`'s channel `name`, with its state, as the
/// session's registry holds it.
pub(super) fn outstanding(tree: &Tree, name: &str) -> Vec<(AgentRequestId, OutboundState)> {
    tree.session
        .channels()
        .into_iter()
        .filter(|channel| channel.name == name)
        .flat_map(|channel| channel.outstanding)
        .map(|request| (request.request, request.state))
        .collect()
}

/// The settlement each notice the backend was given says, in order.
fn outcomes(settlements: &[Settled]) -> Vec<Settlement> {
    settlements
        .iter()
        .map(|notice| notice.outcome.clone())
        .collect()
}

/// A responder for an agent's rounds, its orienting round among them: the first round runs
/// `source`, and every later round's model call is never answered, so no round after it does
/// anything, and nothing the first left open is warned about or cancelled before the close.
pub(super) fn once_then_silent(source: &str) -> Responder {
    let serving = serving(source);
    let rounds = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &RecordedRequest| {
        if !after_result(request) && rounds.fetch_add(1, Ordering::SeqCst) > 0 {
            return Box::pin(std::future::pending());
        }
        serving(request)
    })
}

/// The owner pushes a request; the holder's code receives it with its body and sender and
/// replies; the backend is told once that it was replied, with the reply as JSON, under the
/// owner's id and the request's; and every step is evented as a request of the holder's.
#[tokio::test]
async fn an_inbound_request_is_replied_to() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_REPLIED_KEY",
        vec![(
            "ROLE inbox",
            running(&["d = await runtime.channels['questions'].receive()\n\
                 print(d.body, d.sender, d.id)\nawait d.reply('4')"]),
        )],
        |builder| builder.channel(questions(), given),
    )
    .await;
    let root = ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    let request = ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("the settlement", || !backend.settlements().is_empty()).await;
    tree.seen("channel.settled", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!("2+2? bob {request}\n")]
    );
    let settlements = backend.settlements();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    let notice = &settlements[0];
    assert_eq!(notice.channel, "questions");
    assert_eq!(notice.request.get(), request);
    assert_eq!(notice.item.as_deref(), Some("q-1"));
    assert_eq!(notice.outcome, Settlement::Replied(json!("4")));
    let sent = data_of(&events, "agent.request.sent", &subject);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["request"], request);
    assert_eq!(sent[0]["sender"], "bob");
    assert_eq!(sent[0]["channel"], "questions");
    assert_eq!(
        data_of(&events, "agent.request.received", &subject).len(),
        1
    );
    assert_eq!(data_of(&events, "agent.request.replied", &subject).len(), 1);
    let settled = data_of(&events, "agent.request.ended", &subject);
    assert_eq!(settled[0]["outcome"], "accepted");
    let acknowledged = data_of(&events, "channel.settled", &subject);
    assert_eq!(acknowledged[0]["request"], request);
    assert_eq!(acknowledged[0]["item"], "q-1");
    // A request is no message: the backend hears no consumed notice of it.
    assert!(backend.notices().is_empty());
}

/// `fail` ends a request, which the backend hears as failed with the message; a reply after it
/// raises `RequestEnded` in the holder's code, is evented, and is told the backend as a
/// refusal that changes nothing.
#[tokio::test]
async fn fail_ends_a_request_and_a_late_reply_is_refused() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nd = await runtime.channels['questions'].receive()\n\
                await d.fail('no')\ntry:\n    await d.reply('4')\n\
                except outrig.RequestEnded as e:\n    print('settled', e.ended)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_FAILED_KEY",
        vec![("ROLE inbox", running(&[code]))],
        |builder| builder.channel(questions(), given),
    )
    .await;
    let root = ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    let request = ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("both notices", || backend.settlements().len() == 2).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject), ["settled failed\n"]);
    assert_eq!(
        outcomes(&backend.settlements()),
        [
            Settlement::Failed("no".to_string()),
            Settlement::Refused {
                ended: RequestEnd::Failed
            },
        ]
    );
    let refused = data_of(&events, "agent.request.refused", &subject);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["request"], request);
    assert_eq!(refused[0]["ended"], "failed");
    // The backend's acknowledgments say which notice each was: the end, then the refusal.
    let acknowledged: Vec<Value> = data_of(&events, "channel.settled", &subject)
        .into_iter()
        .map(|settled| settled["refused"].clone())
        .collect();
    assert_eq!(acknowledged, [json!(false), json!(true)]);
}

/// On a typed reply contract, a reply that does not fit raises its problems with their paths in
/// the holder's code and settles nothing; the third ends the request rejected, which the backend
/// hears as failed, naming the problems.
#[tokio::test]
async fn a_reply_that_does_not_fit_raises_and_the_third_fails_the_request() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "d = await runtime.channels['questions'].receive()\n\
                for attempt in range(3):\n\
                \x20   try:\n\
                \x20       await d.reply({'text': 'x'})\n\
                \x20   except Exception as e:\n\
                \x20       print(type(e).__name__, '$.level' in str(e))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_TYPED_REPLY_KEY",
        vec![("ROLE inbox", running(&[code]))],
        |builder| {
            builder.channel(
                questions().sends(Contract::Typed("app.messages:Note".to_string())),
                given,
            )
        },
    )
    .await;
    let root = ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("the notice", || !backend.settlements().is_empty()).await;
    let subject = format!("agent/{}", agent_of(&root));
    tree.seen("exec.completed", 1).await;
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject),
        ["DecodeError True\nDecodeError True\nCompletionRejected True\n"]
    );
    let settlements = backend.settlements();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    let Settlement::Failed(why) = &settlements[0].outcome else {
        panic!("{settlements:?}");
    };
    assert!(why.contains("$.level"), "{why}");
}

/// The owner's cancel of a request its holder has not received takes it out of the window: the
/// holder never sees it, and the backend hears it cancelled by the owner. One the holder has
/// received stays with it, and its reply raises `RequestCancelled`.
#[tokio::test]
async fn the_owner_cancels_an_inbound_request() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let unread = Flag::new();
    let taken = Flag::new();
    // The orienting round holds the first request unread until the test has cancelled it; the
    // round the second opens takes it, and answers once the test has cancelled that one too.
    let first = format!(
        "{}\nprint('pending', runtime.channels['questions'].pending())",
        unread.awaited()
    );
    let second = format!(
        "import outrig\nd = await runtime.channels['questions'].receive()\n{}\n\
         try:\n    await d.reply('4')\nexcept outrig.RequestCancelled as e:\n    print('cancelled')",
        taken.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_OWNER_CANCEL_KEY",
        vec![("ROLE inbox", from_launch(&[&first, &second]))],
        |builder| builder.channel(questions(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    tree.seen("channel.attached", 1).await;
    let one = ask(&channel, "q-1", "bob", json!("SECRET ONE")).await;
    channel
        .cancel(crate::harness::event::AgentRequestId::new(one))
        .expect("cancelled");
    until("the first notice", || backend.settlements().len() == 1).await;
    unread.raise();
    within(root.ready()).await.expect("ready");
    let two = ask(&channel, "q-2", "bob", json!("two")).await;
    tree.seen("agent.request.received", 1).await;
    channel
        .cancel(crate::harness::event::AgentRequestId::new(two))
        .expect("cancelled");
    until("the second notice", || backend.settlements().len() == 2).await;
    taken.raise();
    let subject = format!("agent/{}", agent_of(&root));
    tree.seen("exec.completed", 2).await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject),
        ["pending 0\n", "cancelled\n"]
    );
    let owner = Settlement::Cancelled {
        by: CancelledBy::Owner,
        reason: String::new(),
    };
    // The reply after the second's cancel was refused, which its backend hears too.
    until("the refusal", || backend.settlements().len() == 3).await;
    assert_eq!(
        outcomes(&backend.settlements()),
        [
            owner.clone(),
            owner,
            Settlement::Refused {
                ended: RequestEnd::Cancelled
            }
        ]
    );
    let cancelled = data_of(&events, "agent.request.cancelled", &subject);
    assert_eq!(cancelled.len(), 2);
    assert_eq!(cancelled[0]["request"], one);
    assert_eq!(cancelled[0]["received"], false);
    assert_eq!(cancelled[0]["reason"], Value::Null);
    assert_eq!(cancelled[0]["by"], "owner");
    assert_eq!(cancelled[1]["received"], true);
    assert_eq!(cancelled[1]["by"], "owner");
    // The holder never saw the first: no request its model was sent carries it.
    assert!(
        requests
            .iter()
            .all(|request| !request.body.to_string().contains("SECRET ONE"))
    );
}

/// The rule every request is held to reaches an inbound one on the application's primary: taken
/// and left unanswered, the request opens its round, the round naming it by id and channel, and
/// the warning, and nothing after; then it is cancelled by the host, which the backend hears once,
/// and the primary reads idle.
#[tokio::test]
async fn a_request_taken_and_left_is_warned_about_then_cancelled() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_LIVENESS_KEY",
        vec![(
            "ROLE inbox",
            running(&[
                "d = await runtime.channels['questions'].receive()\nprint('took', d.id)",
                "print('thinking')",
            ]),
        )],
        |builder| builder.channel(questions(), given),
    )
    .await;
    let root = ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    let request = ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("the notice", || !backend.settlements().is_empty()).await;
    tree.seen("channel.settled", 1).await;
    let seq = root.status().seq;
    let rested = within(root.next_stop(seq.saturating_sub(1))).await;
    assert!(
        matches!(rested.state, AgentState::Idle { .. }),
        "{rested:?}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    let opened = openings(&requests, "ROLE inbox");
    assert_eq!(opened.len(), 3, "{opened:#?}");
    assert!(
        opened[0].contains("1 request is waiting on runtime.channels[\"questions\"]"),
        "{}",
        opened[0]
    );
    assert!(
        opened[1].contains(&format!(
            "you took request {request} from runtime.channels[\"questions\"]"
        )),
        "{}",
        opened[1]
    );
    assert!(
        opened[2].contains("your last round ended with requests unanswered"),
        "{}",
        opened[2]
    );
    let settlements = backend.settlements();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    let Settlement::Cancelled {
        by: CancelledBy::Host,
        reason,
    } = &settlements[0].outcome
    else {
        panic!("{settlements:?}");
    };
    assert!(reason.contains("without answering it"), "{reason}");
    let cancelled = data_of(&events, "agent.request.cancelled", &subject);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0]["by"], "host");
    assert!(
        cancelled[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("without answering it")),
        "{cancelled:?}"
    );
}

/// A request never taken earns the round its arrival opened and the warning, and is then
/// cancelled, the backend told once.
#[tokio::test]
async fn a_request_never_taken_is_cancelled_after_two_rounds() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_NEVER_TAKEN_KEY",
        vec![("ROLE inbox", running(&["print('ignoring')"]))],
        |builder| builder.channel(questions(), given),
    )
    .await;
    ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("the notice", || !backend.settlements().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(openings(&requests, "ROLE inbox").len(), 2);
    let settlements = backend.settlements();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    assert!(
        matches!(
            &settlements[0].outcome,
            Settlement::Cancelled { by: CancelledBy::Host, reason } if reason.contains("without reading it")
        ),
        "{settlements:?}"
    );
}

/// A reply in the warning round answers the request: the backend hears it replied.
#[tokio::test]
async fn a_reply_in_the_warning_round_is_answered() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_WARNED_REPLY_KEY",
        vec![(
            "ROLE inbox",
            running(&[
                "d = await runtime.channels['questions'].receive()",
                "print('thinking')",
                "await d.reply('4')",
            ]),
        )],
        |builder| builder.channel(questions(), given),
    )
    .await;
    ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    ask(&channel, "q-1", "bob", json!("2+2?")).await;
    until("the notice", || !backend.settlements().is_empty()).await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(openings(&requests, "ROLE inbox").len(), 3);
    assert_eq!(
        outcomes(&backend.settlements()),
        [Settlement::Replied(json!("4"))]
    );
}

/// A plain item is never warned about: the same primary, with three messages on `inbox` left
/// unread through the rounds their arrivals open, is never warned, nothing is cancelled, and all
/// three are still unread at the close.
#[tokio::test]
async fn a_plain_item_is_never_warned_about() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let questioned: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_PLAIN_EXEMPT_KEY",
        vec![("ROLE inbox", running(&["print('ignoring')"]))],
        |builder| {
            builder
                .channel(inbox(), backend)
                .channel(questions(), questioned)
        },
    )
    .await;
    ready(&tree, holder(&["inbox", "questions"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    for (n, item) in ["m-1", "m-2", "m-3"].into_iter().enumerate() {
        within(channel.deliver(Inbound::message(item, "alice", json!(item))))
            .await
            .expect("queued");
        tree.seen("exec.completed", n + 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (report, events, requests) = tree.end().await;
    let opened = openings(&requests, "ROLE inbox");
    assert_eq!(opened.len(), 3, "{opened:#?}");
    assert!(
        opened.iter().all(|opening| !opening.contains("unanswered")),
        "{opened:#?}"
    );
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
    assert_eq!(report.channels[0].unread, ["m-1", "m-2", "m-3"]);
}

/// The task is not a request: a primary holding an inbound request channel with nothing on it
/// reads idle once ready, the roster says so with nothing outstanding, and no round opens until a
/// push.
#[tokio::test]
async fn a_holder_with_no_request_reads_idle() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_IDLE_KEY",
        vec![("ROLE inbox", running(&["print('nothing')"]))],
        |builder| builder.channel(questions(), backend),
    )
    .await;
    let root = ready(&tree, holder(&["questions"])).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        matches!(root.status().state, AgentState::Idle { .. }),
        "{:?}",
        root.status()
    );
    let roster = tree.session.roster();
    let row = roster
        .agents
        .iter()
        .find(|row| Some(row.id.as_str()) == root.agent())
        .expect("the root's row");
    assert!(
        matches!(row.state, Some(RosterState::Agent(AgentState::Idle { .. }))),
        "{row:?}"
    );
    assert!(
        row.outstanding
            .as_ref()
            .is_none_or(|outstanding| outstanding.unread == 0 && outstanding.taken == 0),
        "{row:?}"
    );
    let (_, _, requests) = tree.end().await;
    assert!(openings(&requests, "ROLE inbox").is_empty());
}

/// An outbound request's code, run in the holder's orienting round: it asks, says what its handle
/// reads before and after the backend takes it, and what awaiting it gives.
const ASKING: &str = "import outrig\nch = runtime.channels['asks']\nh = ch.request('may I?')\n\
                      print(h.status.state, ch.pending())\n\
                      while h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
                      print(h.status.state, ch.pending())\n\
                      try:\n    done, pending = await runtime.wait({h})\n\
                      \x20   print('done', h in done, await h)\n\
                      except Exception as e:\n    print(type(e).__name__, e)";

/// Run [`ASKING`] against a backend that takes the request once let, settle it as `settle` says,
/// and return what the code printed, the owner's answer, and the backend.
async fn asked(
    var: &str,
    definition: ChannelDefinition,
    settle: Settle,
) -> (String, Result<(), SettleError>, Arc<Backend>, Vec<Value>) {
    let go = Arc::new(tokio::sync::Notify::new());
    let backend = Backend::answering([Answer::After(Arc::clone(&go))]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        var,
        vec![("ROLE inbox", once_then_silent(ASKING))],
        |builder| builder.channel(definition, given),
    )
    .await;
    let root = constructed(&tree, holder(&["asks"])).await;
    // Its handle reads `sending` until the backend takes it, which it does once let.
    until("the send", || !backend.sent().is_empty()).await;
    go.notify_one();
    let sent = backend.sent();
    let request = sent[0].request.expect("a request's id");
    assert_eq!(sent[0].kind, crate::harness::ItemKind::Request);
    let channel = tree.session.channel("asks").expect("declared");
    until("the acceptance", || {
        outstanding(&tree, "asks").first().map(|(_, state)| *state) == Some(OutboundState::Waiting)
    })
    .await;
    let answer = within(channel.settle(request, settle)).await;
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    let printed = exec_outputs(&events, &subject).concat();
    let settled = data_of(&events, "channel.request.ended", &subject);
    let sent_events = data_of(&events, "channel.request.sent", &subject);
    assert_eq!(sent_events.len(), 1);
    assert_eq!(sent_events[0]["request"], request.get());
    (printed, answer, backend, settled)
}

/// An outbound request goes to the backend as a send and returns at once, `sending`, then
/// `waiting` once the backend takes it; the owner's reply settles it, and a wait returns it done.
#[tokio::test]
async fn an_outbound_request_is_replied_to() {
    let (printed, answer, backend, settled) = asked(
        "OUTRIG_TEST_REQUEST_OUTBOUND_REPLY_KEY",
        asks(),
        Settle::Reply(json!("yes")),
    )
    .await;
    assert_eq!(answer, Ok(()));
    assert_eq!(printed, "sending 1\nwaiting 0\ndone True yes\n");
    assert_eq!(settled[0]["outcome"], "accepted");
    assert!(backend.settlements().is_empty());
}

/// The owner's `fail` and `cancel` settle the handle raising `AgentRequestFailed` and
/// `RequestCancelled`.
#[tokio::test]
async fn an_outbound_request_fails_or_is_cancelled() {
    let (printed, answer, _, _) = asked(
        "OUTRIG_TEST_REQUEST_OUTBOUND_FAIL_KEY",
        asks(),
        Settle::Fail("no".to_string()),
    )
    .await;
    assert_eq!(answer, Ok(()));
    assert!(printed.ends_with("AgentRequestFailed no\n"), "{printed}");
    let (printed, answer, _, _) = asked(
        "OUTRIG_TEST_REQUEST_OUTBOUND_CANCEL_KEY",
        asks(),
        Settle::Cancel("its peer finished".to_string()),
    )
    .await;
    assert_eq!(answer, Ok(()));
    assert!(printed.contains("RequestCancelled"), "{printed}");
    assert!(printed.contains("its peer finished"), "{printed}");
}

/// A reply that does not fit the channel's reply type ends the request failed, naming the
/// problem: the handle raises `AgentRequestFailed`, the owner's settle says so, and the backend
/// is told the request failed.
#[tokio::test]
async fn an_outbound_reply_that_does_not_fit_fails_the_request() {
    let (printed, answer, backend, settled) = asked(
        "OUTRIG_TEST_REQUEST_OUTBOUND_MISFIT_KEY",
        asks().receives(Contract::Typed("app.messages:Note".to_string())),
        Settle::Reply(json!({"text": "x"})),
    )
    .await;
    let Err(SettleError::Rejected(problems)) = &answer else {
        panic!("{answer:?}");
    };
    assert!(
        problems.iter().any(|problem| problem.contains("$.level")),
        "{problems:?}"
    );
    assert!(printed.contains("AgentRequestFailed"), "{printed}");
    assert!(printed.contains("$.level"), "{printed}");
    assert_eq!(settled[0]["outcome"], "failed");
    let settlements = backend.settlements();
    assert_eq!(settlements.len(), 1, "{settlements:?}");
    assert!(
        matches!(&settlements[0].outcome, Settlement::Failed(why) if why.contains("$.level")),
        "{settlements:?}"
    );
}

/// One wait over a request's handle and a child's: a message arriving meanwhile raises
/// `MessageAvailable` with both pending; then the child's reply returns its handle done, the
/// request still pending; then the owner's settle returns the request's.
#[tokio::test]
async fn one_wait_watches_a_request_and_a_childs_handle() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let messaged: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let release = Arc::new(tokio::sync::Notify::new());
    let held = Arc::clone(&release);
    let helper: Responder =
        super::instance_tests::ready_then(Arc::new(move |request: &RecordedRequest| {
            let release = Arc::clone(&held);
            let first = !after_result(request);
            let canned = if first {
                submit(
                    "h1",
                    &py("d = await runtime.channels['ping'].receive()\nawait d.reply('pong')"),
                )
            } else {
                text_reply("ended")
            };
            Box::pin(async move {
                if first {
                    release.notified().await;
                }
                canned
            })
        }));
    let code = "from app.agents import Helper\nhelper = Helper()\nawait helper.ready()\n\
                c = helper.ping('x')\nh = runtime.channels['asks'].request('may I?')\n\
                try:\n    await runtime.wait({h, c})\n\
                except runtime.MessageAvailable as e:\n    print('message', e.channels, h.done(), c.done())\n\
                await runtime.channels['inbox'].receive()\n\
                done, pending = await runtime.wait({h, c}, return_when=asyncio.FIRST_COMPLETED)\n\
                print(c in done, h in pending)\n\
                done, pending = await runtime.wait({h})\nprint(h in done, await h)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_ONE_WAIT_KEY",
        vec![
            ("ROLE helper", helper),
            ("ROLE inbox", once_then_silent(code)),
        ],
        |builder| builder.channel(inbox(), messaged).channel(asks(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox", "asks"])).await;
    until("the request", || !backend.sent().is_empty()).await;
    let request = backend.sent()[0].request.expect("a request's id");
    let inboxed = tree.session.channel("inbox").expect("declared");
    within(inboxed.deliver(Inbound::message("m-1", "alice", json!("mid-wait"))))
        .await
        .expect("queued");
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.notify_one();
    tree.seen("agent.request.ended", 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let asking = tree.session.channel("asks").expect("declared");
    within(asking.settle(request, Settle::Reply(json!("yes"))))
        .await
        .expect("settled");
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject).concat(),
        "message ('inbox',) False False\nTrue True\nTrue yes\n"
    );
}

/// `h.cancel()` asks the backend to withdraw the request and settles the handle on its answer:
/// withdrawn, after which the owner's settle is refused and evented; or the reply it had already
/// recorded.
#[tokio::test]
async fn cancelling_an_outbound_request_settles_on_the_backends_answer() {
    let code = "import outrig\nh = runtime.channels['asks'].request('may I?')\n\
                while h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
                print(h.cancel(), h.done())\n\
                try:\n    print('replied', await h)\n\
                except outrig.RequestCancelled as e:\n    print('cancelled', e)";
    for (var, verdict, expected) in [
        (
            "OUTRIG_TEST_REQUEST_WITHDRAWN_KEY",
            WithdrawAnswer::Withdrawn,
            "True False\ncancelled request",
        ),
        (
            "OUTRIG_TEST_REQUEST_WITHDRAW_REPLIED_KEY",
            WithdrawAnswer::Replied(json!("too late")),
            "True False\nreplied too late\n",
        ),
    ] {
        let backend = Arc::new(Backend::default());
        backend
            .verdicts
            .lock()
            .unwrap()
            .push_back(Some(verdict.clone()));
        let given: Arc<dyn ChannelBackend> = backend.clone();
        let (tree, _app) = holding(
            var,
            vec![("ROLE inbox", once_then_silent(code))],
            |builder| builder.channel(asks(), given),
        )
        .await;
        let root = ready(&tree, holder(&["asks"])).await;
        let withdrawals = backend.withdrawals();
        assert_eq!(withdrawals.len(), 1);
        assert_eq!(withdrawals[0].end, WithdrawEnd::Cancelled);
        assert_eq!(Some(withdrawals[0].request), backend.sent()[0].request);
        let channel = tree.session.channel("asks").expect("declared");
        let late = within(channel.settle(withdrawals[0].request, Settle::Reply(json!("no")))).await;
        let subject = format!("agent/{}", agent_of(&root));
        let (_, events, _) = tree.end().await;
        let printed = exec_outputs(&events, &subject).concat();
        assert!(printed.starts_with(expected), "{verdict:?}: {printed}");
        let settled = data_of(&events, "channel.request.ended", &subject);
        assert_eq!(settled.len(), 1, "{settled:?}");
        match verdict {
            WithdrawAnswer::Withdrawn => {
                assert_eq!(late, Err(SettleError::Ended(RequestEnd::Cancelled)));
                assert_eq!(settled[0]["outcome"], "cancelled");
            }
            _ => {
                assert_eq!(late, Err(SettleError::Ended(RequestEnd::Accepted)));
                assert_eq!(settled[0]["outcome"], "accepted");
            }
        }
        let refused = data_of(&events, "channel.request.refused", &subject);
        assert_eq!(refused.len(), 1, "{refused:?}");
    }
}

/// A withdrawal its backend never answers leaves the handle and the request `uncertain`; a wait
/// that times out with a request pending settles nothing and tells the backend nothing, and the
/// owner's later settle still settles it.
#[tokio::test]
async fn an_unanswered_withdrawal_leaves_the_request_uncertain() {
    let backend = Arc::new(Backend::default());
    backend.verdicts.lock().unwrap().push_back(None);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let flag = Flag::new();
    let code = format!(
        "ch = runtime.channels['asks']\nh = ch.request('one')\nk = ch.request('two')\n\
         while h.status.state == 'sending' or k.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
         done, pending = await runtime.wait({{k}}, timeout=0.1)\nprint(k in pending, k.done())\n\
         h.cancel()\n\
         while h.status.state != 'uncertain':\n    await asyncio.sleep(0.01)\n\
         print(h.status.state, h.done())\n{}\nprint(await k)",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_UNCERTAIN_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(asks().call_timeout(Duration::from_millis(200)), given),
    )
    .await;
    let root = constructed(&tree, holder(&["asks"])).await;
    let channel = tree.session.channel("asks").expect("declared");
    let withdrawal = OutboundState::Uncertain(ChannelOperation::Withdrawal);
    until("the uncertainty", || {
        outstanding(&tree, "asks")
            .iter()
            .any(|(_, state)| *state == withdrawal)
    })
    .await;
    let sent = backend.sent();
    let (one, two) = (sent[0].request.expect("one"), sent[1].request.expect("two"));
    assert_eq!(
        outstanding(&tree, "asks"),
        [(one, withdrawal), (two, OutboundState::Waiting)]
    );
    // The timed-out wait told the backend nothing about the second.
    assert!(backend.withdrawals().iter().all(|w| w.request != two));
    assert!(backend.settlements().is_empty());
    within(channel.settle(two, Settle::Reply(json!("later"))))
        .await
        .expect("settled");
    flag.raise();
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject).concat(),
        "True False\nuncertain False\nlater\n"
    );
}

/// Closing a channel ends every request outstanding on it cancelled, with the closure as cause:
/// an outbound handle raises `RequestCancelled` naming it, an inbound request's reply raises it,
/// the backend hears each once, a wait says the channel closed once, and a later request raises.
#[tokio::test]
async fn a_peer_s_close_ends_the_requests_on_its_channel() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let asked_backend = Arc::new(Backend::default());
    let asking: Arc<dyn ChannelBackend> = asked_backend.clone();
    let flag = Flag::new();
    let code = format!(
        "import outrig\nh = runtime.channels['asks'].request('may I?')\n\
         d = await runtime.channels['questions'].receive()\n{}\n\
         try:\n    await h\nexcept outrig.RequestCancelled as e:\n    print('handle', 'peer gone' in str(e))\n\
         try:\n    await d.reply('4')\nexcept outrig.RequestCancelled as e:\n    print('reply', 'peer gone' in str(e))\n\
         t = asyncio.create_task(asyncio.sleep(0.2))\n\
         for _ in range(3):\n\
         \x20   try:\n        done, _ = await runtime.wait({{t}})\n        print('waited', len(done))\n\
         \x20   except outrig.ChannelClosed as e:\n        print('wait', e.reason)\n\
         try:\n    runtime.channels['asks'].request('again?')\nexcept outrig.ChannelClosed:\n    print('request raises')",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_PEER_CLOSE_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(questions(), given).channel(asks(), asking),
    )
    .await;
    let root = constructed(&tree, holder(&["questions", "asks"])).await;
    let questioned = tree.session.channel("questions").expect("declared");
    ask(&questioned, "q-1", "bob", json!("2+2?")).await;
    tree.seen("agent.request.received", 1).await;
    let asked = tree.session.channel("asks").expect("declared");
    until("the request", || {
        outstanding(&tree, "asks").first().map(|(_, state)| *state) == Some(OutboundState::Waiting)
    })
    .await;
    questioned.close("peer gone");
    asked.close("peer gone");
    until("both notices", || {
        backend.settlements().len() == 1 && asked_backend.settlements().len() == 1
    })
    .await;
    flag.raise();
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    let printed = exec_outputs(&events, &subject).concat();
    // Each closed channel says so to one wait, and no later one.
    assert_eq!(
        printed,
        "handle True\nreply True\nwait peer gone\nwait peer gone\nwaited 1\nrequest raises\n"
    );
    for settled in [backend.settlements(), asked_backend.settlements()] {
        assert!(
            matches!(
                &settled[0].outcome,
                Settlement::Cancelled { by: CancelledBy::Peer, reason } if reason.contains("peer gone")
            ),
            "{settled:?}"
        );
    }
}

/// A close that ends an outbound request while its send is with the backend tells the backend
/// nothing until the send is answered: the backend has the request, under its reference, before
/// it hears that the request ended.
#[tokio::test]
async fn a_peer_s_close_is_told_after_the_send_it_ended() {
    let go = Arc::new(tokio::sync::Notify::new());
    let backend = Backend::answering([Answer::After(Arc::clone(&go))]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nh = runtime.channels['asks'].request('may I?')\n\
                try:\n    await h\nexcept outrig.RequestCancelled as e:\n    print('handle', 'peer gone' in str(e))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_CLOSE_SENDING_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(asks(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["asks"])).await;
    until("the send", || !backend.sent().is_empty()).await;
    let channel = tree.session.channel("asks").expect("declared");
    channel.close("peer gone");
    tree.seen("channel.request.ended", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        backend.settlements().is_empty(),
        "{:?}",
        backend.settlements()
    );
    go.notify_one();
    until("the notice", || !backend.settlements().is_empty()).await;
    let settled = backend.settlements();
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(Some(settled[0].request), backend.sent()[0].request);
    assert_eq!(settled[0].reference.as_deref(), Some("ref"));
    assert!(
        matches!(
            &settled[0].outcome,
            Settlement::Cancelled { by: CancelledBy::Peer, reason } if reason.contains("peer gone")
        ),
        "{settled:?}"
    );
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject).concat(), "handle True\n");
}

/// A withdrawal waits its turn at the backend as a send does: ten cancelled at once against a
/// backend that never answers are asked eight at a time, the last two once the first eight are
/// given up on, and the backend never runs more than eight calls at once.
#[tokio::test]
async fn withdrawals_wait_their_turn_at_the_backend() {
    let backend = Arc::new(Backend::default());
    backend
        .verdicts
        .lock()
        .unwrap()
        .extend(std::iter::repeat_n(None, 10));
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "ch = runtime.channels['asks']\nhs = [ch.request(f'q{i}') for i in range(10)]\n\
                while any(h.status.state == 'sending' for h in hs):\n    await asyncio.sleep(0.01)\n\
                for h in hs:\n    h.cancel()";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_WITHDRAW_TURNS_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(asks().call_timeout(Duration::from_millis(500)), given),
    )
    .await;
    constructed(&tree, holder(&["asks"])).await;
    until("eight withdrawals", || backend.withdrawals().len() == 8).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(backend.withdrawals().len(), 8, "the ninth waits its turn");
    until("all ten", || backend.withdrawals().len() == 10).await;
    until("every one uncertain", || {
        let outstanding = outstanding(&tree, "asks");
        outstanding.len() == 10
            && outstanding
                .iter()
                .all(|(_, state)| *state == OutboundState::Uncertain(ChannelOperation::Withdrawal))
    })
    .await;
    tree.end().await;
    assert_eq!(backend.most(), 8);
}

/// A request is withdrawn once: the close of a session whose holder cancelled a request, while
/// the backend has not yet answered that withdrawal, asks no second one, and the close lists the
/// request acknowledged only if that one is answered. A second `cancel()` after a withdrawal
/// that went unanswered asks nothing either.
#[tokio::test]
async fn a_request_is_withdrawn_once() {
    let code = "import outrig\nh = runtime.channels['asks'].request('may I?')\n\
                while h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
                h.cancel()";
    for (var, verdict, answered) in [
        (
            "OUTRIG_TEST_REQUEST_WITHDRAWN_ONCE_KEY",
            Some(WithdrawAnswer::Withdrawn),
            true,
        ),
        ("OUTRIG_TEST_REQUEST_WITHDRAWN_NEVER_KEY", None, false),
    ] {
        let backend = Arc::new(Backend::default());
        backend.verdicts.lock().unwrap().push_back(verdict);
        *backend.withdraw_after.lock().unwrap() = Duration::from_millis(500);
        let given: Arc<dyn ChannelBackend> = backend.clone();
        let (tree, _app) = holding(
            var,
            vec![("ROLE inbox", once_then_silent(code))],
            |builder| builder.channel(asks().call_timeout(Duration::from_secs(1)), given),
        )
        .await;
        constructed(&tree, holder(&["asks"])).await;
        until("the withdrawal", || !backend.withdrawals().is_empty()).await;
        let (report, _, _) = tree.end().await;
        let withdrawals = backend.withdrawals();
        assert_eq!(withdrawals.len(), 1, "{withdrawals:?}");
        assert_eq!(withdrawals[0].end, WithdrawEnd::Cancelled);
        let rows: Vec<_> = report
            .channels
            .iter()
            .flat_map(|channel| channel.requests.iter())
            .collect();
        assert_eq!(rows.len(), 1, "{rows:#?}");
        assert_eq!(rows[0].end, RequestEnd::Closed);
        assert_eq!(rows[0].acknowledged, answered, "{rows:#?}");
    }
    let backend = Arc::new(Backend::default());
    backend.verdicts.lock().unwrap().push_back(None);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = format!(
        "{code}\nwhile h.status.state != 'uncertain':\n    await asyncio.sleep(0.01)\n\
         print(h.cancel())"
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_WITHDRAWN_AGAIN_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(asks().call_timeout(Duration::from_millis(200)), given),
    )
    .await;
    let root = ready(&tree, holder(&["asks"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject).concat(), "True\n");
    let withdrawals = backend.withdrawals();
    assert_eq!(withdrawals.len(), 1, "{withdrawals:?}");
}

/// An inbound channel's bound counts its requests unsettled: with one unread and one received
/// and unanswered, a third push is refused full, and taken once one is answered.
#[tokio::test]
async fn the_bound_counts_requests_unsettled() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let flag = Flag::new();
    let code = format!(
        "d = await runtime.channels['questions'].receive()\n{}\nawait d.reply('4')",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_BOUND_KEY",
        vec![("ROLE inbox", running(&[&code, "pass"]))],
        |builder| builder.channel(questions().bound(2), backend),
    )
    .await;
    ready(&tree, holder(&["questions"])).await;
    let channel = tree.session.channel("questions").expect("declared");
    ask(&channel, "q-1", "bob", json!("one")).await;
    tree.seen("agent.request.received", 1).await;
    ask(&channel, "q-2", "bob", json!("two")).await;
    let refused = within(channel.deliver(Inbound::request("q-3", "bob", json!("three")))).await;
    assert_eq!(refused, Err(DeliverError::Full));
    flag.raise();
    tree.seen("agent.request.ended", 1).await;
    within(channel.room()).await;
    ask(&channel, "q-3", "bob", json!("three")).await;
    tree.end().await;
}

/// Owner-driven primaries obey the same rule as runner-driven holders. Taking the request
/// earns a nudge before the warning; leaving it unread earns just the arrival and warning.
#[tokio::test]
async fn a_harness_built_primary_warns_then_cancels_its_requests() {
    for taken in [false, true] {
        let backend = Arc::new(Backend::default());
        let given: Arc<dyn ChannelBackend> = backend.clone();
        let first = if taken {
            "d = await runtime.channels['questions'].receive()\nprint(d.body)"
        } else {
            "pass"
        };
        let mock = mock_http::routed(vec![("", from_launch(&[first, "pass"]))], None).await;
        let mut tree = tree_watched(
            if taken {
                "OUTRIG_TEST_PRIMARY_TAKEN_KEY"
            } else {
                "OUTRIG_TEST_PRIMARY_UNREAD_KEY"
            },
            "",
            mock,
            Watch::Gathered,
            |builder| builder.channel(questions(), given),
        )
        .await;
        let channel = tree.session.channel("questions").expect("declared");
        assert_eq!(channel.holder().as_deref(), Some("primary"));
        let request = ask(&channel, "q-1", "bob", json!("for the primary")).await;
        let rounds = if taken { 3 } else { 2 };
        for _ in 0..rounds {
            assert!(matches!(
                within(tree.session.round()).await.expect("a round"),
                crate::harness::RoundOutcome::Ended(_)
            ));
        }
        // No model call is spent cancelling after the warning. The end is heard before
        // NothingNew, and remains terminal when the owner asks again or closes.
        for _ in 0..2 {
            assert!(matches!(
                within(tree.session.round()).await.expect("idle"),
                crate::harness::RoundOutcome::NothingNew
            ));
        }
        tree.seen("channel.settled", 1).await;
        let (_, events, requests) = tree.end().await;
        let opened = openings(&requests, "");
        assert_eq!(opened.len(), rounds, "{opened:#?}");
        assert!(opened[0].contains("1 request is waiting on runtime.channels[\"questions\"]"));
        assert!(opened[rounds - 1].contains("your last round ended with requests unanswered"));
        if taken {
            assert!(opened[1].contains(&format!(
                "you took request {request} from runtime.channels[\"questions\"]"
            )));
            assert_eq!(
                exec_outputs(&events, "agent/primary"),
                ["for the primary\n", "", ""]
            );
        }
        let settlements = backend.settlements();
        assert_eq!(settlements.len(), 1, "{settlements:?}");
        assert!(matches!(&settlements[0].outcome,
            Settlement::Cancelled { by: CancelledBy::Host, reason }
                if reason.contains(if taken { "without answering it" } else { "without reading it" })));
        let cancelled = data_of(&events, "agent.request.cancelled", "agent/primary");
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0]["request"], request);
        assert_eq!(cancelled[0]["by"], "host");
    }
}

/// A warning is a real opportunity to answer, not an unconditional deferred cancel. A later
/// request starts with a fresh arrival, not the previous request's warning.
#[tokio::test]
async fn a_harness_built_primary_can_reply_in_the_warning_round() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let mock = mock_http::routed(
        vec![(
            "",
            from_launch(&[
                "d = await runtime.channels['questions'].receive()",
                "pass",
                "await d.reply('4')",
                "d = await runtime.channels['questions'].receive()\nawait d.fail('no')",
            ]),
        )],
        None,
    )
    .await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_PRIMARY_WARNING_REPLY_KEY",
        "",
        mock,
        Watch::Gathered,
        |builder| builder.channel(questions(), given),
    )
    .await;
    let channel = tree.session.channel("questions").expect("declared");
    ask(&channel, "q-1", "bob", json!("2+2?")).await;
    for _ in 0..3 {
        within(tree.session.round()).await.expect("a round");
    }
    assert!(matches!(
        within(tree.session.round()).await.expect("idle"),
        crate::harness::RoundOutcome::NothingNew
    ));
    ask(&channel, "q-2", "bob", json!("another")).await;
    within(tree.session.round()).await.expect("a fresh round");
    tree.seen("channel.settled", 2).await;
    let (_, events, requests) = tree.end().await;
    let opened = openings(&requests, "");
    assert_eq!(opened.len(), 4);
    assert!(!opened[3].contains("your last round ended with requests unanswered"));
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
    assert_eq!(
        outcomes(&backend.settlements()),
        [
            Settlement::Replied(json!("4")),
            Settlement::Failed("no".into())
        ]
    );
}

/// A failed or abandoned warning is not a warning the model has necessarily read. The owner
/// may retry it, and a plain channel's unread item never joins the requests being cancelled.
#[tokio::test]
async fn a_harness_built_primary_does_not_spend_a_failed_or_dropped_warning() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let plain: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(tokio::sync::Notify::new());
    let responder: Responder = {
        let calls = calls.clone();
        let seen = seen.clone();
        Arc::new(move |_| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let seen = seen.clone();
            Box::pin(async move {
                match call {
                    1 => mock_http::failure(400),
                    2 => {
                        seen.notify_one();
                        std::future::pending().await
                    }
                    _ => text_reply("ended"),
                }
            })
        })
    };
    let mock = mock_http::routed(vec![("", responder)], None).await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_PRIMARY_FAILED_WARNING_KEY",
        "",
        mock,
        Watch::Gathered,
        |builder| builder.channel(questions(), given).channel(inbox(), plain),
    )
    .await;
    let channel = tree.session.channel("questions").expect("declared");
    let messages = tree.session.channel("inbox").expect("declared");
    ask(&channel, "q-1", "bob", json!("question")).await;
    within(messages.deliver(Inbound::message("m-1", "alice", json!("plain"))))
        .await
        .expect("queued");
    within(tree.session.round()).await.expect("arrival");
    within(tree.session.round())
        .await
        .expect_err("the first warning failed");
    {
        let abandoned = tree.session.round();
        tokio::pin!(abandoned);
        tokio::select! {
            _ = within(seen.notified()) => {},
            _ = &mut abandoned => panic!("the second warning should be held at the provider"),
        }
    }
    assert!(backend.settlements().is_empty());
    within(tree.session.round()).await.expect("warning again");
    assert!(matches!(
        within(tree.session.round()).await.expect("cancel and idle"),
        crate::harness::RoundOutcome::NothingNew
    ));
    tree.seen("channel.settled", 1).await;
    let (_, events, requests) = tree.end().await;
    let opened = openings(&requests, "");
    assert_eq!(opened.len(), 4, "{opened:#?}");
    for warning in &opened[1..] {
        assert!(
            warning.contains("your last round ended with requests unanswered"),
            "{warning}"
        );
        assert!(warning.contains("runtime.channels[\"questions\"]"));
    }
    assert_eq!(
        data_of(&events, "agent.request.cancelled", "agent/primary").len(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(backend.settlements().len(), 1);
}

/// At the close, an inbound request unread and one received and unanswered end `closed`, which
/// the backend hears of each; an outbound request outstanding raises `SessionClosing` at its
/// handle and is withdrawn, `released`. The report lists all three with their ends, a notice the
/// backend never acknowledged as unacknowledged.
#[tokio::test]
async fn the_close_ends_every_request_and_tells_the_backend() {
    let backend = Arc::new(Backend::default());
    *backend.unsettling.lock().unwrap() = Some("q-2".to_string());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let asked_backend = Arc::new(Backend::default());
    let asking: Arc<dyn ChannelBackend> = asked_backend.clone();
    let code = "import outrig\nh = runtime.channels['asks'].request('may I?')\n\
                d = await runtime.channels['questions'].receive()";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REQUEST_CLOSE_ROWS_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(questions(), given).channel(asks(), asking),
    )
    .await;
    let root = constructed(&tree, holder(&["questions", "asks"])).await;
    let questioned = tree.session.channel("questions").expect("declared");
    let one = ask(&questioned, "q-1", "bob", json!("one")).await;
    let two = ask(&questioned, "q-2", "bob", json!("two")).await;
    within(root.ready()).await.expect("ready");
    let outbound = asked_backend.sent()[0].request.expect("a request's id");
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    let closed: Vec<_> = report
        .channels
        .iter()
        .flat_map(|channel| {
            channel
                .requests
                .iter()
                .map(move |at| (channel.channel.as_str(), at))
        })
        .collect();
    assert_eq!(closed.len(), 3, "{closed:#?}");
    for (channel, at) in &closed {
        assert_eq!(at.end, RequestEnd::Closed, "{closed:#?}");
        match *channel {
            "questions" => {
                assert!([one, two].contains(&at.request.get()));
                assert_eq!(at.direction, Direction::Inbound);
                // The backend acknowledged the first's notice, and never the second's.
                assert_eq!(at.acknowledged, at.request.get() == one, "{closed:#?}");
            }
            _ => {
                assert_eq!(at.request, outbound);
                assert_eq!(at.direction, Direction::Outbound);
                assert!(at.acknowledged, "{closed:#?}");
            }
        }
    }
    let settlements = outcomes(&backend.settlements());
    assert!(settlements.contains(&Settlement::Closed), "{settlements:?}");
    let told: std::collections::BTreeSet<u64> = backend
        .settlements()
        .iter()
        .map(|notice| notice.request.get())
        .collect();
    assert_eq!(told, [one, two].into());
    let withdrawals = asked_backend.withdrawals();
    assert_eq!(withdrawals.len(), 1);
    assert_eq!(withdrawals[0].end, WithdrawEnd::Released);
    // The handle ended with the closing error, as `SessionClosing` says it.
    let settled = data_of(&events, "channel.request.ended", &subject);
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0]["outcome"], "closed");
    assert!(
        settled[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("the session closed before")),
        "{settled:?}"
    );
}
