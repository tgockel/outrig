//! The model-request limit end to end (`plan/phase/0003-python/model-policy.md`, "The
//! model-request limit"): a primary agent, a child an agent's code launched, an agent class
//! instance's child and a root its owner constructed, each counting the model requests a
//! provider answered against a limit of its own, stopped at it, and let go on by a reset or a
//! raise; against the routed mock provider, on the host's interpreter.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::agent_tests::{count, post, round, round_end, tool_result};
use super::children_tests::{
    THING, TOTAL, Tree, UNTIL, Watch, data, data_of, exec_outputs, kind, of_agent, opening,
    primary, primary_waits, printed, seen, tree, tree_of, tree_watched,
};
use super::instance_tests::{fizzing, indented, ready_then};
use super::mock_http::{
    self, CannedResponse, MODEL, Responder, failure, scripted, submit, text_reply,
};
use super::root_tests::{REVIEWS, agent_of, owned_watched, reviewer};
use super::runner;
use super::stop_tests::{answered, reached};
use crate::config::Config;
use crate::harness::event::Event;
use crate::harness::{
    AgentState, ApplicationSpec, Argument, ConstructSpec, ModelRequests, RootError, RosterState,
    RoundOutcome, RoundStop, SessionError,
};
use crate::python::testing::{Flag, py, within};

fn nonzero(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("non-zero")
}

/// The count and limit a model-request limit's stop names, when that stopped the round.
fn limit_stop(stopped: Option<&RoundStop>) -> Option<(u32, u32)> {
    match stopped {
        Some(RoundStop::ModelRequestLimit { spent, limit }) => Some((*spent, *limit)),
        _ => None,
    }
}

/// `field` of the data of each of `subject`'s events of `what`.
fn field_of(events: &[Arc<Event>], what: &str, subject: &str, field: &str) -> Vec<Value> {
    data_of(events, what, subject)
        .into_iter()
        .map(|data| data[field].clone())
        .collect()
}

// ---------------------------------------------------------------------------- the primary agent

/// A primary held to 3 makes three requests across two rounds and not the fourth: the second
/// round ends with the limit's stop, completed and not failed, its names intact. Asked again
/// before a reset, no round opens and the message waits; once the owner resets the count, the
/// next round opens on it, reads the reset notice once, and runs on what the first two bound.
#[tokio::test]
async fn a_primary_at_its_limit_stops_and_goes_on_once_reset() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_PRIMARY",
        "model-request-limit = 3",
        vec![(
            "",
            scripted(vec![
                submit("p1", "x = 41"),
                text_reply("one"),
                submit("p2", "y = x + 1"),
                // What the round after the reset is answered with: the fourth is never sent.
                submit("p3", "print(x, y, len(runtime.history.turns) > 0)"),
                text_reply("three"),
            ]),
        )],
    )
    .await;
    assert_eq!(round(&mut tree.session, "go").await, "one");
    let second = round_end(&mut tree.session, "again").await;
    assert_eq!(limit_stop(second.stopped.as_ref()), Some((3, 3)));
    assert_eq!(second.reply, "");
    assert_eq!(
        second.stopped.as_ref().map(ToString::to_string).as_deref(),
        Some(
            "model-request-limit (3) reached: 3 of 3 model requests made; no model request is \
             made until the count is reset or the limit raised"
        )
    );

    // Asked again at the limit: no round opens, and what was sent waits.
    post(&tree.session, "more").await;
    let RoundOutcome::Ended(refused) = within(tree.session.round()).await.expect("an outcome")
    else {
        panic!("a message waited, so the limit answered")
    };
    assert_eq!(limit_stop(refused.stopped.as_ref()), Some((3, 3)));
    assert_eq!(refused.reply, "");

    let read = tree.session.model_requests("primary").expect("its count");
    assert_eq!((read.spent, read.limit), (3, Some(3)));
    let after = tree
        .session
        .reset_model_requests("primary", None)
        .expect("reset");
    assert_eq!((after.spent, after.limit), (0, Some(3)));
    let RoundOutcome::Ended(third) = within(tree.session.round()).await.expect("an outcome") else {
        panic!("what waited opens the round")
    };
    assert_eq!((third.reply.as_str(), third.stopped), ("three", None));
    let read = tree.session.model_requests("primary").expect("its count");
    assert_eq!(read.spent, 2, "the round after the reset");

    let (_, events, requests) = tree.end().await;
    let calls = primary(&requests);
    assert_eq!(calls.len(), 5, "three, then none until the reset, then two");
    assert_eq!(printed(&requests, "p3"), "41 42 True\n", "the names stand");
    let reopened = opening(calls[3]);
    assert!(
        reopened.contains(
            "[outrig] Your model-request count was reset: 3 model requests remain of your limit \
             of 3. Your limit ended your last round before its next model request; nothing it \
             left running was stopped."
        ) && reopened.contains("waiting on runtime.channels[\"user\"]"),
        "{reopened}"
    );
    assert_eq!(count(calls[4], "count was reset"), 1, "told once");
    assert_eq!(kind(&events, "round.started", None).len(), 3);
    assert!(kind(&events, "model.round.failed", None).is_empty());
    assert_eq!(
        field_of(&events, "model.round.completed", "agent/primary", "stopped"),
        [
            Value::Null,
            json!({"kind": "model_request_limit", "spent": 3, "limit": 3}),
            Value::Null
        ]
    );
    assert_eq!(
        data_of(&events, "model.limit.reached", "agent/primary"),
        [json!({"spent": 3, "limit": 3, "round": 2, "requests": []})],
        "once, though asked again"
    );
    assert_eq!(
        data_of(&events, "model.limit.reset", "agent/primary"),
        [json!({"spent_before": 3, "limit_before": 3, "spent_after": 0, "limit_after": 3})]
    );
}

