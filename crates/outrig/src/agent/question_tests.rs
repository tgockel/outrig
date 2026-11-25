//! Typed questions reuse outbound request settlement and cancellation.
use super::children_tests::{Watch, data_of, exec_outputs, tree_watched};
use super::external_request_tests::once_then_silent;
use super::external_tests::{constructed, holder, holding};
use super::mock_http;
use crate::harness::{QUESTION_CHANNEL, QuestionBackend, QuestionNotice, Settle, SettleError};
use crate::python::testing::within;
use serde_json::json;

#[tokio::test]
async fn questions_settle_independently_and_invalid_answers_stay_open() {
    let (backend, mut notices) = QuestionBackend::new();
    let source = r#"
import outrig
qs = [outrig.Question('free', context=outrig.QuestionContext(description='longer background', references=[outrig.QuestionReference('src/parser.rs', line=12, end_line=18, description='existing parser')])), outrig.Question('single', choices=['a', 'b'], other=False),
      outrig.Question('multi', choices=['a', 'b'], multiple=True)]
hs = [runtime.ask(q) for q in qs]
print('open', len(hs))
for h in hs:
    a = await h
    print(a.choices, a.text, a.by)
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTIONS_KEY",
        vec![("ROLE inbox", once_then_silent(source))],
        |b| b.question_backend(backend),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let Some(QuestionNotice::Asked(item)) = within(notices.recv()).await else {
            panic!("not asked")
        };
        ids.push(item.request.unwrap());
        if ids.len() == 1 {
            assert_eq!(item.body["context"]["description"], "longer background");
            assert_eq!(
                item.body["context"]["references"][0]["path"],
                "src/parser.rs"
            );
            assert_eq!(item.body["context"]["references"][0]["line"], 12);
        }
        assert_eq!(item.sender, root.agent().unwrap());
    }
    let ch = tree.session.channel(QUESTION_CHANNEL).unwrap();
    for answer in [
        json!({"choices":["missing"],"text":null,"by":"user","chain":[]}),
        json!({"choices":["a","b"],"text":null,"by":"user","chain":[]}),
        json!({"choices":[],"text":"no","by":"user","chain":[]}),
    ] {
        assert!(matches!(
            within(ch.settle(ids[1], Settle::Reply(answer))).await,
            Err(SettleError::InvalidAnswer(_))
        ));
    }
    for (n, choices, text) in [
        (2, vec!["b", "a"], Some("also")),
        (1, vec!["b"], None),
        (0, vec![], Some("hello")),
    ] {
        within(ch.settle(
            ids[n],
            Settle::Reply(json!({"choices":choices,"text":text,"by":"user","chain":[]})),
        ))
        .await
        .unwrap();
    }
    within(root.ready()).await.unwrap();
    assert!(matches!(
        within(ch.settle(ids[0], Settle::Fail("late".into()))).await,
        Err(SettleError::Ended(_))
    ));
    let subject = format!("agent/{}", root.agent().unwrap());
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject).concat(),
        "open 3\n[] hello user\n['b'] None user\n['a', 'b'] also user\n"
    );
    assert_eq!(data_of(&events, "agent.question.asked", &subject).len(), 3);
    assert_eq!(
        data_of(&events, "agent.question.answered", &subject).len(),
        3
    );
    assert_eq!(
        data_of(&events, "agent.question.refused", &subject).len(),
        4
    );
}

#[tokio::test]
async fn a_primary_question_without_a_backend_fails_clearly() {
    let mock = mock_http::routed(
        vec![(
            "",
            super::children_tests::primary_runs(
                r#"
import outrig
try:
    await runtime.ask(outrig.Question('hello?'))
except Exception as e:
    print(str(e))
"#,
            ),
        )],
        None,
    )
    .await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_QUESTION_NO_BACKEND_KEY",
        "",
        mock,
        Watch::Gathered,
        |b| b,
    )
    .await;
    within(tree.session.user_channel().unwrap().send("go"))
        .await
        .unwrap();
    within(tree.session.round()).await.unwrap();
    let (_, events, _) = tree.end().await;
    assert!(
        exec_outputs(&events, "agent/primary")
            .concat()
            .contains("this application does not take questions")
    );
}

