//! What becomes of a send, and where every item on a channel is (`0003-53`): an uncertain send
//! is resolved by its identity -- once by OutRig before the agent is told, and again on the
//! agent's asking -- never by sending it again, and the session's registry answers at any moment
//! what each channel holds: what is unread, consumed and acknowledged, received and unanswered,
//! outstanding, uncertain, and what each round sent. Each agent's model is the routed mock; the
//! backend is `external_tests`'s, which keeps a durable record of what it stored when the test
//! asks it to.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::children_tests::{
    Tree, Watch, data, data_of, exec_outputs, kind, position, primary_runs, raised, tree_watched,
};
use super::external_request_tests::{ask, asks, once_then_silent, questions};
use super::external_tests::{
    Answer, Backend, constructed, from_launch, holder, holding, inbox, ready, running, until,
};
use super::mock_http;
use super::root_tests::agent_of;
use crate::harness::event::{ItemOutcome, MessageId};
use crate::harness::{
    ChannelBackend, ChannelDefinition, ChannelKind, ChannelOperation, ChannelSnapshot, Contract,
    Delivered, DeliveryId, Inbound, InboundItem, OutboundState, Position, SendAnswer, Settle,
    Verdict, Wake, WithdrawAnswer,
};
use crate::python::testing::{Flag, within};

/// `inbox`, pushed `items` from the start, each `m-N` from `alice` with the body `body-N` and
/// the reference `row-N`.
fn inbox_holding(items: &[usize]) -> ChannelDefinition {
    items.iter().fold(inbox(), |definition, n| {
        definition.deliver(
            Inbound::message(format!("m-{n}"), "alice", json!(format!("body-{n}")))
                .reference(format!("row-{n}")),
        )
    })
}

/// The snapshot of channel `name`.
fn snapshot_of(snapshots: &[ChannelSnapshot], name: &str) -> ChannelSnapshot {
    snapshots
        .iter()
        .find(|snapshot| snapshot.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no channel {name} in {snapshots:?}"))
}

/// The owner's ids of `items`, in order.
fn ids(items: &[InboundItem]) -> Vec<&str> {
    items.iter().map(|item| item.item.as_str()).collect()
}

/// Every inbound id `snapshot` places, with where: none may be in two places.
fn placed(snapshot: &ChannelSnapshot) -> Vec<(String, &'static str)> {
    let mut placed: Vec<(String, &'static str)> = Vec::new();
    for item in &snapshot.unread {
        placed.push((item.item.clone(), "unread"));
    }
    for item in &snapshot.unacknowledged {
        placed.push((item.item.clone(), "unacknowledged"));
    }
    for request in &snapshot.received {
        placed.push((request.item.clone(), "received"));
    }
    placed
}

/// Fail unless no id in `snapshot` is in two places, and the watermarks agree with what is
/// unread: the acknowledged one at or before the consumed one, and every unread message after
/// it.
fn assert_disjoint(snapshot: &ChannelSnapshot) {
    let placed = placed(snapshot);
    let distinct: BTreeSet<&String> = placed.iter().map(|(item, _)| item).collect();
    assert_eq!(distinct.len(), placed.len(), "{placed:?} in {snapshot:?}");
    let message = |item: &InboundItem| item.delivery.message().map(MessageId::get);
    let consumed = snapshot.consumed.as_ref().and_then(message);
    let acknowledged = snapshot.acknowledged.as_ref().and_then(message);
    assert!(acknowledged <= consumed, "{snapshot:?}");
    for item in &snapshot.unread {
        assert!(
            message(item) > consumed,
            "{item:?} is unread at or before {consumed:?}"
        );
    }
    for item in &snapshot.unacknowledged {
        assert!(
            message(item) > acknowledged && message(item) <= consumed,
            "{snapshot:?}"
        );
    }
}

/// A send the backend stores and then answers uncertain -- or never answers within its bound --
/// is asked about once, by its id, before the agent is told: answered accepted, the agent's
/// `send` returns, the backend holds one item, and the record shows the item uncertain and then
/// resolved accepted with its reference. Nothing is sent twice.
#[tokio::test]
async fn uncertain_then_accepted_is_one_item_and_one_acceptance() {
    let backend = Backend::durable(
        [Answer::UncertainKept("the ack was lost"), Answer::NeverKept],
        [],
    );
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code =
        "ch = runtime.channels['inbox']\nprint(await ch.send('x'))\nprint(await ch.send('y'))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_ACCEPTED_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| builder.channel(inbox().call_timeout(Duration::from_millis(300)), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let sent = backend.sent();
    let bodies: Vec<&Value> = sent.iter().map(|item| &item.body).collect();
    assert_eq!(bodies, [&json!("x"), &json!("y")], "each sent once");
    let items: Vec<MessageId> = sent.iter().map(|item| item.item).collect();
    assert_eq!(backend.kept(), items, "one stored item each");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!("{}\n{}\n", items[0].get(), items[1].get())]
    );
    let queries = backend.queries();
    assert_eq!(queries.len(), 2, "asked once each: {queries:?}");
    assert!(
        queries
            .iter()
            .all(|(operation, _)| *operation == ChannelOperation::Send)
    );
    let outcomes = data_of(&events, "channel.item.outcome", &subject);
    let resolved = data_of(&events, "channel.item.resolved", &subject);
    for (n, item) in items.iter().enumerate() {
        assert_eq!(outcomes[n]["item"], json!(item));
        assert_eq!(outcomes[n]["outcome"], "uncertain");
        assert_eq!(resolved[n]["item"], json!(item));
        assert_eq!(resolved[n]["operation"], "send");
        assert_eq!(resolved[n]["answer"], "accepted");
        assert_eq!(resolved[n]["reference"], json!(format!("ref-{item}")));
        assert!(
            position(&events, "channel.item.outcome", |e| data(e)["item"]
                == json!(item))
                < position(&events, "channel.item.resolved", |e| data(e)["item"]
                    == json!(item))
        );
    }
    assert!(
        outcomes[1]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("within")),
        "{outcomes:?}"
    );
}

