//! Child agents end to end: a primary agent's code launches children and gives
//! them work, each child's rounds run against a mock provider, and the whole
//! session runs on the host's interpreter.
//!
//! The mock answers each agent from a script of its own, chosen by a marker
//! the agent's system prompt carries: a child's is in the prompt its parent
//! gave it, and the primary's route takes every other request. What an agent
//! printed is read from the tool result its next call carried; what the
//! session did, from its events once it has shut down.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use super::agent_tests::{
    keyed, messages, post, round, round_end, shut_down, text_of, tool_result, watched,
};
use super::children::RELEASE_WAIT;
use super::mock_http::{
    self, CannedResponse, Gauge, MODEL, RecordedRequest, Responder, Style, scripted, submit,
    system_text, text_reply,
};
use super::runner::ROUND_FAILURES_MAX;
use crate::config::Config;
use crate::harness::event::{Event, Received};
use crate::harness::{Session, SessionBuilder, ShutdownReport};
use crate::python::testing::{Flag, py};

/// A config whose `sonnet` model is the mock at `addr`, a `fast` alias of it,
/// and `top` among the top-level keys.
pub(super) fn config(addr: std::net::SocketAddr, var: &str, top: &str) -> Config {
    let cfg = Config::load_from_str(&format!(
        r#"
default-model = "sonnet"
{top}

[providers.claude]
style                = "messages"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 30
retry-budget-secs    = 0

[models.sonnet]
provider   = "claude"
identifier = "{MODEL}"

[models.fast]
alias = ["sonnet"]

[agents.coding]
preamble = "You write Python."
"#
    ))
    .expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// A session of the `coding` agent, `top` among its config's top-level keys,
/// every model call answered by `routes` -- the last of them the primary's --
/// and the events it publishes.
pub(super) struct Tree {
    pub(super) session: Session,
    pub(super) requests: UnboundedReceiver<RecordedRequest>,
    pub(super) gauge: Arc<Gauge>,
    /// Every event so far, gathered while the session runs, so a busy one
    /// misses none; read by `seen` for a test that waits on one.
    pub(super) events: Arc<std::sync::Mutex<Vec<Arc<Event>>>>,
    /// The task gathering them, which ends with the subscription.
    gathering: JoinHandle<()>,
}

pub(super) async fn tree(var: &str, top: &str, routes: Vec<(&'static str, Responder)>) -> Tree {
    tree_held(var, top, routes, None).await
}

pub(super) async fn tree_held(
    var: &str,
    top: &str,
    routes: Vec<(&'static str, Responder)>,
    hold: Option<usize>,
) -> Tree {
    tree_built(var, top, routes, hold, |builder| builder).await
}

/// [`tree_held`], its session built further by `build`: given skill sources,
/// say, or the places its bindings install to.
pub(super) async fn tree_built(
    var: &str,
    top: &str,
    routes: Vec<(&'static str, Responder)>,
    hold: Option<usize>,
    build: impl FnOnce(SessionBuilder) -> SessionBuilder,
) -> Tree {
    tree_over(var, top, mock_http::routed(routes, hold).await, build).await
}

/// [`tree_built`] over a mock already started.
pub(super) async fn tree_over(
    var: &str,
    top: &str,
    (addr, requests, gauge): (
        std::net::SocketAddr,
        UnboundedReceiver<RecordedRequest>,
        Arc<Gauge>,
    ),
    build: impl FnOnce(SessionBuilder) -> SessionBuilder,
) -> Tree {
    let cfg = config(addr, var, top);
    let (events_out, mut subscription) = watched(None).await;
    let builder = SessionBuilder::new(cfg, Some("coding"), None)
        .events(events_out)
        .secrets(keyed(var));
    let session = build(builder)
        .start_on_host()
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let gathering = tokio::spawn(async move {
        while let Some(received) = subscription.recv().await {
            match received {
                Received::Event(event) => seen.lock().expect("events").push(event),
                Received::Missed(missed) => panic!("missed {missed} events"),
            }
        }
    });
    Tree {
        session,
        requests,
        gauge,
        events,
        gathering,
    }
}

impl Tree {
    /// Shut the session down, and return its report and every event it
    /// published.
    pub(super) async fn end(mut self) -> (ShutdownReport, Vec<Arc<Event>>, Vec<RecordedRequest>) {
        let report = shut_down(self.session).await;
        self.gathering.await.expect("the events were gathered");
        let events = std::mem::take(&mut *self.events.lock().expect("events"));
        (report, events, mock_http::drain(&mut self.requests))
    }

    /// Wait until `count` events of `kind` have been published, failing
    /// rather than hanging: for work a round left running, which the test
    /// has no other way to wait on.
    pub(super) async fn seen(&self, kind: &str, count: usize) {
        let events = Arc::clone(&self.events);
        tokio::time::timeout(Duration::from_secs(20), async move {
            loop {
                let so_far = events
                    .lock()
                    .expect("events")
                    .iter()
                    .filter(|event| event.kind() == kind)
                    .count();
                if so_far >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} {kind} events in time"));
    }
}

/// Every request `marker`'s agent made, in order.
pub(super) fn of_agent<'a>(
    requests: &'a [RecordedRequest],
    marker: &str,
) -> Vec<&'a RecordedRequest> {
    requests
        .iter()
        .filter(|request| system_text(&request.body).contains(marker))
        .collect()
}

/// Every request the primary made: those carrying no child's marker.
pub(super) fn primary(requests: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    requests
        .iter()
        .filter(|request| !system_text(&request.body).contains("ROLE "))
        .collect()
}

/// What the primary's call `id` printed, read from the call after it.
pub(super) fn printed(requests: &[RecordedRequest], id: &str) -> String {
    let primary = primary(requests);
    let after = primary
        .iter()
        .find(|request| {
            request
                .body
                .to_string()
                .contains(&format!("\"tool_use_id\":\"{id}\""))
        })
        .unwrap_or_else(|| panic!("no request carried {id}'s result"));
    tool_result(after, id)
}

/// The events of `kind`, whose subject is `subject` when one is given.
pub(super) fn kind<'a>(
    events: &'a [Arc<Event>],
    kind: &str,
    subject: Option<&str>,
) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.kind() == kind)
        .filter(|event| {
            subject
                .is_none_or(|subject| event.subject.as_ref().map(|s| s.as_str()) == Some(subject))
        })
        .map(Arc::as_ref)
        .collect()
}

/// The `data` an event carries.
pub(super) fn data(event: &Event) -> Value {
    serde_json::to_value(&event.payload).expect("encodes")
}

/// The primary's one call: run `source`, then say `done`.
pub(super) fn primary_runs(source: &str) -> Responder {
    scripted(vec![submit("p1", &py(source)), text_reply("done")])
}

/// A child's round: run `source`, then end the round.
pub(super) fn child_runs(id: &str, source: &str) -> Vec<CannedResponse> {
    vec![submit(id, &py(source)), text_reply("ended")]
}

pub(super) const TOTAL: &str = r#"
from dataclasses import dataclass
@dataclass
class Total:
    total: int
"#;

// ---------------------------------------------------------------------------- work children

/// A session asks its model's provider for its window once, however many
/// agents run on the model, and a child -- here through an alias of it -- is
/// held to what was reported.
#[tokio::test]
async fn a_child_is_held_to_the_window_its_sessions_provider_reported() {
    let mock = mock_http::routed_with(
        vec![
            (
                "ROLE reader",
                scripted(child_runs("c1", "await runtime.complete('ok')")),
            ),
            (
                "",
                primary_runs(
                    "child = await runtime.spawn('r', prompt='ROLE reader', model='fast')\n\
                     print(await child.submit({}, result=str), end='')",
                ),
            ),
        ],
        None,
        Some(Style::Messages.window(MODEL, Some(1_000_000))),
    )
    .await;
    let mut tree = tree_over("OUTRIG_TEST_CHILD_WINDOW", "", mock, |builder| builder).await;
    assert_eq!(round(&mut tree.session, "go").await, "done");
    let (_report, events, requests) = tree.end().await;

    let asked: Vec<_> = requests.iter().filter(|r| r.method == "GET").collect();
    assert_eq!(
        asked.len(),
        1,
        "the session asked once, and the child on the same model did not ask again"
    );
    for agent in ["agent/primary", "agent/c1"] {
        let calls = kind(&events, "model.call", Some(agent));
        assert!(!calls.is_empty(), "{agent} made a call");
        for call in calls {
            let budget = &data(call)["budget"];
            assert_eq!(
                (budget["window"].as_u64(), budget["window_source"].as_str()),
                (Some(1_000_000), Some("provider")),
                "{agent}"
            );
        }
    }
}

/// The whole of a work child: the parent spawns it, submits inputs bound in
/// the child under their names, and awaits a result the child completes with
/// an instance of the result type, bound there under its name; the handle
/// gives back an instance of the parent's class. Its status reads accepted,
/// and the record holds the spawn, the request's steps and the release.
#[tokio::test]
async fn a_parent_awaits_its_work_childs_typed_result() {
    let tree = tree(
        "OUTRIG_TEST_CHILD_BASIC",
        "",
        vec![
            (
                "ROLE adder",
                scripted(child_runs(
                    "c1",
                    "print(xs)\nawait runtime.complete(Total(total=sum(xs)))",
                )),
            ),
            (
                "",
                primary_runs(&format!(
                    "{TOTAL}\n\
                     child = await runtime.spawn('adder', prompt='ROLE adder: add them up')\n\
                     h = child.submit({{'xs': [1, 2, 3]}}, result=Total)\n\
                     r = await h\n\
                     settled = h.status\n\
                     print(type(r) is Total, r.total, h.status.state, h.done(), h.result() is r)\n\
                     await runtime.release([child])\n\
                     print(child.status, h.status == settled, h.status)"
                )),
            ),
        ],
    )
    .await;
    let mut tree = tree;
    assert_eq!(round(&mut tree.session, "go").await, "done");
    let (report, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "True 6 accepted True True\nreleased True accepted (child running)\n",
        "a status is fixed once its request ends"
    );
    let child_opening = &of_agent(&requests, "ROLE adder")[0];
    let opening = child_opening.body["messages"][0].to_string();
    assert!(
        opening.contains("submission") && opening.contains("`xs` (list[int]): [1, 2, 3]"),
        "{opening}"
    );
    assert!(
        !system_text(&child_opening.body).contains("channels[\"user\"]"),
        "a child is not told of a user channel"
    );

    let spawned = kind(&events, "agent.spawned", Some("agent/c1"));
    assert_eq!(spawned.len(), 1);
    assert_eq!(data(spawned[0])["parent"], "agent/primary");
    assert_eq!(data(spawned[0])["kind"], "work");
    for step in ["sent", "received", "replied", "settled"] {
        assert_eq!(
            kind(&events, &format!("agent.request.{step}"), Some("agent/c1")).len(),
            1,
            "{step}"
        );
    }
    let settled = data(kind(&events, "agent.request.settled", None)[0]);
    assert_eq!(settled["outcome"], "accepted");
    assert_eq!(settled["rounds"], serde_json::json!([1]));
    let released = data(kind(&events, "agent.released", Some("agent/primary"))[0]);
    assert_eq!(released["released"], serde_json::json!(["c1"]));
    assert!(
        kind(&events, "exec.submitted", Some("agent/c1")).len() == 1,
        "the child's code is evented as its own"
    );
    assert!(report.requests.is_empty(), "nothing was open at the close");
}

/// A repair redoes only the answer: a child that runs two executions,
/// completes with the wrong shape, and completes again after the problems
/// shows each execution once in the record, and no execution runs twice.
#[tokio::test]
async fn a_repair_runs_no_execution_twice() {
    let child = vec![
        submit("c1", "n = sum(xs)"),
        submit(
            "c2",
            &py(
                "try:\n    await runtime.complete({'total': 'six'})\nexcept ValueError as e:\n    print(e)",
            ),
        ),
        submit("c3", "await runtime.complete({'total': n})"),
        text_reply("ended"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_REPAIR",
        "",
        vec![
            ("ROLE repairer", scripted(child)),
            (
                "",
                primary_runs(&format!(
                    "{TOTAL}\n\
                     child = await runtime.spawn('r', prompt='ROLE repairer')\n\
                     print((await child.submit({{'xs': [1, 2, 3]}}, result=Total)).total)"
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "6\n");
    let child = of_agent(&requests, "ROLE repairer");
    assert!(
        tool_result(child[2], "c2").contains("attempt 1 of 3")
            && tool_result(child[2], "c2").contains("$.total"),
        "{}",
        tool_result(child[2], "c2")
    );
    let ids: Vec<Value> = kind(&events, "exec.submitted", Some("agent/c1"))
        .into_iter()
        .map(|event| data(event)["execid"].clone())
        .collect();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(
        (ids.len(), unique.len()),
        (3, 3),
        "each execution once: {ids:?}"
    );
    let invalid = kind(&events, "agent.request.invalid", None);
    assert_eq!(invalid.len(), 1);
    assert_eq!(data(invalid[0])["attempt"], 1);
}

/// Past the attempt limit the submission ends rejected, carrying the last
/// value and its problems, and no further round asks for it.
#[tokio::test]
async fn the_attempt_limit_rejects_a_work_childs_third_wrong_answer() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_ATTEMPTS",
        "",
        vec![
            (
                "ROLE wrong",
                scripted(child_runs(
                    "c1",
                    "for n in range(3):\n\
                     \x20   try:\n\
                     \x20       await runtime.complete({'total': str(n)})\n\
                     \x20   except Exception as e:\n\
                     \x20       print(type(e).__name__)",
                )),
            ),
            (
                "",
                primary_runs(&format!(
                    "{TOTAL}\nimport outrig\n\
                     child = await runtime.spawn('w', prompt='ROLE wrong')\n\
                     h = child.submit({{}}, result=Total)\n\
                     try:\n    await h\n\
                     except outrig.CompletionRejected as e:\n\
                     \x20   print(e.value, [str(p) for p in e.problems], h.status)"
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "{'total': '2'} ['$.total: expected an integer, not a string'] failed: rejected (child running)\n"
    );
    let child = of_agent(&requests, "ROLE wrong");
    assert_eq!(
        tool_result(child[1], "c1"),
        "DecodeError\nDecodeError\nCompletionRejected\n"
    );
    assert_eq!(child.len(), 2, "no round follows the rejection");
    assert_eq!(kind(&events, "agent.request.invalid", None).len(), 3);
}

/// A work child whose submission's round, the round asking for its result and the warning round
/// all end without completing it has the submission cancelled, saying so, with no fourth call for
/// it and no invalid answer recorded. The nudge says nothing of what follows it; the warning names
/// each way out and that ending the round cancels. The child is never idle between the cancel and
/// the round of the submission queued behind it, which completes, and is idle after that.
#[tokio::test]
async fn a_work_child_that_never_completes_is_cancelled_after_three_rounds() {
    let child = vec![
        text_reply("I will not complete it"),
        text_reply("Still not"),
        text_reply("Not even now"),
        submit("c1", &py("await runtime.complete(Total(total=sum(xs)))")),
        text_reply("ended"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_ONE_SHOT",
        "",
        vec![
            ("ROLE stubborn", scripted(child)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE stubborn')
                    first = child.submit({'xs': [1]}, result=Total)
                    second = child.submit({'xs': [2, 3]}, result=Total)
                    print(second.status.state)
                    seen = []
                    async def watch():
                        while True:
                            state = child.status
                            if second.done():
                                return
                            if first.done():
                                seen.append(state)
                            await asyncio.sleep(0.001)
                    watching = asyncio.create_task(watch())
                    try:
                        await first
                    except outrig.RequestCancelled as e:
                        print(e, e.__cause__)
                    print((await second).total)
                    await watching
                    print(len(seen) > 0, 'idle' in seen)
                    await until(lambda: child.status == 'idle')
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let id = said
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("the cancelled submission's id")
        .to_string();
    assert_eq!(
        said,
        format!(
            "queued\nsubmission {id} was cancelled: the child ended its rounds without completing \
             it None\n5\nTrue False\n"
        ),
        "no round failed on a model call, so nothing is chained"
    );
    let child = of_agent(&requests, "ROLE stubborn");
    assert_eq!(
        child.len(),
        4,
        "three rounds for the first, and one call for the second, whose round ends on its \
         completion"
    );
    let nudge = opening(child[1]);
    assert!(
        nudge.contains(&format!("submission {id} is still open"))
            && !nudge.contains("fail")
            && !nudge.contains("cancel"),
        "{nudge}"
    );
    let warning = opening(child[2]);
    assert!(
        warning.contains("your last two rounds ended without completing it")
            && warning.contains("`await runtime.fail(message)`")
            && warning.contains("it is cancelled, and its caller gets an exception"),
        "{warning}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
    let settled: Vec<(Value, Value)> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| (data(event)["outcome"].clone(), data(event)["cause"].clone()))
        .collect();
    assert_eq!(
        settled,
        [
            (Value::from("cancelled"), Value::Null),
            (Value::from("accepted"), Value::Null)
        ]
    );
    let cancelled = data(kind(&events, "agent.request.cancelled", None)[0]);
    assert_eq!(cancelled["received"], true, "it was given to the child");
    assert!(
        cancelled["reason"]
            .as_str()
            .is_some_and(|why| why.contains("without completing it")),
        "{cancelled}"
    );
}

/// The model calls a filtered reply costs a round: the call, and the call sent again without the
/// latest results. The config's `retry-budget-secs = 0` leaves no retry between them.
const FILTERED_CALLS: usize = 2;

/// A work child whose round runs code and then meets only replies the provider filtered fails
/// that round; the next two end on plain stops without completing, and the submission is
/// cancelled. The parent's exception chains the failed round as its `__cause__`, an
/// `AgentRequestFailed` naming the finish reason, which the settled event carries too.
#[tokio::test]
async fn a_cancel_chains_the_round_a_filtered_reply_failed() {
    let mut child = vec![submit("c1", &py("print('ran')"))];
    child.extend(std::iter::repeat_n(
        mock_http::Style::Messages.empty("refusal"),
        FILTERED_CALLS,
    ));
    child.push(text_reply("done"));
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_CHAINED_CAUSE",
        "",
        vec![
            ("ROLE filtered", scripted(child)),
            (
                "",
                primary_runs(&format!(
                    "{TOTAL}\nimport outrig\n\
                     child = await runtime.spawn('s', prompt='ROLE filtered')\n\
                     first = child.submit({{'xs': [1]}}, result=Total)\n\
                     try:\n    await first\n\
                     except outrig.RequestCancelled as e:\n    \
                         print(type(e.__cause__).__name__, e.__cause__)"
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    assert!(
        said.starts_with("AgentRequestFailed ") && said.contains("finish reason refusal"),
        "{said}"
    );
    assert_eq!(
        of_agent(&requests, "ROLE filtered").len(),
        1 + FILTERED_CALLS + 2,
        "the failed round's calls, then the nudge and the warning"
    );
    let settled = data(kind(&events, "agent.request.settled", None)[0]);
    assert_eq!(settled["outcome"], "cancelled");
    assert!(
        settled["cause"]
            .as_str()
            .is_some_and(|cause| cause.contains("finish reason refusal")),
        "{settled}"
    );
}

// ---------------------------------------------------------------------------- request children

const THING: &str = r#"
from dataclasses import dataclass
import outrig
@dataclass
class Thing:
    n: int
"#;

/// A request sent to a request child idle since its launch starts a round,
/// whose opening counts it; the child receives it, its reply is checked
/// against the reply type, and the handle gives the parent an instance of it.
#[tokio::test]
async fn a_request_to_an_idle_child_starts_a_round_and_its_reply_settles_it() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_REQUEST",
        "",
        vec![
            (
                "ROLE counter",
                scripted(child_runs(
                    "c1",
                    "d = await runtime.channels['fizz'].receive()\n\
                     print(d.id, d.body, d.sender)\n\
                     await d.reply(Thing(n=len(d.body)))",
                )),
            ),
            (
                "",
                primary_runs(&format!(
                    "{THING}\n\
                     child = await runtime.spawn('c', prompt='ROLE counter', \
                     requests={{'fizz': outrig.Request(str, Thing, doc='count it')}})\n\
                     h = child.channels['fizz'].request('hello')\n\
                     r = await h\n\
                     print(h.id, r, type(r) is Thing)"
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    let child = of_agent(&requests, "ROLE counter");
    let opening = child[0].body["messages"][0].to_string();
    assert!(
        opening.contains("1 request is waiting on runtime.channels[\\\"fizz\\\"]"),
        "{opening}"
    );
    let system = system_text(&child[0].body);
    assert!(
        system.contains("count it") && system.contains("You have no `runtime.complete`"),
        "{system}"
    );
    let said = tool_result(child[1], "c1");
    let id = said.split_whitespace().next().expect("an id").to_string();
    assert_eq!(said, format!("{id} hello primary\n"));
    assert_eq!(printed(&requests, "p1"), format!("{id} Thing(n=5) True\n"));
}

/// A request arriving while the child's round runs is announced on the next
/// tool result, in requests, not messages.
#[tokio::test]
async fn a_request_arriving_during_a_round_is_announced_on_the_next_result() {
    let flag = Flag::new();
    let child = vec![
        submit("c1", "d = await runtime.channels['fizz'].receive()"),
        // Says it runs, then waits for the second request to arrive.
        submit(
            "c2",
            &format!(
                "open({flag}, 'w').close()\n\
                 while not runtime.channels['fizz'].pending():\n    await asyncio.sleep(0.01)",
                flag = flag.py()
            ),
        ),
        submit(
            "c3",
            "await d.reply(Thing(n=1))\nwhile runtime.channels['fizz'].pending():\n    await (await runtime.channels['fizz'].receive()).reply(Thing(n=2))",
        ),
        text_reply("ended"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_ANNOUNCED",
        "",
        vec![
            ("ROLE announced", scripted(child)),
            (
                "",
                primary_runs(&format!(
                    "{THING}\n\
                     child = await runtime.spawn('a', prompt='ROLE announced', requests={{'fizz': (str, Thing)}})\n\
                     first = child.channels['fizz'].request('a')\n\
                     {wait}\n\
                     second = child.channels['fizz'].request('b')\n\
                     print((await first).n, (await second).n)",
                    wait = flag.awaited()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1 2\n");
    let child = of_agent(&requests, "ROLE announced");
    let result = tool_result(child[2], "c2");
    assert!(
        result.starts_with("[1 request is waiting on runtime.channels[\"fizz\"]]"),
        "{result}"
    );
}

/// Python that waits until `test()` holds, failing rather than hanging.
const UNTIL: &str = r#"
import os, time
async def until(test):
    deadline = time.monotonic() + 20
    while not test():
        if time.monotonic() > deadline:
            raise TimeoutError('it never held')
        await asyncio.sleep(0.01)
"#;

/// The primary's one call, as [`primary_runs`], with `Thing`, `Total` and
/// `until` defined before `source`.
pub(super) fn primary_waits(source: &str) -> Responder {
    primary_runs(&format!("{THING}{TOTAL}{UNTIL}\n{}", py(source)))
}

/// Python that watches children for the one state none may be in: idle while it holds a request
/// it took. `watch(child, handles)` starts a task that reads the child's status, then its handles,
/// every millisecond, and adds the child's name to `idle_holding` whenever it reads `idle` while
/// one it took -- `received`, or a submission `running` -- has not ended. A request not yet taken
/// on an idle child is the moment before its round opens, and is not noted.
const WATCH: &str = r#"
idle_holding = []
def watch(child, handles):
    async def run():
        while True:
            state = child.status
            taken = [h for h in handles if not h.done() and h.status.state in ('received', 'running')]
            if state == 'idle' and taken:
                idle_holding.append(child.name)
            await asyncio.sleep(0.001)
    return asyncio.get_running_loop().create_task(run())
"#;

/// The text of the message `request` ended on: for a round's first call, its
/// opening.
pub(super) fn opening(request: &RecordedRequest) -> String {
    text_of(&messages(request).last().expect("a message")["content"])
}

/// A request the child took and never answers is asked for three times: in the round its arrival
/// opened, in the round whose opening names it by id with how long it has waited, and in the
/// warning round, which says ending it so cancels the request. Then the host cancels it, saying
/// why: the handle ends cancelled and the child idle, the record names the host and that the child
/// had taken it, no answer was counted, and a reply from a task the child left running is refused.
#[tokio::test]
async fn a_request_taken_and_never_answered_is_cancelled_after_three_rounds() {
    let (go, out) = (Flag::new(), Flag::new());
    let taking = format!(
        r#"
        import os, outrig
        d = await runtime.channels['fizz'].receive()
        async def late():
            while not os.path.exists({go}):
                await asyncio.sleep(0.01)
            try:
                await d.reply(Thing(n=1))
            except outrig.RequestCancelled as e:
                with open({out}, 'w') as f:
                    f.write(f'{{type(e).__name__}}: {{e}}')
        left = asyncio.get_running_loop().create_task(late())
        "#,
        go = go.py(),
        out = out.py()
    );
    let child = vec![
        submit("c1", &py(&taking)),
        text_reply("taken"),
        text_reply("still not answering"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNCHANGED",
        "",
        vec![
            ("ROLE silent", scripted(child)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE silent', requests={{'fizz': (str, Thing)}})
                    h = child.channels['fizz'].request('a')
                    try:
                        await h
                    except outrig.RequestCancelled as e:
                        print(h.id, e)
                    print(h.status.state, h.status.reason)
                    await until(lambda: child.status == 'idle')
                    print(child.status, h.done())
                    open({go}, 'w').close()
                    await until(lambda: os.path.exists({out}))
                    print(open({out}).read())
                    "#,
                    go = go.py(),
                    out = out.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let id = said.split_whitespace().next().expect("an id").to_string();
    let why =
        format!("request {id} was cancelled: the child ended its rounds without answering it");
    assert_eq!(
        said,
        format!(
            "{id} {why}\nfailed cancelled\nidle True\nRequestCancelled: {why}; it takes no answer\n"
        )
    );
    let child = of_agent(&requests, "ROLE silent");
    assert_eq!(
        child.len(),
        4,
        "the round its arrival opened, its nudge, the warning, and nothing after"
    );
    let nudge = opening(child[2]);
    assert!(
        nudge.contains(&format!(
            "you took request {id} from runtime.channels[\"fizz\"] "
        )) && nudge.contains("s ago and have not answered it")
            && !nudge.contains("cancel"),
        "{nudge}"
    );
    let warning = opening(child[3]);
    assert!(
        warning.contains(&format!(
            "your last round ended with requests unanswered: you took request {id} from"
        )) && warning.contains("they are cancelled, and each sender gets an exception")
            && !warning.contains("receive()"),
        "{warning}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
    let cancelled = data(kind(&events, "agent.request.cancelled", None)[0]);
    assert_eq!(cancelled["received"], true);
    assert_eq!(cancelled["reason"], why.as_str());
    assert_eq!(
        data(kind(&events, "agent.request.settled", None)[0])["outcome"],
        "cancelled"
    );
    assert_eq!(
        data(kind(&events, "agent.request.refused", None)[0])["ended"],
        "cancelled"
    );
}

/// A request that arrives while the child holds others -- one taken, one unread -- opens a round
/// once the round it came in ends, whose opening names the taken one by id and counts the two
/// unread; a reply to each then settles its handle, the old one's with its value.
#[tokio::test]
async fn a_new_arrival_opens_a_round_naming_the_old_requests_too() {
    let flag = Flag::new();
    let child = vec![
        submit("c1", "d1 = await runtime.channels['fizz'].receive()"),
        text_reply("took one"),
        // The round naming d1 says it runs, then waits for the third request to arrive.
        submit(
            "c2",
            &format!(
                "open({flag}, 'w').close()\n\
                 while runtime.channels['fizz'].pending() < 2:\n    await asyncio.sleep(0.01)",
                flag = flag.py()
            ),
        ),
        text_reply("waited"),
        submit(
            "c3",
            &py(r#"
                await d1.reply(Thing(n=d1.id))
                for _ in range(2):
                    d = await runtime.channels['fizz'].receive()
                    await d.reply(Thing(n=d.id))
                "#),
        ),
        text_reply("ended"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_NEW_ARRIVAL",
        "",
        vec![
            ("ROLE late", scripted(child)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('l', prompt='ROLE late', requests={{'fizz': (str, Thing)}})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    await until(lambda: os.path.exists({flag}))
                    h3 = child.channels['fizz'].request('c')
                    for h in (h1, h2, h3):
                        print(h.id, (await h).n)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let answered: Vec<(&str, &str)> = said
        .lines()
        .map(|line| line.split_once(' ').expect("an id and a value"))
        .collect();
    assert_eq!(answered.len(), 3);
    assert!(answered.iter().all(|(id, n)| id == n), "{said}");
    let child = of_agent(&requests, "ROLE late");
    assert_eq!(child.len(), 6);
    let named = opening(child[4]);
    assert!(
        named.contains("2 requests are waiting on runtime.channels[\"fizz\"]")
            && named.contains(&format!("you took request {} from", answered[0].0))
            && !named.contains("your last round ended"),
        "{named}"
    );
}

/// A request the child never takes is asked for twice: in the round its arrival opened, and in
/// the warning round, which says to take it with `receive()` first. Then the host takes it out of
/// the child's queue and cancels it: `pending()` is 0, and its body never reached the model. One
/// that arrives during a round, announced there, gets the round after it, whose opening counts it,
/// then the warning, then the cancel. Neither counts an attempt.
#[tokio::test]
async fn a_request_never_taken_is_cancelled_after_two_rounds() {
    let mut idle = tree(
        "OUTRIG_TEST_CHILD_UNREAD_IDLE",
        "",
        vec![
            ("ROLE ignoring", scripted(vec![text_reply("not now")])),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('i', prompt='ROLE ignoring', requests={'fizz': (str, Thing)})
                    h = child.channels['fizz'].request('a secret body')
                    try:
                        await h
                    except outrig.RequestCancelled as e:
                        print(e)
                    await until(lambda: child.status == 'idle')
                    print(child.channels['fizz'].pending(), h.status.state, h.status.reason)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut idle.session, "go").await;
    let (_, events, requests) = idle.end().await;
    let said = printed(&requests, "p1");
    assert!(
        said.ends_with(
            " was cancelled: the child ended its rounds without reading it\n0 failed cancelled\n"
        ),
        "{said}"
    );
    let child = of_agent(&requests, "ROLE ignoring");
    assert_eq!(
        child.len(),
        2,
        "the round its arrival opened, and the warning"
    );
    let warning = opening(child[1]);
    assert!(
        warning.contains("your last round ended with requests unanswered: 1 request is waiting")
            && warning.contains(
                "Take an unread one first, with `await runtime.channels[\"fizz\"].receive()`"
            ),
        "{warning}"
    );
    assert!(
        child
            .iter()
            .all(|request| !request.body.to_string().contains("a secret body")),
        "the body never reached the child's model"
    );
    assert_eq!(
        data(kind(&events, "agent.request.cancelled", None)[0])["received"],
        false
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());

    let mut announced = tree(
        "OUTRIG_TEST_CHILD_UNREAD_ANNOUNCED",
        "",
        vec![
            (
                "ROLE ignoring",
                scripted(vec![
                    submit(
                        "c1",
                        &py(r#"
                            d = await runtime.channels['fizz'].receive()
                            await d.reply(Thing(n=1))
                            while runtime.channels['fizz'].pending() < 1:
                                await asyncio.sleep(0.01)
                            "#),
                    ),
                    text_reply("leaving the second"),
                    text_reply("still leaving it"),
                ]),
            ),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('i', prompt='ROLE ignoring', requests={'fizz': (str, Thing)})
                    first = child.channels['fizz'].request('a')
                    await until(lambda: first.status.state != 'waiting')
                    second = child.channels['fizz'].request('b')
                    await first
                    try:
                        await second
                    except outrig.RequestCancelled as e:
                        print('cancelled', 'without reading it' in str(e))
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut announced.session, "go").await;
    let (_, events, requests) = announced.end().await;
    assert_eq!(printed(&requests, "p1"), "cancelled True\n");
    let child = of_agent(&requests, "ROLE ignoring");
    assert_eq!(
        child.len(),
        4,
        "the first round's two calls, the round after it, and the warning"
    );
    let result = tool_result(child[1], "c1");
    assert!(
        result.starts_with("[1 request is waiting on runtime.channels[\"fizz\"]]"),
        "{result}"
    );
    let after = opening(child[2]);
    assert!(
        after.contains("1 request is waiting on runtime.channels[\"fizz\"]")
            && !after.contains("your last round ended"),
        "{after}"
    );
    assert!(
        opening(child[3]).contains("your last round ended with requests unanswered"),
        "{}",
        opening(child[3])
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
}

/// The rounds of a request child that takes its request, ends that round and the one naming it,
/// and then runs `source` in the warning round.
fn warned_then(source: &str) -> Vec<CannedResponse> {
    vec![
        submit("c1", "d = await runtime.channels['fizz'].receive()"),
        text_reply("taken"),
        text_reply("not yet"),
        submit("c2", &py(source)),
        text_reply("answered"),
    ]
}

/// A child that replies in its warning round has its request answered with the value; one that
/// waits there with `runtime.wait` on a task, and replies once it ends, has its request answered
/// too, since its round has not ended while it waits. Neither is cancelled.
#[tokio::test]
async fn a_reply_or_a_wait_in_the_warning_round_is_not_cancelled() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_WARNED_REPLY",
        "",
        vec![
            (
                "ROLE replier",
                scripted(warned_then("await d.reply(Thing(n=7))")),
            ),
            (
                "ROLE waiter",
                scripted(warned_then(
                    r#"
                    slow = asyncio.create_task(asyncio.sleep(0.3))
                    await runtime.wait({slow})
                    await d.reply(Thing(n=8))
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    r = await runtime.spawn('r', prompt='ROLE replier', requests={'fizz': (str, Thing)})
                    w = await runtime.spawn('w', prompt='ROLE waiter', requests={'fizz': (str, Thing)})
                    print((await r.channels['fizz'].request('a')).n, (await w.channels['fizz'].request('b')).n)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "7 8\n");
    for role in ["ROLE replier", "ROLE waiter"] {
        let child = of_agent(&requests, role);
        assert_eq!(child.len(), 5, "{role}");
        assert!(
            opening(child[3]).contains("your last round ended with requests unanswered"),
            "{role}: {}",
            opening(child[3])
        );
    }
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
}

/// Anything the child changes begins the count again. With two requests taken and both left, a
/// warning round that answers one is followed by an ordinary round; when that one and the warning
/// after it change nothing, the other is cancelled, and the first stays answered.
#[tokio::test]
async fn anything_the_child_changes_begins_the_count_again() {
    let child = vec![
        submit(
            "c1",
            "d1 = await runtime.channels['fizz'].receive()\n\
             d2 = await runtime.channels['fizz'].receive()",
        ),
        text_reply("took both"),
        text_reply("nudged"),
        submit("c2", "await d1.reply(Thing(n=1))"),
        text_reply("answered one"),
        text_reply("leaving the other"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_COUNT_AGAIN",
        "",
        vec![
            ("ROLE twice", scripted(child)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('t', prompt='ROLE twice', requests={'fizz': (str, Thing)})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    print((await h1).n)
                    try:
                        await h2
                    except outrig.RequestCancelled as e:
                        print('cancelled', 'without answering it' in str(e))
                    print(h1.status.state)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1\ncancelled True\naccepted\n");
    let child = of_agent(&requests, "ROLE twice");
    assert_eq!(
        child.len(),
        7,
        "the first round's two calls, the nudge, the warning's two, an ordinary round, a warning"
    );
    let warned = |n: usize| opening(child[n]).contains("your last round ended with requests");
    assert_eq!(
        [2, 3, 5, 6].map(warned),
        [false, true, false, true],
        "nudge, warning, ordinary, warning"
    );
}

/// A warning round whose model call fails before its code ran is opened again with the same
/// opening, and nothing is cancelled until that round ends. One whose code ran before a call
/// failed neither gives the warning nor counts as the round after it: the next round is the
/// warning again, and only once it ends is the request cancelled.
#[tokio::test]
async fn a_round_ending_in_an_error_is_no_unchanged_round() {
    let taken = || {
        vec![
            submit("c1", "d = await runtime.channels['fizz'].receive()"),
            text_reply("taken"),
            text_reply("nudged"),
        ]
    };
    let mut before = taken();
    before.extend([mock_http::failure(500), text_reply("warned")]);
    let mut after = taken();
    after.extend([
        submit("c2", "print('looked')"),
        mock_http::failure(500),
        text_reply("warned again"),
    ]);
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_ERROR_ROUND",
        "",
        vec![
            ("ROLE before", scripted(before)),
            ("ROLE after", scripted(after)),
            (
                "",
                primary_waits(
                    r#"
                    b = await runtime.spawn('b', prompt='ROLE before', requests={'fizz': (str, Thing)})
                    a = await runtime.spawn('a', prompt='ROLE after', requests={'fizz': (str, Thing)})
                    for h in (b.channels['fizz'].request('x'), a.channels['fizz'].request('y')):
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
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "cancelled\ncancelled\n");
    let warning = "your last round ended with requests unanswered";

    let before = of_agent(&requests, "ROLE before");
    assert_eq!(
        before.len(),
        5,
        "the warning round opened again once, and nothing after"
    );
    assert!(
        opening(before[3]).contains(warning),
        "{}",
        opening(before[3])
    );
    assert_eq!(opening(before[3]), opening(before[4]), "the same opening");

    let after = of_agent(&requests, "ROLE after");
    assert_eq!(
        after.len(),
        6,
        "the failed warning round's two calls, then the warning again"
    );
    for n in [3, 5] {
        assert!(opening(after[n]).contains(warning), "{}", opening(after[n]));
    }
    assert_eq!(
        kind(&events, "model.round.failed", Some("agent/c2")).len(),
        1
    );
    assert_eq!(kind(&events, "agent.request.cancelled", None).len(), 2);
}

/// A round stopped at the tool-call cap has ended as any other: the nudge round it cut short is
/// followed by the warning round, in which the child fails the request on purpose, and the handle
/// ends failed with that message, not cancelled.
#[tokio::test]
async fn a_round_stopped_at_the_cap_counts_as_ended() {
    let child = vec![
        submit("c1", "d = await runtime.channels['fizz'].receive()"),
        text_reply("taken"),
        submit("c2", "print('one')"),
        submit("c3", "print('two, never run')"),
        submit("c4", "await d.fail('the file is not there')"),
        text_reply("failed it"),
    ];
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_CAPPED",
        "tool-call-max = 1",
        vec![
            ("ROLE capped", scripted(child)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('c', prompt='ROLE capped', requests={'fizz': (str, Thing)})
                    h = child.channels['fizz'].request('x')
                    try:
                        await h
                    except outrig.AgentRequestFailed as e:
                        print(e.message, h.status.state, h.status.reason)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "the file is not there failed failed\n"
    );
    let child = of_agent(&requests, "ROLE capped");
    assert_eq!(child.len(), 6);
    assert!(
        opening(child[4]).contains("your last round ended with requests unanswered"),
        "{}",
        opening(child[4])
    );
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
}

/// No child is idle while it holds a request. Each way a child's rounds end is driven under the
/// watcher [`WATCH`] starts: a request taken and never answered, one never taken, a model that
/// fails for good, and a release. It never finds a child idle with a request it took open, and
/// wherever a child is idle, every handle sent to it has ended. The spent budget and the close
/// are watched in their own tests.
#[tokio::test]
async fn no_child_is_idle_holding_a_request() {
    let held = Flag::new();
    let holding = format!(
        r#"
        d = await runtime.channels['fizz'].receive()
        open({held}, 'w').close()
        while True:
            await asyncio.sleep(0.05)
        "#,
        held = held.py()
    );
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_NEVER_IDLE_HOLDING",
        "",
        vec![
            (
                "ROLE taker",
                scripted(child_runs(
                    "t1",
                    "d = await runtime.channels['fizz'].receive()",
                )),
            ),
            ("ROLE ignorer", scripted(vec![text_reply("not now")])),
            ("ROLE broken", scripted(vec![mock_http::failure(401)])),
            ("ROLE holder", scripted(vec![submit("h1", &py(&holding))])),
            (
                "",
                primary_waits(&format!(
                    "{WATCH}{}",
                    py(&format!(
                        r#"
                        spec = {{'fizz': (str, Thing)}}
                        for role in ('taker', 'ignorer', 'broken', 'holder'):
                            child = await runtime.spawn(role, prompt=f'ROLE {{role}}', requests=spec)
                            handles = [child.channels['fizz'].request(body) for body in ('a', 'b')]
                            watching = watch(child, handles)
                            if role == 'holder':
                                await until(lambda: os.path.exists({held}))
                                await runtime.release([child])
                            ended = []
                            for h in handles:
                                try:
                                    await h
                                except outrig.AgentError as e:
                                    ended.append(type(e).__name__)
                            await until(lambda: child.status in ('idle', 'released'))
                            print(role, child.status, all(h.done() for h in handles), ended)
                            watching.cancel()
                        print(idle_holding)
                        "#,
                        held = held.py()
                    ))
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "taker idle True ['RequestCancelled', 'RequestCancelled']\n\
         ignorer idle True ['RequestCancelled', 'RequestCancelled']\n\
         broken idle True ['AgentRequestFailed', 'AgentRequestFailed']\n\
         holder released True ['AgentReleased', 'AgentReleased']\n\
         []\n"
    );
}

/// What a child runs as it takes its request: the notice of that request's end then fails to
/// encode, as out of memory, until `freed` is raised.
fn muted(freed: &Flag) -> String {
    format!(
        r#"
        import __main__, os
        d = await runtime.channels['fizz'].receive()
        encode = __main__._encode
        def short(message):
            if message.get('t') == 'agent.ended' and message.get('request') == d.id \
                    and not os.path.exists({freed}):
                raise MemoryError
            return encode(message)
        __main__._encode = short
        "#,
        freed = freed.py()
    )
}

/// The rounds of a request child that runs `taking` as it takes its request, ends that round, the
/// one naming the request and the warning, and then answers the next request.
fn taking_then_answering(taking: &str) -> Responder {
    scripted(vec![
        submit("c1", &py(taking)),
        text_reply("taken"),
        text_reply("not yet"),
        text_reply("still not"),
        submit(
            "c2",
            "e = await runtime.channels['fizz'].receive()\nawait e.reply(Thing(n=2))",
        ),
        text_reply("answered"),
    ])
}

/// The rounds of a request child that replies to its request as it takes it, the reply's notice
/// failing as [`muted`] has it, and holds that round open until `finish` is raised: until then,
/// only its release, or the close, can tell the host the request ended.
fn replying_then_holding(freed: &Flag, finish: &Flag) -> Responder {
    let source = format!(
        r#"{muted}
        import asyncio
        await d.reply(Thing(n=1))
        while not os.path.exists({finish}):
            await asyncio.sleep(0.05)
        "#,
        muted = muted(freed),
        finish = finish.py()
    );
    scripted(vec![submit("c1", &py(&source)), text_reply("held")])
}

/// An end the host does not hear is asked for again. Out of memory, the interpreter can lose its
/// notice of an end it made, or a settle the host sent: here `mute`'s notice of its request's cancel
/// fails to encode until memory frees, after the request has ended, and the first settle `deaf` is
/// sent is lost as the reader loses a line it had no memory to handle. Each request ends cancelled,
/// and is recorded so once; each child goes idle, and takes the next request it is sent.
#[tokio::test]
async fn an_end_the_host_does_not_hear_is_asked_for_again() {
    let freed = Flag::new();
    let deaf = r#"
        import __main__
        d = await runtime.channels['fizz'].receive()
        settle, lost = __main__._ROUTES['settle'], []
        def deaf(kernel, request, message):
            if request == d.id and not lost:
                lost.append(request)
                return
            settle(kernel, request, message)
        __main__._ROUTES['settle'] = deaf
        "#;
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_END",
        "",
        vec![
            ("ROLE mute", taking_then_answering(&muted(&freed))),
            ("ROLE deaf", taking_then_answering(deaf)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    for role in ('mute', 'deaf'):
                        child = await runtime.spawn(role, prompt=f'ROLE {{role}}', requests={{'fizz': (str, Thing)}})
                        h = child.channels['fizz'].request('a')
                        try:
                            await h
                        except outrig.RequestCancelled:
                            pass
                        open({freed}, 'w').close()
                        await until(lambda: child.status == 'idle')
                        print(role, h.status.reason, (await child.channels['fizz'].request('b')).n)
                    "#,
                    freed = freed.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "mute cancelled 2\ndeaf cancelled 2\n"
    );
    assert_eq!(kind(&events, "agent.request.cancelled", None).len(), 2);
    let outcomes: Vec<Value> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        ["cancelled", "accepted", "cancelled", "accepted"],
        "each end recorded once"
    );
}

/// Requests that keep arriving while an end is unheard wake the runner, and do not put off sending
/// the settle again: with one sent every 0.2 s, more often than the settle is sent again, the end
/// is heard once memory frees, and an arrival is answered.
#[tokio::test]
async fn arrivals_do_not_put_off_asking_for_an_end_again() {
    let freed = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_ARRIVALS",
        "",
        vec![
            ("ROLE mute", taking_then_answering(&muted(&freed))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('m', prompt='ROLE mute', requests={{'fizz': (str, Thing)}})
                    try:
                        await child.channels['fizz'].request('a')
                    except outrig.RequestCancelled:
                        pass
                    open({freed}, 'w').close()
                    arrivals = []
                    while not any(h.done() for h in arrivals):
                        if len(arrivals) == 50:
                            raise TimeoutError('no arrival was answered in 10 s')
                        arrivals.append(child.channels['fizz'].request('b'))
                        await asyncio.sleep(0.2)
                    print(await next(h for h in arrivals if h.done()))
                    "#,
                    freed = freed.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "Thing(n=2)\n");
}

/// A child released before the host hears an end it made tells the host that end as it is
/// released, and its kernel keeps nothing of the request. Each child replies with its notice of the
/// end failing to encode, and holds its round open, so nothing else tells the host: `told`'s memory
/// frees before its release, and the host records the request answered; `lost`'s does not, and the
/// request goes all the same.
#[tokio::test]
async fn a_release_tells_an_unheard_end_and_keeps_nothing() {
    let (freed, never) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_RELEASE",
        "",
        vec![
            ("ROLE told", replying_then_holding(&freed, &never)),
            ("ROLE lost", replying_then_holding(&never, &never)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__
                    for role in ('told', 'lost'):
                        child = await runtime.spawn(role, prompt=f'ROLE {{role}}', requests={{'fizz': (str, Thing)}})
                        h = child.channels['fizz'].request('a')
                        n = (await h).n
                        if role == 'told':
                            open({freed}, 'w').close()
                        await runtime.release([child])
                        print(role, n, h.status.state, h.id in __main__._works)
                    "#,
                    freed = freed.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "told 1 accepted False\nlost 1 accepted False\n"
    );
    let outcomes: Vec<Value> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        ["accepted"],
        "told's end, once; lost's is never heard"
    );
}

/// A request whose end the host has not heard when the session closes is told then: the child's
/// notice of its reply fails to encode until memory frees, which it does before the close, while
/// the child holds its round open, and the report has the request answered, not terminated.
#[tokio::test]
async fn the_close_tells_an_unheard_end() {
    let (freed, closing, finish) = (Flag::new(), Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_CLOSE",
        "",
        vec![
            ("ROLE told", replying_then_holding(&freed, &finish)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('told', prompt='ROLE told', requests={{'fizz': (str, Thing)}})
                    print((await child.channels['fizz'].request('a')).n)
                    open({freed}, 'w').close()
                    open({closing}, 'w').close()
                    "#,
                    freed = freed.py(),
                    closing = closing.py()
                )),
            ),
        ],
    )
    .await;
    let closer = tree.session.closer();
    let (end, ()) = tokio::join!(round_end(&mut tree.session, "go"), async {
        raised(&closing).await;
        closer.close_admission();
    });
    assert_eq!(end.reply, "done");
    finish.raise();
    let (report, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1\n");
    let outcomes: Vec<_> = report.requests.iter().map(|row| row.outcome).collect();
    assert_eq!(outcomes, [crate::harness::event::RequestEnd::Accepted]);
}

/// A request counted on its child before the child's release, and recorded only once the release
/// has ended the child's kernel -- and so looked at what it holds -- ends released. Its notice of
/// that fails to encode, and it is kept no longer all the same: nothing would settle it again.
#[tokio::test]
async fn a_request_racing_its_childs_release_is_not_kept() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_RACE",
        "",
        vec![
            ("ROLE racer", scripted(vec![text_reply("idle")])),
            (
                "",
                primary_waits(
                    r#"
                    import __main__, asyncio, threading, time
                    child = await runtime.spawn('r', prompt='ROLE racer', requests={'fizz': (str, Thing)})
                    encode = __main__._encode
                    def short(message):
                        if message.get('t') == 'agent.ended' and message.get('agent') == child.id:
                            raise MemoryError
                        return encode(message)
                    __main__._encode = short
                    record = child._child
                    take, taken = record.take, threading.Event()
                    def late(what):
                        take(what)
                        taken.set()
                        while child.id in __main__._kernels:
                            time.sleep(0.001)
                    record.take = late
                    sending = asyncio.ensure_future(asyncio.to_thread(child.channels['fizz'].request, 'late'))
                    await asyncio.to_thread(taken.wait)
                    await runtime.release([child])
                    h = await sending
                    print(h.status.state, h.status.reason, h.id in __main__._works)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "failed released False\n");
}

/// A request whose notice to the host fails to encode, as out of memory, is one the host never
/// settles: cancelled, its notice of that failing too, it is kept no longer.
#[tokio::test]
async fn a_request_the_host_never_heard_of_is_not_kept() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_UNHEARD_REQUEST",
        "",
        vec![
            ("ROLE unheard", scripted(vec![text_reply("idle")])),
            (
                "",
                primary_waits(
                    r#"
                    import __main__
                    child = await runtime.spawn('u', prompt='ROLE unheard', requests={'fizz': (str, Thing)})
                    encode = __main__._encode
                    def short(message):
                        if message.get('t') == 'agent.request' and message.get('child') == child.id \
                                or message.get('t') == 'agent.ended' and message.get('agent') == child.id:
                            raise MemoryError
                        return encode(message)
                    __main__._encode = short
                    h = child.channels['fizz'].request('a')
                    print(h.cancel(), h.status.state, h.status.reason, h.id in __main__._works)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True failed cancelled False\n");
    assert!(kind(&events, "agent.request.sent", None).is_empty());
}

/// A request whose notice to the host fails only as it is written -- memory running out once the
/// whole line has gone -- may be one the host has, and here is: the host cancels it when the child
/// leaves it unread, its notice of that end failing to encode until memory frees, and the end is
/// asked for again and heard, so the child takes the next request.
#[tokio::test]
async fn a_request_whose_notice_failed_as_it_was_written_is_settled() {
    let freed = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_WRITTEN_REQUEST",
        "",
        vec![
            (
                "ROLE written",
                scripted(vec![
                    text_reply("not taking it"),
                    text_reply("still not"),
                    submit(
                        "c1",
                        "e = await runtime.channels['fizz'].receive()\nawait e.reply(Thing(n=2))",
                    ),
                    text_reply("answered"),
                ]),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__, asyncio, os
                    child = await runtime.spawn('w', prompt='ROLE written', requests={{'fizz': (str, Thing)}})
                    write, encode, failed = __main__._write_line, __main__._encode, []
                    def written(line):
                        write(line)
                        if b'"agent.request"' in line and not failed:
                            failed.append(line)
                            raise MemoryError
                    __main__._write_line = written
                    h = child.channels['fizz'].request('a')
                    def short(message):
                        if message.get('t') == 'agent.ended' and message.get('request') == h.id \
                                and not os.path.exists({freed}):
                            raise MemoryError
                        return encode(message)
                    __main__._encode = short
                    try:
                        await h
                    except outrig.RequestCancelled:
                        pass
                    open({freed}, 'w').close()
                    print(len(failed), (await asyncio.wait_for(child.channels['fizz'].request('b'), 10)).n)
                    "#,
                    freed = freed.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1 2\n");
}

/// Five requests sent before the child receives any, served one per round:
/// every round after the first is opened by the host with no arrival after
/// the five, counting what is still waiting, and all five settle with the
/// replies that carry their ids. Then the same five received in the first
/// round and answered one per round: each later opening names the unanswered
/// ones by id, with how long each has waited.
#[tokio::test]
async fn serial_service_settles_a_whole_batch() {
    const ANSWER_ONE: &str =
        "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=d.id))";
    const SEND_FIVE: &str = r#"
        child = await runtime.spawn('q', prompt='ROLE queue', requests={'fizz': (str, Thing)})
        hs = [child.channels['fizz'].request(str(n)) for n in range(5)]
        print(*[h.id for h in hs])
        settled = []
        for h in hs:
            settled.append((await h).n == h.id)
        print(settled)
        "#;
    const ALL: &str = "[True, True, True, True, True]";

    let mut one_each = Vec::new();
    for n in 1..=5 {
        one_each.extend(child_runs(&format!("c{n}"), ANSWER_ONE));
    }
    let mut served = tree(
        "OUTRIG_TEST_CHILD_SERIAL",
        "",
        vec![
            ("ROLE queue", scripted(one_each)),
            ("", primary_waits(SEND_FIVE)),
        ],
    )
    .await;
    round(&mut served.session, "go").await;
    let (_, events, requests) = served.end().await;
    let said = printed(&requests, "p1");
    assert_eq!(said.lines().nth(1), Some(ALL), "{said}");
    let child = of_agent(&requests, "ROLE queue");
    assert_eq!(child.len(), 10);
    for (round, waiting) in [
        (1, "4 requests are"),
        (2, "3 requests are"),
        (3, "2 requests are"),
        (4, "1 request is"),
    ] {
        let text = opening(child[2 * round]);
        assert!(
            text.contains(&format!("{waiting} waiting on runtime.channels[\"fizz\"]"))
                && !text.contains("you took"),
            "{text}"
        );
    }
    assert!(kind(&events, "agent.request.invalid", None).is_empty());

    let mut received_first = child_runs(
        "c1",
        r#"
        ds = []
        while len(ds) < 5:
            ds.append(await runtime.channels['fizz'].receive())
        d = ds.pop(0)
        await d.reply(Thing(n=d.id))
        "#,
    );
    for n in 2..=5 {
        received_first.extend(child_runs(
            &format!("c{n}"),
            "d = ds.pop(0)\nawait d.reply(Thing(n=d.id))",
        ));
    }
    let mut taken = tree(
        "OUTRIG_TEST_CHILD_SERIAL_TAKEN",
        "",
        vec![
            ("ROLE queue", scripted(received_first)),
            ("", primary_waits(SEND_FIVE)),
        ],
    )
    .await;
    round(&mut taken.session, "go").await;
    let (_, events, requests) = taken.end().await;
    let said = printed(&requests, "p1");
    assert_eq!(said.lines().nth(1), Some(ALL), "{said}");
    let ids: Vec<&str> = said.lines().next().expect("the ids").split(' ').collect();
    let child = of_agent(&requests, "ROLE queue");
    assert_eq!(child.len(), 10);
    for round in 1..=4 {
        let text = opening(child[2 * round]);
        for (n, id) in ids.iter().enumerate() {
            let named = text.contains(&format!("you took request {id} from"));
            assert_eq!(
                named,
                n >= round,
                "request {id} in round {}: {text}",
                round + 1
            );
        }
        assert_eq!(
            text.matches("s ago and have not answered it").count(),
            5 - round,
            "{text}"
        );
        assert!(!text.contains("waiting on"), "{text}");
    }
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
}

/// Five requests waiting, the child answering one per round; during the first
/// round, after its reply, a sixth arrives, so the result says five wait. The
/// host still opens the next round -- the reply changed what the child holds
/// -- whose opening counts five, and all six settle.
#[tokio::test]
async fn an_arrival_during_the_round_does_not_hide_a_reply() {
    let flag = Flag::new();
    let mut script = child_runs(
        "c1",
        &format!(
            r#"
            d = await runtime.channels['fizz'].receive()
            await d.reply(Thing(n=d.id))
            open({flag}, 'w').close()
            while runtime.channels['fizz'].pending() < 5:
                await asyncio.sleep(0.01)
            "#,
            flag = flag.py()
        ),
    );
    for n in 2..=6 {
        script.extend(child_runs(
            &format!("c{n}"),
            "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=d.id))",
        ));
    }
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_ARRIVAL",
        "",
        vec![
            ("ROLE queue", scripted(script)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('q', prompt='ROLE queue', requests={{'fizz': (str, Thing)}})
                    hs = [child.channels['fizz'].request(str(n)) for n in range(5)]
                    await until(lambda: os.path.exists({flag}))
                    hs.append(child.channels['fizz'].request('5'))
                    settled = []
                    for h in hs:
                        settled.append((await h).n == h.id)
                    print(settled)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "[True, True, True, True, True, True]\n"
    );
    let child = of_agent(&requests, "ROLE queue");
    assert_eq!(child.len(), 12);
    let result = tool_result(child[1], "c1");
    assert!(
        result.starts_with("[5 requests are waiting on runtime.channels[\"fizz\"]]"),
        "{result}"
    );
    let next = opening(child[2]);
    assert!(
        next.contains("5 requests are waiting on runtime.channels[\"fizz\"]"),
        "{next}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
}

/// Four requests sent before the child receives any: `receive()` gives them
/// in send order, each delivery carrying its handle's id; a reply settles the
/// handle it names and no other; `fail` settles one with its message while the
/// child's code goes on; and a reply past the attempt limit ends that request
/// rejected, with the last value and its problems, while the child answers
/// the rest and no model call follows.
#[tokio::test]
async fn requests_are_answered_in_order_and_each_answer_settles_its_own() {
    let flag = Flag::new();
    let child = child_runs(
        "c1",
        &format!(
            r#"
            import os
            ds = []
            for _ in range(4):
                ds.append(await runtime.channels['fizz'].receive())
            a, b, c, d = ds
            print(a.id, b.id, c.id, d.id, a.body, b.body, c.body, d.body)
            await a.reply(Thing(n=1))
            while not os.path.exists({flag}):
                await asyncio.sleep(0.01)
            await b.fail('why')
            for _ in range(3):
                try:
                    await c.reply({{'n': 'three'}})
                except Exception as e:
                    print(type(e).__name__)
            await d.reply(Thing(n=4))
            "#,
            flag = flag.py()
        ),
    );
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_IN_ORDER",
        "",
        vec![
            ("ROLE orderly", scripted(child)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('o', prompt='ROLE orderly', requests={{'fizz': (str, Thing)}})
                    hs = [child.channels['fizz'].request(s) for s in ('one', 'two', 'three', 'four')]
                    print(*[h.id for h in hs])
                    await until(lambda: hs[0].done())
                    print(hs[0].result().n, hs[1].done(), hs[1].status)
                    open({flag}, 'w').close()
                    for h in hs[1:]:
                        try:
                            print((await h).n)
                        except outrig.AgentRequestFailed as e:
                            print('failed:', e.message)
                        except outrig.CompletionRejected as e:
                            print('rejected:', e.value, [str(p) for p in e.problems])
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let ids = said.lines().next().expect("the ids").to_string();
    assert_eq!(
        said,
        format!(
            "{ids}\n1 False received (child running)\nfailed: why\n\
             rejected: {{'n': 'three'}} ['$.n: expected an integer, not a string']\n4\n"
        )
    );
    let child = of_agent(&requests, "ROLE orderly");
    assert_eq!(child.len(), 2, "no model call follows the rejection");
    assert_eq!(
        tool_result(child[1], "c1"),
        format!("{ids} one two three four\nDecodeError\nDecodeError\nCompletionRejected\n")
    );
    assert_eq!(kind(&events, "agent.request.invalid", None).len(), 3);
}

// ---------------------------------------------------------------------------- handles

/// Cancelling the code that awaits a handle cancels that wait and never the
/// work: the submission settles later, and a new await returns its result.
#[tokio::test]
async fn a_cancelled_waiter_leaves_the_work_running() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_WAITER",
        "",
        vec![
            (
                "ROLE patient",
                scripted(child_runs(
                    "c1",
                    &format!(
                        r#"
                        import os
                        while not os.path.exists({flag}):
                            await asyncio.sleep(0.01)
                        await runtime.complete(Total(total=sum(xs)))
                        "#,
                        flag = flag.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('p', prompt='ROLE patient')
                    h = child.submit({{'xs': [1, 2]}}, result=Total)
                    await until(lambda: h.status.state == 'running')
                    try:
                        await asyncio.wait_for(h, 0.05)
                    except asyncio.TimeoutError:
                        print('gave up', h.done(), h.status)
                    waiter = asyncio.ensure_future(asyncio.wait({{h.future}}))
                    await asyncio.sleep(0.05)
                    waiter.cancel()
                    open({flag}, 'w').close()
                    print((await h).total, h.status)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "gave up False running (child running)\n3 accepted (child running)\n"
    );
}

/// `h.cancel()` ends the submission and not the child: the child's answer to
/// it then raises, evented, and the next submission, queued behind it, gets
/// the child's next round; a queued submission cancelled never reaches the
/// child. A second completion after one was accepted is refused the same
/// way, and the handle keeps its result.
#[tokio::test]
async fn a_cancel_ends_the_submission_not_the_child() {
    let flag = Flag::new();
    let mut child = child_runs(
        "c1",
        &format!(
            r#"
            import os
            while not os.path.exists({flag}):
                await asyncio.sleep(0.01)
            try:
                await runtime.complete(Total(total=1))
            except Exception as e:
                print(type(e).__name__)
            "#,
            flag = flag.py()
        ),
    );
    child.extend(child_runs(
        "c2",
        r#"
        await runtime.complete(Total(total=sum(xs)))
        try:
            await runtime.complete(Total(total=0))
        except Exception as e:
            print(type(e).__name__)
        "#,
    ));
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_CANCEL_WORK",
        "",
        vec![
            ("ROLE busy", scripted(child)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('b', prompt='ROLE busy')
                    h1 = child.submit({{'xs': [1]}}, result=Total)
                    await until(lambda: h1.status.state == 'running')
                    h2 = child.submit({{'xs': [2, 3]}}, result=Total)
                    h3 = child.submit({{'xs': [4]}}, result=Total)
                    print(h1.id, h2.id, h3.id)
                    print(h2.status, h3.status)
                    print(h3.cancel(), h3.status)
                    print(h1.cancel(), h1.status, child.status)
                    open({flag}, 'w').close()
                    print((await h2).total, h2.status)
                    await until(lambda: child.status == 'idle')
                    print((await h2).total)
                    for h in (h1, h3):
                        try:
                            await h
                        except outrig.RequestCancelled:
                            print('cancelled')
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let ids = said.lines().next().expect("the ids").to_string();
    let third = ids.split(' ').nth(2).expect("a third id").to_string();
    assert_eq!(
        said,
        format!(
            "{ids}\nqueued (child running) queued (child running)\n\
             True failed: cancelled (child running)\n\
             True failed: cancelled (child running) running\n\
             5 accepted (child running)\n5\ncancelled\ncancelled\n"
        )
    );
    let child = of_agent(&requests, "ROLE busy");
    assert_eq!(
        child.len(),
        4,
        "a round for the cancelled submission and one for the next"
    );
    assert_eq!(tool_result(child[1], "c1"), "RequestCancelled\n");
    assert_eq!(tool_result(child[3], "c2"), "RequestSettled\n");
    assert!(
        child
            .iter()
            .all(|request| !opening(request).contains(&format!("submission {third} "))),
        "the cancelled queued submission never reached the child"
    );
    let refused: Vec<Value> = kind(&events, "agent.request.refused", None)
        .into_iter()
        .map(|event| data(event)["ended"].clone())
        .collect();
    assert_eq!(refused, ["cancelled", "accepted"]);
}

/// What a work child sends on its `progress` channel arrives on the handle's
/// `progress`, in order, as deliveries, while the handle stays unsettled and
/// reads running; the completion then settles it. Each report is evented on
/// the submission.
#[tokio::test]
async fn progress_arrives_in_order_and_settles_nothing() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_PROGRESS",
        "",
        vec![
            (
                "ROLE reporter",
                scripted(child_runs(
                    "c1",
                    &format!(
                        r#"
                        import os
                        for step in ('a', 'b', 'c'):
                            await runtime.channels['progress'].send(step)
                        while not os.path.exists({flag}):
                            await asyncio.sleep(0.01)
                        await runtime.complete(Total(total=len(xs)))
                        "#,
                        flag = flag.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('r', prompt='ROLE reporter')
                    h = child.submit({{'xs': [1, 2]}}, result=Total)
                    for _ in range(3):
                        d = await h.progress.receive()
                        print(type(d).__name__, d.body, d.sender, h.done(), h.status)
                    open({flag}, 'w').close()
                    print((await h).total)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "Delivery a c1 False running (child running)\n\
         Delivery b c1 False running (child running)\n\
         Delivery c c1 False running (child running)\n2\n"
    );
    let reported: Vec<Value> = kind(&events, "agent.request.progress", Some("agent/c1"))
        .into_iter()
        .map(|event| data(event)["body"].clone())
        .collect();
    assert_eq!(reported, ["a", "b", "c"]);
}

/// A handle is waited on by `runtime.wait` and, through `future`, by
/// `asyncio.wait`, each returning when one settles with that one done; a
/// wait cancelled leaves both requests running, and they settle later. A
/// reply to a request that has ended raises in the child, evented, and the
/// handle keeps its value.
#[tokio::test]
async fn handles_work_in_runtime_wait_and_asyncio_wait() {
    let (first, second) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_WAITS",
        "",
        vec![
            (
                "ROLE waited",
                scripted(child_runs(
                    "c1",
                    &format!(
                        r#"
                        import os
                        async def flag(path):
                            while not os.path.exists(path):
                                await asyncio.sleep(0.01)
                        d1 = await runtime.channels['fizz'].receive()
                        d2 = await runtime.channels['fizz'].receive()
                        await flag({first})
                        await d2.reply(Thing(n=2))
                        await flag({second})
                        await d1.reply(Thing(n=1))
                        try:
                            await d1.reply(Thing(n=9))
                        except Exception as e:
                            print(type(e).__name__, e.ended)
                        "#,
                        first = first.py(),
                        second = second.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    await runtime.channels['user'].receive()
                    child = await runtime.spawn('w', prompt='ROLE waited', requests={{'fizz': (str, Thing)}})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    either = dict(return_when=asyncio.FIRST_COMPLETED)
                    one = asyncio.ensure_future(runtime.wait({{h1, h2}}, **either))
                    other = asyncio.ensure_future(asyncio.wait({{h1.future, h2.future}}, **either))
                    await asyncio.sleep(0.05)
                    one.cancel()
                    other.cancel()
                    print(h1.done(), h2.done())
                    open({first}, 'w').close()
                    done, pending = await runtime.wait({{h1, h2}}, **either)
                    print(done == {{h2}}, pending == {{h1}})
                    done, pending = await asyncio.wait({{h1.future, h2.future}}, **either)
                    print(done == {{h2.future}}, pending == {{h1.future}})
                    open({second}, 'w').close()
                    print((await h1).n, (await h2).n)
                    await until(lambda: child.status == 'idle')
                    print((await h1).n)
                    "#,
                    first = first.py(),
                    second = second.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "False False\nTrue True\nTrue True\n1 2\n1\n"
    );
    let child = of_agent(&requests, "ROLE waited");
    assert_eq!(tool_result(child[1], "c1"), "RequestSettled accepted\n");
    let refused = kind(&events, "agent.request.refused", None);
    assert_eq!(refused.len(), 1);
    assert_eq!(data(refused[0])["ended"], "accepted");
}

/// A task nobody named that only awaits a handle, as `asyncio.ensure_future` makes of one, is told
/// of in a wait as the handle is -- by its request and its child -- in what the wait is on and in
/// what was done when it ended, though a finished task's frame no longer says. That holds for one
/// that finished during the first second, before the wait was told of. A task the code named keeps
/// its name, whether it was named before the wait or while it ran, and the sets `runtime.wait`
/// returns hold the tasks.
#[tokio::test]
async fn a_wait_on_a_wrapped_handle_names_the_handle() {
    let (quick, release) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_WRAPPED_WAIT",
        "",
        vec![
            (
                "ROLE held",
                scripted(child_runs(
                    "c1",
                    &format!(
                        r#"
                        import os
                        async def until(path):
                            while not os.path.exists(path):
                                await asyncio.sleep(0.01)
                        d = await runtime.channels['fizz'].receive()
                        await until({quick})
                        await d.reply(Thing(n=1))
                        d = await runtime.channels['fizz'].receive()
                        await until({release})
                        await d.reply(Thing(n=2))
                        "#,
                        quick = quick.py(),
                        release = release.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    await runtime.channels['user'].receive()
                    child = await runtime.spawn('w', prompt='ROLE held', requests={{'fizz': (str, Thing)}})
                    first = child.channels['fizz'].request('a')
                    second = child.channels['fizz'].request('b')
                    early = asyncio.ensure_future(first)
                    bare = asyncio.ensure_future(second)
                    named = asyncio.ensure_future(second)
                    named.set_name('mine')
                    renamed = asyncio.ensure_future(second)
                    # The wait is told of once it has lasted a second: `early` is done by then, and
                    # `bare`, `named` and `renamed` are done after. `renamed` is named while the
                    # wait runs.
                    loop = asyncio.get_running_loop()
                    loop.call_later(0.2, lambda: renamed.set_name('late'))
                    loop.call_later(0.3, lambda: open({quick}, 'w').close())
                    loop.call_later(1.3, lambda: open({release}, 'w').close())
                    fs = {{early, bare, named, renamed}}
                    done, pending = await runtime.wait(fs)
                    print(done == fs, pending == set(), (await first).n, (await second).n)
                    "#,
                    quick = quick.py(),
                    release = release.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True True 1 2\n");

    let waited = |kind_of: &str, field: &str| -> Vec<Value> {
        let found = kind(&events, kind_of, Some("agent/primary"));
        assert_eq!(found.len(), 1, "{kind_of}: {found:?}");
        data(found[0])[field].as_array().cloned().expect("a list")
    };
    let on = waited("exec.wait.started", "on");
    assert_eq!(on, waited("exec.wait.ended", "done"));
    assert_eq!(on.len(), 4, "{on:?}");
    // Sorted by name: the two tasks the code named, then the two requests on the child `w`.
    for (task, name) in on[..2].iter().zip(["late", "mine"]) {
        assert_eq!(task["kind"], "task", "{on:?}");
        assert_eq!(task["name"], name);
    }
    for request in &on[2..] {
        assert_eq!(request["kind"], "request", "{on:?}");
        assert_eq!(request["channel"], "fizz");
        assert_eq!(request["child"], "c1");
        assert_eq!(request["name"], "w");
    }
    assert!(on[2]["id"].is_u64() && on[2]["id"] != on[3]["id"], "{on:?}");
}

// ---------------------------------------------------------------------------- limits and the tree

/// A request child's round that receives one request and replies with its id.
const ANSWER_ONE: &str =
    "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=d.id))";

/// A responder for any number of children's rounds, each served the same:
/// a round's first call runs `source`, and the call after its result ends the
/// round.
pub(super) fn serving(source: &str) -> Responder {
    let source = source.to_string();
    let next = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &RecordedRequest| {
        let canned = if after_result(request) {
            text_reply("ended")
        } else {
            submit(
                &format!("s{}", next.fetch_add(1, Ordering::SeqCst)),
                &py(&source),
            )
        };
        Box::pin(async move { canned })
    })
}

/// Whether `request` is the call after one of the agent's executions: it
/// carries that execution's result.
pub(super) fn after_result(request: &RecordedRequest) -> bool {
    messages(request)
        .last()
        .and_then(|message| message["content"].as_array())
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
}

/// A request held at the provider: `arrived` is told as it comes in, and it
/// is answered once `go` is.
#[derive(Default)]
pub(super) struct Hold {
    pub(super) arrived: Notify,
    pub(super) go: Notify,
}

/// `script`, as [`scripted`] answers it, its first response held by `hold`.
pub(super) fn held(script: Vec<CannedResponse>, hold: Arc<Hold>) -> Responder {
    let answer = scripted(script);
    let first = Arc::new(AtomicBool::new(true));
    Arc::new(move |request: &RecordedRequest| {
        let canned = answer(request);
        let hold = first
            .swap(false, Ordering::SeqCst)
            .then(|| Arc::clone(&hold));
        Box::pin(async move {
            if let Some(hold) = hold {
                hold.arrived.notify_one();
                hold.go.notified().await;
            }
            canned.await
        })
    })
}

/// Wait until `flag` is raised, failing rather than hanging.
pub(super) async fn raised(flag: &Flag) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !flag.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the flag was raised in time");
}

/// The child marker -- `ROLE leaf` -- whose agent made `request`, or `""` for
/// the primary.
fn role(request: &RecordedRequest) -> String {
    let system = system_text(&request.body);
    system
        .split_once("ROLE ")
        .and_then(|(_, rest)| rest.split(|c: char| !c.is_alphanumeric()).next())
        .map_or_else(String::new, |word| format!("ROLE {word}"))
}

/// What each execution of `subject`'s printed, in order.
pub(super) fn exec_outputs(events: &[Arc<Event>], subject: &str) -> Vec<String> {
    kind(events, "exec.completed", Some(subject))
        .into_iter()
        .map(|event| {
            data(event)["output"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// Where the event of `kind` that `test` picks sits among `events`.
pub(super) fn position(
    events: &[Arc<Event>],
    kind: &str,
    test: impl Fn(&Event) -> bool,
) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind() == kind && test(event))
        .map(|(n, _)| n)
        .collect()
}

/// Twenty coroutines each spawn a child, submit, await and release, under a
/// set token budget and `model-concurrency-max = 8`, against a provider that
/// holds each request until eight are open: all twenty children are live at
/// once, launched before any child's model call, the provider never holds
/// more than eight requests, and all twenty settle within the budget.
#[tokio::test]
async fn twenty_children_run_at_once_under_the_permits() {
    let mut tree = tree_held(
        "OUTRIG_TEST_CHILD_GATHER",
        "token-budget = 1000000\nmodel-concurrency-max = 8",
        vec![
            (
                "ROLE gatherer",
                serving("await runtime.complete(Total(total=n * 2))"),
            ),
            (
                "",
                primary_waits(
                    r#"
                    launched = []
                    everyone = asyncio.Event()
                    async def one(n):
                        child = await runtime.spawn(f'g{n}', prompt='ROLE gatherer')
                        launched.append(child)
                        if len(launched) == 20:
                            everyone.set()
                        await everyone.wait()
                        result = await child.submit({'n': n}, result=Total)
                        await runtime.release([child])
                        return result.total
                    totals = await asyncio.gather(*(one(n) for n in range(20)))
                    print(totals == [n * 2 for n in range(20)])
                    "#,
                ),
            ),
        ],
        Some(8),
    )
    .await;
    round(&mut tree.session, "go").await;
    assert!(!tree.session.ledger().budget_spent());
    let gauge = Arc::clone(&tree.gauge);
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True\n");
    assert_eq!(gauge.most(), 8, "eight at once, and never more");
    let spawned = position(&events, "agent.spawned", |_| true);
    assert_eq!(spawned.len(), 20);
    let first_release = position(&events, "agent.released", |_| true)[0];
    let first_child_call = position(&events, "model.attempt", |event| {
        event
            .subject
            .as_ref()
            .is_some_and(|s| s.as_str() != "agent/primary")
    })[0];
    assert!(spawned[19] < first_release, "all twenty were live at once");
    assert!(
        spawned[19] < first_child_call,
        "no launch waited on a model call"
    );
}

/// With 64 children resident -- two mid-round, the rest idle -- a 65th
/// spawn raises at once with nothing launched; releasing one lets the next
/// launch.
#[tokio::test]
async fn the_65th_launch_fails_at_once() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SIXTY_FIVE",
        "",
        vec![
            (
                "ROLE busy",
                serving(&format!(
                    "import os\n\
                     d = await runtime.channels['fizz'].receive()\n\
                     while not os.path.exists({flag}):\n    await asyncio.sleep(0.01)\n\
                     await d.reply(Thing(n=1))",
                    flag = flag.py()
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    spec = {{'fizz': (str, Thing)}}
                    children = []
                    for n in range(64):
                        role = 'ROLE busy' if n < 2 else 'ROLE idle'
                        children.append(await runtime.spawn(f'k{{n}}', prompt=role, requests=spec))
                    hs = [children[n].channels['fizz'].request('x') for n in range(2)]
                    await until(lambda: all(h.status.state == 'received' for h in hs))
                    started = time.monotonic()
                    try:
                        await runtime.spawn('one too many', prompt='ROLE idle')
                    except outrig.AgentLimitReached as e:
                        print('refused', time.monotonic() - started < 1, e)
                    await runtime.release([children[-1]])
                    print((await runtime.spawn('in its place', prompt='ROLE idle')).id)
                    open({flag}, 'w').close()
                    print([(await h).n for h in hs])
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "refused True children-max (64) children are resident in this session; release one \
         before launching another\nc65\n[1, 1]\n"
    );
    let spawned = kind(&events, "agent.spawned", None);
    assert_eq!(spawned.len(), 65);
    assert!(
        spawned
            .iter()
            .all(|event| data(event)["name"] != "one too many")
    );
}

/// With `children-max = 2`: a child whose code holds its loop is found
/// wedged at its release and still counts, a released one does not, so one
/// spawn succeeds and the next raises.
#[tokio::test]
async fn a_wedged_child_counts_and_a_released_one_does_not() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_WEDGED",
        "children-max = 2",
        vec![
            (
                "ROLE stuck",
                scripted(child_runs(
                    "c1",
                    &format!(
                        r#"
                        import os, time
                        d = await runtime.channels['fizz'].receive()
                        while not os.path.exists({flag}):
                            time.sleep(0.01)
                        "#,
                        flag = flag.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    a = await runtime.spawn('a', prompt='ROLE stuck', requests={{'fizz': (str, Thing)}})
                    h = a.channels['fizz'].request('x')
                    await until(lambda: h.status.state == 'received')
                    await asyncio.sleep(0.1)
                    await runtime.release([a])
                    print(a.status)
                    b = await runtime.spawn('b', prompt='ROLE other')
                    await runtime.release([b])
                    print(b.status)
                    c = await runtime.spawn('c', prompt='ROLE other')
                    try:
                        await runtime.spawn('d', prompt='ROLE other')
                    except outrig.AgentLimitReached as e:
                        print('refused', e)
                    open({flag}, 'w').close()
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "wedged\nreleased\nrefused children-max (2) children are resident in this session; \
         release one before launching another\n"
    );
    assert_eq!(kind(&events, "agent.wedged", Some("agent/c1")).len(), 1);
}

/// With `model-concurrency-max = 1` and the one permit held by a request the
/// provider keeps open, a second child's round queues for it; releasing that
/// child takes its request out of the queue, and when the held request
/// completes a third child's is sent at once. The permit count reads 1
/// throughout, then 0.
#[tokio::test]
async fn releasing_a_child_frees_its_queued_place() {
    let hold = Arc::new(Hold::default());
    let (sent, go) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_QUEUED_PLACE",
        "model-concurrency-max = 1",
        vec![
            (
                "ROLE hold",
                held(child_runs("c1", ANSWER_ONE), Arc::clone(&hold)),
            ),
            ("ROLE queued", scripted(child_runs("c1", ANSWER_ONE))),
            ("ROLE third", scripted(child_runs("c1", ANSWER_ONE))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    spec = {{'fizz': (str, Thing)}}
                    a = await runtime.spawn('a', prompt='ROLE hold', requests=spec)
                    ha = a.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({sent}))
                    b = await runtime.spawn('b', prompt='ROLE queued', requests=spec)
                    hb = b.channels['fizz'].request('b')
                    await until(lambda: b.status == 'running')
                    await asyncio.sleep(0.1)
                    await runtime.release([b])
                    c = await runtime.spawn('c', prompt='ROLE third', requests=spec)
                    hc = c.channels['fizz'].request('c')
                    await until(lambda: c.status == 'running')
                    await asyncio.sleep(0.1)
                    open({go}, 'w').close()
                    print((await ha).n == ha.id, (await hc).n == hc.id)
                    try:
                        await hb
                    except outrig.AgentReleased:
                        print('released')
                    "#,
                    sent = sent.py(),
                    go = go.py()
                )),
            ),
        ],
    )
    .await;
    let ledger = tree.session.ledger().clone();
    let gauge = Arc::clone(&tree.gauge);
    let (end, counts) = tokio::join!(round_end(&mut tree.session, "go"), async {
        tokio::time::timeout(Duration::from_secs(20), hold.arrived.notified())
            .await
            .expect("the first child's request arrived");
        let held = ledger.places_held();
        sent.raise();
        raised(&go).await;
        let queued = (ledger.places_held(), gauge.open());
        hold.go.notify_one();
        (held, queued)
    });
    assert_eq!(end.reply, "done");
    assert_eq!(
        counts,
        (1, (1, 1)),
        "one held, and the provider holds only that one"
    );
    assert_eq!(tree.session.ledger().places_held(), 0);
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True True\nreleased\n");
    let order: Vec<String> = requests.iter().map(role).collect();
    assert!(!order.contains(&"ROLE queued".to_string()), "{order:?}");
    assert_eq!(order[..3], ["", "ROLE hold", "ROLE third"], "{order:?}");
}

/// A work child's round ends on the accepted submission only when nothing
/// else waits: with a second submission queued, the round carries on past the
/// first, whose completion ends nothing, and the second gets a round that
/// ends on its own acceptance.
#[tokio::test]
async fn a_work_child_with_a_second_submission_queued_keeps_its_round() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SECOND_SUBMISSION",
        "",
        vec![
            (
                "ROLE adder",
                serving("await runtime.complete(Total(total=sum(xs)))"),
            ),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('adder', prompt='ROLE adder: add them up')
                    a = child.submit({'xs': [1, 2]}, result=Total)
                    b = child.submit({'xs': [3, 4]}, result=Total)
                    print((await a).total, (await b).total)
                    await runtime.release([child])
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "3 7\n");
    let stopped: Vec<Value> = kind(&events, "model.round.completed", Some("agent/c1"))
        .into_iter()
        .map(|event| data(event)["stopped"].clone())
        .collect();
    assert_eq!(stopped.len(), 2, "{stopped:?}");
    assert_eq!(stopped[0], Value::Null, "the first round was not cut");
    assert!(
        stopped[1]
            .as_str()
            .is_some_and(|why| why.contains("was accepted and nothing else waits")),
        "{stopped:?}"
    );
    assert_eq!(
        of_agent(&requests, "ROLE adder").len(),
        3,
        "the first round spoke after completing; the second did not"
    );
}

/// A request child that answered one of two requests keeps its round: what it
/// holds open is its requests, and it answers them as it chooses, so nothing
/// says it is done. The first round ends on the model's own word, after a
/// second call. (The second ends however the release that follows its answer
/// finds it, which is not this test's to pin.)
#[tokio::test]
async fn a_request_child_with_a_second_request_waiting_keeps_its_round() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SECOND_REQUEST",
        "",
        vec![
            ("ROLE fizz", serving(ANSWER_ONE)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn(
                        'fizz', prompt='ROLE fizz', requests={'fizz': (str, Thing)})
                    a = child.channels['fizz'].request('a')
                    b = child.channels['fizz'].request('b')
                    print((await a).n == a.id, (await b).n == b.id)
                    await runtime.release([child])
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True True\n");
    let rounds = kind(&events, "model.round.completed", Some("agent/c1"));
    let first = data(rounds[0]);
    assert_eq!(first["stopped"], Value::Null, "the first round was not cut");
    assert_eq!(
        first["calls"].as_array().map(Vec::len),
        Some(2),
        "it answered one request, then made another call: {first}"
    );
}

/// A release that finds its child in a model call cannot end the round, which
/// is waiting on the provider: it gives the round its wait, and then drops it.
/// The drop is recorded under the child before the release is, and the
/// child's runner is over with it -- the child is gone.
#[tokio::test]
async fn releasing_a_child_in_a_model_call_drops_its_round_when_the_wait_is_over() {
    let hold = Arc::new(Hold::default());
    let (sent, over) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_RELEASE_IN_CALL",
        "",
        vec![
            (
                "ROLE slow",
                held(child_runs("c1", ANSWER_ONE), Arc::clone(&hold)),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn(
                        'slow', prompt='ROLE slow', requests={{'fizz': (str, Thing)}})
                    h = child.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({sent}))
                    await runtime.release([child])
                    print(child.status)
                    open({over}, 'w').close()
                    "#,
                    sent = sent.py(),
                    over = over.py()
                )),
            ),
        ],
    )
    .await;
    let (end, ()) = tokio::join!(round_end(&mut tree.session, "go"), async {
        tokio::time::timeout(Duration::from_secs(20), hold.arrived.notified())
            .await
            .expect("the child's request arrived");
        sent.raise();
        raised(&over).await;
        hold.go.notify_one();
    });
    assert_eq!(end.reply, "done");
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "released\n");
    assert_eq!(
        kind(&events, "model.round.dropped", Some("agent/c1")).len(),
        1
    );
    assert!(kind(&events, "model.round.completed", Some("agent/c1")).is_empty());
    assert_eq!(kind(&events, "agent.stopped", Some("agent/c1")).len(), 1);
    let dropped = position(&events, "model.round.dropped", |_| true);
    let released = position(&events, "agent.released", |_| true);
    assert!(
        dropped[0] < released[0],
        "the release is recorded once the round it dropped is: {dropped:?} before {released:?}"
    );
}

/// A session's close that comes while a release waits on its child's round
/// stops that round at once, as it stops every child's, and does not leave it
/// to the wait: the release holds the runner to wait on it, and the registry
/// must still be able to stop it. The child's model call is in flight when the
/// release begins, so only the close ends the round before the wait does.
#[tokio::test]
async fn a_close_during_a_releases_wait_stops_the_released_childs_round() {
    let hold = Arc::new(Hold::default());
    let (sent, releasing) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_CLOSE_IN_RELEASE",
        "",
        vec![
            (
                "ROLE slow",
                held(child_runs("c1", ANSWER_ONE), Arc::clone(&hold)),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn(
                        'slow', prompt='ROLE slow', requests={{'fizz': (str, Thing)}})
                    h = child.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({sent}))
                    open({releasing}, 'w').close()
                    await runtime.release([child])
                    "#,
                    sent = sent.py(),
                    releasing = releasing.py()
                )),
            ),
        ],
    )
    .await;
    let closer = tree.session.closer();
    let events = Arc::clone(&tree.events);
    let count = move |kind: &str| {
        events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind() == kind)
            .count()
    };
    let (_, elapsed) = tokio::join!(round_end(&mut tree.session, "go"), async {
        tokio::time::timeout(Duration::from_secs(20), hold.arrived.notified())
            .await
            .expect("the child's request arrived");
        sent.raise();
        raised(&releasing).await;
        // The release has the runner by now, and waits on its model call.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(count("agent.released"), 0, "the release is still waiting");
        let closed = std::time::Instant::now();
        closer.close_admission();
        tokio::time::timeout(Duration::from_secs(20), async {
            while count("model.round.dropped") == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the round was dropped");
        let elapsed = closed.elapsed();
        hold.go.notify_one();
        elapsed
    });
    assert!(
        elapsed < RELEASE_WAIT / 2,
        "the close stopped the round, not the release's wait: {elapsed:?}"
    );
    tree.end().await;
}

/// With `model-concurrency-max = 1`, a parent awaiting a child awaiting a
/// grandchild completes: no execution holds the permit while it waits, and
/// the three agents' requests go one after another.
#[tokio::test]
async fn a_waiting_parent_holds_no_permit() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_THREE_LEVELS",
        "model-concurrency-max = 1",
        vec![
            (
                "ROLE leaf",
                scripted(child_runs("l1", "await runtime.complete(Total(total=1))")),
            ),
            (
                "ROLE middle",
                scripted(child_runs(
                    "m1",
                    r#"
                    leaf = await runtime.spawn('leaf', prompt='ROLE leaf')
                    below = await leaf.submit({}, result=Total)
                    await runtime.complete(Total(total=below.total + 1))
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    middle = await runtime.spawn('middle', prompt='ROLE middle')
                    print((await middle.submit({}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let gauge = Arc::clone(&tree.gauge);
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "2\n");
    // Each agent's first call before the one it launched. A child's round
    // ends on its completion, so the primary's is the one call that follows.
    let order: Vec<String> = requests.iter().map(role).collect();
    assert_eq!(order, ["", "ROLE middle", "ROLE leaf", ""], "{order:?}");
    assert_eq!(gauge.most(), 1);
}

/// Each call reports 19 tokens, so the primary's first call and the child's
/// spend 38 of a 30-token budget: the child's next call is refused, its
/// submission ends `BudgetExhausted` in the waiter, and the main agent's
/// round stops naming `token-budget` -- and its next round too, at once.
#[tokio::test]
async fn a_spent_budget_ends_the_childs_work_and_stops_the_main_agent() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_BUDGET",
        "token-budget = 30",
        vec![
            (
                "ROLE spender",
                scripted(vec![
                    submit("c1", "print('working')"),
                    text_reply("never sent"),
                ]),
            ),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE spender')
                    h = child.submit({}, result=Total)
                    try:
                        await h
                    except outrig.BudgetExhausted as e:
                        print(type(e).__name__, h.status.state, h.status.reason)
                    "#,
                ),
            ),
        ],
    )
    .await;
    let first = round(&mut tree.session, "go").await;
    let second = round(&mut tree.session, "again").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["BudgetExhausted failed budget\n"]
    );
    assert_eq!(
        of_agent(&requests, "ROLE spender").len(),
        1,
        "none after the budget was spent"
    );
    assert_eq!(primary(&requests).len(), 1, "nor any for the main agent");
    for stopped in [&first, &second] {
        assert!(
            stopped.contains("token-budget (30 tokens) is spent"),
            "{stopped}"
        );
    }
}

/// An idle child holds nothing, so a budget another child spent ends nothing of its: a request
/// sent to it afterwards wakes its runner, which ends the request `BudgetExhausted` with no round,
/// and the child is idle, holding nothing.
#[tokio::test]
async fn a_request_to_an_idle_child_once_the_budget_is_spent_ends_at_once() {
    // Each call reports 19 tokens: the primary's first and the spender's first spend 38 of 30.
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_BUDGET_IDLE",
        "token-budget = 30",
        vec![
            ("ROLE idle", scripted(vec![text_reply("never sent")])),
            (
                "ROLE spender",
                scripted(vec![submit("b1", "print('spending')"), text_reply("never sent")]),
            ),
            (
                "",
                primary_waits(&format!(
                    "{WATCH}{}",
                    py(r#"
                    a = await runtime.spawn('a', prompt='ROLE idle', requests={'fizz': (str, Thing)})
                    b = await runtime.spawn('b', prompt='ROLE spender')
                    try:
                        await asyncio.wait_for(b.submit({}, result=Total), 10)
                    except outrig.BudgetExhausted:
                        print('spender budget')
                    h = a.channels['fizz'].request('x')
                    watch(a, [h])
                    try:
                        await asyncio.wait_for(h, 10)
                    except outrig.BudgetExhausted:
                        print('idle', h.status.reason)
                    await until(lambda: a.status == 'idle')
                    print(a.status, h.done(), idle_holding)
                    "#)
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["spender budget\nidle budget\nidle True []\n"]
    );
    assert!(
        of_agent(&requests, "ROLE idle").is_empty(),
        "no round for a request it could not answer"
    );
    assert_eq!(
        of_agent(&requests, "ROLE spender").len(),
        1,
        "no call past the budget"
    );
}

/// A round that fails is opened again with what it opened with: a work
/// child's retry shows the submission it was given, inputs and all, rather
/// than asking for a result the model never saw the request for.
#[tokio::test]
async fn a_failed_round_opens_again_with_what_it_opened_with() {
    let mut child = vec![mock_http::failure(500)];
    child.extend(child_runs(
        "c1",
        "await runtime.complete(Total(total=sum(xs)))",
    ));
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_RETRY",
        "",
        vec![
            ("ROLE flaky", scripted(child)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('f', prompt='ROLE flaky')
                    print((await child.submit({'xs': [1, 2]}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "3\n");
    let child = of_agent(&requests, "ROLE flaky");
    assert_eq!(
        child.len(),
        2,
        "the failed call, then the round again, which ends on the completion"
    );
    assert_eq!(opening(child[0]), opening(child[1]), "the same opening");
    assert!(
        opening(child[1]).contains("`xs` (list[int]): [1, 2]"),
        "{}",
        opening(child[1])
    );
    assert_eq!(
        kind(&events, "model.round.failed", Some("agent/c1")).len(),
        1
    );
}

/// A round that fails before its code ran, every time, is the same request
/// each time, so it is opened again only so often: the child then fails what
/// it holds, naming the error, and makes no call again -- not for the request
/// its round opened on, nor for one sent after. What it failed is not kept for
/// it either: a parent that sends request after request, each failing, leaves
/// none queued on the channel.
#[tokio::test]
async fn a_round_that_keeps_failing_ends_what_the_child_holds() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_FAILING",
        "",
        vec![
            ("ROLE failing", scripted(vec![mock_http::failure(500)])),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('f', prompt='ROLE failing', requests={'fizz': (str, Thing)})
                    for body in ('a', 'b', 'c'):
                        try:
                            await asyncio.wait_for(child.channels['fizz'].request(body), 30)
                        except outrig.AgentRequestFailed as e:
                            print('in a row' in e.message, 'failed on purpose' in e.message)
                    print(child.channels['fizz'].pending())
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "True True\nTrue True\nTrue True\n0\n"
    );
    assert_eq!(
        of_agent(&requests, "ROLE failing").len(),
        ROUND_FAILURES_MAX as usize,
        "one call for each round, and none after the last"
    );
    assert_eq!(
        kind(&events, "model.round.failed", Some("agent/c1")).len(),
        ROUND_FAILURES_MAX as usize
    );
}

/// Failures are counted in a row: a round that runs ends the count, so a child
/// that fails more rounds than the limit allows, with one between that ran,
/// answers both of its submissions.
#[tokio::test]
async fn a_round_that_runs_ends_the_count_of_failed_rounds() {
    let complete = "await runtime.complete(Total(total=sum(xs)))";
    let just_under = ROUND_FAILURES_MAX as usize - 1;
    let mut script = vec![mock_http::failure(500); just_under];
    script.extend(child_runs("c1", complete));
    script.extend(vec![mock_http::failure(500); just_under]);
    script.extend(child_runs("c2", complete));
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_FAILING_BETWEEN",
        "",
        vec![
            ("ROLE flaky", scripted(script)),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('f', prompt='ROLE flaky')
                    print((await child.submit({'xs': [1, 2]}, result=Total)).total)
                    print((await child.submit({'xs': [3, 4]}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "3\n7\n");
    assert_eq!(
        kind(&events, "model.round.failed", Some("agent/c1")).len(),
        2 * just_under
    );
}

/// A round that fails after its code ran is not opened again, since what the
/// code did stands: here it completed the first submission, and the call
/// after failed. Once the wait has passed, the next round opens on what is
/// open then -- the second submission, queued meanwhile -- and the first is
/// never given again.
#[tokio::test]
async fn a_round_that_fails_after_its_code_ran_opens_on_what_is_open_then() {
    let complete = py("await runtime.complete(Total(total=sum(xs)))");
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_RETRY_AFTER_WORK",
        "",
        vec![
            (
                "ROLE flaky",
                scripted(vec![
                    submit("c1", &complete),
                    mock_http::failure(500),
                    submit("c2", &complete),
                    text_reply("ended"),
                ]),
            ),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('f', prompt='ROLE flaky')
                    first = child.submit({'xs': [1, 2]}, result=Total)
                    second = child.submit({'xs': [3, 4]}, result=Total)
                    print((await first).total, (await second).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "3 7\n");
    let child = of_agent(&requests, "ROLE flaky");
    assert_eq!(
        child.len(),
        3,
        "the first round's two calls, one failing, and the second's, which ends on its completion"
    );
    let next = opening(child[2]);
    assert!(
        next.contains("`xs` (list[int]): [3, 4]") && !next.contains("[1, 2]"),
        "{next}"
    );
    assert_eq!(
        kind(&events, "model.round.failed", Some("agent/c1")).len(),
        1
    );
}

/// With one place among requests in flight, a child's request waiting for it
/// while another child's is answered finds the budget that answer spent: the
/// place passes on only once what the answer used is counted, and a request
/// that takes it is checked against the budget before it is sent. The waiting
/// request is never sent, and the request made of its child ends
/// `BudgetExhausted`.
#[tokio::test]
async fn a_request_waiting_for_its_place_is_not_sent_past_the_budget() {
    // Each call reports 19 tokens: the primary's first and the first child's
    // spend 38 of 30.
    let hold = Arc::new(Hold::default());
    let (sent, queued) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_BUDGET_QUEUED",
        "model-concurrency-max = 1\ntoken-budget = 30",
        vec![
            (
                "ROLE first",
                held(child_runs("a1", ANSWER_ONE), Arc::clone(&hold)),
            ),
            ("ROLE second", scripted(child_runs("b1", ANSWER_ONE))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    spec = {{'fizz': (str, Thing)}}
                    a = await runtime.spawn('a', prompt='ROLE first', requests=spec)
                    ha = a.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({sent}))
                    b = await runtime.spawn('b', prompt='ROLE second', requests=spec)
                    hb = b.channels['fizz'].request('b')
                    await until(lambda: b.status == 'running')
                    await asyncio.sleep(0.1)
                    open({queued}, 'w').close()
                    print((await ha).n == ha.id)
                    try:
                        await hb
                    except outrig.BudgetExhausted:
                        print('budget', hb.status.reason)
                    "#,
                    sent = sent.py(),
                    queued = queued.py()
                )),
            ),
        ],
    )
    .await;
    let (stopped, ()) = tokio::join!(round(&mut tree.session, "go"), async {
        tokio::time::timeout(Duration::from_secs(20), hold.arrived.notified())
            .await
            .expect("the first child's request arrived");
        sent.raise();
        raised(&queued).await;
        hold.go.notify_one();
    });
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["True\nbudget budget\n"]
    );
    assert_eq!(of_agent(&requests, "ROLE first").len(), 1);
    assert_eq!(of_agent(&requests, "ROLE second").len(), 0, "never sent");
    assert!(
        stopped.contains("token-budget (30 tokens) is spent"),
        "{stopped}"
    );
}

/// A child whose model can never answer -- the provider refuses its key --
/// fails what it holds and makes no call again: not for the request its round
/// opened on, nor for one sent after.
#[tokio::test]
async fn a_child_whose_model_cannot_answer_fails_what_it_holds() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_TERMINAL",
        "",
        vec![
            ("ROLE refused", scripted(vec![mock_http::failure(401)])),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('r', prompt='ROLE refused', requests={'fizz': (str, Thing)})
                    for body in ('a', 'b'):
                        h = child.channels['fizz'].request(body)
                        try:
                            await asyncio.wait_for(h, 10)
                        except outrig.AgentRequestFailed as e:
                            print(h.status.reason, 'will not recover' in e.message)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "failed True\nfailed True\n");
    assert_eq!(
        of_agent(&requests, "ROLE refused").len(),
        1,
        "no call after the first refusal"
    );
}

/// `requests-max` at its default: 256 requests sent at once to a child that
/// answers one per round are all accepted and settle with their ids, and the
/// 257th raises at once, never sent.
#[tokio::test]
async fn requests_max_refuses_the_257th() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_REQUESTS_MAX",
        "",
        vec![
            (
                "ROLE server",
                serving(&format!(
                    "import os\n\
                     while not os.path.exists({flag}):\n    await asyncio.sleep(0.01)\n\
                     {ANSWER_ONE}",
                    flag = flag.py()
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE server', requests={{'fizz': (str, Thing)}})
                    hs = [child.channels['fizz'].request(str(n)) for n in range(256)]
                    try:
                        child.channels['fizz'].request('one too many')
                    except outrig.AgentLimitReached as e:
                        print('refused:', e)
                    open({flag}, 'w').close()
                    settled = []
                    for h in hs:
                        settled.append((await h).n == h.id)
                    print(len(settled), all(settled))
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    post(&tree.session, "go").await;
    tokio::time::timeout(Duration::from_secs(300), tree.session.round())
        .await
        .expect("the round ended in time")
        .expect("the round ran");
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "refused: child 's' (c1) already holds 256 unsettled requests, its requests-max, so \
         nothing was sent; cancel one, or wait for an answer\n256 True\n"
    );
    assert_eq!(kind(&events, "agent.request.sent", None).len(), 256);
    assert_eq!(
        of_agent(&requests, "ROLE server").len(),
        512,
        "one request a round"
    );
}

/// A received request counts against `requests-max` until it is answered,
/// and an unread one until it is cancelled; `submit` counts the same way.
#[tokio::test]
async fn requests_max_counts_received_requests_and_submissions() {
    let (took, go) = (Flag::new(), Flag::new());
    let child = vec![
        submit(
            "c1",
            &format!(
                "import os\n\
                 d1 = await runtime.channels['fizz'].receive()\n\
                 open({took}, 'w').close()\n\
                 while not os.path.exists({go}):\n    await asyncio.sleep(0.01)\n\
                 await d1.reply(Thing(n=1))",
                took = took.py(),
                go = go.py()
            ),
        ),
        text_reply("answered it"),
        submit(
            "c2",
            &py(r#"
                for _ in range(2):
                    d = await runtime.channels['fizz'].receive()
                    await d.reply(Thing(n=d.id))
                "#),
        ),
        text_reply("ended"),
    ];
    let mut two = tree(
        "OUTRIG_TEST_CHILD_REQUESTS_TWO",
        "requests-max = 2",
        vec![
            ("ROLE slow", scripted(child)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE slow', requests={{'fizz': (str, Thing)}})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    await until(lambda: os.path.exists({took}))
                    try:
                        child.channels['fizz'].request('c')
                    except outrig.AgentLimitReached:
                        print('refused')
                    print(h2.cancel())
                    h3 = child.channels['fizz'].request('c')
                    open({go}, 'w').close()
                    print((await h1).n)
                    h4 = child.channels['fizz'].request('d')
                    print((await h3).n == h3.id, (await h4).n == h4.id)
                    "#,
                    took = took.py(),
                    go = go.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut two.session, "go").await;
    let (_, events, requests) = two.end().await;
    assert_eq!(printed(&requests, "p1"), "refused\nTrue\n1\nTrue True\n");
    assert_eq!(kind(&events, "agent.request.sent", None).len(), 4);

    let flag = Flag::new();
    let mut work = child_runs(
        "c1",
        &format!(
            r#"
            import os
            while not os.path.exists({flag}):
                await asyncio.sleep(0.01)
            await runtime.complete(Total(total=1))
            "#,
            flag = flag.py()
        ),
    );
    work.extend(child_runs("c2", "await runtime.complete(Total(total=2))"));
    let mut one = tree(
        "OUTRIG_TEST_CHILD_REQUESTS_ONE",
        "requests-max = 1",
        vec![
            ("ROLE one", scripted(work)),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('w', prompt='ROLE one')
                    h1 = child.submit({{}}, result=Total)
                    await until(lambda: h1.status.state == 'running')
                    try:
                        child.submit({{}}, result=Total)
                    except outrig.AgentLimitReached:
                        print('refused')
                    open({flag}, 'w').close()
                    print((await h1).total, (await child.submit({{}}, result=Total)).total)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut one.session, "go").await;
    let (_, _, requests) = one.end().await;
    assert_eq!(printed(&requests, "p1"), "refused\n1 2\n");
}

/// A release naming one child twice, or one this agent did not launch,
/// raises and releases none of its batch: each child named still takes a
/// submission.
#[tokio::test]
async fn a_release_batch_is_all_or_nothing() {
    let mut adders = child_runs("c1", "await runtime.complete(Total(total=sum(xs)))");
    adders.extend(child_runs(
        "c1",
        "await runtime.complete(Total(total=sum(xs)))",
    ));
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_RELEASE_BATCH",
        "",
        vec![
            (
                "ROLE parent",
                scripted(child_runs(
                    "c1",
                    r#"
                    import outrig
                    outrig._stranger = await runtime.spawn('g', prompt='ROLE grand')
                    await runtime.complete(Total(total=0))
                    "#,
                )),
            ),
            ("ROLE adder", scripted(adders)),
            (
                "",
                primary_waits(
                    r#"
                    p = await runtime.spawn('p', prompt='ROLE parent')
                    await p.submit({}, result=Total)
                    stranger = outrig._stranger
                    a = await runtime.spawn('a', prompt='ROLE adder')
                    b = await runtime.spawn('b', prompt='ROLE adder')
                    for batch in ([a, a, b], [a, stranger]):
                        try:
                            await runtime.release(batch)
                        except ValueError as e:
                            print('refused:', e)
                    print(a.status, b.status)
                    print((await a.submit({'xs': [1]}, result=Total)).total)
                    print((await b.submit({'xs': [2]}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let lines: Vec<&str> = said.lines().collect();
    assert_eq!(
        lines[0],
        "refused: child \"c3\" was named more than once in this release"
    );
    assert!(
        lines[1].starts_with("refused: \"c2\" is not a child this agent launched and still holds")
            && lines[1].ends_with("nothing was released"),
        "{said}"
    );
    assert_eq!(lines[2..], ["idle idle", "1", "2"]);
    assert!(kind(&events, "agent.released", None).is_empty());
}

/// Releasing a child releases its own child too: the request the child had
/// received and not answered ends `AgentReleased`, and so does the
/// grandchild's submission, while a background task the child started is
/// cancelled and its kernel is gone.
#[tokio::test]
async fn a_released_childs_subtree_goes_with_it() {
    let (log, held) = (Flag::new(), Flag::new());
    let middle = format!(
        r#"
        d = await runtime.channels['fizz'].receive()
        leaf = await runtime.spawn('leaf', prompt='ROLE leaf')
        below = leaf.submit({{}}, result=int)
        async def forever():
            try:
                await asyncio.sleep(3600)
            except asyncio.CancelledError:
                with open({log}, 'a') as f:
                    f.write('cancelled')
                raise
        sleeping = asyncio.create_task(forever())
        while below.status.state != 'running':
            await asyncio.sleep(0.01)
        open({held}, 'w').close()
        while True:
            await asyncio.sleep(0.05)
        "#,
        log = log.py(),
        held = held.py()
    );
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SUBTREE",
        "",
        vec![
            ("ROLE leaf", scripted(child_runs("l1", "await asyncio.sleep(3600)"))),
            ("ROLE middle", scripted(vec![submit("m1", &py(&middle))])),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__
                    middle = await runtime.spawn('m', prompt='ROLE middle', requests={{'fizz': (str, Thing)}})
                    h = middle.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({held}))
                    await runtime.release([middle])
                    try:
                        await h
                    except outrig.AgentReleased:
                        print('released', middle.status)
                    print(open({log}).read(), middle.id in __main__._kernels)
                    "#,
                    log = log.py(),
                    held = held.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "released released\ncancelled False\n"
    );
    let released = data(kind(&events, "agent.released", Some("agent/primary"))[0]);
    assert_eq!(released["released"], serde_json::json!(["c1"]));
    assert_eq!(released["ended"], serde_json::json!(["c1", "c2"]));
    assert_eq!(released["requests"].as_array().map(Vec::len), Some(2));
    let outcomes: Vec<Value> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(outcomes, ["released", "released"]);
    for child in ["agent/c1", "agent/c2"] {
        assert_eq!(
            kind(&events, "agent.stopped", Some(child)).len(),
            1,
            "{child} is gone"
        );
    }
}

/// A spawn the host hears of after the agent asking for it was released
/// launches nothing: the release has passed, and a child launched now would
/// outlive it. The child's code sends the spawn once its kernel has been told
/// to end, stepping it by hand so no await gives its loop the chance to cancel
/// the spawn, and holds that loop while the host reads it.
#[tokio::test]
async fn a_spawn_heard_after_its_parent_was_released_launches_nothing() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SPAWN_AFTER_RELEASE",
        "",
        vec![
            ("ROLE late", scripted(child_runs("l1", "print('launched')"))),
            (
                "ROLE parent",
                scripted(child_runs(
                    "c1",
                    r#"
                    import time
                    d = await runtime.channels['fizz'].receive()
                    while not runtime._kernel.ended:
                        time.sleep(0.01)
                    late = runtime.spawn('late', prompt='ROLE late')
                    late.send(None)
                    time.sleep(1)
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    parent = await runtime.spawn('p', prompt='ROLE parent', requests={'fizz': (str, Thing)})
                    h = parent.channels['fizz'].request('x')
                    await until(lambda: h.status.state == 'received')
                    await runtime.release([parent])
                    print(parent.status)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "released\n");
    let launched: Vec<Option<String>> = kind(&events, "agent.spawned", None)
        .into_iter()
        .map(|event| event.subject.as_ref().map(|s| s.as_str().to_string()))
        .collect();
    assert_eq!(launched, [Some("agent/c1".to_string())]);
}

/// A spawn naming an alias sends the child's model calls to the model the
/// alias resolves to, and one naming no configured model fails with the
/// configured names, launching nothing.
#[tokio::test]
async fn model_resolves_through_config() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_MODEL",
        "[models.haiku]\nprovider = \"claude\"\nidentifier = \"mock-haiku\"\n\n\
         [models.quick]\nalias = [\"haiku\"]\n",
        vec![
            ("ROLE quick", scripted(child_runs("c1", ANSWER_ONE))),
            (
                "",
                primary_waits(
                    r#"
                    c = await runtime.spawn('q', prompt='ROLE quick', model='quick', requests={'fizz': (str, Thing)})
                    h = c.channels['fizz'].request('a')
                    print((await h).n == h.id)
                    try:
                        await runtime.spawn('x', prompt='ROLE quick', model='nope')
                    except ValueError as e:
                        print(e)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    assert!(
        said.starts_with("True\n")
            && said.contains("\"nope\"")
            && said.contains("Configured models: fast, haiku, quick, sonnet"),
        "{said}"
    );
    let child = of_agent(&requests, "ROLE quick");
    assert!(
        child
            .iter()
            .all(|request| request.body["model"] == "mock-haiku")
    );
    assert!(
        primary(&requests)
            .iter()
            .all(|request| request.body["model"] == MODEL)
    );
    let spawned = kind(&events, "agent.spawned", None);
    assert_eq!(spawned.len(), 1);
    assert_eq!(data(spawned[0])["model"], "quick");
}

/// Each call reports 19 tokens. A grandchild's usage is in its own round's
/// event and in the total of its parent's round that launched it; the main
/// round's total is its own usage and each descendant's, each once; each
/// submission's settled event names the round it spanned.
#[tokio::test]
async fn spend_adds_up_once_each() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SPEND",
        "",
        vec![
            (
                "ROLE leaf",
                scripted(child_runs("l1", "await runtime.complete(Total(total=1))")),
            ),
            (
                "ROLE middle",
                scripted(child_runs(
                    "m1",
                    r#"
                    leaf = await runtime.spawn('leaf', prompt='ROLE leaf')
                    below = await leaf.submit({}, result=Total)
                    await runtime.complete(Total(total=below.total + 1))
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    middle = await runtime.spawn('middle', prompt='ROLE middle')
                    print((await middle.submit({}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, _) = tree.end().await;
    let ids = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .expect("attempt ids")
            .iter()
            .map(Value::to_string)
            .collect()
    };
    // Each agent's one round: its own attempts and usage, and its tree's --
    // what the round counted as it ran, and what landed after it ended,
    // published as usage naming it.
    let spent = |subject: &str| {
        let completed = kind(&events, "model.round.completed", Some(subject));
        assert_eq!(completed.len(), 1, "{subject}");
        let round = data(completed[0]);
        let mut own = ids(&round["attempts"]);
        own.sort();
        let mut total = round["total"]["total_tokens"].as_u64().expect("a total");
        let mut descendants = ids(&round["descendants"]);
        for late in kind(&events, "model.round.usage", Some(subject)) {
            let late = data(late);
            assert_eq!(late["round"], round["round"]);
            total += late["usage"]["total_tokens"].as_u64().expect("a usage");
            descendants.extend(ids(&late["attempts"]));
        }
        descendants.sort();
        let usage = round["usage"]["total_tokens"].as_u64().expect("a usage");
        (own, usage, descendants, total)
    };
    let (main, middle, leaf) = (spent("agent/primary"), spent("agent/c1"), spent("agent/c2"));
    assert_eq!(
        (leaf.1, middle.1, main.1),
        (19, 19, 38),
        "each round's own usage: a child's one call, and the primary's two"
    );
    assert_eq!(
        (leaf.3, middle.3, main.3),
        (19, 38, 76),
        "each tree's total"
    );
    assert!(leaf.2.is_empty());
    assert_eq!(
        middle.2, leaf.0,
        "the grandchild's attempts, in its parent's round"
    );
    let mut below_main = [middle.0.clone(), leaf.0.clone()].concat();
    below_main.sort();
    assert_eq!(main.2, below_main, "each descendant's attempt, once");
    for settled in kind(&events, "agent.request.settled", None) {
        assert_eq!(data(settled)["rounds"], serde_json::json!([1]));
    }
}

/// At the close, a running submission, a request waiting and a request
/// received each end: `await`, `asyncio.wait` over `future`, and a second
/// await all raise `SessionClosing`, each under a deadline. A request
/// answered before the close keeps its value, and a spawn after it fails at
/// once. The shutdown report lists the three.
#[tokio::test]
async fn at_the_close_every_wait_raises_and_none_hangs() {
    let (closing, finish) = (Flag::new(), Flag::new());
    let wait_to_finish = format!(
        "import os\nwhile not os.path.exists({finish}):\n    await asyncio.sleep(0.01)",
        finish = finish.py()
    );
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_CLOSE",
        "",
        vec![
            ("ROLE worker", scripted(child_runs("c1", &wait_to_finish))),
            ("ROLE ignoring", scripted(child_runs("c1", &wait_to_finish))),
            (
                "ROLE holder",
                scripted(child_runs(
                    "c1",
                    &format!(
                        "d1 = await runtime.channels['fizz'].receive()\n\
                         await d1.reply(Thing(n=1))\n\
                         d2 = await runtime.channels['fizz'].receive()\n\
                         {wait_to_finish}"
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    "{WATCH}{}",
                    py(&format!(
                        r#"
                    spec = {{'fizz': (str, Thing)}}
                    w = await runtime.spawn('w', prompt='ROLE worker')
                    running = w.submit({{}}, result=Total)
                    i = await runtime.spawn('i', prompt='ROLE ignoring', requests=spec)
                    waiting = i.channels['fizz'].request('a')
                    k = await runtime.spawn('k', prompt='ROLE holder', requests=spec)
                    answered = k.channels['fizz'].request('first')
                    received = k.channels['fizz'].request('second')
                    for child, handles in ((w, [running]), (i, [waiting]), (k, [answered, received])):
                        watch(child, handles)
                    await until(lambda: running.status.state == 'running' and received.status.state == 'received')
                    open({closing}, 'w').close()
                    await until(lambda: running.done() and waiting.done() and received.done())
                    for name, h in (('running', running), ('waiting', waiting), ('received', received)):
                        raised = []
                        try:
                            await asyncio.wait_for(h, 10)
                        except outrig.SessionClosing:
                            raised.append('await')
                        done, _ = await asyncio.wait({{h.future}}, timeout=10)
                        if isinstance(done.pop().exception(), outrig.SessionClosing):
                            raised.append('asyncio.wait')
                        try:
                            await asyncio.wait_for(h, 10)
                        except outrig.SessionClosing:
                            raised.append('again')
                        print(name, raised, h.status.reason)
                    print((await answered).n)
                    started = time.monotonic()
                    try:
                        await runtime.spawn('late', prompt='ROLE late')
                    except outrig.SessionClosing:
                        print('no spawn', time.monotonic() - started < 1)
                    print(idle_holding)
                    "#,
                        closing = closing.py()
                    ))
                )),
            ),
        ],
    )
    .await;
    let closer = tree.session.closer();
    let (end, ()) = tokio::join!(round_end(&mut tree.session, "go"), async {
        raised(&closing).await;
        closer.close_admission();
    });
    assert_eq!(end.reply, "done");
    finish.raise();
    let (report, _, requests) = tree.end().await;
    let all = "['await', 'asyncio.wait', 'again'] closed";
    assert_eq!(
        printed(&requests, "p1"),
        format!("running {all}\nwaiting {all}\nreceived {all}\n1\nno spawn True\n[]\n")
    );
    assert_eq!(report.requests.len(), 3);
    assert!(
        report
            .requests
            .iter()
            .all(|row| row.outcome == crate::harness::event::RequestEnd::Closed),
        "{:?}",
        report.requests
    );
}

/// A spawn from an agent at `subagent-depth-max` raises the limit's error.
#[tokio::test]
async fn a_spawn_at_the_depth_limit_is_refused() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_DEPTH",
        "subagent-depth-max = 2",
        vec![
            (
                "ROLE deep",
                scripted(child_runs(
                    "c1",
                    r#"
                    import outrig
                    try:
                        await runtime.spawn('deeper', prompt='ROLE deeper')
                    except outrig.AgentLimitReached as e:
                        print('refused:', e)
                    await runtime.complete(Total(total=0))
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    child = await runtime.spawn('d', prompt='ROLE deep')
                    print((await child.submit({}, result=Total)).total)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "0\n");
    assert_eq!(
        exec_outputs(&events, "agent/c1"),
        [
            "refused: subagent-depth-max is 2, and this agent is at depth 2, so it cannot launch \
             a child\n"
        ]
    );
    assert_eq!(kind(&events, "agent.spawned", None).len(), 1);
}

/// Inputs given at the spawn are bound in the child before its first round,
/// whose instructions list them.
#[tokio::test]
async fn spawn_inputs_are_bound_before_the_first_round() {
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SPAWN_INPUTS",
        "",
        vec![
            (
                "ROLE based",
                scripted(child_runs(
                    "c1",
                    "d = await runtime.channels['fizz'].receive()\n\
                     await d.reply(Thing(n=base + len(d.body)))",
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    c = await runtime.spawn('b', prompt='ROLE based', inputs={'base': 10}, requests={'fizz': (str, Thing)})
                    print((await c.channels['fizz'].request('abc')).n)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "13\n");
    let system = system_text(&of_agent(&requests, "ROLE based")[0].body);
    assert!(
        system.contains("These names were bound for you before you began:\n- `base` (int): 10"),
        "{system}"
    );
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::LaunchSpec;
    use crate::container::ExecOptions;
    use crate::harness::Verdict;
    use crate::python::testing::{ALPINE, pull};

    /// A child's code runs in the session's container, as its parent's does:
    /// what it completes with is read from the container's filesystem.
    #[tokio::test]
    async fn a_childs_code_runs_in_the_session_container() {
        pull(ALPINE).await;
        let logs = tempfile::tempdir().expect("a logs dir");
        let var = "OUTRIG_TEST_CHILD_E2E";
        let (addr, mut requests, _) = mock_http::routed(
            vec![
                (
                    "ROLE reader",
                    scripted(child_runs(
                        "c1",
                        "await runtime.complete(open('/etc/alpine-release').read())",
                    )),
                ),
                (
                    "",
                    primary_runs(
                        "child = await runtime.spawn('r', prompt='ROLE reader')\n\
                         print(await child.submit({}, result=str), end='')",
                    ),
                ),
            ],
            None,
        )
        .await;
        let mut session = SessionBuilder::new(config(addr, var, ""), Some("coding"), None)
            .secrets(keyed(var))
            .container(LaunchSpec::from_image(
                ALPINE,
                BTreeMap::new(),
                logs.path().to_path_buf(),
            ))
            .start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(round(&mut session, "go").await, "done");
        let release = session
            .outrig()
            .expect("a container")
            .exec_capture(
                &["cat".to_string(), "/etc/alpine-release".to_string()],
                &ExecOptions::new(),
            )
            .await
            .expect("cat runs");
        let report = shut_down(session).await;
        assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
        let recorded = mock_http::drain(&mut requests);
        assert_eq!(
            printed(&recorded, "p1"),
            String::from_utf8_lossy(&release.stdout)
        );
    }
}
