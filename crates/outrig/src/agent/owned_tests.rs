//! Agent classes from a host program, end to end: a program such as `outrig run-new` runs
//! declares an `outrig.Agent` subclass and drives its instance, a root child of a session on the
//! host whose rounds run against the mock provider. The program prints what it saw, a JSON value
//! a line, and what the session did is in its `events.jsonl`.
//!
//! The mock routes each child by a marker its system prompt carries, in its class's docstring.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use super::agent_tests::keyed;
use super::children_tests::{Hold, held};
use super::mock_http::{self, Gauge, MODEL, RecordedRequest, Responder, submit, text_reply};
use crate::config::Config;
use crate::events::of_kind;
use crate::harness::RunningProgram;
use crate::owner::tests::{Bed, Ran, Sessions};
use crate::python::testing::py;

/// The global configuration: the mock's `sonnet` as `default-model`, `fast` an alias of it, the
/// session's events recorded, and `top` among the top-level keys.
fn configured(bed: &Bed, addr: SocketAddr, var: &str, top: &str) -> Config {
    bed.config(
        &format!(
            r#"
default-model = "sonnet"
{top}

[events]
mode = "record"

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
"#
        ),
        false,
    )
}

/// A program running, its sessions on the host and their model calls answered by `routes`.
struct Owned {
    bed: Bed,
    program: RunningProgram,
    requests: UnboundedReceiver<RecordedRequest>,
    /// How many model calls were open at once.
    gauge: Arc<Gauge>,
}