/// A send uncertain when sent and when OutRig first asks is raised with its id, listed
/// uncertain in the registry, and resolved later by the agent's own `resolve`: accepted, with
/// its reference, the backend holding one item. Resolved, it is uncertain no more.
#[tokio::test]
async fn still_uncertain_is_raised_with_the_id_and_resolved_later() {
    let backend = Backend::durable(
        [Answer::UncertainKept("the ack was lost")],
        [SendAnswer::Uncertain {
            reason: "the store is down".to_string(),
        }],
    );
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let flag = Flag::new();
    let code = format!(
        "import outrig\nch = runtime.channels['inbox']\n\
         try:\n    await ch.send('x')\nexcept outrig.SendUncertain as e:\n    item, said = e.item, 'resolve' in str(e)\n\
         print('uncertain', item, said)\n{}\n\
         r = await ch.resolve(item)\nprint(r.outcome, r.reference)\n\
         try:\n    await ch.resolve(item)\nexcept ValueError as e:\n    print('resolved', 'not uncertain' in str(e) or 'is uncertain' in str(e))",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_LATER_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    until("the uncertainty", || {
        !snapshot_of(&tree.session.channels(), "inbox")
            .uncertain
            .is_empty()
    })
    .await;
    let item = backend.sent()[0].item;
    let uncertain = snapshot_of(&tree.session.channels(), "inbox").uncertain;
    assert_eq!(uncertain.len(), 1);
    assert_eq!(uncertain[0].item, item);
    assert_eq!(uncertain[0].agent, agent_of(&root));
    assert!(
        uncertain[0].reason.contains("the store is down"),
        "{uncertain:?}"
    );
    flag.raise();
    let subject = format!("agent/{}", agent_of(&root));
    tree.seen("exec.completed", 1).await;
    assert!(
        snapshot_of(&tree.session.channels(), "inbox")
            .uncertain
            .is_empty()
    );
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!(
            "uncertain {item} True\naccepted ref-{item}\nresolved True\n"
        )]
    );
    assert_eq!(backend.kept(), [item], "one stored item");
    assert_eq!(backend.sent().len(), 1, "sent once");
    // Asked twice -- by OutRig, then by the agent -- and refused the third time unasked.
    assert_eq!(backend.queries().len(), 2);
    let answers: Vec<Value> = data_of(&events, "channel.item.resolved", &subject)
        .into_iter()
        .map(|data| data["answer"].clone())
        .collect();
    assert_eq!(answers, [json!("uncertain"), json!("accepted")]);
    assert!(report.channels[0].uncertain.is_empty(), "{report:?}");
}

/// A send the backend rejects before taking it raises saying a retry is safe, and the retry
/// succeeds under a new id; one whose resolution answers not accepted is the same. The backend
/// holds the retried item alone.
#[tokio::test]
async fn known_not_accepted_is_retry_safe() {
    let backend = Backend::durable(
        [
            Answer::NotAccept("the thread is locked"),
            Answer::Accept,
            Answer::Uncertain("the store timed out"),
        ],
        [],
    );
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\n\
                try:\n    await ch.send('x')\nexcept outrig.SendNotAccepted as e:\n    print('not', 'safe' in str(e))\n\
                print(await ch.send('x'))\n\
                try:\n    await ch.send('z')\nexcept outrig.SendNotAccepted as e:\n    print('resolved not', 'safe' in str(e))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_NOT_ACCEPTED_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let sent = backend.sent();
    let items: Vec<MessageId> = sent.iter().map(|item| item.item).collect();
    assert_eq!(items.len(), 3);
    assert_eq!(backend.kept(), [items[1]], "the retry alone");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!("not True\n{}\nresolved not True\n", items[1].get())]
    );
    // Only the uncertain one was asked about.
    let queries = backend.queries();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].1.item, items[2]);
    let resolved = data_of(&events, "channel.item.resolved", &subject);
    assert_eq!(resolved[0]["answer"], "not_accepted");
    // Not taken is no uncertainty, at the close or before it.
    assert!(report.channels[0].uncertain.is_empty(), "{report:?}");
}