/// With limit 20 and warnings at 10 and 1 remaining: the warning naming 10 is in the result of
/// the call that left 10, after the announcement of a message that arrived as that call ran; the
/// one naming 1 opens the round after the call that left 1; each is told once, and both are in
/// the conversation the store holds.
#[tokio::test]
async fn each_warning_is_read_once_where_the_model_reads_next() {
    let flag = Flag::new();
    let mut script: Vec<CannedResponse> =
        (1..10).map(|n| submit(&format!("p{n}"), "pass")).collect();
    script.push(submit(
        "p10",
        &py(&format!(
            "import asyncio, os\nwhile not os.path.exists({}):\n    await asyncio.sleep(0.01)",
            flag.py()
        )),
    ));
    script.push(text_reply("end one"));
    script.extend((12..19).map(|n| submit(&format!("p{n}"), "pass")));
    script.push(text_reply("end two"));
    script.push(text_reply("end three"));
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_WARNINGS",
        "model-request-limit = 20\nmodel-request-warnings = [10, 1]",
        vec![("", scripted(script))],
    )
    .await;
    let user = tree.session.user_channel().expect("a user channel");
    let events = Arc::clone(&tree.events);
    post(&tree.session, "go").await;
    let poke = async {
        seen(&events, "exec.submitted", 10).await;
        within(user.send("mid")).await.expect("sent");
        flag.raise();
    };
    let (first, ()) = tokio::join!(within(tree.session.round()), poke);
    assert!(matches!(first, Ok(RoundOutcome::Ended(_))), "{first:?}");
    assert_eq!(round(&mut tree.session, "two").await, "end two");
    assert_eq!(round(&mut tree.session, "three").await, "end three");
    let read = tree.session.model_requests("primary").expect("its count");
    assert_eq!(read.spent, 20);
    assert_eq!(read.warned, [10, 1]);

    let (_, events, requests) = tree.end().await;
    let calls = primary(&requests);
    assert_eq!(calls.len(), 20);
    let result = tool_result(calls[10], "p10");
    let (told, warned) = (
        result.find("waiting on runtime.channels[\"user\"]"),
        result.find("[outrig] 10 model requests remain of your limit of 20."),
    );
    assert!(
        matches!((told, warned), (Some(told), Some(warned)) if told < warned),
        "the delivery, then the warning: {result}"
    );
    assert!(
        opening(calls[19]).contains("[outrig] 1 model request remains of your limit of 20."),
        "{}",
        opening(calls[19])
    );
    for text in [
        "10 model requests remain of your limit",
        "1 model request remains of your limit",
    ] {
        assert_eq!(count(calls[19], text), 1, "{text} told once, and kept");
    }
    let warned: Vec<(Value, Value)> = data_of(&events, "model.limit.warned", "agent/primary")
        .into_iter()
        .map(|warned| (warned["threshold"].clone(), warned["spent"].clone()))
        .collect();
    assert_eq!(warned, [(json!(10), json!(10)), (json!(1), json!(19))]);
}

/// A warning with nothing delivered beside it opens the result alone.
#[tokio::test]
async fn a_warning_alone_opens_the_result() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_WARNING_ALONE",
        "model-request-limit = 3\nmodel-request-warnings = [2]",
        vec![(
            "",
            scripted(vec![submit("p1", "print('hi')"), text_reply("done")]),
        )],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    let result = printed(&requests, "p1");
    assert!(
        result.starts_with("[outrig] 2 model requests remain of your limit of 3.")
            && result.ends_with("\nhi\n"),
        "{result}"
    );
}