/// Start `script`, its model's key named `var` and `top` among its configuration's top-level
/// keys, every model call its sessions make answered by `routes`.
async fn owned(
    var: &str,
    top: &str,
    script: &str,
    routes: Vec<(&'static str, Responder)>,
) -> Owned {
    owned_with(var, top, script, |_| routes).await
}

/// [`owned`], its routes made knowing the bed the program runs in.
async fn owned_with(
    var: &str,
    top: &str,
    script: &str,
    routes: impl FnOnce(&Bed) -> Vec<(&'static str, Responder)>,
) -> Owned {
    owned_held(var, top, script, routes, None).await
}

/// [`owned_with`], each model call held by the mock until `hold` are open at once, or a second
/// has passed.
async fn owned_held(
    var: &str,
    top: &str,
    script: &str,
    routes: impl FnOnce(&Bed) -> Vec<(&'static str, Responder)>,
    hold: Option<usize>,
) -> Owned {
    let bed = Bed::new();
    let (addr, requests, gauge) = mock_http::routed(routes(&bed), hold).await;
    let config = configured(&bed, addr, var, top);
    bed.write("program.py", script);
    let sessions = Sessions {
        secrets: Some(Arc::new(keyed(var))),
        ..bed.sessions()
    };
    let program = bed
        .start("program.py", config, sessions, |program| program)
        .await;
    Owned {
        bed,
        program,
        requests,
        gauge,
    }
}

impl Owned {
    /// Wait for the program and its sessions to end, and return what it wrote, every record of
    /// its sessions' events, and every model call they made.
    async fn finish(mut self) -> (Ran, Vec<Value>, Vec<RecordedRequest>) {
        let ran = self.bed.finish(self.program).await;
        let records = self
            .bed
            .session_dirs()
            .iter()
            .flat_map(|dir| crate::events::recorded(&dir.join("logs")))
            .collect();
        (ran, records, mock_http::drain(&mut self.requests))
    }
}

/// The program's `ProjectReader`, its child marked `ROLE reader`, the requirements' own.
const READER: &str = r#"
class ProjectReader(outrig.Agent):
    """ROLE reader. You read the project, and answer questions about it."""

    @outrig.agent
    async def explain(self, question: str) -> str:
        """Answer `question` about the project."""
        ...
"#;

/// The requirements' program against a mock model: the constructor and the first call return
/// while the child's first model call is held at the provider, `asyncio.wait` takes the handle's
/// future, `result()` reads the reply, one awaited call works in one line, and the second request's
/// round reads what the first bound -- one child, whose parent is the program.
#[tokio::test]
async fn the_requirements_reader_answers_from_the_program() {
    let hold = Arc::new(Hold::default());
    let script = format!(
        r#"
import asyncio, json
import outrig, wait
{READER}
async def main():
    async with outrig.Session() as session:
        agent = ProjectReader(model="fast")
        handle = agent.explain("What does this project do?")
        print(json.dumps([handle.done(), handle.id, str(handle.status)]), flush=True)
        wait.raise_flag("constructed")
        done, pending = await asyncio.wait({{handle.future}}, timeout=60)
        first = handle.result()
        second = await agent.explain("What did you read first?")
        print(json.dumps([len(done), len(pending), first, second, type(first).__name__,
                          handle.status.state]), flush=True)
        await agent.release()
    print(json.dumps(session.shutdown_report.verdict), flush=True)

asyncio.run(main())
"#
    );
    let run = owned(
        "OUTRIG_TEST_OWNED_READER",
        "",
        &script,
        vec![(
            "ROLE reader",
            held(
                vec![
                    text_reply("ready"),
                    submit(
                        "r1",
                        &py("d = await runtime.channels['explain'].receive()\n\
                             first = d.body\n\
                             await d.reply('It reads ' + d.body)"),
                    ),
                    text_reply("answered"),
                    submit(
                        "r2",
                        &py("d = await runtime.channels['explain'].receive()\n\
                             await d.reply('First it read: ' + first)"),
                    ),
                    text_reply("answered"),
                ],
                Arc::clone(&hold),
            ),
        )],
    )
    .await;
    run.bed.raised("constructed").await;
    hold.go.notify_one();
    let (ran, records, requests) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [
            json!([false, null, "waiting (child idle)"]),
            json!([
                1,
                0,
                "It reads What does this project do?",
                "First it read: What does this project do?",
                "str",
                "accepted"
            ]),
            json!("clean"),
        ],
        "{}",
        ran.stderr
    );
    let spawned = of_kind(&records, "agent.spawned");
    assert_eq!(spawned.len(), 1, "one spawn");
    assert_eq!(
        (
            &spawned[0]["parent"],
            &spawned[0]["depth"],
            &spawned[0]["model"]
        ),
        (&json!("owner"), &json!(1), &json!("fast"))
    );
    let started = of_kind(&records, "agent.instance.started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["parent"], "owner");
    assert_eq!(
        (&started[0]["module"], &started[0]["qualname"]),
        (&json!("__main__"), &json!("ProjectReader"))
    );
    assert_eq!(
        started[0]["channels"],
        json!([{"name": "explain", "request": "str", "reply": "str"}])
    );
    let sent = of_kind(&records, "agent.request.sent");
    assert_eq!(sent.len(), 2);
    assert!(
        sent.iter().all(|data| data["sender"] == "owner"),
        "{sent:?}"
    );
    let released = of_kind(&records, "agent.instance.released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["how"], "explicit");
    assert!(
        requests
            .iter()
            .all(|request| request.body["model"] == MODEL),
        "the alias's model answered every call"
    );
}

/// The requirements' typed reply: `Description` and its `Literal` outcome, declared in the
/// program. The child's first reply holds an outcome outside the `Literal`, which the program's
/// decoding finds and the child's `reply` raises; its next execution replies validly, and the
/// result is an instance of the program's own class, no execution having run twice. Three
/// invalid replies to a second request end it `CompletionRejected`, carrying the last value and
/// its problems.
#[tokio::test]
async fn a_reply_is_decoded_into_the_programs_class_and_repaired() {
    let script = r#"
        import asyncio, json
        from dataclasses import dataclass
        from typing import Literal
        import outrig

        @dataclass
        class Description:
            outcome: Literal["complete", "partial"]
            summary: str

        class Describer(outrig.Agent):
            """ROLE describer. You describe what you are asked about."""

            @outrig.agent
            async def describe(self, subject: str) -> Description:
                """Describe `subject`."""
                ...

        async def main():
            async with outrig.Session():
                async with Describer() as agent:
                    repaired = await agent.describe("first")
                    try:
                        await agent.describe("second")
                    except outrig.CompletionRejected as e:
                        rejected = [e.value, [str(p) for p in e.problems]]
            print(json.dumps([type(repaired) is Description, repaired.outcome, repaired.summary,
                              rejected]), flush=True)

        asyncio.run(main())
        "#;
    let run = owned(
        "OUTRIG_TEST_OWNED_DESCRIBER",
        "",
        script,
        vec![(
            "ROLE describer",
            super::mock_http::scripted(vec![
                text_reply("ready"),
                submit(
                    "e1",
                    &py("d = await runtime.channels['describe'].receive()\n\
                         await d.reply({'outcome': 'done', 'summary': 'a first look'})"),
                ),
                submit(
                    "e2",
                    "await d.reply(Description(outcome='complete', summary='looked at ' + d.body))",
                ),
                text_reply("described"),
                submit(
                    "e3",
                    &py("d = await runtime.channels['describe'].receive()\n\
                         for _ in range(3):\n    \
                             try:\n        \
                                 await d.reply({'outcome': 'done', 'summary': 'again'})\n    \
                             except Exception as e:\n        \
                                 print(type(e).__name__)"),
                ),
                text_reply("gave up"),
            ]),
        )],
    )
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            true,
            "complete",
            "looked at first",
            [
                {"outcome": "done", "summary": "again"},
                ["$.outcome: expected one of \"complete\", \"partial\", not \"done\""]
            ]
        ])],
        "{}",
        ran.stderr
    );
    let settled = of_kind(&records, "agent.request.settled");
    let outcomes: Vec<&Value> = settled.iter().map(|data| &data["outcome"]).collect();
    assert_eq!(outcomes, [&json!("accepted"), &json!("rejected")]);
    let invalid = of_kind(&records, "agent.request.invalid");
    let of_request = |request: &Value| -> Vec<(u64, Value)> {
        invalid
            .iter()
            .filter(|data| data["request"] == *request)
            .map(|data| (data["attempt"].as_u64().unwrap(), data["problems"].clone()))
            .collect()
    };
    let outcome = json!(["$.outcome: expected one of \"complete\", \"partial\", not \"done\""]);
    assert_eq!(
        of_request(&settled[0]["request"]),
        [(1, outcome.clone())],
        "one invalid reply named $.outcome"
    );
    assert_eq!(
        of_request(&settled[1]["request"]),
        [(1, outcome.clone()), (2, outcome.clone()), (3, outcome)]
    );
    let executions: Vec<String> = of_kind(&records, "exec.submitted")
        .into_iter()
        .map(|data| data["execid"].to_string())
        .collect();
    let once: std::collections::BTreeSet<&String> = executions.iter().collect();
    assert_eq!(executions.len(), 3, "{executions:?}");
    assert_eq!(
        once.len(),
        executions.len(),
        "no execution ran twice: {executions:?}"
    );
}