/// A body outside the contract, a send on a channel its far end closed, and one the session's
/// close refuses each raise with the reason, and none reaches the backend's `send` or
/// `resolve`.
#[tokio::test]
async fn a_refused_send_never_reaches_the_backend() {
    let backend = Backend::durable([], []);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let fed = Backend::durable([], []);
    let feeding: Arc<dyn ChannelBackend> = fed.clone();
    let (reached, flag) = (Flag::new(), Flag::new());
    let code = format!(
        "import outrig\nch = runtime.channels['inbox']\n\
         try:\n    await ch.send(1)\nexcept TypeError as e:\n    print('type', 'sends str' in str(e))\n\
         try:\n    await runtime.channels['feed'].send('x')\nexcept outrig.ChannelClosed as e:\n    print('closed', e.reason)\n\
         open({}, 'w').close()\n{}\n\
         try:\n    await ch.send('late')\nexcept outrig.SessionClosing as e:\n    print('closing')",
        reached.py(),
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_REFUSED_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| {
            builder
                .channel(inbox(), given)
                .channel(ChannelDefinition::new("feed"), feeding)
        },
    )
    .await;
    tree.session
        .channel("feed")
        .expect("declared")
        .close("peer gone");
    let root = constructed(&tree, holder(&["inbox", "feed"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    // Admission closes before the last send is made, so it meets the close.
    raised(&reached).await;
    tree.session.closer().close_admission();
    flag.raise();
    tree.seen("exec.completed", 1).await;
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject),
        ["type True\nclosed peer gone\nclosing\n"]
    );
    for backend in [&backend, &fed] {
        assert!(backend.sent().is_empty(), "{:?}", backend.sent());
        assert!(backend.queries().is_empty(), "{:?}", backend.queries());
    }
}

/// An outbound request whose send is uncertain, and stays so when OutRig first asks, reads
/// `uncertain` and does not settle; the agent's `resolve(h.id)` answering accepted moves it to
/// `waiting`, where the owner's settle settles it, and one answering not accepted ends it so
/// that awaiting it raises `SendNotAccepted`.
#[tokio::test]
async fn an_outbound_requests_uncertainty_is_resolved_by_its_id() {
    let backend = Backend::durable(
        [
            Answer::UncertainKept("the ack was lost"),
            Answer::Uncertain("the store timed out"),
        ],
        [
            SendAnswer::Uncertain {
                reason: "the store is down".to_string(),
            },
            SendAnswer::Uncertain {
                reason: "the store is down".to_string(),
            },
        ],
    );
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let flag = Flag::new();
    let code = format!(
        "import outrig\nch = runtime.channels['asks']\n\
         h = ch.request('may I?')\nwhile h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
         k = ch.request('and I?')\nwhile k.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
         done, pending = await runtime.wait({{h, k}}, timeout=0.1)\n\
         print(h.status.state, k.status.state, len(pending))\n\
         r = await ch.resolve(h.id)\nprint(r.outcome, r.reference, h.status.state)\n\
         r = await ch.resolve(k.id)\nprint(r.outcome)\n\
         try:\n    await k\nexcept outrig.SendNotAccepted as e:\n    print('not accepted', 'safe' in str(e))\n\
         {}\nprint(await h)",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_REQUEST_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(asks(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["asks"])).await;
    let channel = tree.session.channel("asks").expect("declared");
    // The code resolves the first, then the second: wait for both answers to have landed.
    until("the first request waiting, the second ended", || {
        let outstanding = snapshot_of(&tree.session.channels(), "asks").outstanding;
        outstanding.len() == 1 && outstanding[0].state == OutboundState::Waiting
    })
    .await;
    let sent = backend.sent();
    let request = sent[0].request.expect("a request");
    let outstanding = snapshot_of(&tree.session.channels(), "asks").outstanding;
    assert_eq!(outstanding.len(), 1, "the second ended: {outstanding:?}");
    assert_eq!(outstanding[0].request, request);
    assert_eq!(outstanding[0].item, sent[0].item);
    assert_eq!(
        outstanding[0].reference,
        Some(format!("ref-{}", sent[0].item))
    );
    within(channel.settle(request, Settle::Reply(json!("yes"))))
        .await
        .expect("settled");
    flag.raise();
    within(root.ready()).await.expect("ready");
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!(
            "uncertain uncertain 2\naccepted ref-{} waiting\nnot_accepted\nnot accepted True\nyes\n",
            sent[0].item
        )]
    );
    assert_eq!(sent.len(), 2, "nothing sent again");
    let resolved = data_of(&events, "channel.item.resolved", &subject);
    let answers: Vec<(&Value, &Value)> = resolved
        .iter()
        .map(|data| (&data["request"], &data["answer"]))
        .collect();
    let (one, two) = (json!(request), json!(sent[1].request.expect("a request")));
    assert_eq!(
        answers,
        [
            (&one, &json!("uncertain")),
            (&two, &json!("uncertain")),
            (&one, &json!("accepted")),
            (&two, &json!("not_accepted")),
        ]
    );
}