/// A session with a spent token budget and an agent at its limit reports the budget's stop:
/// the budget ends every agent's calls, the limit one agent's.
#[tokio::test]
async fn a_spent_budget_is_the_stop_reported_over_the_limit() {
    // Each call reports 19 tokens: two spend 38 of 30, and two of 2.
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_UNDER_BUDGET",
        "token-budget = 30\nmodel-request-limit = 2",
        vec![(
            "",
            scripted(vec![submit("p1", "pass"), submit("p2", "pass")]),
        )],
    )
    .await;
    let end = round_end(&mut tree.session, "go").await;
    assert_eq!(
        end.stopped,
        Some(RoundStop::TokenBudget { budget: 30 }),
        "the budget, before the limit"
    );
    let read = tree.session.model_requests("primary").expect("its count");
    assert_eq!((read.spent, read.limit), (2, Some(2)));
    tree.end().await;
}

// ---------------------------------------------------------------------------- what counts

/// A provider and a chain of two, both on the mock: `retried` retries a transient failure within
/// its budget, `chain` moves from its head to its member on any failure.
fn chain_config(addr: std::net::SocketAddr, var: &str) -> Config {
    let cfg = Config::load_from_str(&format!(
        r#"
default-model = "chain"
model-request-limit = 10
model-request-warnings = [9, 8]

[providers.retrying]
style                = "messages"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 30
retry-budget-secs    = 30

[providers.once]
style                = "messages"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 30
retry-budget-secs    = 0

[models.retried]
provider   = "retrying"
identifier = "{MODEL}"

[models.head]
provider   = "once"
identifier = "{MODEL}"

[models.member]
provider   = "once"
identifier = "{MODEL}"

[models.chain]
alias = ["head", "member"]

[agents.coding]
preamble = "You write Python."
"#
    ))
    .expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// The distinct calls of `subject`'s `model.attempt` events a provider answered, and those it
/// answered none of.
fn calls_answered(events: &[Arc<Event>], subject: &str) -> (usize, usize) {
    let attempts = data_of(events, "model.attempt", subject);
    let calls = |answered: bool| -> BTreeSet<u64> {
        attempts
            .iter()
            .filter(|attempt| attempt["error"].is_null() == answered)
            .filter_map(|attempt| attempt["call_id"].as_u64())
            .collect()
    };
    let answered = calls(true);
    (answered.len(), calls(false).difference(&answered).count())
}

/// A call retried after a `503`, and one an alias moved from its head's `500` to its member,
/// each count once: one call, however many requests it sent.
#[tokio::test]
async fn a_retried_or_moved_call_counts_once() {
    const VAR: &str = "OUTRIG_TEST_LIMIT_ONCE";
    let mock = mock_http::routed(
        vec![
            (
                "ROLE retried",
                scripted(vec![
                    failure(503).header("Retry-After", "0"),
                    submit("c1", "await runtime.complete(Total(total=1))"),
                ]),
            ),
            (
                "",
                scripted(vec![
                    failure(500),
                    submit(
                        "p1",
                        &format!(
                            "{TOTAL}\n{}",
                            py(r#"
                            child = await runtime.spawn('r', prompt='ROLE retried', model='retried')
                            print((await child.submit({}, result=Total)).total)
                            "#)
                        ),
                    ),
                    text_reply("done"),
                ]),
            ),
        ],
        None,
    )
    .await;
    let cfg = chain_config(mock.0, VAR);
    let mut tree = tree_of(cfg, VAR, mock, Watch::Gathered, |builder| builder).await;
    round(&mut tree.session, "go").await;
    let primary_count = tree.session.model_requests("primary").expect("its count");
    let child_count = tree.session.model_requests("c1").expect("its count");
    assert_eq!(primary_count.spent, 2, "two calls, three requests");
    assert_eq!(child_count.spent, 1, "one call, two requests");
    let (_, events, requests) = tree.end().await;
    let result = printed(&requests, "p1");
    assert!(
        result.ends_with("\n1\n"),
        "after the warning the call left: {result}"
    );
    assert_eq!(kind(&events, "model.failover", None).len(), 1);
    assert_eq!(
        data_of(&events, "model.attempt", "agent/c1").len(),
        2,
        "retried once"
    );
    for (subject, spent) in [("agent/primary", 2), ("agent/c1", 1)] {
        assert_eq!(calls_answered(&events, subject), (spent, 0), "{subject}");
        let expected: Vec<Value> = (1..=spent).map(|n| json!(n)).collect();
        assert_eq!(
            field_of(&events, "model.limit.warned", subject, "spent"),
            expected,
            "{subject}: a warning per call answered"
        );
    }
}

/// An outage spends nothing: a root whose rounds fail on `503`s past the retry budget, three
/// times, is unavailable and holds its request open with its count unchanged, and once the
/// provider answers, counts only the calls it answered.
#[tokio::test]
async fn an_outage_spends_nothing() {
    const WAIT: Duration = Duration::from_millis(300);
    runner::FIRST_WAIT.set(Some(WAIT));
    let (tree, _app) = owned_watched(
        "OUTRIG_TEST_LIMIT_OUTAGE",
        "model-request-limit = 10",
        vec![(
            "ROLE reviewer",
            ready_then(scripted(vec![
                failure(503),
                failure(503),
                failure(503),
                submit("c1", &py(REVIEWS)),
                text_reply("reviewed"),
            ])),
        )],
        Watch::Gathered,
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    runner::FIRST_WAIT.set(None);
    let counted = |root: &crate::harness::RootHandle| root.model_requests().expect("its count");
    assert_eq!(counted(&root).spent, 1, "its orienting round's");
    let seq = root.status().seq;
    let request = root.request("review", json!("a.py")).expect("sent");
    let stop = within(root.next_stop(seq)).await;
    assert!(
        matches!(stop.state, AgentState::Unavailable { .. }),
        "{stop:?}"
    );
    assert_eq!(counted(&root).spent, 1, "the failed calls spent nothing");
    reached(&root, stop.seq, |state| {
        matches!(state, AgentState::Idle { .. })
    })
    .await;
    assert_eq!(
        answered(&request).await,
        json!({"path": "a.py", "ok": true})
    );
    assert_eq!(
        counted(&root).spent,
        3,
        "the orienting round's, and the two answered"
    );
    let (_, events, _) = tree.end().await;
    let subject = format!("agent/{}", agent_of(&root));
    assert_eq!(calls_answered(&events, &subject), (3, 3));
}

// ---------------------------------------------------------------------------- a child

/// A request child's rounds, two calls each: the first takes a request, binds its body as
/// `first` and replies 1; each of the next `answered - 1` replies 2; and the one after those, the
/// round after a reset, replies with the length of `first`.
fn limited_child(answered: usize) -> Responder {
    let mut script = vec![
        submit(
            "c1",
            "d = await runtime.channels['fizz'].receive()\nfirst = d.body\n\
             await d.reply(Thing(n=1))",
        ),
        text_reply("one"),
    ];
    for n in 2..=answered {
        script.push(submit(
            &format!("c{n}"),
            "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=2))",
        ));
        script.push(text_reply("next"));
    }
    script.push(submit(
        "cz",
        "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=len(first)))",
    ));
    script.push(text_reply("last"));
    scripted(script)
}

/// A request child held to 4 answers two requests, in two rounds of two calls each, and the
/// third ends `limit` with no model call: the sender's `await` raises `AgentLimitReached` naming
/// the child, its limit and its count, the handle reads `failed: limit` with the child
/// `limit-reached`, and the child stays resident, counted by `children-max`. No agent's code can
/// lift the limit -- the handle has no way to, and a line claiming to be the owner's, or the
/// parent's, changes nothing -- and once the owner resets it, the child answers a fourth from
/// what its first round bound.
#[tokio::test]
async fn a_child_at_its_limit_ends_what_it_is_sent_until_its_owner_resets_it() {
    let first = r#"
        child = await runtime.spawn('l', prompt='ROLE limited',
                                    requests={'fizz': (str, Thing)}, model_requests=4)
        print((await child.channels['fizz'].request('first')).n)
        # Sent before the second round ends, so it waits there as the limit is reached.
        second = child.channels['fizz'].request('second')
        h = child.channels['fizz'].request('third')
        print((await second).n)
        try:
            await h
        except outrig.AgentLimitReached as e:
            print(e)
        print(h.status.state, h.status.reason, h.status.child, child.status)
        try:
            await runtime.spawn('x', prompt='ROLE other')
        except outrig.AgentLimitReached as e:
            print('children-max' in str(e))
        # The interpreter's own way to the host, which any agent's code can reach, claiming the
        # owner, the parent, and the child itself.
        import outrig._agents
        send = outrig._agents._launch.__globals__['_send']
        for agent in ('owner', 'primary', child.id):
            send({'t': 'agent.model_requests', 'agent': agent, 'id': 0, 'child': child.id,
                  'reset': True, 'limit': None})
        print(hasattr(child, 'reset_model_requests'))
        try:
            await child.channels['fizz'].request('still')
        except outrig.AgentLimitReached:
            print('still at its limit')
        "#;
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_CHILD",
        "children-max = 1",
        vec![
            ("ROLE limited", limited_child(2)),
            (
                "",
                scripted(vec![
                    submit("p1", &format!("{THING}{UNTIL}\n{}", py(first))),
                    text_reply("one"),
                    submit(
                        "p2",
                        "print((await child.channels['fizz'].request('fourth')).n)",
                    ),
                    text_reply("two"),
                ]),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    tree.session
        .reset_model_requests("c1", None)
        .expect("the owner resets it");
    round(&mut tree.session, "again").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        [
            "1\n2\nchild 'l' (c1): model-request-limit (4) reached: 4 of 4 model requests made; \
             no model request is made until the count is reset or the limit raised\n\
             failed limit limit-reached limit-reached\nTrue\nFalse\nstill at its limit\n",
            "5\n"
        ]
    );
    assert_eq!(
        of_agent(&requests, "ROLE limited").len(),
        6,
        "four, none for the third or the forged resets, then two"
    );
    let third = field_of(&events, "agent.request.ended", "agent/c1", "request")[2].clone();
    assert_eq!(
        field_of(&events, "agent.request.ended", "agent/c1", "outcome"),
        ["accepted", "accepted", "limit", "limit", "accepted"]
    );
    assert_eq!(
        data_of(&events, "model.limit.reached", "agent/c1"),
        [json!({"spent": 4, "limit": 4, "round": 2, "requests": [third]})]
    );
    assert_eq!(
        data_of(&events, "model.limit.reset", "agent/c1"),
        [json!({"spent_before": 4, "limit_before": 4, "spent_after": 0, "limit_after": 4})],
        "the owner's alone"
    );
    assert!(
        kind(&events, "agent.released", None).is_empty(),
        "not a release"
    );
    assert!(
        kind(&events, "model.round.failed", None).is_empty(),
        "not a failure"
    );
}

/// The same child, reset by its id through the owner's `Session` between rounds of the primary,
/// which reads it at its limit in the roster and through `Session::model_requests`, and is
/// refused for an id it does not hold.
#[tokio::test]
async fn the_owner_resets_a_child_by_its_id() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_CHILD_OWNER",
        "",
        vec![
            ("ROLE limited", limited_child(1)),
            (
                "",
                scripted(vec![
                    submit(
                        "p1",
                        &format!(
                            "{THING}\n{}",
                            py(r#"
                            child = await runtime.spawn('l', prompt='ROLE limited',
                                                        requests={'fizz': (str, Thing)},
                                                        model_requests=(2, []))
                            print((await child.channels['fizz'].request('first')).n)
                            try:
                                await child.channels['fizz'].request('second')
                            except outrig.AgentLimitReached:
                                print('limit')
                            "#)
                        ),
                    ),
                    text_reply("one"),
                    submit(
                        "p2",
                        "print((await child.channels['fizz'].request('third')).n)",
                    ),
                    text_reply("two"),
                ]),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let read = tree.session.model_requests("c1").expect("its count");
    assert_eq!(
        read,
        ModelRequests {
            spent: 2,
            limit: Some(2),
            warnings: Vec::new(),
            warned: Vec::new(),
        }
    );
    let roster = tree.session.roster();
    let row = roster
        .agents
        .iter()
        .find(|row| row.id == "c1")
        .expect("listed");
    assert_eq!(
        row.state,
        Some(RosterState::Agent(AgentState::LimitReached {
            spent: 2,
            limit: 2,
            round: Some(1),
        }))
    );
    assert!(matches!(
        tree.session.reset_model_requests("c9", None),
        Err(SessionError::NoAgent(_))
    ));
    let after = tree
        .session
        .reset_model_requests("c1", Some(nonzero(3)))
        .expect("reset");
    assert_eq!((after.spent, after.limit), (0, Some(3)));
    round(&mut tree.session, "again").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1\nlimit\n");
    assert_eq!(
        printed(&requests, "p2"),
        "5\n",
        "from what its first round bound"
    );
    assert_eq!(
        field_of(&events, "model.limit.reset", "agent/c1", "limit_after"),
        [3]
    );
}

/// A child holding one request it took and one unread reaches its limit at its tool call's
/// return: its round ends at the next call, both end `limit`, each sender's `await` raises once,
/// the unread one leaves its queue, and no round follows -- not the warning round either, so
/// neither is cancelled.
#[tokio::test]
async fn a_limit_reached_mid_round_ends_what_the_child_holds() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_MID_ROUND",
        "",
        vec![
            (
                "ROLE taker",
                scripted(vec![
                    submit(
                        "t1",
                        "import asyncio\nd = await runtime.channels['fizz'].receive()\n\
                         while not runtime.channels['fizz'].pending():\n    \
                         await asyncio.sleep(0.01)",
                    ),
                    text_reply("never sent"),
                ]),
            ),
            (
                "",
                primary_waits(
                    r#"
                    c = await runtime.spawn('t', prompt='ROLE taker',
                                            requests={'fizz': (str, Thing)}, model_requests=1)
                    a = c.channels['fizz'].request('a')
                    b = c.channels['fizz'].request('b')
                    for h in (a, b):
                        try:
                            await h
                        except outrig.AgentLimitReached:
                            print(h.status.reason, h.status.child)
                    print(c.channels['fizz'].pending(), c.status)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["limit limit-reached\nlimit limit-reached\n0 limit-reached\n"]
    );
    assert_eq!(of_agent(&requests, "ROLE taker").len(), 1, "nothing after");
    assert_eq!(
        field_of(&events, "agent.request.ended", "agent/c1", "outcome"),
        ["limit", "limit"]
    );
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
    let ids = field_of(&events, "agent.request.ended", "agent/c1", "request");
    assert_eq!(
        field_of(&events, "model.limit.reached", "agent/c1", "requests"),
        [json!(ids)],
        "once, naming both"
    );
    assert_eq!(
        field_of(&events, "model.round.completed", "agent/c1", "stopped"),
        [json!({"kind": "model_request_limit", "spent": 1, "limit": 1})]
    );
}

/// Below its limit, a child whose round leaves what it holds unchanged still gets the warning
/// round, which counts one request as any round does.
#[tokio::test]
async fn the_warning_round_counts_below_the_limit() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_WARNING_ROUND",
        "model-request-limit = 10",
        vec![
            ("ROLE ignoring", scripted(vec![text_reply("not now")])),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('i', prompt='ROLE ignoring',
                                                requests={'fizz': (str, Thing)})
                    h = child.channels['fizz'].request('a body')
                    try:
                        await h
                    except outrig.RequestCancelled:
                        print('cancelled')
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let read = tree.session.model_requests("c1").expect("its count");
    assert_eq!(read.spent, 2, "its round and the warning's");
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, "agent/primary"), ["cancelled\n"]);
}

// ---------------------------------------------------------------------------- an instance

/// A session whose primary declares `Limited`, an agent class its child marked
/// `ROLE limited-instance`, constructs one held to 1, waits for `ready()`, sends it one request,
/// and waits for `ready()` again, printing what each raised; its instance's calls answered by
/// `instance`.
async fn limited_instance(var: &str, instance: Responder) -> Tree {
    tree(
        var,
        "",
        vec![
            ("ROLE limited-instance", instance),
            (
                "",
                primary_waits(&format!(
                    r#"
{class}
                    inst = Limited(model_requests=1)
                    try:
                        await inst.ready()
                        print('ready')
                    except outrig.AgentLimitReached as e:
                        print('not ready:', 'model-request-limit (1)' in str(e))
                    try:
                        await inst.fizz('x')
                    except outrig.AgentLimitReached as e:
                        print('limit:', 'model-request-limit (1)' in str(e))
                    try:
                        await inst.ready()
                        print('still ready')
                    except outrig.AgentLimitReached:
                        print('still not ready')
                    "#,
                    class = indented(&fizzing("Limited", "limited-instance"), 20),
                )),
            ),
        ],
    )
    .await
}

/// Under every request counting, an instance held to 1 spends it on its orienting round, so
/// `ready()` settles and its first request ends `limit`.
#[tokio::test]
async fn an_instance_spends_its_orienting_round() {
    let mut tree = limited_instance(
        "OUTRIG_TEST_LIMIT_ORIENTING",
        ready_then(scripted(vec![text_reply("never sent")])),
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["ready\nlimit: True\nstill ready\n"]
    );
    assert_eq!(of_agent(&requests, "ROLE limited-instance").len(), 1);
    assert!(
        kind(&events, "agent.instance.released", None)
            .into_iter()
            .all(|released| data(released)["how"] == "closing"),
        "released by the session's close, and not by the limit"
    );
}

/// An orienting round the limit stops part way -- at its second call, after a tool call -- has
/// ended, so `ready()` settles all the same.
#[tokio::test]
async fn an_orienting_round_the_limit_stops_has_ended() {
    let mut tree = limited_instance(
        "OUTRIG_TEST_LIMIT_ORIENTING_STOPPED",
        scripted(vec![
            submit("o1", "print('getting ready')"),
            text_reply("never sent"),
        ]),
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["ready\nlimit: True\nstill ready\n"]
    );
    assert_eq!(of_agent(&requests, "ROLE limited-instance").len(), 1);
    let ready = data_of(&events, "agent.instance.ready", "agent/c1");
    assert_eq!(ready[0]["error"], Value::Null);
}

/// An orienting round that cannot open -- the count, as a resume would carry it, already at the
/// limit -- makes `ready()` raise `AgentLimitReached`, and the instance's ready event names the
/// limit.
#[tokio::test]
async fn ready_raises_when_the_orienting_round_cannot_open() {
    runner::SPENT.set(Some(1));
    let mut tree = limited_instance(
        "OUTRIG_TEST_LIMIT_NOT_READY",
        scripted(vec![text_reply("never sent")]),
    )
    .await;
    round(&mut tree.session, "go").await;
    runner::SPENT.set(None);
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["not ready: True\nlimit: True\nstill not ready\n"]
    );
    assert!(of_agent(&requests, "ROLE limited-instance").is_empty());
    let ready = data_of(&events, "agent.instance.ready", "agent/c1");
    assert!(
        ready[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("model-request-limit (1)")),
        "{ready:?}"
    );
}

/// `model_requests=` takes a count, or a count and a list of counts, each at least 1, and refuses
/// anything else at the call, launching nothing.
#[tokio::test]
async fn the_keyword_is_checked_at_the_call() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_OWN",
        "",
        vec![
            (
                "ROLE limited-instance",
                ready_then(scripted(vec![text_reply("never sent")])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
{class}
                    for bad in (0, '3', (3, [0]), (3, 1), True):
                        try:
                            await runtime.spawn('x', prompt='p', model_requests=bad)
                        except (TypeError, ValueError) as e:
                            print(type(e).__name__)
                    try:
                        Limited(model_requests=0)
                    except ValueError:
                        print('constructor')
                    inst = Limited(model_requests=(5, [2]))
                    await inst.ready()
                    print('ready')
                    "#,
                    class = indented(&fizzing("Limited", "limited-instance"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let read = tree.session.model_requests("c1").expect("its count");
    assert_eq!((read.limit, read.warnings), (Some(5), vec![2]));
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["ValueError\nTypeError\nValueError\nTypeError\nTypeError\nconstructor\nready\n"]
    );
    assert_eq!(
        kind(&events, "agent.spawned", None).len(),
        1,
        "nothing refused was launched"
    );
}

// ---------------------------------------------------------------------------- a root

/// A root held to 3 spends one on its orienting round and two answering a request: its stop
/// read says it reached its limit, with the round; its roster row says so too; its count, read
/// through its handle, is what the continuation metadata carries; a further request ends
/// `RootError::Limit`; and once its owner resets it, the next is answered.
#[tokio::test]
async fn the_owner_reads_and_resets_a_root() {
    let (tree, _app) = owned_watched(
        "OUTRIG_TEST_LIMIT_ROOT",
        "",
        vec![(
            "ROLE reviewer",
            ready_then(scripted(vec![
                submit("c1", &py(REVIEWS)),
                text_reply("reviewed"),
                submit("c2", &py(REVIEWS)),
                text_reply("reviewed again"),
            ])),
        )],
        Watch::Gathered,
        |builder| builder,
    )
    .await;
    let root = within(
        tree.session
            .construct(reviewer().model_request_limit(nonzero(3))),
    )
    .await
    .expect("constructed");
    within(root.ready()).await.expect("ready");
    let seq = root.status().seq;
    let first = root.request("review", json!("a.py")).expect("sent");
    within(first.outcome()).await.expect("answered");
    let stop = reached(&root, seq, |state| {
        matches!(state, AgentState::LimitReached { .. })
    })
    .await;
    assert_eq!(
        stop.state,
        AgentState::LimitReached {
            spent: 3,
            limit: 3,
            round: Some(2),
        }
    );
    let counted = root.model_requests().expect("its count");
    assert_eq!(
        serde_json::to_value(&counted).expect("encodes"),
        json!({"spent": 3, "limit": 3, "warnings": [], "warned": []}),
        "the continuation metadata's `model_requests`"
    );
    let agent = agent_of(&root);
    assert_eq!(tree.session.model_requests(&agent), Some(counted));
    let roster = tree.session.roster();
    let row = roster
        .agents
        .iter()
        .find(|row| row.id == agent)
        .expect("listed");
    assert_eq!(row.state, Some(RosterState::Agent(stop.state.clone())));
    let refused = root.request("review", json!("b.py")).expect("sent");
    assert!(
        matches!(within(refused.outcome()).await, Err(RootError::Limit(_))),
        "ended at once"
    );
    let after = root.reset_model_requests(None).expect("reset");
    assert_eq!((after.spent, after.limit), (0, Some(3)));
    let seq = root.status().seq;
    let again = root.request("review", json!("c.py")).expect("sent");
    within(again.outcome())
        .await
        .expect("answered after the reset");
    // Once the round that answered it has made its last call.
    let rested = within(root.next_stop(seq)).await;
    assert!(
        matches!(rested.state, AgentState::Idle { .. }),
        "{rested:?}"
    );
    tree.session.close_admission();
    assert!(matches!(
        root.raise_model_request_limit(nonzero(9)),
        Err(RootError::Closed(_))
    ));
    assert_eq!(
        root.model_requests().map(|read| (read.spent, read.limit)),
        Some((2, Some(3))),
        "a raise after the close changes nothing"
    );
    let (_, events, _) = tree.end().await;
    assert_eq!(
        field_of(
            &events,
            "model.limit.reset",
            &format!("agent/{agent}"),
            "spent_after"
        ),
        [0],
        "the reset, and not the refused raise"
    );
}

/// A root whose constructor waits on `flag`, a file, before it lets the root launch.
const HELD: &str = r#"
import os, time
import outrig


class Held(outrig.Agent):
    """ROLE held. You were constructed."""

    def __init__(self, flag):
        super().__init__()
        while not os.path.exists(flag):
            time.sleep(0.01)
"#;

/// The owner can change a root's limit as soon as the roster lists it, while it is still
/// launching -- its constructor not yet returned -- and the root runs under the change: the count
/// it launches with is the one the change was made to.
#[tokio::test]
async fn a_change_made_while_a_child_launches_holds() {
    const VAR: &str = "OUTRIG_TEST_LIMIT_LAUNCHING";
    let dir = tempfile::tempdir().expect("an application directory");
    std::fs::create_dir_all(dir.path().join("held")).expect("the package's directory");
    std::fs::write(dir.path().join("held/__init__.py"), "").expect("write __init__.py");
    std::fs::write(dir.path().join("held/agents.py"), HELD).expect("write agents.py");
    let application = ApplicationSpec::directory(dir.path()).imports(["held.agents"]);
    let mock = mock_http::routed(
        vec![(
            "ROLE held",
            ready_then(scripted(vec![text_reply("never sent")])),
        )],
        None,
    )
    .await;
    let tree = tree_watched(
        VAR,
        "model-request-limit = 2",
        mock,
        Watch::Gathered,
        |builder| builder.no_primary().application(application),
    )
    .await;
    let flag = Flag::new();
    let spec = ConstructSpec::new("held.agents:Held", "sonnet").arg(
        "flag",
        Argument::data(json!(flag.path().to_str().expect("a UTF-8 path"))),
    );
    let raise = async {
        let id = within(async {
            loop {
                if let Some(row) = tree.session.roster().agents.first() {
                    return row.id.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let raised = tree
            .session
            .raise_model_request_limit(&id, nonzero(7))
            .expect("raised while it launches");
        flag.raise();
        (id, raised)
    };
    let (root, (id, raised)) = tokio::join!(within(tree.session.construct(spec)), raise);
    let root = root.expect("constructed");
    within(root.ready()).await.expect("ready");
    assert_eq!(agent_of(&root), id);
    assert_eq!(raised.limit, Some(7));
    let read = root.model_requests().expect("its count");
    assert_eq!(
        (read.spent, read.limit),
        (1, Some(7)),
        "its orienting round's, under the raised limit"
    );
    tree.end().await;
}

// ---------------------------------------------------------------------------- the close

/// At the close, a child idle at its limit holds nothing and ends as an idle child does, and the
/// primary stopped at its limit with a task its code left running ends as any running Python
/// does; a reset sent once admission has closed is refused and changes nothing.
#[tokio::test]
async fn the_close_ends_agents_at_their_limit_as_any_other() {
    let mut tree = tree(
        "OUTRIG_TEST_LIMIT_CLOSE",
        "model-request-limit = 2",
        vec![
            ("ROLE limited", limited_child(1)),
            (
                "",
                scripted(vec![
                    submit(
                        "p1",
                        &format!(
                            "{THING}\n{}",
                            py(r#"
                            import asyncio
                            child = await runtime.spawn('l', prompt='ROLE limited',
                                                        requests={'fizz': (str, Thing)})
                            print((await child.channels['fizz'].request('first')).n)
                            sleeper = asyncio.get_running_loop().create_task(asyncio.sleep(3600))
                            "#)
                        ),
                    ),
                    submit("p2", "pass"),
                ]),
            ),
        ],
    )
    .await;
    let end = round_end(&mut tree.session, "go").await;
    assert_eq!(limit_stop(end.stopped.as_ref()), Some((2, 2)));
    assert!(!end.still_running.is_empty(), "the task runs on");
    tree.session.close_admission();
    assert!(matches!(
        tree.session.reset_model_requests("c1", None),
        Err(SessionError::Closed(_))
    ));
    assert!(matches!(
        tree.session
            .raise_model_request_limit("primary", nonzero(5)),
        Err(SessionError::Closed(_))
    ));
    for agent in ["primary", "c1"] {
        let read = tree.session.model_requests(agent).expect("its count");
        assert_eq!(
            (read.spent, read.limit),
            (2, Some(2)),
            "{agent} is unchanged"
        );
    }
    let (report, events, _) = tree.end().await;
    assert!(report.requests.is_empty(), "{report:?}");
    assert!(
        kind(&events, "model.limit.reset", None).is_empty(),
        "nothing changed"
    );
}