/// The planned rules hold for types the program declares: an `alias("file-path")` is the key and
/// the field's own name an unknown key; a nested dataclass, a `dict[str, T]`, a required but
/// nullable field and a defaulted one decode as `outrig.schema` says; and a method of two
/// parameters, one defaulted, reaches the child as its frozen generated message. The child's
/// instance of the class made again from the program's description encodes to what the program
/// decodes into its own, a dict in the order the child made it, as a dict input reaches the child
/// in the program's.
#[tokio::test]
async fn the_planned_rules_hold_for_types_the_program_declares() {
    let script = r#"
        import asyncio, dataclasses, json
        from dataclasses import dataclass, field
        from typing import Annotated, Optional
        import outrig
        from outrig.schema import alias

        @dataclass
        class Location:
            file_path: Annotated[str, alias("file-path")]
            line: int

        @dataclass
        class Finding:
            where: Location
            notes: dict[str, str]
            reference: Optional[str]
            severity: str = "low"
            tags: list[str] = field(default_factory=lambda: ["new"])

        class Finder(outrig.Agent):
            """ROLE finder. You find what you are asked for."""

            @outrig.agent
            async def find(self, query: str, limit: int = 2) -> Finding:
                """Find `query`, at most `limit` times."""
                ...

        async def main():
            async with outrig.Session():
                async with Finder(order={"z": 1, "y": 2}) as agent:
                    found = await agent.find("needle")
            print(json.dumps([type(found.where) is Location, found.where.file_path,
                              found.where.line, found.notes, found.reference, found.severity,
                              found.tags, list(found.notes)]), flush=True)

        asyncio.run(main())
        "#;
    let run = owned(
        "OUTRIG_TEST_OWNED_FINDER",
        "",
        script,
        vec![(
            "ROLE finder",
            super::mock_http::scripted(vec![
                text_reply("ready"),
                submit(
                    "f1",
                    &py(r#"
                        d = await runtime.channels['find'].receive()
                        seen = []
                        for wrong in ({'where': {'file_path': 'a', 'line': 1}, 'notes': {},
                                       'reference': None},
                                      {'where': {'file-path': 'a', 'line': 1}, 'notes': {}}):
                            try:
                                await d.reply(wrong)
                            except Exception as e:
                                seen.append(type(e).__name__)
                        try:
                            d.body.limit = 3
                        except Exception as e:
                            seen.append(type(e).__name__)
                        await d.reply(Finding(
                            where=Location(file_path='src/a.py', line=3),
                            notes={'seen': ' '.join(seen), 'query': d.body.query,
                                   'limit': str(d.body.limit), 'got': type(d.body).__name__,
                                   'order': ' '.join(order)},
                            reference=None))
                        "#),
                ),
                text_reply("found"),
            ]),
        )],
    )
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            true,
            "src/a.py",
            3,
            {
                "got": "FinderFindMessage",
                "limit": "2",
                "query": "needle",
                "seen": "DecodeError DecodeError FrozenInstanceError",
                "order": "z y"
            },
            null,
            "low",
            ["new"],
            ["seen", "query", "limit", "got", "order"]
        ])],
        "{}",
        ran.stderr
    );
    let problems: Vec<&Value> = of_kind(&records, "agent.request.invalid")
        .into_iter()
        .map(|data| &data["problems"])
        .collect();
    assert_eq!(
        problems,
        [
            &json!([
                "$.where.file_path: unknown key; that field's key is \"file-path\"",
                "$.where.file-path: missing required key"
            ]),
            &json!(["$.reference: missing required key"]),
        ]
    );
}

/// A responder for any number of a child's rounds, each answered the same: the round's first call
/// runs `source`, and the call after its result ends the round. `{flags}` in `source` reads as the
/// directory the program raises its flags in.
fn serving_in(bed: &Bed, source: &str) -> Responder {
    let flags = bed.dir.to_str().expect("a UTF-8 path");
    super::children_tests::serving(&source.replace("{flags}", flags))
}

/// An agent class whose child, `ROLE asker`, answers each request `done <question>` -- one whose
/// question starts `hold` once a flag of that name is raised -- and says how an answer it could
/// not give was refused in a flag `refused-<question>`.
const ASKER: &str = r#"
class Asker(outrig.Agent):
    """ROLE asker. You answer each question."""

    @outrig.agent
    async def ask(self, question: str) -> str:
        """Answer `question`."""
        ...

async def until(test):
    for _ in range(3000):
        if test():
            return
        await asyncio.sleep(0.01)
    raise TimeoutError("never")
"#;

/// What the asker's child runs in each round: each request waiting is answered by a task of its
/// own, so a held one waits for its flag while the rest are answered.
const ASKS: &str = r#"
import asyncio, os
async def answer(d):
    if d.body.startswith('hold'):
        while not os.path.exists(os.path.join('{flags}', d.body)):
            await asyncio.sleep(0.02)
    try:
        await d.reply('done ' + d.body)
    except Exception as e:
        with open(os.path.join('{flags}', 'refused-' + d.body), 'w') as f:
            f.write(type(e).__name__)
channel = runtime.channels['ask']
while channel.pending():
    asyncio.create_task(answer(await channel.receive()))
"#;

/// The session an instance runs in is the one `session=` names, else the current one, and with
/// neither the constructor raises; so does one called with no running event loop, and one given
/// something that is no session.
#[tokio::test]
async fn construction_finds_its_session_and_loop() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    out = []
    try:
        Asker()
    except RuntimeError as e:
        out.append(str(e))
    async with outrig.Session() as outer:
        async with outrig.Session() as inner:
            agent = Asker(session=outer)
            out.append([agent._outrig.session is outer, outrig.current_session() is inner])
            try:
                await asyncio.to_thread(Asker, session=outer)
            except RuntimeError as e:
                out.append(str(e))
            try:
                Asker(session=object())
            except TypeError as e:
                out.append(str(e))
            await agent.release()
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_CONSTRUCTION", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, ASKS))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            "no outrig.Session is entered here: enter one with `async with outrig.Session()`, \
             or pass the session where it is needed",
            [true, true],
            "Asker() is constructed on the running event loop its requests' handles belong to, \
             and none runs here",
            "session= takes an outrig.Session, not object"
        ])],
        "{}",
        ran.stderr
    );
    assert_eq!(of_kind(&records, "agent.instance.started").len(), 1);
}