#[tokio::test]
async fn question_limits_declines_cancellation_and_close_are_distinct() {
    let (backend, mut notices) = QuestionBackend::new();
    let source = r#"
import outrig
for q in [outrig.Question('context too large', context=outrig.QuestionContext(description='x' * 65537)), outrig.Question('bad reference', context=outrig.QuestionContext(references=[outrig.QuestionReference('src/a.rs', line=2, end_line=1)])), outrig.Question('x' * 65537), outrig.Question('x', choices=['a'] * 33), outrig.Question('x', choices=['a', 'a'])]:
    try: runtime.ask(q)
    except (ValueError, TypeError) as e: print('invalid', str(e))
hs = [runtime.ask(outrig.Question(str(i))) for i in range(32)]
try: runtime.ask(outrig.Question('too many'))
except outrig.AgentLimitReached: print('open limit')
for h in hs[:2]:
    try: await h
    except Exception as e: print(type(e).__name__, str(e))
hs[2].cancel()
try: await hs[2]
except outrig.RequestCancelled: print('cancelled')
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_LIMITS_KEY",
        vec![("ROLE inbox", once_then_silent(source))],
        |b| b.question_backend(backend),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    let mut ids = Vec::new();
    for _ in 0..32 {
        if let Some(QuestionNotice::Asked(item)) = within(notices.recv()).await {
            ids.push(item.request.unwrap());
        } else {
            panic!("missing question");
        }
    }
    let ch = tree.session.channel(QUESTION_CHANNEL).unwrap();
    within(ch.settle(ids[0], Settle::Fail("not relevant".into())))
        .await
        .unwrap();
    within(ch.settle(ids[1], Settle::Fail("not now".into())))
        .await
        .unwrap();
    let Some(QuestionNotice::Withdrawn(cancelled)) = within(notices.recv()).await else {
        panic!("no withdrawal");
    };
    assert_eq!(cancelled.request, ids[2]);
    within(root.ready()).await.unwrap();
    assert!(matches!(
        within(ch.settle(
            ids[2],
            Settle::Reply(json!({"choices":[],"text":"late","by":"user","chain":[]}))
        ))
        .await,
        Err(SettleError::Ended(_))
    ));
    let subject = format!("agent/{}", root.agent().unwrap());
    let (_, events, _) = tree.end().await;
    let printed = exec_outputs(&events, &subject).concat();
    assert!(printed.contains("size limit 65536"), "{printed}");
    assert!(printed.contains("choices limit 32"), "{printed}");
    assert!(printed.contains("duplicate choice label"), "{printed}");
    assert!(printed.contains("open limit"), "{printed}");
    assert!(
        printed.contains("AgentRequestFailed not relevant"),
        "{printed}"
    );
    assert!(printed.contains("cancelled"), "{printed}");
    let ends = data_of(&events, "agent.question.cancelled", &subject);
    assert_eq!(ends.len(), 30);
    assert_eq!(ends.iter().filter(|e| e["reason"] == "closed").count(), 29);
}

#[tokio::test]
async fn a_settled_primary_question_opens_a_round_without_exposing_its_answer() {
    let (backend, mut notices) = QuestionBackend::new();
    let mock = mock_http::routed(vec![("", super::children_tests::serving_each(vec![
        "import outrig\nawait runtime.channels['user'].receive()\nh = runtime.ask(outrig.Question('decision'))".into(),
        "print('observed', (await h).text)".into(),
    ]))], None).await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_QUESTION_WAKE_KEY",
        "",
        mock,
        Watch::Gathered,
        |b| b.question_backend(backend),
    )
    .await;
    within(tree.session.user_channel().unwrap().send("go"))
        .await
        .unwrap();
    within(tree.session.round()).await.unwrap();
    let Some(QuestionNotice::Asked(item)) = within(notices.recv()).await else {
        panic!("missing question");
    };
    let ch = tree.session.channel(QUESTION_CHANNEL).unwrap();
    within(ch.settle(
        item.request.unwrap(),
        Settle::Reply(json!({"choices":[],"text":"secret decision","by":"user","chain":[]})),
    ))
    .await
    .unwrap();
    within(tree.session.round()).await.unwrap();
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary").concat(),
        "observed secret decision\n"
    );
    let before_read = &requests[2].body.to_string();
    assert!(
        before_read.contains("question handle(s) settled"),
        "{before_read}"
    );
    assert!(!before_read.contains("secret decision"), "{before_read}");
}

