//! Agent classes end to end: a subclass of `outrig.Agent` in an agent's code,
//! each instance one child whose rounds run against the mock provider, on the
//! host's interpreter. The interpreter's own half -- the declaration's checks,
//! the requests held before the child exists, the notices -- is
//! `interpreter_tests`'.
//!
//! The mock routes each agent by a marker its system prompt carries, which for
//! an instance's child is in the class's docstring, the child's prompt. Each
//! instance's child has a round as it launches, which the mock answers with
//! text unless a test says otherwise.

use std::sync::Arc;

use serde_json::json;

use super::agent_tests::{round, round_end};
use super::children_tests::{
    Hold, child_runs, data, exec_outputs, held, kind, of_agent, opening, position, primary_waits,
    printed, raised, serving, tree,
};
use super::mock_http::{RecordedRequest, Responder, scripted, submit, system_text, text_reply};
use crate::harness::event::{Event, RequestEnd};
use crate::python::testing::{Flag, py};

/// Whether `request` is the first call of an instance's launch round.
fn launching(request: &RecordedRequest) -> bool {
    opening(request).contains("you have just been launched")
}

/// `responder`, with each launch round answered at once with text: the child
/// is ready, having done nothing.
fn ready_then(responder: Responder) -> Responder {
    Arc::new(move |request: &RecordedRequest| {
        if launching(request) {
            Box::pin(async { text_reply("ready") })
        } else {
            responder(request)
        }
    })
}

/// Every request `marker`'s agent made past its launch round.
fn past_launch<'a>(requests: &'a [RecordedRequest], marker: &str) -> Vec<&'a RecordedRequest> {
    of_agent(requests, marker)
        .into_iter()
        .filter(|request| !launching(request))
        .collect()
}

/// The subject an event belongs to, or `""` for the session's own.
fn subject(event: &Event) -> &str {
    event
        .subject
        .as_ref()
        .map_or("", |subject| subject.as_str())
}

/// The subject of the child that serves the instance of agent class
/// `qualname`, from its `agent.instance.started`.
fn child_of(events: &[Arc<Event>], qualname: &str) -> String {
    kind(events, "agent.instance.started", None)
        .into_iter()
        .find(|event| data(event)["qualname"] == qualname)
        .map(|event| subject(event).to_string())
        .unwrap_or_else(|| panic!("no instance of {qualname}"))
}