/// A handle's waits are the program's: `asyncio.wait` timing out, and the awaiting task
/// cancelled, leave the request pending, and a new await gives its reply. `cancel()` ends a
/// request the child has received, whose late answer is refused, and the instance answers the
/// next. With `requests-max` requests open a call raises at once and sends nothing, until one
/// ends. Leaving `async with` releases the instance, ending what is open `AgentReleased`, and a
/// call after it raises, sending nothing.
#[tokio::test]
async fn waits_cancels_and_releases_hold_on_the_host() {
    let script = format!(
        r#"
import asyncio, json, os
import outrig, wait
{ASKER}
async def main():
    out = []
    async with outrig.Session():
        async with Asker() as agent:
            slow = agent.ask("hold-slow")
            done, pending = await asyncio.wait({{slow.future}}, timeout=0.3)
            out.append([len(done), len(pending), slow.done()])
            try:
                await asyncio.wait_for(slow, 0.3)
            except TimeoutError:
                out.append(slow.done())
            wait.raise_flag("hold-slow")
            out.append(await slow)
            gone = agent.ask("hold-gone")
            await until(lambda: gone.status.state == "received")
            out.append([gone.cancel(), gone.done(), gone.status.state, gone.status.reason])
            try:
                await gone
            except outrig.RequestCancelled:
                out.append("cancelled")
            wait.raise_flag("hold-gone")
            await until(lambda: os.path.exists("refused-hold-gone"))
            out.append(open("refused-hold-gone").read())
            out.append(await agent.ask("next"))
            kept, spare = agent.ask("hold-kept"), agent.ask("hold-spare")
            try:
                agent.ask("third")
            except outrig.AgentLimitReached:
                out.append("limit")
            out.append(spare.cancel())
            out.append(await agent.ask("after"))
        try:
            await kept
        except outrig.AgentReleased:
            out.append("released")
        try:
            agent.ask("late")
        except outrig.AgentReleased:
            out.append("late")
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with(
        "OUTRIG_TEST_OWNED_WAITS",
        "requests-max = 2",
        &script,
        |bed| vec![("ROLE asker", serving_in(bed, ASKS))],
    )
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            [0, 1, false],
            false,
            "done hold-slow",
            [true, true, "failed", "cancelled"],
            "cancelled",
            "RequestCancelled",
            "done next",
            "limit",
            true,
            "done after",
            "released",
            "late"
        ])],
        "{}",
        ran.stderr
    );
    let bodies: Vec<String> = of_kind(&records, "agent.request.sent")
        .into_iter()
        .map(|data| data["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(bodies.len(), 6, "{bodies:?}");
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains("third") && !body.contains("late")),
        "nothing was sent past the limit, nor after the release: {bodies:?}"
    );
}

/// Arguments are checked at the call: `explain(1)` on a `str` channel raises `TypeError` and sends
/// nothing. `model=` names a configured model or alias, an unknown one raising at `ready()` and
/// at the first handle with the configured names, and later calls raising it too. With `model=`
/// omitted the child runs on `default-model`, and neither `default-agent` nor an `[agents.<name>]`
/// table named as the class is -- with a model and a preamble of its own -- changes what it gets.
#[tokio::test]
async fn models_and_arguments_are_checked_where_the_design_says() {
    let script = r#"
        import asyncio, json
        import outrig

        class Plain(outrig.Agent):
            """ROLE plain. You answer plainly."""

            @outrig.agent
            async def explain(self, question: str) -> str:
                """Answer `question`."""
                ...

        async def main():
            out = []
            async with outrig.Session():
                agent = Plain()
                try:
                    agent.explain(1)
                except TypeError as e:
                    out.append(str(e).splitlines()[0])
                out.append(await agent.explain("x"))
                bad = Plain(model="nope")
                for wait in (bad.ready(), bad.explain("y")):
                    try:
                        await wait
                    except ValueError as e:
                        out.append("Configured models: fast, other, sonnet" in str(e))
                try:
                    bad.explain("z")
                except ValueError:
                    out.append("later")
                await agent.release()
            print(json.dumps(out), flush=True)

        asyncio.run(main())
        "#;
    let run = owned(
        "OUTRIG_TEST_OWNED_MODELS",
        r#"default-agent = "Plain"

[agents.Plain]
model    = "other"
preamble = "PROFILE PREAMBLE"

[models.other]
provider   = "claude"
identifier = "other-model"
"#,
        script,
        vec![(
            "ROLE plain",
            super::children_tests::serving(
                "d = await runtime.channels['explain'].receive()\nawait d.reply('plainly ' + d.body)",
            ),
        )],
    )
    .await;
    let (ran, records, requests) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            "Plain.explain() was given arguments outside their declared types:",
            "plainly x",
            true,
            true,
            "later"
        ])],
        "{}",
        ran.stderr
    );
    assert_eq!(
        of_kind(&records, "agent.request.sent").len(),
        1,
        "only x was sent"
    );
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request.body["model"], MODEL, "default-model's identifier");
        assert!(
            !super::mock_http::system_text(&request.body).contains("PROFILE PREAMBLE"),
            "no profile's preamble"
        );
    }
}

/// An agent class whose child, `ROLE launching`, has its first round's call held at the provider:
/// an instance still launching.
const LAUNCHING: &str = r#"
class Launching(outrig.Agent):
    """ROLE launching. You are slow to get ready."""

    @outrig.agent
    async def ask(self, question: str) -> str:
        """Answer `question`."""
        ...
"#;

/// An instance alive when its session's block is left is released by the close: its open request
/// ends `SessionClosing`, the report lists it, a later call raises the same and sends nothing, and
/// a release then returns at once. `ShutdownException` is no `AgentError`.
#[tokio::test]
async fn the_session_exit_releases_a_live_instance() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    out = []
    session = outrig.Session()
    async with session:
        agent = Asker()
        held = agent.ask("hold-forever")
        await until(lambda: held.status.state == "received")
    try:
        await held
    except outrig.SessionClosing:
        out.append("closing")
    report = session.shutdown_report
    out.append([[r.child, r.channel, r.outcome] for r in report.requests])
    out.append(issubclass(outrig.ShutdownException, outrig.AgentError))
    try:
        agent.ask("after")
    except outrig.SessionClosing:
        out.append("after")
    await agent.release()
    out.append("released")
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_EXIT", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, ASKS))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            "closing",
            [["c1", "ask", "closed"]],
            false,
            "after",
            "released"
        ])],
        "{}",
        ran.stderr
    );
    let released = of_kind(&records, "agent.instance.released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["how"], "closing");
    assert_eq!(released[0]["requests"].as_array().map(Vec::len), Some(1));
    assert_eq!(of_kind(&records, "agent.request.sent").len(), 1);
}