#[tokio::test]
async fn closing_the_owner_question_route_reaches_each_root_endpoint() {
    let (backend, mut notices) = QuestionBackend::new();
    let source = r#"
import outrig
h = runtime.ask(outrig.Question('stay open'))
try: await h
except outrig.RequestCancelled: print('closed')
try: runtime.ask(outrig.Question('again'))
except outrig.ChannelClosed: print('no new questions')
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_CLOSE_ROUTE_KEY",
        vec![("ROLE inbox", super::children_tests::serving(source))],
        |b| b.question_backend(backend),
    )
    .await;
    let first = constructed(&tree, holder(&[])).await;
    let second = constructed(&tree, holder(&[])).await;
    for _ in 0..2 {
        assert!(matches!(
            within(notices.recv()).await,
            Some(QuestionNotice::Asked(_))
        ));
    }
    tree.session
        .channel(QUESTION_CHANNEL)
        .unwrap()
        .close("owner gone");
    within(first.ready()).await.unwrap();
    within(second.ready()).await.unwrap();
    let subjects = [
        first.agent().unwrap().to_string(),
        second.agent().unwrap().to_string(),
    ];
    let (_, events, _) = tree.end().await;
    for agent in subjects {
        assert_eq!(
            exec_outputs(&events, &format!("agent/{agent}")).concat(),
            "closed\nno new questions\n"
        );
    }
}

#[tokio::test]
async fn question_handles_wait_pending_and_settled_without_consuming_answers() {
    let (backend, mut notices) = QuestionBackend::new();
    let source = r#"
import outrig
ch = runtime.channels['__questions']
h = runtime.ask(outrig.Question('one'))
k = ch.request(outrig.Question('two'))
w = ch.request(outrig.Question('wrapped'))
wrapped = asyncio.ensure_future(w)
e = runtime.ask(outrig.Question('declined'))
done, pending = await runtime.wait([h, k, wrapped], timeout=0)
assert done == set() and pending == {h, k, wrapped}
done, pending = await runtime.wait([h, k, wrapped])
assert done == {h, k, wrapped} and not pending
other = asyncio.get_running_loop().create_future()
done, pending = await asyncio.wait([e.future, other], return_when=asyncio.FIRST_EXCEPTION)
assert done == {e.future} and pending == {other}
assert ch._counts()['pending'] == 3  # Only the task has read an answer.
done, pending = await runtime.wait([e], return_when=asyncio.FIRST_EXCEPTION)
assert done == {e} and not pending
assert ch._counts()['pending'] == 3
done, pending = await runtime.wait([h, k, e], timeout=0)
assert done == {h, k, e} and not pending
assert ch._counts()['pending'] == 3
assert h.future.result().text == 'yes'
assert (await k.future).text == 'yes'
assert str(e.future.exception()) == 'declined'
assert ch._counts()['pending'] == 1
try: e.future.result()
except outrig.AgentRequestFailed as error: assert str(error) == 'declined'
else: raise AssertionError('decline was not preserved')
assert ch._counts()['pending'] == 0
other.cancel()
print('wait and reads correct')
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_HANDLE_WAIT_KEY",
        vec![("ROLE inbox", once_then_silent(source))],
        |b| b.question_backend(backend),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let Some(QuestionNotice::Asked(item)) = within(notices.recv()).await else {
            panic!("not asked")
        };
        ids.push(item.request.unwrap());
    }
    // Exercise the delayed wait description, including ensure_future's wrapper.
    tree.seen("exec.wait.started", 1).await;
    let channel = tree.session.channel(QUESTION_CHANNEL).unwrap();
    for id in &ids[..3] {
        within(channel.settle(
            *id,
            Settle::Reply(json!({"choices":[],"text":"yes","by":"user","chain":[]})),
        ))
        .await
        .unwrap();
    }
    within(channel.settle(ids[3], Settle::Fail("declined".into())))
        .await
        .unwrap();
    within(root.ready()).await.unwrap();
    let subject = format!("agent/{}", root.agent().unwrap());
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject).concat(),
        "wait and reads correct\n"
    );
    let waits = data_of(&events, "exec.wait.started", &subject);
    assert!(
        waits.iter().any(|w| w["on"]
            .as_array()
            .is_some_and(|on| on.len() == 3 && on.iter().all(|v| v["kind"] == "request"))),
        "{waits:?}"
    );
}