/// `text` with every line but blank ones indented by `by` spaces, to sit in a
/// test's Python, which `py` dedents.
fn indented(text: &str, by: usize) -> String {
    let pad = " ".repeat(by);
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `bytes` as the digest a declaration carries.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An agent class named `name`, its child marked `ROLE <role>`, with one
/// request method, `fizz`, which takes a `str` and replies with a `Thing`.
fn fizzing(name: &str, role: &str) -> String {
    format!(
        r#"
class {name}(outrig.Agent):
    """ROLE {role}. Answer requests on fizz."""
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Reply with a Thing."""
        ...
"#
    )
}

/// What a child runs to answer one request on `fizz` with its id.
const ANSWER_ONE: &str =
    "d = await runtime.channels['fizz'].receive()\nawait d.reply(Thing(n=d.id))";

/// `SomethingElse` and the maintainer's `FooAgent`, its child marked `ROLE
/// foo`, with braces in its docstring.
const FOO: &str = r#"
@dataclass
class SomethingElse:
    label: str

class FooAgent(outrig.Agent):
    """ROLE foo. The class docstring; {message} and {} stay braces."""
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Request channel fizz: count the message's characters."""
        ...
    @outrig.agent
    async def buzz(self, foo: int, bar: str) -> SomethingElse:
        """Request channel buzz: label foo with bar."""
        ...
"#;

/// What `FooAgent`'s child runs in a round: answer each request waiting on
/// either channel.
const FOO_ANSWERS: &str = r#"
for name in ('fizz', 'buzz'):
    while runtime.channels[name].pending():
        d = await runtime.channels[name].receive()
        if name == 'fizz':
            await d.reply(Thing(n=len(d.body)))
        else:
            await d.reply(SomethingElse(label=f'{d.body.foo}:{d.body.bar}'))
"#;

/// The maintainer's example from `agent-classes.md`: the constructor returns
/// before the child exists, the three requests made at once settle with what
/// the child replied, `runtime.wait` takes their handles, and the record holds
/// one spawn and the instance's events in their order -- the requests made
/// before the child existed sent as it did, and so before its first round
/// ends, wherever that is.
#[tokio::test]
async fn the_maintainers_example_runs_against_a_mock_model() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_EXAMPLE",
        "",
        vec![
            ("ROLE foo", ready_then(serving(FOO_ANSWERS))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__
                    from dataclasses import dataclass
                    {FOO}
                    await runtime.channels['user'].receive()
                    foo = FooAgent()
                    print(sorted(__main__._kernels))
                    fizz = foo.fizz("Hello"); b1 = foo.buzz(1, "one"); b2 = foo.buzz(2, "two")
                    done, more = await runtime.wait({{fizz, b1, b2}})
                    print(len(done), len(more), (await fizz).n, (await b1).label,
                          (await b2).label, type(await b1) is SomethingElse)
                    await foo.release()
                    "#,
                    FOO = indented(FOO, 20),
                )),
            ),
        ],
    )
    .await;
    assert_eq!(round(&mut tree.session, "go").await, "done");
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "['primary']\n3 0 5 1:one 2:two True\n",
        "constructed with no child yet, so before any model call of its own"
    );
    assert_eq!(kind(&events, "agent.spawned", None).len(), 1, "one spawn");
    assert!(
        events
            .iter()
            .all(|event| !event.kind().starts_with("agent.call.")),
        "a method call is a request, not a typed call"
    );
    let child = Some("agent/c1");
    let started = position(&events, "agent.instance.started", |_| true);
    let ready = position(&events, "agent.instance.ready", |_| true);
    let sent = position(&events, "agent.request.sent", |_| true);
    let released = position(&events, "agent.instance.released", |_| true);
    assert_eq!(
        (started.len(), ready.len(), sent.len(), released.len()),
        (1, 1, 3, 1)
    );
    assert!(sent.iter().all(|at| started[0] < *at), "started first");
    assert!(started[0] < ready[0] && ready[0] < released[0]);
    for event in kind(&events, "agent.request.sent", child) {
        let id = data(event)["request"].clone();
        let at = |kind: &str| {
            position(&events, kind, |event| data(event)["request"] == id)
                .first()
                .copied()
                .unwrap_or_else(|| panic!("{kind} for {id}"))
        };
        let (sent, received, replied, settled) = (
            at("agent.request.sent"),
            at("agent.request.received"),
            at("agent.request.replied"),
            at("agent.request.settled"),
        );
        assert!(sent < received && received < replied && replied < settled);
        assert!(settled < released[0], "released last");
    }
    for settled in kind(&events, "agent.request.settled", child) {
        let rounds = data(settled)["rounds"].clone();
        assert!(rounds.as_array().is_some_and(|r| !r.is_empty()), "{rounds}");
    }
    let started = data(kind(&events, "agent.instance.started", child)[0]);
    assert_eq!(started["parent"], "agent/primary");
    assert_eq!(started["module"], "outrig_session_primary");
    assert_eq!(started["qualname"], "FooAgent");
    assert!(
        started["digest"].as_str().is_some_and(|d| d.len() == 64),
        "the submission's digest: {started}"
    );
    assert_eq!(started["inputs"], json!([]));
    assert_eq!(
        started["channels"],
        json!([
            {"name": "fizz", "request": "str", "reply": "Thing"},
            {"name": "buzz", "request": "FooAgentBuzzMessage", "reply": "SomethingElse"},
        ])
    );
    assert_eq!(
        data(kind(&events, "agent.instance.ready", child)[0])["error"],
        json!(null)
    );
    let released = data(kind(&events, "agent.instance.released", child)[0]);
    assert_eq!(released["how"], "explicit");
    assert_eq!(released["instance"], started["instance"]);
    assert_eq!(released["requests"], json!([]), "none was open");
    assert!(
        released["usage"]["input_tokens"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "{released}"
    );
}