/// The instance events are a kernel's: an instance the program drops idle is collected and then
/// released, and a handle dropped before its request ended is recorded; at the close, an
/// instance still launching never becomes ready -- `ready()` raises the closing error, and no
/// `agent.instance.ready` is recorded for it -- and it and the one still alive are released by it.
#[tokio::test]
async fn collection_and_the_close_are_evented_as_for_a_kernel() {
    let hold = Arc::new(Hold::default());
    let script = format!(
        r#"
import asyncio, gc, json
import outrig
{ASKER}
{LAUNCHING}
async def next_of(events, kind):
    async for event in events:
        if isinstance(event, outrig.Event) and event.type == kind:
            return event

async def main():
    out = []
    async with outrig.Session() as session:
        events = session.events()
        dropped = Asker()
        await dropped.ready()
        del dropped
        gc.collect()
        out.append((await next_of(events, "agent.instance.released")).data["how"])
        kept = Asker()
        handle = kept.ask("hold-on")
        await until(lambda: handle.status.state == "received")
        request = handle.id
        del handle
        gc.collect()
        out.append((await next_of(events, "agent.instance.handle.collected")).data["request"]
                   == request)
        launching = Launching()
        await next_of(events, "agent.instance.started")
    try:
        await launching.ready()
    except outrig.SessionClosing:
        out.append("never ready")
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let held_launch = Arc::clone(&hold);
    let run = owned_with("OUTRIG_TEST_OWNED_COLLECTED", "", &script, move |bed| {
        vec![
            ("ROLE asker", serving_in(bed, ASKS)),
            (
                "ROLE launching",
                held(vec![text_reply("ready")], held_launch),
            ),
        ]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    hold.go.notify_one();
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!(["collected", true, "never ready"])],
        "{}",
        ran.stderr
    );
    let subject_of = |kind: &str, qualname: &str| -> String {
        records
            .iter()
            .find(|record| {
                record["type"] == format!("org.outrig.{kind}")
                    && record["data"]["qualname"] == qualname
            })
            .map(|record| record["subject"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    };
    let launching = subject_of("agent.instance.started", "Launching");
    assert!(!launching.is_empty());
    assert_eq!(of_kind(&records, "agent.instance.collected").len(), 1);
    assert_eq!(
        of_kind(&records, "agent.instance.handle.collected").len(),
        1
    );
    let mut hows: Vec<String> = of_kind(&records, "agent.instance.released")
        .into_iter()
        .map(|data| data["how"].as_str().unwrap_or_default().to_string())
        .collect();
    hows.sort();
    assert_eq!(hows, ["closing", "closing", "collected"]);
    let ready: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == "org.outrig.agent.instance.ready")
        .map(|record| &record["subject"])
        .collect();
    assert_eq!(ready.len(), 2, "{ready:?}");
    assert!(ready.iter().all(|subject| **subject != json!(launching)));
}

/// A root child counts against the session's limits as any child does: with `children-max = 1`
/// and one instance resident, a second's `ready()` and first handle raise `AgentLimitReached`,
/// until the first is released.
#[tokio::test]
async fn a_root_child_counts_against_children_max() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    out = []
    async with outrig.Session():
        first = Asker()
        await first.ready()
        second = Asker()
        for wait in (second.ready(), second.ask("x")):
            try:
                await wait
            except outrig.AgentLimitReached:
                out.append("limit")
        await first.release()
        third = Asker()
        await third.ready()
        out.append("third")
        await third.release()
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with(
        "OUTRIG_TEST_OWNED_CHILDREN",
        "children-max = 1",
        &script,
        |bed| vec![("ROLE asker", serving_in(bed, ASKS))],
    )
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!(["limit", "limit", "third"])],
        "{}",
        ran.stderr
    );
    assert_eq!(
        of_kind(&records, "agent.spawned").len(),
        2,
        "the refused launch made none"
    );
}

/// A root child spends the session's token budget as any child does: once its first round's
/// first call has spent it, that round ends, and a request ends `BudgetExhausted` with no further
/// model call.
#[tokio::test]
async fn a_root_childs_request_ends_when_the_budget_is_spent() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    out = []
    async with outrig.Session():
        agent = Asker()
        await agent.ready()
        try:
            await agent.ask("x")
        except outrig.BudgetExhausted:
            out.append("budget")
        out.append(await agent.ready() is None)
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with(
        "OUTRIG_TEST_OWNED_BUDGET",
        "token-budget = 10",
        &script,
        |bed| vec![("ROLE asker", serving_in(bed, ASKS))],
    )
    .await;
    let (ran, _, requests) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(ran.json(), [json!(["budget", true])], "{}", ran.stderr);
    assert_eq!(
        requests.len(),
        1,
        "the first round's first call spent the budget, and no call followed it"
    );
}

/// A root child's model calls take the session's places: with `model-concurrency-max = 1`, two
/// instances launched together never have two calls open at once.
#[tokio::test]
async fn a_root_childs_calls_take_the_sessions_places() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    async with outrig.Session():
        first, second = Asker(), Asker()
        await asyncio.gather(first.ready(), second.ready())
    print(json.dumps("both"), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_held(
        "OUTRIG_TEST_OWNED_PLACES",
        "model-concurrency-max = 1",
        &script,
        |bed| vec![("ROLE asker", serving_in(bed, ASKS))],
        Some(2),
    )
    .await;
    let gauge = Arc::clone(&run.gauge);
    let (ran, _, requests) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(ran.json(), [json!("both")], "{}", ran.stderr);
    assert_eq!(requests.len(), 4, "two rounds of two calls");
    assert_eq!(gauge.most(), 1, "one call open at a time");
}