#[tokio::test]
async fn a_question_asker_can_recover_an_uncertain_send() {
    use super::external_tests::{Answer, Backend};
    use crate::harness::{ChannelOperation, SendAnswer};
    let backend = Backend::durable(
        [Answer::UncertainKept("lost ack")],
        [SendAnswer::Uncertain {
            reason: "offline".into(),
        }],
    );
    let given = backend.clone();
    let source = r#"
import outrig
ch = runtime.channels['__questions']
h = runtime.ask(outrig.Question('recover'))
while h.status.state == 'sending': await asyncio.sleep(.01)
assert h.status.state == 'uncertain'
r = await ch.resolve(h.id)
assert r.outcome == 'accepted' and h.status.state == 'waiting'
print('recovered', (await h).text)
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_RECOVER_SEND_KEY",
        vec![("ROLE inbox", once_then_silent(source))],
        |b| b.question_backend(given),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    crate::python::testing::until("the explicit recovery", || backend.queries().len() == 2).await;
    assert!(
        backend
            .queries()
            .iter()
            .all(|(op, _)| *op == ChannelOperation::Send)
    );
    let id = backend.sent()[0].request.unwrap();
    within(tree.session.channel(QUESTION_CHANNEL).unwrap().settle(
        id,
        Settle::Reply(json!({"choices":[],"text":"yes","by":"user","chain":[]})),
    ))
    .await
    .unwrap();
    within(root.ready()).await.unwrap();
    let subject = format!("agent/{}", root.agent().unwrap());
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject).concat(), "recovered yes\n");
}

#[tokio::test]
async fn withdrawal_and_recovery_check_question_answers_before_ending_them() {
    use super::external_tests::Backend;
    use crate::harness::WithdrawAnswer;
    let answer = |choices: &[&str], text: Option<&str>| {
        WithdrawAnswer::Replied(
            json!({"choices":choices,"text":text,"by":"application","chain":[]}),
        )
    };
    let backend = Backend::durable([], []);
    backend.verdicts.lock().unwrap().extend([
        Some(answer(&["missing"], None)),
        Some(answer(&["a", "b"], None)),
        Some(answer(&["b", "a"], None)),
        Some(answer(&["missing"], None)),
    ]);
    backend.withdrawal_resolutions.lock().unwrap().extend([
        Some(answer(&["a"], None)),
        Some(answer(&[], Some("forbidden"))),
        Some(answer(&["b"], None)),
        Some(answer(&["b", "a"], None)),
    ]);
    let given = backend.clone();
    let source = r#"
import outrig
ch = runtime.channels['__questions']
for i in range(4):
    h = runtime.ask(outrig.Question(str(i), choices=['a', 'b'], multiple=i >= 2, other=False))
    while h.status.state == 'sending': await asyncio.sleep(.01)
    h.cancel()
    while not h.done() and h.status.state != 'uncertain': await asyncio.sleep(.01)
    if i != 2:
        assert not h.done() and len(ch._open) == 1
        r = await ch.resolve(h.id, 'withdrawal')
        if i == 1:
            assert r.outcome == 'uncertain' and not h.done() and len(ch._open) == 1
            print('recovery refused')
            r = await ch.resolve(h.id, 'withdrawal')
        assert r.outcome == 'replied'
    print(i, (await h).choices)
"#;
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_WITHDRAW_VALIDATE_KEY",
        vec![("ROLE inbox", once_then_silent(source))],
        |b| b.question_backend(given),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    within(root.ready()).await.unwrap();
    let subject = format!("agent/{}", root.agent().unwrap());
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject).concat(),
        "0 ['a']\nrecovery refused\n1 ['b']\n2 ['a', 'b']\n3 ['a', 'b']\n"
    );
    assert_eq!(
        data_of(&events, "agent.question.refused", &subject).len(),
        4
    );
    assert_eq!(
        data_of(&events, "agent.question.answered", &subject).len(),
        4
    );
    assert_eq!(backend.queries().len(), 4);
}