/// A class declared in a module: one child answers both requests, the second
/// round reading what the first bound and the input the constructor gave; the
/// child's instructions are the docstring as written, the input manifest, and
/// each channel's docstring with its types' schemas; and the start event
/// carries the module's digest.
#[tokio::test]
async fn context_persists_across_requests_and_the_instructions_are_as_declared() {
    const MEMO: &str = r#"import outrig
from dataclasses import dataclass


@dataclass
class Echo:
    text: str


class MemoAgent(outrig.Agent):
    """ROLE memo. Remember a word, then recall it; {message} and {} stay braces."""

    @outrig.agent
    async def remember(self, word: str) -> Echo:
        """Bind `word` for a later request."""
        ...

    @outrig.agent
    async def recall(self) -> Echo:
        """Say what was bound, `seed` times over."""
        ...
"#;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("memo_agent.py"), MEMO).expect("write");
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_MEMO",
        "",
        vec![
            (
                "ROLE memo",
                ready_then(scripted(vec![
                    submit(
                        "c1",
                        "d = await runtime.channels['remember'].receive()\nword = d.body\n\
                         await d.reply(Echo(text=word))",
                    ),
                    text_reply("bound"),
                    submit(
                        "c2",
                        "d = await runtime.channels['recall'].receive()\n\
                         await d.reply(Echo(text=word * seed))",
                    ),
                    text_reply("said"),
                ])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import sys
                    sys.path.insert(0, {dir:?})
                    import memo_agent
                    m = memo_agent.MemoAgent(seed=3)
                    print((await m.remember("kept")).text)
                    print((await m.recall()).text)
                    await m.release()
                    "#,
                    dir = dir.path().to_str().expect("a UTF-8 path"),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "kept\nkeptkeptkept\n");
    assert_eq!(kind(&events, "agent.spawned", None).len(), 1, "one child");
    let child = of_agent(&requests, "ROLE memo");
    assert!(launching(child[0]), "{}", opening(child[0]));
    let system = system_text(&child[0].body);
    for expected in [
        "ROLE memo. Remember a word, then recall it; {message} and {} stay braces.",
        "- `seed` (int): 3",
        "`runtime.channels[\"remember\"]`: Bind `word` for a later request.",
        "`runtime.channels[\"recall\"]`: Say what was bound, `seed` times over.",
        "A request is a `str`",
        "A request is a `None`",
        "Its reply is a `Echo`",
        "\"text\"",
        "You have no `runtime.complete`",
        "as you are launched, to get ready in",
    ] {
        assert!(system.contains(expected), "{expected:?} in {system}");
    }
    let started = data(kind(&events, "agent.instance.started", None)[0]);
    assert_eq!(started["module"], "memo_agent");
    assert_eq!(started["qualname"], "MemoAgent");
    assert_eq!(started["digest"], sha256_hex(MEMO.as_bytes()));
    assert_eq!(
        started["inputs"],
        json!([{"name": "seed", "type": "int", "preview": "3"}])
    );
}

/// The mark decides, not the body: a marked method whose body would raise
/// sends a request and raises nothing; an unmarked `async def` is an ordinary
/// coroutine method that sends nothing. A call is no coroutine function and
/// `help()` says it returns a handle; an argument of the wrong type is refused
/// with nothing sent; one parameter arrives as its value, several as the
/// class's frozen message dataclass.
#[tokio::test]
async fn the_mark_decides_and_the_types_are_derived() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_MARK",
        "",
        vec![
            (
                "ROLE marked",
                ready_then(serving(
                    r#"
                    for name in ('fizz', 'buzz'):
                        while runtime.channels[name].pending():
                            d = await runtime.channels[name].receive()
                            if name == 'fizz':
                                print(type(d.body).__name__, d.body)
                                await d.reply(Thing(n=len(d.body)))
                            else:
                                try:
                                    d.body.foo = 9
                                except Exception as e:
                                    print(type(d.body).__name__, d.body.foo, d.body.bar,
                                          type(e).__name__)
                                await d.reply(Thing(n=d.body.foo))
                    "#,
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    import inspect
                    class Marked(outrig.Agent):
                        """ROLE marked. Answer each request."""
                        @outrig.agent
                        async def fizz(self, message: str) -> Thing:
                            """Count the characters."""
                            raise AssertionError("the body ran")
                        @outrig.agent
                        async def buzz(self, foo: int, bar: str) -> Thing:
                            """Take foo."""
                            ...
                        async def plain(self) -> int:
                            """An ordinary coroutine method."""
                            ...
                    m = Marked()
                    coroutine = m.plain()
                    print(inspect.iscoroutinefunction(Marked.fizz), inspect.iscoroutine(coroutine))
                    coroutine.close()
                    try:
                        m.fizz(1)
                    except TypeError as e:
                        print('refused:', 'message: expected str, not int' in str(e))
                    print((await m.fizz("abc")).n, (await m.buzz(4, "four")).n)
                    help(m)
                    await m.release()
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
        said.starts_with("False True\nrefused: True\n3 4\n"),
        "{said}"
    );
    for expected in [
        "fizz(self, message: str)",
        "buzz(self, foo: int, bar: str)",
        "returns the request's handle at once",
        "plain(self)",
    ] {
        assert!(said.contains(expected), "{expected:?} in {said}");
    }
    assert_eq!(
        kind(&events, "agent.request.sent", None).len(),
        2,
        "the refused call and the ordinary method sent nothing"
    );
    let outputs = exec_outputs(&events, "agent/c1").concat();
    assert!(
        outputs.contains("str abc")
            && outputs.contains("MarkedBuzzMessage 4 four FrozenInstanceError"),
        "{outputs}"
    );
}

/// A request the child read and left unanswered earns one more round, which
/// names it by id, and a reply there settles it; one it never answers earns
/// that round and no other, the child idle and the request open, until a new
/// request opens a round naming the old one too.
#[tokio::test]
async fn re_prompting_follows_the_summary_rule() {
    let mut answered = tree(
        "OUTRIG_TEST_INSTANCE_NUDGE",
        "",
        vec![
            (
                "ROLE nudged",
                ready_then(scripted(vec![
                    submit("c1", "d = await runtime.channels['fizz'].receive()"),
                    text_reply("took it"),
                    submit("c2", "await d.reply(Thing(n=7))"),
                    text_reply("answered"),
                ])),
            ),
            (
                "",
                primary_waits(&format!(
                    "{}\ns = Nudged()\nawait s.ready()\nh = s.fizz('a')\nprint(h.id, (await h).n)",
                    fizzing("Nudged", "nudged")
                )),
            ),
        ],
    )
    .await;
    round(&mut answered.session, "go").await;
    let (_, events, requests) = answered.end().await;
    let said = printed(&requests, "p1");
    let id = said.split_whitespace().next().expect("an id").to_string();
    assert_eq!(said, format!("{id} 7\n"));
    let child = past_launch(&requests, "ROLE nudged");
    assert!(
        opening(child[2]).contains(&format!(
            "you took request {id} from runtime.channels[\"fizz\"]"
        )),
        "{}",
        opening(child[2])
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());

    // The round the second request opens says so through a flag, so the
    // first is cancelled only after that round has opened on it.
    let opened = Flag::new();
    let mut silent = tree(
        "OUTRIG_TEST_INSTANCE_SILENT",
        "",
        vec![
            (
                "ROLE silent",
                ready_then(scripted(vec![
                    submit("c1", "d = await runtime.channels['fizz'].receive()"),
                    text_reply("taken"),
                    text_reply("still not answering"),
                    submit("c2", &format!("open({}, 'w').close()", opened.py())),
                    text_reply("not answering either"),
                ])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {class}
                    s = Silent()
                    await s.ready()
                    h = s.fizz('a')
                    await until(lambda: h.status.state == 'received' and h.status.child == 'idle')
                    print(h.id, h.done(), h.status)
                    h2 = s.fizz('b')
                    await until(lambda: os.path.exists({opened}))
                    print(h.cancel(), h.status.state, h.status.reason)
                    "#,
                    class = indented(&fizzing("Silent", "silent"), 20),
                    opened = opened.py(),
                )),
            ),
        ],
    )
    .await;
    round(&mut silent.session, "go").await;
    let (_, events, requests) = silent.end().await;
    let said = printed(&requests, "p1");
    let id = said.split_whitespace().next().expect("an id").to_string();
    assert_eq!(
        said,
        format!("{id} False received (child idle)\nTrue failed cancelled\n")
    );
    let child = past_launch(&requests, "ROLE silent");
    // The round that received it, the one that named it, and then only the
    // round the second request opened, which names the first too.
    assert!(
        opening(child[2]).contains(&format!("you took request {id} from")),
        "{}",
        opening(child[2])
    );
    let both = opening(child[3]);
    assert!(
        both.contains(&format!("you took request {id} from"))
            && both.contains("1 request is waiting on runtime.channels[\"fizz\"]"),
        "{both}"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
}

/// Three requests sent together to a child that answers one per round all
/// settle, the host opening a round after each of the first two replies with
/// no arrival after the three.
#[tokio::test]
async fn three_requests_served_one_per_round_all_settle() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_SERIAL",
        "",
        vec![
            ("ROLE serial", ready_then(serving(ANSWER_ONE))),
            (
                "",
                primary_waits(&format!(
                    "{}\nt = Serial()\nawait t.ready()\nhs = [t.fizz(str(n)) for n in range(3)]\n\
                     print([(await h).n == h.id for h in hs])",
                    fizzing("Serial", "serial")
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    // The child's last round ends after the reply the primary's waited for: the
    // primary's round, the child's launch round, and its three.
    tree.seen("model.round.completed", 5).await;
    let (_, _, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "[True, True, True]\n");
    let child = past_launch(&requests, "ROLE serial");
    assert_eq!(child.len(), 6, "three rounds of two calls");
    for (round, waiting) in [(1, "2 requests are"), (2, "1 request is")] {
        let text = opening(child[2 * round]);
        assert!(
            text.contains(&format!("{waiting} waiting on runtime.channels[\"fizz\"]")),
            "{text}"
        );
    }
}

/// A reply of the wrong shape three times ends a request `CompletionRejected`
/// and the child is not asked about it again; twice and then right settles it
/// with the value, two `agent.request.invalid` recorded; `fail` settles one
/// `AgentRequestFailed` with its message.
#[tokio::test]
async fn invalid_replies_count_attempts_and_fail_ends_a_request() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_ATTEMPTS",
        "",
        vec![
            (
                "ROLE attempts",
                ready_then(scripted(vec![
                    submit(
                        "c1",
                        &py(r#"
                            ds = {}
                            while len(ds) < 3:
                                d = await runtime.channels['fizz'].receive()
                                ds[d.body] = d
                            async def reply(d, value):
                                try:
                                    await d.reply(value)
                                except Exception as e:
                                    print(d.body, type(e).__name__)
                            for _ in range(3):
                                await reply(ds['wrong'], {'n': 'x'})
                            for _ in range(2):
                                await reply(ds['right'], {'n': 'x'})
                            await reply(ds['right'], Thing(n=1))
                            await ds['fail'].fail('why')
                            "#),
                    ),
                    text_reply("done"),
                ])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {class}
                    t = Attempts()
                    hs = {{body: t.fizz(body) for body in ('wrong', 'right', 'fail')}}
                    for body, h in hs.items():
                        try:
                            print(body, (await h).n)
                        except outrig.AgentError as e:
                            print(body, type(e).__name__, str(e) if body == 'fail' else '')
                    "#,
                    class = indented(&fizzing("Attempts", "attempts"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    // The child's round ends after the replies the primary waited for: the
    // primary's round, the child's launch round, and its one.
    tree.seen("model.round.completed", 3).await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "wrong CompletionRejected \nright 1\nfail AgentRequestFailed why\n"
    );
    assert_eq!(
        past_launch(&requests, "ROLE attempts").len(),
        2,
        "nothing open after the round, so no further call"
    );
    let ids: Vec<_> = kind(&events, "agent.request.sent", None)
        .into_iter()
        .map(|event| data(event)["request"].clone())
        .collect();
    let invalid = |id: &serde_json::Value| -> Vec<u64> {
        kind(&events, "agent.request.invalid", None)
            .into_iter()
            .map(data)
            .filter(|data| data["request"] == *id)
            .map(|data| data["attempt"].as_u64().expect("an attempt"))
            .collect()
    };
    assert_eq!(invalid(&ids[0]), [1, 2, 3]);
    assert_eq!(invalid(&ids[1]), [1, 2]);
    let outcomes: Vec<_> = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(outcomes, ["rejected", "accepted", "failed"]);
}

/// Releasing an instance settles the request it holds `AgentReleased` and
/// releases what its child launched, whose waiters settle too; a second release
/// returns at once, and a call after it raises with nothing sent. `async with`
/// releases on exit when its body raises, and the exception goes on.
#[tokio::test]
async fn release_settles_and_closes() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_RELEASE",
        "",
        vec![
            (
                "ROLE holder",
                ready_then(scripted(vec![
                    submit(
                        "c1",
                        "g = await runtime.spawn('g', prompt='ROLE grand: never answers', \
                         requests={'ping': (str, Thing)})\n\
                         gh = g.channels['ping'].request('x')\n\
                         d = await runtime.channels['fizz'].receive()",
                    ),
                    text_reply("holding"),
                    text_reply("still holding"),
                ])),
            ),
            ("ROLE grand", scripted(vec![text_reply("not answering")])),
            (
                "ROLE ctx",
                ready_then(scripted(vec![text_reply("nothing")])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {holder}
                    {ctx}
                    held = Holder()
                    h = held.fizz('kept')
                    await until(lambda: h.status.state == 'received')
                    await held.release()
                    try:
                        await h
                    except outrig.AgentReleased:
                        print('released', h.status.state, h.status.reason)
                    started = time.monotonic()
                    await held.release()
                    print('again', time.monotonic() - started < 1)
                    try:
                        held.fizz('late')
                    except outrig.AgentReleased:
                        print('refused')
                    try:
                        async with Ctx() as c:
                            raise KeyError('body')
                    except KeyError:
                        print('propagated')
                    "#,
                    holder = indented(&fizzing("Holder", "holder"), 20),
                    ctx = indented(&fizzing("Ctx", "ctx"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "released failed released\nagain True\nrefused\npropagated\n"
    );
    let sent = kind(&events, "agent.request.sent", None);
    assert_eq!(
        sent.len(),
        2,
        "the kept request and the grandchild's; none late"
    );
    let grand = sent
        .iter()
        .find(|event| event.subject.as_ref().map(|s| s.as_str()) == Some("agent/c2"))
        .map(|event| data(event)["request"].clone())
        .expect("the grandchild's request");
    let settled = kind(&events, "agent.request.settled", None)
        .into_iter()
        .map(data)
        .find(|data| data["request"] == grand)
        .expect("the grandchild's request settled");
    assert_eq!(settled["outcome"], "released");
    let releases = kind(&events, "agent.released", Some("agent/primary"));
    assert_eq!(data(releases[0])["ended"], json!(["c1", "c2"]));
    let released: Vec<_> = kind(&events, "agent.instance.released", None)
        .into_iter()
        .map(data)
        .collect();
    assert_eq!(released.len(), 2);
    assert_eq!(released[0]["how"], "explicit");
    let kept = kind(&events, "agent.request.sent", Some("agent/c1"))
        .first()
        .map(|event| data(event)["request"].clone())
        .expect("the kept request");
    assert_eq!(released[0]["requests"], json!([kept]));
    assert_eq!(released[1]["how"], "async_with");
}

/// An instance nothing holds is released once the requests made of it have
/// ended: one dropped with a request in flight, its handle dropped too, stays
/// until the request settles, and one dropped idle goes at once; each is
/// recorded collected, then released.
#[tokio::test]
async fn a_dropped_instance_is_released_once_its_requests_end() {
    let flag = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_DROPPED",
        "",
        vec![
            (
                "ROLE dropped",
                ready_then(scripted(child_runs(
                    "c1",
                    &format!(
                        "d = await runtime.channels['fizz'].receive()\n{}\nawait d.reply(Thing(n=1))",
                        flag.awaited()
                    ),
                ))),
            ),
            ("ROLE idle", ready_then(scripted(vec![text_reply("nothing")]))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import gc
                    {dropped}
                    {idle}
                    Dropped().fizz('x')
                    gc.collect()
                    i = Idle()
                    await i.ready()
                    del i
                    gc.collect()
                    "#,
                    dropped = indented(&fizzing("Dropped", "dropped"), 20),
                    idle = indented(&fizzing("Idle", "idle"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    tree.seen("agent.instance.released", 1).await;
    {
        let events = tree.events.lock().expect("events");
        let dropped = child_of(&events, "Dropped");
        assert!(
            position(&events, "agent.instance.released", |event| subject(event)
                == dropped)
            .is_empty(),
            "not while its request is open"
        );
    }
    flag.raise();
    tree.seen("agent.instance.released", 2).await;
    let (_, events, _) = tree.end().await;
    for (class, handle) in [("Dropped", true), ("Idle", false)] {
        let child = child_of(&events, class);
        let at = |kind: &str| position(&events, kind, |event| subject(event) == child);
        let (collected, released) = (
            at("agent.instance.collected"),
            at("agent.instance.released"),
        );
        assert_eq!((collected.len(), released.len()), (1, 1), "{child}");
        assert!(collected[0] < released[0], "{child}");
        assert_eq!(data(&events[released[0]])["how"], "collected", "{child}");
        assert_eq!(
            at("agent.instance.handle.collected").len(),
            usize::from(handle)
        );
        if handle {
            let settled = at("agent.request.settled");
            assert_eq!(data(&events[settled[0]])["outcome"], "accepted");
            assert!(settled[0] < collected[0], "the request first");
        }
    }
}

/// What the launch meets surfaces at `ready()` and at a request, never in the
/// constructor, but for an input outside the subset; and an instance whose
/// launch was refused records nothing.
#[tokio::test]
async fn errors_surface_where_the_design_says() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_ERRORS",
        "",
        vec![(
            "",
            primary_waits(&format!(
                r#"
                {class}
                try:
                    Nowhere(x=object())
                except TypeError as e:
                    print('constructor:', 'x is a object' in str(e))
                n = Nowhere(model='nope')
                print('returned')
                h = n.fizz('x')
                for what, wait in (('ready', n.ready()), ('handle', h)):
                    try:
                        await wait
                    except ValueError as e:
                        print(what, 'Configured models: fast, sonnet' in str(e))
                try:
                    n.fizz('later')
                except ValueError as e:
                    print('later', '"nope"' in str(e))
                "#,
                class = indented(&fizzing("Nowhere", "nowhere"), 16),
            )),
        )],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "constructor: True\nreturned\nready True\nhandle True\nlater True\n"
    );
    assert!(kind(&events, "agent.spawned", None).is_empty(), "no spawn");
    assert!(
        events
            .iter()
            .all(|event| !event.kind().starts_with("agent.instance")
                && event.kind() != "agent.request.sent"),
        "a refused launch records nothing"
    );
}

/// With `children-max = 1` and an idle instance resident, a second instance's
/// `ready()` and its first handle raise `AgentLimitReached` and it makes no
/// spawn; releasing the first makes room for the next.
#[tokio::test]
async fn an_idle_instance_counts_against_children_max() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_CHILDREN_MAX",
        "children-max = 1",
        vec![
            ("ROLE first", ready_then(scripted(vec![text_reply("idle")]))),
            ("ROLE third", ready_then(serving(ANSWER_ONE))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {first}
                    {second}
                    {third}
                    a = First()
                    await a.ready()
                    b = Second()
                    h = b.fizz('x')
                    for what, wait in (('ready', b.ready()), ('handle', h)):
                        try:
                            await wait
                        except outrig.AgentLimitReached as e:
                            print(what, 'children-max' in str(e))
                    await a.release()
                    c = Third()
                    await c.ready()
                    print('room', (await c.fizz('y')).n > 0)
                    await c.release()
                    "#,
                    first = indented(&fizzing("First", "first"), 20),
                    second = indented(&fizzing("Second", "second"), 20),
                    third = indented(&fizzing("Third", "third"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "ready True\nhandle True\nroom True\n"
    );
    assert_eq!(
        kind(&events, "agent.spawned", None).len(),
        2,
        "the first and the third"
    );
}

/// An instance constructed by a child already at `subagent-depth-max` fails
/// at `ready()`, as every launch the host refuses does.
#[tokio::test]
async fn an_instance_from_a_child_at_the_depth_limit_fails_at_ready() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_DEPTH",
        "subagent-depth-max = 2",
        vec![
            (
                "ROLE deep",
                ready_then(scripted(child_runs(
                    "c1",
                    &format!(
                        "import outrig\nd = await runtime.channels['fizz'].receive()\n{}\ninner = Inner()\n\
                         try:\n    await inner.ready()\n    n = 0\n\
                         except outrig.AgentLimitReached as e:\n    \
                         n = int('subagent-depth-max is 2' in str(e))\n\
                         await d.reply(Thing(n=n))",
                        fizzing("Inner", "inner")
                    ),
                ))),
            ),
            (
                "",
                primary_waits(&format!(
                    "{}\nd = Deep()\nprint((await d.fizz('go')).n)\nawait d.release()",
                    fizzing("Deep", "deep")
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "1\n");
    assert_eq!(kind(&events, "agent.spawned", None).len(), 1);
}

/// `requests-max` counts what an instance holds: with it at 2, one request
/// received and unanswered and one unread, a third call raises at the call
/// with nothing sent, and cancelling the unread one makes room; and two calls
/// made before the child exists count the same way.
#[tokio::test]
async fn requests_max_counts_an_instances_unsettled_requests() {
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_REQUESTS_MAX",
        "requests-max = 2",
        vec![
            (
                "ROLE capped",
                ready_then(scripted(vec![
                    submit("c1", "d = await runtime.channels['fizz'].receive()"),
                    text_reply("took one"),
                    text_reply("not now"),
                ])),
            ),
            (
                "ROLE early",
                ready_then(scripted(vec![text_reply("nothing")])),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {capped}
                    {early}
                    c = Capped()
                    await c.ready()
                    h1 = c.fizz('a')
                    await until(lambda: h1.status.state == 'received' and h1.status.child == 'idle')
                    h2 = c.fizz('b')
                    try:
                        c.fizz('c')
                    except outrig.AgentLimitReached as e:
                        print('limit', 'requests-max' in str(e))
                    h2.cancel()
                    h3 = c.fizz('d')
                    print('room', h3.status.state)
                    e = Early()
                    held = [e.fizz('1'), e.fizz('2')]
                    try:
                        e.fizz('3')
                    except outrig.AgentLimitReached:
                        print('held limit', [h.status.state for h in held])
                    "#,
                    capped = indented(&fizzing("Capped", "capped"), 20),
                    early = indented(&fizzing("Early", "early"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "limit True\nroom waiting\nheld limit ['waiting', 'waiting']\n"
    );
    let bodies: Vec<String> = kind(&events, "agent.request.sent", None)
        .into_iter()
        .map(|event| data(event)["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !bodies.contains(&"\"c\"".to_string()) && !bodies.contains(&"\"3\"".to_string()),
        "the refused calls sent nothing: {bodies:?}"
    );
}

/// `h.cancel()` before the child takes the request removes it, so it is never
/// received; after, the child's reply raises `RequestCancelled` and the handle
/// reads cancelled.
#[tokio::test]
async fn cancellation_before_and_after_receipt() {
    let (taken, go) = (Flag::new(), Flag::new());
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_CANCEL",
        "",
        vec![
            (
                "ROLE cancelled",
                ready_then(scripted(child_runs(
                    "c1",
                    &format!(
                        "import outrig\nd = await runtime.channels['fizz'].receive()\nopen({taken}, 'w').close()\n\
                         {wait}\ntry:\n    await d.reply(Thing(n=1))\n\
                         except outrig.RequestCancelled:\n    print('reply refused')",
                        taken = taken.py(),
                        wait = go.awaited(),
                    ),
                ))),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {class}
                    c = Cancelled()
                    first = c.fizz('first')
                    await until(lambda: os.path.exists({taken}))
                    second = c.fizz('second')
                    print(second.cancel(), second.status)
                    print(first.cancel(), first.status)
                    open({go}, 'w').close()
                    "#,
                    class = indented(&fizzing("Cancelled", "cancelled"), 20),
                    taken = taken.py(),
                    go = go.py(),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    // The child's round, whose reply was refused, ends after the primary's: the
    // primary's round, the child's launch round, and its one.
    tree.seen("model.round.completed", 3).await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "True failed: cancelled (child running)\nTrue failed: cancelled (child running)\n"
    );
    let ids: Vec<_> = kind(&events, "agent.request.sent", None)
        .into_iter()
        .map(|event| data(event)["request"].clone())
        .collect();
    let received = |id: &serde_json::Value| {
        kind(&events, "agent.request.received", None)
            .into_iter()
            .any(|event| data(event)["request"] == *id)
    };
    assert!(received(&ids[0]) && !received(&ids[1]));
    let cancelled: Vec<_> = kind(&events, "agent.request.cancelled", None)
        .into_iter()
        .map(|event| {
            (
                data(event)["request"].clone(),
                data(event)["received"].clone(),
            )
        })
        .collect();
    assert_eq!(
        cancelled,
        [
            (ids[1].clone(), json!(false)),
            (ids[0].clone(), json!(true))
        ]
    );
    assert!(
        exec_outputs(&events, "agent/c1")
            .concat()
            .contains("reply refused"),
        "the child's reply raised"
    );
}

/// The session closes while one instance is in its launch round and another
/// holds two requests unanswered: both `ready()` calls and both handles raise
/// the closing error within the test's deadline, each instance is released by
/// the close, the report lists the two requests, and an instance collected
/// after the close is recorded and released no further.
#[tokio::test]
async fn the_close_ends_launching_and_working_instances() {
    let (closing, finish) = (Flag::new(), Flag::new());
    let hold = Arc::new(Hold::default());
    let mut tree = tree(
        "OUTRIG_TEST_INSTANCE_CLOSE",
        "",
        vec![
            ("ROLE launching", held(vec![text_reply("ready")], Arc::clone(&hold))),
            (
                "ROLE working",
                scripted(child_runs(
                    "c1",
                    &format!(
                        "ds = [await runtime.channels['fizz'].receive() for _ in range(2)]\n{}",
                        finish.awaited()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import gc
                    {launching}
                    {working}
                    a = Launching()
                    b = Working()
                    hs = [b.fizz('x'), b.fizz('y')]
                    await until(lambda: all(h.status.state == 'received' for h in hs))
                    open({closing}, 'w').close()
                    await until(lambda: all(h.done() for h in hs))
                    for what, wait in (('a', a.ready()), ('b', b.ready()), ('x', hs[0]), ('y', hs[1])):
                        try:
                            await asyncio.wait_for(wait, 10)
                        except outrig.SessionClosing:
                            print(what, 'closed')
                    del a
                    _ = gc.collect()
                    "#,
                    launching = indented(&fizzing("Launching", "launching"), 20),
                    working = indented(&fizzing("Working", "working"), 20),
                    closing = closing.py(),
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
    tree.seen("agent.instance.collected", 1).await;
    finish.raise();
    hold.go.notify_one();
    let (report, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "a closed\nb closed\nx closed\ny closed\n"
    );
    let (launching, working) = (child_of(&events, "Launching"), child_of(&events, "Working"));
    assert_eq!(report.requests.len(), 2);
    assert!(
        report
            .requests
            .iter()
            .all(|row| row.outcome == RequestEnd::Closed
                && format!("agent/{}", row.child) == working),
        "{:?}",
        report.requests
    );
    let released: Vec<_> = kind(&events, "agent.instance.released", None)
        .into_iter()
        .map(|event| (subject(event).to_string(), data(event)))
        .collect();
    assert_eq!(released.len(), 2, "each once, by the close: {released:?}");
    assert!(released.iter().all(|(_, data)| data["how"] == "closing"));
    let requests = |child: &str| {
        released
            .iter()
            .find(|(subject, _)| subject == child)
            .map(|(_, data)| data["requests"].as_array().map_or(0, Vec::len))
    };
    assert_eq!(
        (requests(&launching), requests(&working)),
        (Some(0), Some(2))
    );
    let collected = kind(&events, "agent.instance.collected", None);
    assert_eq!(collected.len(), 1);
    assert_eq!(subject(collected[0]), launching);
    assert!(
        kind(&events, "agent.instance.ready", None).is_empty(),
        "neither was ready"
    );
}