/// When the session's interpreter dies, what the program waits on ends rather than waiting for
/// good: an open request ends `SessionClosing` as `terminated`, an instance still launching never
/// becomes ready, and a request made after is refused the same way.
#[tokio::test]
async fn the_programs_waits_end_when_the_interpreter_dies() {
    let hold = Arc::new(Hold::default());
    let script = format!(
        r#"
import asyncio, json
import outrig, wait
{ASKER}
{LAUNCHING}
async def main():
    out = []
    async with outrig.Session() as session:
        agent = Asker()
        held = agent.ask("hold-forever")
        await until(lambda: held.status.state == "received")
        stuck = Launching()
        wait.raise_flag("dying")
        try:
            await held
        except outrig.SessionClosing:
            out.append(held.status.reason)
        try:
            await stuck.ready()
        except outrig.SessionClosing:
            out.append("stuck")
        try:
            await agent.ask("after")
        except outrig.SessionClosing:
            out.append("after")
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let held_launch = Arc::clone(&hold);
    let run = owned_with("OUTRIG_TEST_OWNED_DEATH", "", &script, move |bed| {
        vec![
            ("ROLE asker", serving_in(bed, ASKS)),
            (
                "ROLE launching",
                held(vec![text_reply("ready")], held_launch),
            ),
        ]
    })
    .await;
    run.bed.raised("dying").await;
    hold.arrived.notified().await;
    // From a kernel of the test's own, whose slot no round holds.
    let interpreter = crate::owner::tests::interpreter(&run.program, 1);
    crate::python::testing::within(interpreter.open("killer"))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    interpreter.send_line(json!({
        "t": "exec", "agent": "killer", "id": 1, "src": "import os\nos._exit(9)",
    }));
    let (ran, _, _) = run.finish().await;
    hold.go.notify_one();
    assert_eq!(
        ran.json(),
        [json!(["terminated", "stuck", "after"])],
        "{}",
        ran.stderr
    );
}

/// What the asker's child runs for `a_cancel_is_the_childs_kernels_to_decide`: it fails a
/// request asking `fail`, then raises the flag `failed`, and answers any other `done <question>`.
const DECIDES: &str = r#"
import asyncio, os
async def answer(d):
    if d.body == 'fail':
        await d.fail('it could not')
        open(os.path.join('{flags}', 'failed'), 'w').close()
    else:
        await d.reply('done ' + d.body)
channel = runtime.channels['ask']
while channel.pending():
    asyncio.create_task(answer(await channel.receive()))
"#;

/// A request cancelled before the launch answered ends in the program and never reaches the
/// child. One the child has failed, cancelled from a loop that has not run since, is not cancelled:
/// `cancel()` says no, and the handle and the session's record both say it failed.
#[tokio::test]
async fn a_cancel_is_the_childs_kernels_to_decide() {
    let script = format!(
        r#"
import asyncio, json, os, time
import outrig
{ASKER}
async def main():
    async with outrig.Session():
        agent = Asker()
        held = agent.ask("never")
        cancelled = held.cancel()
        await agent.ready()
        answered = await agent.ask("next")
        failing = agent.ask("fail")
        # The loop runs nothing until the child has failed it, so the end is heard off the loop.
        while not os.path.exists("failed"):
            time.sleep(0.01)
        late = failing.cancel()
        try:
            await failing
        except outrig.AgentRequestFailed as e:
            failed = str(e)
        print(json.dumps([cancelled, held.status.reason, answered, late, failing.status.reason,
                          failed]), flush=True)
        await agent.release()

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_DECIDES", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, DECIDES))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            true,
            "cancelled",
            "done next",
            false,
            "failed",
            "it could not"
        ])],
        "{}",
        ran.stderr
    );
    let sent: Vec<&Value> = of_kind(&records, "agent.request.sent")
        .iter()
        .map(|data| &data["channel"])
        .collect();
    assert_eq!(sent.len(), 2, "the held request was never sent: {sent:?}");
    let settled: Vec<&Value> = of_kind(&records, "agent.request.settled")
        .iter()
        .map(|data| &data["outcome"])
        .collect();
    assert_eq!(settled, [&json!("accepted"), &json!("failed")]);
}

/// What the asker's child runs for `a_rejection_and_a_late_verdict_are_each_for_their_reply`: to
/// `nan` it replies twice with what the program refuses and then with what has no JSON form; to
/// any other it replies, cancels its wait for the verdict, and replies twice more, first with what
/// the program refuses.
const JUDGED: &str = r#"
import asyncio, contextlib, math
async def answer(d):
    if d.body == 'nan':
        values = [7, 8, math.nan]
    else:
        first = asyncio.create_task(d.reply('first'))
        await asyncio.sleep(0)
        first.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await first
        values = [7, 'third']
    for value in values:
        with contextlib.suppress(Exception):
            await d.reply(value)
channel = runtime.channels['ask']
while channel.pending():
    asyncio.create_task(answer(await channel.receive()))
"#;

/// A request rejected for a last reply its child's kernel found wrong itself carries that reply's
/// problems, not the one before's. And a verdict on a reply whose wait the child cancelled is never
/// taken for the next one's: the program's refusal of that one stands, and the third is accepted.
#[tokio::test]
async fn a_rejection_and_a_late_verdict_are_each_for_their_reply() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    async with outrig.Session():
        async with Asker() as agent:
            try:
                await agent.ask("nan")
            except outrig.CompletionRejected as e:
                rejected = [e.value, [p.message for p in e.problems]]
            late = await agent.ask("late")
    print(json.dumps([rejected, late]), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_JUDGED", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, JUDGED))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!([
            [
                null,
                ["$ is nan, and a float that is not finite has no JSON form"]
            ],
            "third"
        ])],
        "{}",
        ran.stderr
    );
    let attempts: Vec<(Value, Value)> = of_kind(&records, "agent.request.settled")
        .iter()
        .map(|data| (data["outcome"].clone(), data["attempts"].clone()))
        .collect();
    assert_eq!(
        attempts,
        [(json!("rejected"), json!(3)), (json!("accepted"), json!(2))],
        "the cancelled wait's reply was no attempt"
    );
}

