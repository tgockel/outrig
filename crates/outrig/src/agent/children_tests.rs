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
use super::mock_http::{
    self, CannedResponse, Gauge, MODEL, RecordedRequest, Responder, scripted, submit, system_text,
    text_reply,
};
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
style                = "anthropic"
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
    let (addr, requests, gauge) = mock_http::routed(routes, hold).await;
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
        system_text(&child_opening.body).contains("No user reaches you"),
        "a child is told it has no user"
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

/// A work child whose round and the one more round asking for its result
/// both end without completing fails the submission, saying so, with no
/// third call and no invalid answer recorded; a submission queued behind it
/// then gets its own round and completes.
#[tokio::test]
async fn a_work_child_that_ends_two_rounds_without_completing_fails() {
    let child = vec![
        text_reply("I will not complete it"),
        text_reply("Still not"),
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
                primary_runs(&format!(
                    "{TOTAL}\nimport outrig\n\
                     child = await runtime.spawn('s', prompt='ROLE stubborn')\n\
                     first = child.submit({{'xs': [1]}}, result=Total)\n\
                     second = child.submit({{'xs': [2, 3]}}, result=Total)\n\
                     print(second.status.state)\n\
                     try:\n    await first\n\
                     except outrig.CompletionRejected as e:\n    print(e)\n\
                     print((await second).total)"
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    assert!(
        said.starts_with("queued\n")
            && said.contains("ended without completing")
            && said.ends_with("5\n"),
        "{said}"
    );
    let child = of_agent(&requests, "ROLE stubborn");
    assert_eq!(
        child.len(),
        4,
        "two rounds for the first, one for the second"
    );
    let nudge = child[1].body["messages"].to_string();
    assert!(nudge.contains("is still open"), "{nudge}");
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
    let outcomes: Vec<Value> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(outcomes, ["rejected", "accepted"]);
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

/// The text of the message `request` ended on: for a round's first call, its
/// opening.
pub(super) fn opening(request: &RecordedRequest) -> String {
    text_of(&messages(request).last().expect("a message")["content"])
}

/// A request the child received and left unanswered earns one more round,
/// whose opening names it by id with how long it has waited; when that round
/// ends with nothing changed, nothing follows, and the request stays open with
/// its status saying the child is idle, until a cancel ends it.
#[tokio::test]
async fn an_unchanged_state_opens_nothing_and_a_cancel_ends_the_request() {
    let child = vec![
        submit("c1", "d = await runtime.channels['fizz'].receive()"),
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
                primary_waits(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE silent', requests={'fizz': (str, Thing)})
                    h = child.channels['fizz'].request('a')
                    await until(lambda: h.status.state == 'received' and child.status == 'idle')
                    print(h.id, h.done(), h.status)
                    print(h.cancel(), h.status)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    let said = printed(&requests, "p1");
    let id = said.split_whitespace().next().expect("an id").to_string();
    assert_eq!(
        said,
        format!("{id} False received (child idle)\nTrue failed: cancelled (child idle)\n")
    );
    let child = of_agent(&requests, "ROLE silent");
    assert_eq!(child.len(), 3, "the round, its nudge, and nothing after");
    let nudge = opening(child[2]);
    assert!(
        nudge.contains(&format!(
            "you took request {id} from runtime.channels[\"fizz\"] "
        )) && nudge.contains("s ago and have not answered it"),
        "{nudge}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
    let cancelled = kind(&events, "agent.request.cancelled", None);
    assert_eq!(data(cancelled[0])["received"], true);
}

/// With the child idle on one request received and unanswered and one
/// unread, a third request opens a round whose opening names the received one
/// by id and counts two unread; a reply to each then settles its handle, the
/// old one's with its value.
#[tokio::test]
async fn a_new_arrival_opens_a_round_naming_the_old_requests_too() {
    let child = vec![
        submit("c1", "d1 = await runtime.channels['fizz'].receive()"),
        text_reply("took one"),
        text_reply("idle now"),
        submit(
            "c2",
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
                primary_waits(
                    r#"
                    child = await runtime.spawn('l', prompt='ROLE late', requests={'fizz': (str, Thing)})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    await until(lambda: h1.status.state == 'received' and child.status == 'idle')
                    h3 = child.channels['fizz'].request('c')
                    for h in (h1, h2, h3):
                        print(h.id, (await h).n)
                    "#,
                ),
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
    assert_eq!(child.len(), 5);
    let named = opening(child[3]);
    assert!(
        named.contains("2 requests are waiting on runtime.channels[\"fizz\"]")
            && named.contains(&format!("you took request {} from", answered[0].0)),
        "{named}"
    );
}

/// A request that arrives while the child is idle and is never received earns
/// the round its arrival opened and no more; one that arrives during a round,
/// announced there, earns the round after it, whose opening counts it, and no
/// more. Neither counts an attempt.
#[tokio::test]
async fn an_unread_request_earns_no_round_past_the_first_that_counts_it() {
    let flag = Flag::new();
    let mut idle = tree(
        "OUTRIG_TEST_CHILD_UNREAD_IDLE",
        "",
        vec![
            (
                "ROLE ignoring",
                scripted(vec![
                    submit("c1", &format!("open({}, 'w').close()", flag.py())),
                    text_reply("not now"),
                ]),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    child = await runtime.spawn('i', prompt='ROLE ignoring', requests={{'fizz': (str, Thing)}})
                    h = child.channels['fizz'].request('a')
                    await until(lambda: os.path.exists({flag}))
                    await until(lambda: child.status == 'idle')
                    print(h.status)
                    "#,
                    flag = flag.py()
                )),
            ),
        ],
    )
    .await;
    round(&mut idle.session, "go").await;
    let (_, events, requests) = idle.end().await;
    assert_eq!(printed(&requests, "p1"), "waiting (child idle)\n");
    assert_eq!(
        of_agent(&requests, "ROLE ignoring").len(),
        2,
        "the round its arrival opened, and no more"
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
                    await until(lambda: child.status == 'idle')
                    print(second.status)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut announced.session, "go").await;
    let (_, events, requests) = announced.end().await;
    assert_eq!(printed(&requests, "p1"), "waiting (child idle)\n");
    let child = of_agent(&requests, "ROLE ignoring");
    assert_eq!(
        child.len(),
        3,
        "the first round's two calls and the one round after it"
    );
    let result = tool_result(child[1], "c1");
    assert!(
        result.starts_with("[1 request is waiting on runtime.channels[\"fizz\"]]"),
        "{result}"
    );
    let after = opening(child[2]);
    assert!(
        after.contains("1 request is waiting on runtime.channels[\"fizz\"]"),
        "{after}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
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
        let after_result = messages(request)
            .last()
            .and_then(|message| message["content"].as_array())
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"));
        let canned = if after_result {
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

/// A request held at the provider: `arrived` is told as it comes in, and it
/// is answered once `go` is.
#[derive(Default)]
struct Hold {
    arrived: Notify,
    go: Notify,
}

/// `script`, as [`scripted`] answers it, its first response held by `hold`.
fn held(script: Vec<CannedResponse>, hold: Arc<Hold>) -> Responder {
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
async fn raised(flag: &Flag) {
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
fn exec_outputs(events: &[Arc<Event>], subject: &str) -> Vec<String> {
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
    // Each agent's first call before the one it launched, and its last once
    // that one has answered; the last calls follow their own code's ends,
    // which race.
    let mut order: Vec<String> = requests.iter().map(role).collect();
    assert_eq!(order[..3], ["", "ROLE middle", "ROLE leaf"], "{order:?}");
    order[3..].sort();
    assert_eq!(order[3..], ["", "ROLE leaf", "ROLE middle"], "{order:?}");
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

/// The budget spent by one child ends what another, idle, holds open: its
/// runner hears the budget go, and a request it never took ends
/// `BudgetExhausted` rather than waiting for a round that cannot open.
#[tokio::test]
async fn a_spent_budget_ends_what_an_idle_child_holds() {
    // Each call reports 19 tokens: the primary's first and the idle child's
    // two spend 57 of 70, and the spender's first spends the rest.
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_BUDGET_IDLE",
        "token-budget = 70",
        vec![
            (
                "ROLE idle",
                scripted(vec![
                    submit("a1", &format!("open({}, 'w').close()", flag.py())),
                    text_reply("not now"),
                ]),
            ),
            (
                "ROLE spender",
                scripted(vec![submit("b1", "print('spending')"), text_reply("never sent")]),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    a = await runtime.spawn('a', prompt='ROLE idle', requests={{'fizz': (str, Thing)}})
                    h = a.channels['fizz'].request('x')
                    await until(lambda: os.path.exists({flag}) and a.status == 'idle')
                    b = await runtime.spawn('b', prompt='ROLE spender')
                    for handle in (b.submit({{}}, result=Total), h):
                        try:
                            await asyncio.wait_for(handle, 10)
                        except outrig.BudgetExhausted:
                            print('budget', handle.status.reason)
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
        exec_outputs(&events, "agent/primary"),
        ["budget budget\nbudget budget\n"]
    );
    assert_eq!(
        of_agent(&requests, "ROLE idle").len(),
        2,
        "no call after its round"
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
    assert_eq!(child.len(), 3, "the failed call, then the round again");
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
    assert_eq!(child.len(), 4, "two rounds of two calls, one failing");
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
    let child = vec![
        submit("c1", "d1 = await runtime.channels['fizz'].receive()"),
        text_reply("took one"),
        text_reply("holding it"),
        submit("c2", "await d1.reply(Thing(n=1))"),
        text_reply("answered it"),
        submit(
            "c3",
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
                primary_waits(
                    r#"
                    child = await runtime.spawn('s', prompt='ROLE slow', requests={'fizz': (str, Thing)})
                    h1 = child.channels['fizz'].request('a')
                    h2 = child.channels['fizz'].request('b')
                    await until(lambda: h1.status.state == 'received' and child.status == 'idle')
                    try:
                        child.channels['fizz'].request('c')
                    except outrig.AgentLimitReached:
                        print('refused')
                    print(h2.cancel())
                    h3 = child.channels['fizz'].request('c')
                    print((await h1).n)
                    h4 = child.channels['fizz'].request('d')
                    print((await h3).n == h3.id, (await h4).n == h4.id)
                    "#,
                ),
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
    let log = Flag::new();
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
        "#,
        log = log.py()
    );
    let mut tree = tree(
        "OUTRIG_TEST_CHILD_SUBTREE",
        "",
        vec![
            ("ROLE leaf", scripted(child_runs("l1", "await asyncio.sleep(3600)"))),
            (
                "ROLE middle",
                scripted(vec![
                    submit("m1", &py(&middle)),
                    text_reply("working on it"),
                    text_reply("still working on it"),
                ]),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__
                    middle = await runtime.spawn('m', prompt='ROLE middle', requests={{'fizz': (str, Thing)}})
                    h = middle.channels['fizz'].request('a')
                    await until(lambda: h.status.state == 'received' and middle.status == 'idle')
                    await runtime.release([middle])
                    try:
                        await h
                    except outrig.AgentReleased:
                        print('released', middle.status)
                    print(open({log}).read(), middle.id in __main__._kernels)
                    "#,
                    log = log.py()
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
        (38, 38, 38),
        "each round's own usage"
    );
    assert_eq!(
        (leaf.3, middle.3, main.3),
        (38, 76, 114),
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
            ("ROLE ignoring", scripted(vec![text_reply("not now")])),
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
                    r#"
                    spec = {{'fizz': (str, Thing)}}
                    w = await runtime.spawn('w', prompt='ROLE worker')
                    running = w.submit({{}}, result=Total)
                    i = await runtime.spawn('i', prompt='ROLE ignoring', requests=spec)
                    waiting = i.channels['fizz'].request('a')
                    k = await runtime.spawn('k', prompt='ROLE holder', requests=spec)
                    answered = k.channels['fizz'].request('first')
                    received = k.channels['fizz'].request('second')
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
                    "#,
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
    let all = "['await', 'asyncio.wait', 'again'] closed";
    assert_eq!(
        printed(&requests, "p1"),
        format!("running {all}\nwaiting {all}\nreceived {all}\n1\nno spawn True\n")
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
        tool_result(of_agent(&requests, "ROLE deep")[1], "c1"),
        "refused: subagent-depth-max is 2, and this agent is at depth 2, so it cannot launch a \
         child\n"
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