/// A withdrawal its backend never answers leaves the handle `uncertain`. Asking about the send
/// then changes nothing, though it was accepted; asking about the withdrawal settles it as the
/// backend recorded it: with the reply it had, or cancelled.
#[tokio::test]
async fn a_withdrawals_uncertainty_is_resolved_by_its_id() {
    let backend = Backend::durable(
        [],
        [SendAnswer::Accepted {
            reference: Some("row-1".to_string()),
        }],
    );
    backend.verdicts.lock().unwrap().extend([None, None]);
    backend.withdrawal_resolutions.lock().unwrap().extend([
        Some(WithdrawAnswer::Replied(json!("yes"))),
        Some(WithdrawAnswer::Withdrawn),
    ]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['asks']\n\
                h = ch.request('one')\nwhile h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
                h.cancel()\nwhile h.status.state != 'uncertain':\n    await asyncio.sleep(0.01)\n\
                r = await ch.resolve(h.id)\nprint(r.outcome, h.status.state, h.done())\n\
                r = await ch.resolve(h.id, 'withdrawal')\nprint(r.outcome, await h)\n\
                k = ch.request('two')\nwhile k.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
                k.cancel()\nwhile k.status.state != 'uncertain':\n    await asyncio.sleep(0.01)\n\
                r = await ch.resolve(k.id, 'withdrawal')\nprint(r.outcome)\n\
                try:\n    await k\nexcept outrig.RequestCancelled:\n    print('cancelled')\n\
                try:\n    await ch.resolve(k.id, 'withdrawal')\nexcept ValueError:\n    print('ended')";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_WITHDRAWAL_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| builder.channel(asks().call_timeout(Duration::from_millis(200)), given),
    )
    .await;
    let root = ready(&tree, holder(&["asks"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        ["accepted uncertain False\nreplied yes\nwithdrawn\ncancelled\nended\n"]
    );
    let operations: Vec<(Value, Value)> = data_of(&events, "channel.item.resolved", &subject)
        .into_iter()
        .map(|data| (data["operation"].clone(), data["answer"].clone()))
        .collect();
    assert_eq!(
        operations,
        [
            (json!("send"), json!("accepted")),
            (json!("withdrawal"), json!("replied")),
            (json!("withdrawal"), json!("withdrawn")),
        ]
    );
    let queries = backend.queries();
    assert_eq!(queries[1].0, ChannelOperation::Withdrawal);
    assert_eq!(queries[1].1.request, backend.sent()[0].request);
    let ended: Vec<Value> = data_of(&events, "channel.request.ended", &subject)
        .into_iter()
        .map(|data| data["outcome"].clone())
        .collect();
    assert_eq!(ended, [json!("accepted"), json!("cancelled")]);
    // Withdrawn once each: resolving asks nothing to be withdrawn again.
    assert_eq!(backend.withdrawals().len(), 2);
}

/// `send` returns the item's id: on an external channel the one its backend was given, and on
/// the user channel the one its `message.sent` carries.
#[tokio::test]
async fn send_returns_the_id() {
    let backend = Backend::durable([], []);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_SEND_ID_KEY",
        vec![(
            "ROLE inbox",
            from_launch(&["print(await runtime.channels['inbox'].send('x'))"]),
        )],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    let item = backend.sent()[0].item;
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!("{}\n", item.get())]
    );

    let mock = mock_http::routed(
        vec![(
            "",
            primary_runs("print(type(i := await runtime.channels['user'].send('x')).__name__, i)"),
        )],
        None,
    )
    .await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_RESOLVE_USER_ID_KEY",
        "",
        mock,
        Watch::Gathered,
        |b| b,
    )
    .await;
    let user = tree.session.user_channel().expect("a user channel");
    within(user.send("go")).await.expect("delivered");
    within(tree.session.round()).await.expect("a round");
    assert_eq!(within(user.receive()).await.as_deref(), Some("x"));
    let (_, events, _) = tree.end().await;
    let sent: Vec<Value> = data_of(&events, "message.sent", "agent/primary")
        .into_iter()
        .filter(|data| data["channel"] == "user" && data["from"] == "agent/primary")
        .collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        [format!("int {}\n", sent[0]["message"])]
    );
}