/// What the asker's child runs for `instances_start_and_release_once_with_ids_of_their_own`: to
/// `inner` it makes an agent class instance of its own, waits for it and releases it, then
/// answers; to any other it answers at once.
const MAKES: &str = r#"
import asyncio
import outrig

class Inner(outrig.Agent):
    """ROLE inner. You are made by another agent."""

    @outrig.agent
    async def ping(self) -> str:
        """Answer."""
        ...

async def answer(d):
    if d.body == 'inner':
        inner = Inner()
        await inner.ready()
        await inner.release()
    await d.reply('done ' + d.body)
channel = runtime.channels['ask']
while channel.pending():
    asyncio.create_task(answer(await channel.receive()))
"#;

/// Root children's instances and one a root child made each have an id no other has; and each
/// instance that started is released once -- one whose release runs on as the session's block is
/// left, and one whose launch meets the close, alike -- with nothing the program awaits left
/// waiting.
#[tokio::test]
async fn instances_start_and_release_once_with_ids_of_their_own() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    out = []
    async with outrig.Session():
        first, second = Asker(), Asker()
        out.append(await first.ask("inner"))
        await second.ready()
        releasing = asyncio.create_task(second.release())
        late = Asker()
    await releasing
    try:
        await late.ready()
        out.append("ready")
    except outrig.SessionClosing:
        out.append("closed")
    print(json.dumps(out), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_MAKES", "", &script, |bed| {
        vec![
            ("ROLE asker", serving_in(bed, MAKES)),
            ("ROLE inner", serving_in(bed, "pass")),
        ]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let out = ran.json();
    assert_eq!(out.len(), 1, "{}", ran.stderr);
    assert_eq!(out[0][0], "done inner");
    assert!(["ready", "closed"].contains(&out[0][1].as_str().unwrap_or_default()));
    let started: Vec<u64> = of_kind(&records, "agent.instance.started")
        .iter()
        .map(|data| data["instance"].as_u64().expect("an instance id"))
        .collect();
    let ids: std::collections::BTreeSet<&u64> = started.iter().collect();
    assert!(started.len() >= 3, "{started:?}");
    assert_eq!(
        ids.len(),
        started.len(),
        "no two instances share an id: {started:?}"
    );
    let released: Vec<u64> = of_kind(&records, "agent.instance.released")
        .iter()
        .map(|data| data["instance"].as_u64().expect("an instance id"))
        .collect();
    let mut once = released.clone();
    once.sort_unstable();
    let mut each = started.clone();
    each.sort_unstable();
    assert_eq!(
        once, each,
        "each instance that started released once: {released:?}"
    );
}

/// What the picker's child runs: it answers each request first with a string the program's
/// `Literal` refuses, which holds a lone surrogate no UTF-8 line can carry, then `yes`.
const PICKS: &str = r#"
import asyncio
async def answer(d):
    for value in ('\ud83d', 'yes'):
        try:
            await d.reply(value)
        except Exception:
            pass
channel = runtime.channels['pick']
while channel.pending():
    asyncio.create_task(answer(await channel.receive()))
"#;