#[tokio::test]
async fn question_recovery_authorizes_the_asker_not_every_question_holder() {
    use super::external_tests::{Answer, Backend};
    use crate::harness::SendAnswer;
    let backend = Backend::durable(
        [Answer::UncertainKept("lost")],
        [SendAnswer::Uncertain {
            reason: "offline".into(),
        }],
    );
    let given = backend.clone();
    let dir = tempfile::tempdir().unwrap();
    let ticket = dir.path().join("ticket");
    let go = dir.path().join("go");
    let source = format!(
        r#"
import os, outrig
ch = runtime.channels['__questions']
if os.path.exists({ticket:?}):
    ident = int(open({ticket:?}).read())
    try: await ch.resolve(ident)
    except ValueError as e:
        assert 'was never made there' in str(e)
        print('foreign refused')
    else: raise AssertionError('another asker resolved this request')
else:
    h = runtime.ask(outrig.Question('recover'))
    while h.status.state == 'sending': await asyncio.sleep(.01)
    open({ticket:?}, 'w').write(str(h.id))
    while not os.path.exists({go:?}): await asyncio.sleep(.01)
    assert (await ch.resolve(h.id)).outcome == 'accepted'
    print((await h).text)
"#,
        ticket = ticket.to_str().unwrap(),
        go = go.to_str().unwrap()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_RECOVER_OWNER_KEY",
        vec![("ROLE inbox", super::children_tests::serving(&source))],
        |b| b.question_backend(given),
    )
    .await;
    let first = constructed(&tree, holder(&[])).await;
    crate::python::testing::until("ticket", || ticket.exists()).await;
    let second = constructed(&tree, holder(&[])).await;
    within(second.ready()).await.unwrap();
    assert_eq!(
        backend.queries().len(),
        1,
        "foreign holder never reached the backend"
    );
    std::fs::write(&go, "").unwrap();
    crate::python::testing::until("owner recovery", || backend.queries().len() == 2).await;
    let id = backend.sent()[0].request.unwrap();
    within(tree.session.channel(QUESTION_CHANNEL).unwrap().settle(
        id,
        Settle::Reply(json!({"choices":[],"text":"yes","by":"user","chain":[]})),
    ))
    .await
    .unwrap();
    within(first.ready()).await.unwrap();
    let foreign = format!("agent/{}", second.agent().unwrap());
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &foreign).concat(),
        "foreign refused\n"
    );
}

#[tokio::test]
async fn a_stored_backend_reply_after_question_close_only_acknowledges_cleanup() {
    use super::external_tests::Backend;
    use crate::harness::{Verdict, WithdrawAnswer};
    let backend = Backend::durable([], []);
    backend
        .verdicts
        .lock()
        .unwrap()
        .push_back(Some(WithdrawAnswer::Replied(
            json!({"choices":[],"text":"stored reply","by":"application","chain":[]}),
        )));
    let given = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_QUESTION_CLOSED_CLEANUP_KEY",
        vec![(
            "ROLE inbox",
            once_then_silent("import outrig\nh = runtime.ask(outrig.Question('pending'))"),
        )],
        |b| b.question_backend(given),
    )
    .await;
    let root = constructed(&tree, holder(&[])).await;
    within(root.ready()).await.unwrap();
    crate::python::testing::until("backend took question", || !backend.sent().is_empty()).await;
    let subject = format!("agent/{}", root.agent().unwrap());
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert!(data_of(&events, "agent.question.answered", &subject).is_empty());
    let cancelled = data_of(&events, "agent.question.cancelled", &subject);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0]["reason"], "closed");
}