/// The registry lists, per channel: its descriptor; the items unread, in order, with both ids
/// and their references; the consumed watermark at the last item received and the acknowledged
/// one at the last the backend confirmed; the inbound request received and unanswered, with how
/// long it has waited; the outbound requests outstanding, with their states and references; the
/// message whose send is uncertain; and what the round sent, by channel, count and id. No id is
/// in two places, and every event about these items names an id the snapshot holds.
#[tokio::test]
async fn the_registry_lists_what_each_channel_holds() {
    let backend = Backend::answering([Answer::Accept, Answer::Uncertain("the store timed out")]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let questioner = Arc::new(Backend::default());
    let questioned: Arc<dyn ChannelBackend> = questioner.clone();
    let asked = Backend::answering([Answer::Accept, Answer::Uncertain("the store timed out")]);
    let asking: Arc<dyn ChannelBackend> = asked.clone();
    let (first, last) = (Flag::new(), Flag::new());
    let code = format!(
        "import outrig\nch = runtime.channels['inbox']\nasks = runtime.channels['asks']\n\
         d1 = await ch.receive()\n{}\nd2 = await ch.receive()\n\
         q = await runtime.channels['questions'].receive()\n\
         a = await ch.send('a')\n\
         try:\n    await ch.send('b')\nexcept outrig.SendUncertain as e:\n    b = e.item\n\
         h = asks.request('one')\nwhile h.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
         k = asks.request('two')\nwhile k.status.state == 'sending':\n    await asyncio.sleep(0.01)\n\
         {}",
        first.awaited(),
        last.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_REGISTRY_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| {
            builder
                .channel(inbox_holding(&[1, 2, 3, 4]).reference("thread-7"), given)
                .channel(
                    questions()
                        .deliver(Inbound::request("q-1", "bob", json!("2+2?")).reference("row-q")),
                    questioned,
                )
                .channel(asks(), asking)
        },
    )
    .await;
    let root = constructed(&tree, holder(&["inbox", "questions", "asks"])).await;
    let agent = agent_of(&root);
    tree.seen("channel.consumed", 1).await;
    // The second receipt's notice fails from here on, so it stays unacknowledged.
    *backend.refusing.lock().unwrap() = true;
    first.raise();
    until("the sends and requests", || {
        asked.sent().len() == 2
            && backend.notices().len() >= 2
            && snapshot_of(&tree.session.channels(), "asks")
                .outstanding
                .iter()
                .all(|request| request.state != OutboundState::Sending)
    })
    .await;
    let snapshots = tree.session.channels();
    let names: Vec<&str> = snapshots
        .iter()
        .map(|snapshot| snapshot.name.as_str())
        .collect();
    assert_eq!(names, ["inbox", "questions", "asks"]);
    for snapshot in &snapshots {
        assert_disjoint(snapshot);
    }

    let inbox = snapshot_of(&snapshots, "inbox");
    assert_eq!(inbox.kind, ChannelKind::Plain);
    assert_eq!(inbox.receives, Contract::Text);
    assert_eq!(inbox.sends, Contract::Text);
    assert_eq!(inbox.bound, 256);
    assert_eq!(inbox.wake, Wake::Wake);
    assert_eq!(inbox.holder.as_deref(), Some(agent.as_str()));
    assert_eq!(inbox.reference.as_deref(), Some("thread-7"));
    assert_eq!(ids(&inbox.unread), ["m-3", "m-4"]);
    assert_eq!(inbox.unread[0].reference.as_deref(), Some("row-3"));
    let (three, four) = (
        inbox.unread[0].delivery.message().expect("a message"),
        inbox.unread[1].delivery.message().expect("a message"),
    );
    assert!(three < four, "in order");
    assert_eq!(ids(&inbox.unacknowledged), ["m-2"]);
    assert_eq!(
        inbox.consumed.as_ref().map(|item| item.item.as_str()),
        Some("m-2")
    );
    assert_eq!(
        inbox.acknowledged.as_ref().map(|item| item.item.as_str()),
        Some("m-1")
    );
    assert_eq!(
        inbox
            .acknowledged
            .as_ref()
            .and_then(|item| item.reference.as_deref()),
        Some("row-1")
    );
    let inbox_sent = backend.sent();
    assert_eq!(inbox.uncertain.len(), 1);
    assert_eq!(inbox.uncertain[0].item, inbox_sent[1].item);
    assert_eq!(inbox.rounds.len(), 1, "{:?}", inbox.rounds);
    assert_eq!(inbox.rounds[0].agent, agent);
    let items: Vec<MessageId> = inbox_sent.iter().map(|item| item.item).collect();
    assert_eq!(inbox.rounds[0].items, items);

    let questions = snapshot_of(&snapshots, "questions");
    assert!(questions.unread.is_empty());
    assert_eq!(questions.received.len(), 1);
    assert_eq!(questions.received[0].item, "q-1");
    assert_eq!(questions.received[0].reference.as_deref(), Some("row-q"));
    assert!(questions.received[0].waited > Duration::ZERO);

    let asks = snapshot_of(&snapshots, "asks");
    let asked_sent = asked.sent();
    let states: Vec<(Option<u64>, OutboundState, Option<&str>)> = asks
        .outstanding
        .iter()
        .map(|request| {
            (
                Some(request.request.get()),
                request.state,
                request.reference.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            (
                asked_sent[0].request.map(|id| id.get()),
                OutboundState::Waiting,
                Some("ref")
            ),
            (
                asked_sent[1].request.map(|id| id.get()),
                OutboundState::Uncertain(ChannelOperation::Send),
                None
            ),
        ]
    );
    assert_eq!(asks.outstanding[1].item, asked_sent[1].item);
    assert_eq!(
        asks.rounds[0].round, inbox.rounds[0].round,
        "one round sent on both"
    );
    let asked_items: Vec<MessageId> = asked_sent.iter().map(|item| item.item).collect();
    assert_eq!(asks.rounds[0].items, asked_items);

    // Every item event about the inbox names an id the snapshot holds.
    let held: BTreeSet<u64> = inbox
        .unread
        .iter()
        .chain(&inbox.unacknowledged)
        .chain(&inbox.consumed)
        .chain(&inbox.acknowledged)
        .filter_map(|item| item.delivery.message().map(MessageId::get))
        .collect();
    {
        let events = tree.events.lock().expect("events");
        for event in kind(&events, "message.sent", None) {
            let data = super::children_tests::data(event);
            if data["channel"] == "inbox" && data["from"] == "backend" {
                assert!(
                    held.contains(&data["message"].as_u64().unwrap_or_default()),
                    "{data}"
                );
            }
        }
    }
    last.raise();
    *backend.refusing.lock().unwrap() = false;
    within(root.ready()).await.expect("ready");
    let (_, _, _) = tree.end().await;
}

/// A snapshot taken while the owner pushes and the holder's code receives, as fast as each can,
/// never shows an id in two places, nor an unread item at or before the consumed watermark.
#[tokio::test]
async fn a_snapshot_is_of_one_moment() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "ch = runtime.channels['inbox']\nfor _ in range(40):\n    await ch.receive()";
    let (tree, _app) = holding(
        "OUTRIG_TEST_REGISTRY_RACE_KEY",
        vec![("ROLE inbox", running(&[code]))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let _root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    let pushing = tokio::spawn(async move {
        for n in 0..40 {
            let item = Inbound::message(format!("m-{n}"), "alice", json!(n.to_string()));
            within(channel.deliver(item)).await.expect("queued");
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !pushing.is_finished() || backend.notices().len() < 40 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the receipts never came"
        );
        let snapshot = snapshot_of(&tree.session.channels(), "inbox");
        assert_disjoint(&snapshot);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    pushing.await.expect("pushed");
    let snapshot = snapshot_of(&tree.session.channels(), "inbox");
    assert_disjoint(&snapshot);
    assert!(snapshot.unread.is_empty(), "{snapshot:?}");
    let (_, _, _) = tree.end().await;
}

/// Handed over, a channel's positions are the snapshot's: the consumed and acknowledged
/// watermarks and the unread items, in order. A push afterward of an unread item, or of one
/// consumed and never acknowledged, is a repeat, and queues nothing.
#[tokio::test]
async fn positions_are_handed_over() {
    let backend = Backend::durable([], []);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let flag = Flag::new();
    let code = format!(
        "ch = runtime.channels['inbox']\nawait ch.receive()\n{}\nawait ch.receive()",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_POSITIONS_KEY",
        vec![("ROLE inbox", once_then_silent(&code))],
        |builder| builder.channel(inbox_holding(&[1, 2, 3, 4]), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    tree.seen("channel.consumed", 1).await;
    *backend.refusing.lock().unwrap() = true;
    flag.raise();
    until("the second's notice", || backend.notices().len() >= 2).await;
    within(root.ready()).await.expect("ready");
    let channel = tree.session.channel("inbox").expect("declared");
    within(channel.hand_over()).await.expect("handed over");
    let snapshot = snapshot_of(&tree.session.channels(), "inbox");
    let positions = backend.positions();
    assert_eq!(positions.len(), 1);
    let handed = &positions[0];
    assert_eq!(handed.channel, "inbox");
    assert_eq!(handed.consumed, snapshot.consumed);
    assert_eq!(handed.acknowledged, snapshot.acknowledged);
    assert_eq!(handed.unread, snapshot.unread);
    assert_eq!(
        handed.consumed.as_ref().map(|item| item.item.as_str()),
        Some("m-2")
    );
    assert_eq!(
        handed.acknowledged.as_ref().map(|item| item.item.as_str()),
        Some("m-1")
    );
    assert_eq!(ids(&handed.unread), ["m-3", "m-4"]);
    let again = |n: usize| Inbound::message(format!("m-{n}"), "alice", json!(format!("body-{n}")));
    let unread = within(channel.deliver(again(3))).await.expect("a repeat");
    assert_eq!(
        unread,
        Delivered::Repeat {
            delivery: snapshot.unread[0].delivery,
            position: Position::Unread,
        }
    );
    let consumed = within(channel.deliver(again(2))).await.expect("a repeat");
    assert!(
        matches!(
            consumed,
            Delivered::Repeat {
                position: Position::Consumed,
                ..
            }
        ),
        "{consumed:?}"
    );
    assert_eq!(
        ids(&snapshot_of(&tree.session.channels(), "inbox").unread),
        ["m-3", "m-4"]
    );
    *backend.refusing.lock().unwrap() = false;
    let (_, _, _) = tree.end().await;
}

/// A consumed notice the backend refuses is told again until it answers: the consumed
/// watermark moves as the holder's code receives the item, the acknowledged one only once the
/// backend answers, when `channel.consumed` is recorded, once.
#[tokio::test]
async fn the_watermarks_move_apart() {
    let backend = Arc::new(Backend::default());
    *backend.refusing.lock().unwrap() = true;
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_WATERMARKS_KEY",
        vec![(
            "ROLE inbox",
            from_launch(&["await runtime.channels['inbox'].receive()"]),
        )],
        |builder| builder.channel(inbox_holding(&[1]), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    until("a notice told again", || backend.notices().len() >= 2).await;
    let snapshot = snapshot_of(&tree.session.channels(), "inbox");
    assert_eq!(
        snapshot.consumed.as_ref().map(|item| item.item.as_str()),
        Some("m-1")
    );
    assert_eq!(snapshot.acknowledged, None);
    assert_eq!(ids(&snapshot.unacknowledged), ["m-1"]);
    {
        let events = tree.events.lock().expect("events");
        assert!(kind(&events, "channel.consumed", None).is_empty());
    }
    *backend.refusing.lock().unwrap() = false;
    tree.seen("channel.consumed", 1).await;
    let snapshot = snapshot_of(&tree.session.channels(), "inbox");
    assert_eq!(snapshot.acknowledged, snapshot.consumed);
    assert!(snapshot.unacknowledged.is_empty());
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(data_of(&events, "channel.consumed", &subject).len(), 1);
    // Told at least once, every time of the same item: the backend dedups by its id.
    let notices = backend.notices();
    assert!(notices.len() >= 2);
    assert!(notices.iter().all(|(_, item)| item == "m-1"), "{notices:?}");
}

/// One item through the four positions: in the backend's store and not yet pushed, the
/// registry knows nothing of it; pushed, it waits unread, `pending()` counts it, and the
/// watermark has not moved; received into a variable and not printed, the watermark has moved
/// and nothing the model was given or the execution recorded holds its body; printed, the
/// execution's output carries it.
#[tokio::test]
async fn an_item_moves_through_four_positions() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (taken, printed) = (Flag::new(), Flag::new());
    let code = format!(
        "ch = runtime.channels['inbox']\nprint(ch.pending())\n{}\nd = await ch.receive()\n{}\n\
         print(d.body)",
        taken.awaited(),
        printed.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_FOUR_POSITIONS_KEY",
        vec![("ROLE inbox", running(&[&code]))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let channel = tree.session.channel("inbox").expect("declared");
    let before = snapshot_of(&tree.session.channels(), "inbox");
    assert!(
        before.unread.is_empty() && before.consumed.is_none(),
        "{before:?}"
    );
    let item = Inbound::message("m-1", "alice", json!("the secret word"));
    within(channel.deliver(item)).await.expect("queued");
    let pushed = snapshot_of(&tree.session.channels(), "inbox");
    assert_eq!(ids(&pushed.unread), ["m-1"]);
    assert_eq!(pushed.consumed, None);
    taken.raise();
    until("the receipt", || {
        snapshot_of(&tree.session.channels(), "inbox")
            .consumed
            .is_some()
    })
    .await;
    {
        let events = tree.events.lock().expect("events");
        assert!(kind(&events, "exec.completed", Some(&subject)).is_empty());
    }
    printed.raise();
    tree.seen("exec.completed", 1).await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject), ["1\nthe secret word\n"]);
    // What reached the model is what was printed, and only after it was.
    let told: Vec<&mock_http::RecordedRequest> = requests
        .iter()
        .filter(|request| request.body.to_string().contains("the secret word"))
        .collect();
    assert!(
        told.iter()
            .all(|request| request.body.to_string().contains("1\\nthe secret word")),
        "the body reached the model only as printed"
    );
}

/// At the close, each message uncertain is asked about once in the drain: one its backend then
/// answers accepted is listed so, with its reference; one it cannot tell of is listed
/// uncertain. Neither is sent again, and neither changes the verdict: its holder was told.
#[tokio::test]
async fn the_close_asks_once_after_each_uncertain_item() {
    let stored = Backend::durable(
        [Answer::UncertainKept("the ack was lost")],
        [SendAnswer::Uncertain {
            reason: "the store is down".to_string(),
        }],
    );
    let storing: Arc<dyn ChannelBackend> = stored.clone();
    let lost = Backend::answering([Answer::Uncertain("the store timed out")]);
    let losing: Arc<dyn ChannelBackend> = lost.clone();
    let code = "import outrig\n\
                for name in ('inbox', 'feed'):\n    try:\n        await runtime.channels[name].send('x')\n    except outrig.SendUncertain as e:\n        print(name, 'uncertain')";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_CLOSE_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder
                .channel(inbox(), storing)
                .channel(ChannelDefinition::new("feed"), losing)
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox", "feed"])).await;
    let agent = agent_of(&root);
    let subject = format!("agent/{agent}");
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        ["inbox uncertain\nfeed uncertain\n"]
    );
    let (inbox, feed) = (&report.channels[0], &report.channels[1]);
    let item = stored.sent()[0].item;
    assert_eq!(inbox.uncertain.len(), 1, "{inbox:?}");
    assert_eq!(inbox.uncertain[0].item, item);
    assert_eq!(inbox.uncertain[0].agent, agent);
    assert_eq!(inbox.uncertain[0].outcome, ItemOutcome::Accepted);
    assert_eq!(inbox.uncertain[0].reference, Some(format!("ref-{item}")));
    assert_eq!(feed.uncertain.len(), 1, "{feed:?}");
    assert_eq!(feed.uncertain[0].item, lost.sent()[0].item);
    assert_eq!(feed.uncertain[0].outcome, ItemOutcome::Uncertain);
    assert_eq!(feed.uncertain[0].reference, None);
    for backend in [&stored, &lost] {
        assert_eq!(backend.sent().len(), 1, "sent once in all");
        // Once as each was sent, once in the drain.
        assert_eq!(backend.queries().len(), 2);
    }
    assert!(inbox.sends.is_empty() && feed.sends.is_empty());
    assert_eq!(data_of(&events, "channel.item.resolved", &subject).len(), 4);
}

/// The agent's own resolution of a message, still running as the session closes, answers
/// accepted after the close's query of it answered uncertain: the close's row says accepted,
/// whichever question was answered first.
#[tokio::test]
async fn an_acceptance_after_the_close_is_reported() {
    let backend = Backend::durable(
        [Answer::UncertainKept("the ack was lost")],
        [
            SendAnswer::Uncertain {
                reason: "the store is down".to_string(),
            },
            SendAnswer::Accepted {
                reference: Some("found-late".to_string()),
            },
            SendAnswer::Uncertain {
                reason: "the store is down again".to_string(),
            },
        ],
    );
    let gate = Arc::new(tokio::sync::Notify::new());
    backend
        .held
        .lock()
        .unwrap()
        .extend([None, Some(Arc::clone(&gate))]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\n\
                try:\n    await ch.send('x')\nexcept outrig.SendUncertain as e:\n    item = e.item\n\
                await ch.resolve(item)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_ACROSS_CLOSE_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    // OutRig's question as the send ended, then the agent's, which waits.
    until("the agent's resolution", || backend.queries().len() == 2).await;
    let release = async {
        // The close's question, answered uncertain, before the agent's is let answer.
        until("the close's resolution", || backend.queries().len() == 3).await;
        gate.notify_one();
    };
    let ((report, _, _), ()) = tokio::join!(tree.end(), release);
    let item = backend.sent()[0].item;
    let uncertain = &report.channels[0].uncertain;
    assert_eq!(uncertain.len(), 1, "{report:?}");
    assert_eq!(uncertain[0].item, item);
    assert_eq!(uncertain[0].agent, agent_of(&root));
    assert_eq!(uncertain[0].outcome, ItemOutcome::Accepted, "{uncertain:?}");
    assert_eq!(uncertain[0].reference.as_deref(), Some("found-late"));
    assert_eq!(backend.sent().len(), 1, "sent once");
}

/// The agent's own resolution of a message, running as admission closes, answers accepted
/// before the drain asks: the close's row says accepted, with its reference, and the drain
/// asks nothing more of it. The close lists the message in its report as it closes, and the
/// registry lists it uncertain until the answer.
#[tokio::test]
async fn an_acceptance_between_the_close_and_the_drain_is_reported() {
    let backend = Backend::durable(
        [Answer::UncertainKept("the ack was lost")],
        [
            SendAnswer::Uncertain {
                reason: "the store is down".to_string(),
            },
            SendAnswer::Accepted {
                reference: Some("found-late".to_string()),
            },
        ],
    );
    let gate = Arc::new(tokio::sync::Notify::new());
    backend
        .held
        .lock()
        .unwrap()
        .extend([None, Some(Arc::clone(&gate))]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\n\
                try:\n    await ch.send('x')\nexcept outrig.SendUncertain as e:\n    item = e.item\n\
                print((await ch.resolve(item)).outcome)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_RESOLVE_BEFORE_DRAIN_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    // OutRig's question as the send ended, then the agent's, which waits.
    until("the agent's resolution", || backend.queries().len() == 2).await;
    let item = backend.sent()[0].item;
    let still = |tree: &Tree| -> Vec<MessageId> {
        let inbox = snapshot_of(&tree.session.channels(), "inbox");
        inbox
            .uncertain
            .iter()
            .map(|uncertain| uncertain.item)
            .collect()
    };
    assert_eq!(still(&tree), [item]);
    tree.session.close_admission();
    assert_eq!(still(&tree), [item], "uncertain until answered");
    gate.notify_one();
    tree.seen("exec.completed", 1).await;
    assert!(still(&tree).is_empty(), "settled by the answer");
    let (report, events, _) = tree.end().await;
    let subject = format!("agent/{}", agent_of(&root));
    assert_eq!(exec_outputs(&events, &subject), ["accepted\n"]);
    let uncertain = &report.channels[0].uncertain;
    assert_eq!(uncertain.len(), 1, "{report:?}");
    assert_eq!(uncertain[0].item, item);
    assert_eq!(uncertain[0].outcome, ItemOutcome::Accepted, "{uncertain:?}");
    assert_eq!(uncertain[0].reference.as_deref(), Some("found-late"));
    assert_eq!(backend.queries().len(), 2, "the drain asks nothing more");
    assert_eq!(backend.sent().len(), 1, "sent once");
}

/// The shutdown begins after the interpreter has exited but before the exit's hook has told the
/// session that admission closed: the drain still asks about the message whose send is
/// uncertain, and the close's row says what its backend answered.
#[tokio::test]
async fn the_drain_asks_before_the_exit_is_heard() {
    let backend = Backend::durable(
        [Answer::UncertainKept("the ack was lost")],
        [
            SendAnswer::Uncertain {
                reason: "the store is down".to_string(),
            },
            SendAnswer::Accepted {
                reference: Some("found-late".to_string()),
            },
        ],
    );
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import os, outrig\nch = runtime.channels['inbox']\n\
                try:\n    await ch.send('x')\nexcept outrig.SendUncertain:\n    pass\n\
                os._exit(1)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_DRAIN_BEFORE_EXIT_HEARD_KEY",
        vec![("ROLE inbox", once_then_silent(code))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    // The hook that tells the session the interpreter exited, replaced by one that never does.
    tree.session.interpreter().on_exit(|| {});
    constructed(&tree, holder(&["inbox"])).await;
    within(tree.session.interpreter().gone()).await;
    let item = backend.sent()[0].item;
    let (report, _, _) = tree.end().await;
    let uncertain = &report.channels[0].uncertain;
    assert_eq!(uncertain.len(), 1, "{report:?}");
    assert_eq!(uncertain[0].item, item);
    assert_eq!(uncertain[0].outcome, ItemOutcome::Accepted, "{uncertain:?}");
    assert_eq!(uncertain[0].reference.as_deref(), Some("found-late"));
    // Once as it was sent, once in the drain.
    assert_eq!(backend.queries().len(), 2);
    assert_eq!(backend.sent().len(), 1, "sent once");
}

/// [`DeliveryId`] of request `id`, for comparing with the registry's.
fn request_delivery(id: u64) -> DeliveryId {
    DeliveryId::Request(crate::harness::event::AgentRequestId::new(id))
}

/// Pushed requests are listed unread by both ids until received.
#[tokio::test]
async fn an_unread_request_is_listed_by_both_ids() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_REGISTRY_REQUESTS_KEY",
        vec![("ROLE inbox", once_then_silent("pass"))],
        |builder| builder.channel(questions(), given),
    )
    .await;
    let root = constructed(&tree, holder(&["questions"])).await;
    within(root.ready()).await.expect("ready");
    let channel = tree.session.channel("questions").expect("declared");
    let request = ask(&channel, "q-1", "bob", json!("2+2?")).await;
    let snapshot = snapshot_of(&tree.session.channels(), "questions");
    assert_eq!(ids(&snapshot.unread), ["q-1"]);
    assert_eq!(snapshot.unread[0].delivery, request_delivery(request));
    assert!(snapshot.received.is_empty());
    let (_, _, _) = tree.end().await;
}