/// A verdict whose problem names text no line can carry -- a lone surrogate the child replied
/// with -- still reaches the child, escaped: its reply raises it, and its next is accepted.
#[tokio::test]
async fn a_verdict_reaches_the_child_whatever_it_names() {
    let script = r#"
        import asyncio, json
        from typing import Literal
        import outrig

        class Picker(outrig.Agent):
            """ROLE picker. You pick."""

            @outrig.agent
            async def pick(self, question: str) -> Literal["yes", "no"]:
                """Pick for `question`."""
                ...

        async def main():
            async with outrig.Session():
                async with Picker() as agent:
                    picked = await agent.pick("which")
            print(json.dumps(picked), flush=True)

        asyncio.run(main())
        "#;
    let run = owned_with("OUTRIG_TEST_OWNED_PICKS", "", script, |bed| {
        vec![("ROLE picker", serving_in(bed, PICKS))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(ran.json(), [json!("yes")], "{}", ran.stderr);
    let invalid = of_kind(&records, "agent.request.invalid");
    assert_eq!(invalid.len(), 1, "{invalid:?}");
    assert!(
        invalid[0]["problems"][0]
            .as_str()
            .is_some_and(|problem| problem.contains("\\ud83d")),
        "{invalid:?}"
    );
}

/// A release that reaches the engine once admission has closed releases nothing: the instance is
/// the close's to release, and its release is recorded once, `closing`, with no release by the
/// program.
#[tokio::test]
async fn a_release_after_the_close_is_the_closes() {
    let script = format!(
        r#"
import asyncio, json
import outrig
{ASKER}
async def main():
    async with outrig.Session() as session:
        agent = Asker()
        await agent.ready()
        await asyncio.gather(session.close_admission(), agent.release())
    print(json.dumps("done"), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_LATE_RELEASE", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, ASKS))]
    })
    .await;
    let (ran, records, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(ran.json(), [json!("done")], "{}", ran.stderr);
    let released: Vec<&Value> = of_kind(&records, "agent.instance.released")
        .iter()
        .map(|data| &data["how"])
        .collect();
    assert_eq!(released, [&json!("closing")]);
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "org.outrig.agent.released"
                && record["subject"] == "owner"),
        "the program released nothing"
    );
}

/// What the program gives its child -- its inputs, its classes' defaults, the types of its
/// channels -- reaches the child as the program holds it: an int past 64 bits stays that int, and
/// a dict in a dataclass keeps its order, as no JSON the engine parsed would keep them.
#[tokio::test]
async fn what_the_program_gives_its_child_arrives_unchanged() {
    let script = r#"
        import asyncio
        from dataclasses import dataclass, field
        from typing import Literal
        import outrig

        @dataclass
        class Notes:
            order: dict[str, int]
            count: int = 2**64
            kept: dict[str, int] = field(default_factory=lambda: {"z": 1, "y": 2})

        class Keeper(outrig.Agent):
            """ROLE keeper. You keep what you are given."""

            @outrig.agent
            async def look(self, pick: Literal[18446744073709551616, 0]) -> str:
                """Say what you were given."""
                ...

        async def main():
            async with outrig.Session():
                async with Keeper(big=2**64, held=Notes(order={"z": 1, "y": 2})) as agent:
                    seen = await agent.look(2**64)
            print(seen, flush=True)

        asyncio.run(main())
        "#;
    let run = owned(
        "OUTRIG_TEST_OWNED_UNCHANGED",
        "",
        script,
        vec![(
            "ROLE keeper",
            super::mock_http::scripted(vec![
                text_reply("ready"),
                submit(
                    "f1",
                    &py(r#"
                        import json
                        d = await runtime.channels['look'].receive()
                        made = Notes(order={})
                        await d.reply(json.dumps([repr(big), list(held.order), repr(held.count),
                                                  list(held.kept), repr(made.count),
                                                  list(made.kept), repr(d.body)]))
                        "#),
                ),
                text_reply("looked"),
            ]),
        )],
    )
    .await;
    let (ran, _, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    let big = "18446744073709551616";
    assert_eq!(
        ran.json(),
        [json!([
            big,
            ["z", "y"],
            big,
            ["z", "y"],
            big,
            ["z", "y"],
            big
        ])],
        "{}",
        ran.stderr
    );
}

/// Each dataclass instance the program gives its child -- an input, one in its fields, a default
/// -- is in the child an instance of its own class, though a union's members share their fields
/// and the instance's JSON cannot say which it is.
#[tokio::test]
async fn instances_keep_their_classes_in_the_child() {
    let script = r#"
        import asyncio
        from dataclasses import dataclass, field
        import outrig

        @dataclass
        class Left:
            n: int

        @dataclass
        class Right:
            n: int

        @dataclass
        class Pair:
            first: Left | Right
            second: Left | Right = field(default_factory=lambda: Right(2))
            rows: list[Left | Right] = field(default_factory=list)

        class Keeper(outrig.Agent):
            """ROLE keeper. You keep what you are given."""

            @outrig.agent
            async def look(self) -> str:
                """Say what you were given."""
                ...

        async def main():
            async with outrig.Session():
                async with Keeper(pair=Pair(first=Right(1), rows=[Left(3), Right(4)])) as agent:
                    seen = await agent.look()
            print(seen, flush=True)

        asyncio.run(main())
        "#;
    let run = owned(
        "OUTRIG_TEST_OWNED_CLASSES",
        "",
        script,
        vec![(
            "ROLE keeper",
            super::mock_http::scripted(vec![
                text_reply("ready"),
                submit(
                    "f1",
                    &py(r#"
                        import json
                        d = await runtime.channels['look'].receive()
                        made = Pair(first=Left(0))
                        await d.reply(json.dumps([type(pair.first).__name__, pair.first.n,
                                                  type(pair.second).__name__,
                                                  [type(row).__name__ for row in pair.rows],
                                                  type(made.second).__name__]))
                        "#),
                ),
                text_reply("looked"),
            ]),
        )],
    )
    .await;
    let (ran, _, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!(["Right", 1, "Right", ["Left", "Right"], "Right"])],
        "{}",
        ran.stderr
    );
}

/// What the asker's child runs for `the_reply_that_ends_a_request_is_the_one_returned`: it replies
/// `first`, and once the program has accepted that reply, and before the reply ends the request,
/// replies `second` from another thread, which the program accepts too; the first then ends the
/// request, and the second's end, which finds it ended, comes after. What each reply did is in the
/// flag `said`.
const TWICE: &str = r#"
import __main__, asyncio, json, os, threading
channel = runtime.channels['ask']
if channel.pending():
    d = await channel.receive()
    real = __main__._Work.end
    go, at_end, done = threading.Event(), threading.Event(), threading.Event()
    said = {}

    def end(work, reason, **kwargs):
        if reason is not None:
            return real(work, reason, **kwargs)
        if threading.current_thread() is helper:
            at_end.set()
            done.wait(30)
            return real(work, reason, **kwargs)
        go.set()
        at_end.wait(30)
        try:
            return real(work, reason, **kwargs)
        finally:
            done.set()

    async def reply(value):
        try:
            await d.reply(value)
            said[value] = 'accepted'
        except Exception as e:
            said[value] = type(e).__name__

    helper = threading.Thread(target=lambda: go.wait(30) and asyncio.run(reply('second')))
    __main__._Work.end = end
    helper.start()
    try:
        await reply('first')
        await asyncio.to_thread(helper.join, 30)
    finally:
        __main__._Work.end = real
    with open(os.path.join('{flags}', 'saying'), 'w') as f:
        f.write(json.dumps([said.get('first'), said.get('second')]))
    os.replace(os.path.join('{flags}', 'saying'), os.path.join('{flags}', 'said'))
"#;

/// Two replies the program accepted, from two threads: the one that ended the request is the one
/// its handle gives the program, whichever was accepted last.
#[tokio::test]
async fn the_reply_that_ends_a_request_is_the_one_returned() {
    let script = format!(
        r#"
import asyncio, json, os
import outrig
{ASKER}
async def main():
    async with outrig.Session():
        async with Asker() as agent:
            answer = await agent.ask("twice")
            await until(lambda: os.path.exists("said"))
    with open("said") as f:
        said = json.load(f)
    print(json.dumps([answer, said]), flush=True)

asyncio.run(main())
"#
    );
    let run = owned_with("OUTRIG_TEST_OWNED_TWICE", "", &script, |bed| {
        vec![("ROLE asker", serving_in(bed, TWICE))]
    })
    .await;
    let (ran, _, _) = run.finish().await;
    assert_eq!(ran.exit.code(), 0, "{}", ran.stderr);
    assert_eq!(
        ran.json(),
        [json!(["first", ["accepted", "RequestSettled"]])],
        "{}",
        ran.stderr
    );
}
