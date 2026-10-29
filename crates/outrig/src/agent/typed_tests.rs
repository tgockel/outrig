//! Declared functions end to end: `@outrig.agent` in an agent's code, each
//! call a fresh child whose rounds run against the mock provider, on the
//! host's interpreter. The interpreter's own half -- the declaration's checks,
//! the handle before the child exists, the notices -- is `interpreter_tests`'.
//!
//! The mock routes each agent by a marker its system prompt carries, which for
//! a call's child is in the declaration's docstring, the child's prompt.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::agent_tests::round;
use super::children_tests::{
    Tree, child_runs, data, kind, of_agent, opening, position, primary, primary_waits, printed,
    serving, tree,
};
use super::mock_http::{scripted, system_text, text_reply};
use crate::python::testing::{Flag, py};

const REVIEW: &str = include_str!("../python/review_fixture.py");
const EXAMPLE: &str = include_str!("../python/review_fixture.json");

/// The maintainer's declaration, in a module of its own beside the fixture,
/// with the mock's marker opening its docstring.
const REVIEW_SKILL: &str = r#"import outrig
from review_fixture import ReviewResult


@outrig.agent
async def analyze_code(section: str) -> ReviewResult:
    """ROLE reviewer. Review one section of a unified diff for correctness and safety.

    The section is available to you as the variable `section`. Report each
    problem you find under a short problem-forward kebab-case id; {section}
    and {} here are braces, not placeholders.
    """
    ...
"#;

/// A module declaring `add`, whose digest a test watches change.
const WORK: &str = r#"import outrig
from dataclasses import dataclass


@dataclass
class Total:
    total: int


@outrig.agent
async def add(xs: list[int]) -> Total:
    "ROLE adder: add them up"
    ...
"#;

/// `bytes` as the digest a declaration carries.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `path` as a Python string literal.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("a UTF-8 temp path"))
}

/// Shut `tree` down and check the one call it ran: `agent.call.started` under
/// the child's subject names the declaration, `agent.call.settled` follows
/// the child's release with the outcome, and the two share the submission's
/// id. Returns the events and requests.
async fn one_call(
    tree: Tree,
    child: &str,
    module: &str,
    qualname: &str,
    outcome: &str,
) -> (
    Vec<Arc<crate::harness::event::Event>>,
    Vec<super::mock_http::RecordedRequest>,
) {
    let (_, events, requests) = tree.end().await;
    let subject = format!("agent/{child}");
    let started = kind(&events, "agent.call.started", Some(&subject));
    assert_eq!(started.len(), 1, "one start for {child}");
    let started = data(started[0]);
    assert_eq!(started["parent"], "agent/primary");
    assert_eq!(started["module"], module);
    assert_eq!(started["qualname"], qualname);
    let settled = kind(&events, "agent.call.settled", Some(&subject));
    assert_eq!(settled.len(), 1, "one end for {child}");
    let settled = data(settled[0]);
    assert_eq!(settled["outcome"], outcome);
    assert_eq!(settled["request"], started["request"]);
    let sent = data(kind(&events, "agent.request.sent", Some(&subject))[0]);
    assert_eq!(
        sent["request"], started["request"],
        "the call's id is its submission's"
    );
    let released = position(&events, "agent.released", |event| {
        data(event)["ended"]
            .as_array()
            .is_some_and(|ended| ended.iter().any(|id| id == child))
    });
    let ended = position(&events, "agent.call.settled", |event| {
        event.subject.as_ref().map(|s| s.as_str()) == Some(subject.as_str())
    });
    assert!(
        released.first() < ended.first(),
        "the call settles once its child is released: {released:?} before {ended:?}"
    );
    (events, requests)
}

/// The maintainer's `analyze_code` from `typed-agents.md`, declared in a
/// module beside `ReviewResult`: a call returns a handle at once with the
/// submission's id, and awaiting it gives a `ReviewResult` equal to the
/// example completion decoded, from a child that completed with that JSON.
/// The docstring reaches the child as written, braces and all, and the
/// argument's text is only in the submission's manifest; the start event
/// names the module, the function and the file's digest, and the settled
/// event carries the child's usage.
#[tokio::test]
async fn the_maintainers_analyze_code_returns_a_review_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("review_fixture.py"), REVIEW).expect("write");
    std::fs::write(dir.path().join("review_fixture.json"), EXAMPLE).expect("write");
    std::fs::write(dir.path().join("review_skill.py"), REVIEW_SKILL).expect("write");
    let example = literal(&dir.path().join("review_fixture.json"));
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_REVIEW",
        "",
        vec![
            (
                "ROLE reviewer",
                scripted(child_runs(
                    "c1",
                    &format!("import json\nawait runtime.complete(json.load(open({example})))"),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import json, sys
                    sys.path.insert(0, {dir})
                    import review_skill
                    from review_fixture import ReviewResult
                    section = "diff --git a/src/net/retry.py b/src/net/retry.py\n+while True: {{retry}} and {{}}\n"
                    h = review_skill.analyze_code(section)
                    print(h.status, h.done())
                    result = await h
                    expected = outrig.schema.declare(ReviewResult).decode(json.load(open({example})))
                    print(type(result) is ReviewResult, result == expected,
                          result.notes['unbounded-retry-loop'].severity, h.status, h.done())
                    "#,
                    dir = literal(dir.path()),
                )),
            ),
        ],
    )
    .await;
    assert_eq!(round(&mut tree.session, "go").await, "done");
    let (events, requests) = one_call(tree, "c1", "review_skill", "analyze_code", "accepted").await;
    assert_eq!(
        printed(&requests, "p1"),
        "queued (child idle) False\nTrue True high accepted (child released) True\n"
    );
    let child = of_agent(&requests, "ROLE reviewer");
    let system = system_text(&child[0].body);
    assert!(
        system.contains(
            "ROLE reviewer. Review one section of a unified diff for correctness and safety.\n\n\
             The section is available to you as the variable `section`. Report each\n\
             problem you find under a short problem-forward kebab-case id; {section}\n\
             and {} here are braces, not placeholders."
        ),
        "the docstring as written: {system}"
    );
    assert!(
        !system.contains("retry.py"),
        "no argument text in the instructions: {system}"
    );
    let opened = opening(child[0]);
    assert!(
        opened.contains("`section` (str): \"diff --git a/src/net/retry.py")
            && opened.contains("pre-existing-issues")
            && opened.contains("a `ReviewResult`"),
        "the manifest and the result's schema open the submission: {opened}"
    );
    let started = data(kind(&events, "agent.call.started", None)[0]);
    assert_eq!(started["digest"], sha256_hex(REVIEW_SKILL.as_bytes()));
    let settled = data(kind(&events, "agent.call.settled", None)[0]);
    assert!(
        settled["usage"]["input_tokens"]
            .as_u64()
            .is_some_and(|n| n >= 12),
        "the child's usage: {settled}"
    );
    let spawned = data(kind(&events, "agent.spawned", None)[0]);
    assert_eq!(spawned["name"], "analyze_code");
    assert_eq!(spawned["inputs"], json!([]));
}

/// The digest is of the source that declared the function: after the module's
/// file changes and is imported again, a call's start event carries another.
#[tokio::test]
async fn the_digest_follows_the_source_that_declared_the_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("work.py");
    std::fs::write(&path, WORK).expect("write");
    let changed = format!("{WORK}\n# changed since\n");
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_DIGEST",
        "",
        vec![
            (
                "ROLE adder",
                serving("await runtime.complete(Total(total=sum(xs)))"),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import importlib, sys
                    sys.path.insert(0, {dir})
                    import work
                    print((await work.add([1, 2])).total)
                    with open({path}, 'w') as f:
                        f.write({changed:?})
                    work = importlib.reload(work)
                    print((await work.add([3])).total)
                    "#,
                    dir = literal(dir.path()),
                    path = literal(&path),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "3\n3\n");
    let digests: Vec<Value> = kind(&events, "agent.call.started", None)
        .into_iter()
        .map(|event| data(event)["digest"].clone())
        .collect();
    assert_eq!(
        digests,
        [
            json!(sha256_hex(WORK.as_bytes())),
            json!(sha256_hex(changed.as_bytes()))
        ]
    );
    for event in kind(&events, "agent.call.started", None) {
        assert_eq!(data(event)["module"], "work");
    }
}

/// A `gather` of three calls makes three children, each released as its call
/// settles, and a call whose child launched a child of its own leaves neither:
/// afterward the parent holds no child and no call.
#[tokio::test]
async fn a_gather_of_three_calls_makes_three_children_each_gone_once_settled() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_GATHER",
        "",
        vec![
            (
                "ROLE leaf",
                serving("await runtime.complete(Total(total=n))"),
            ),
            (
                "ROLE nested",
                serving(
                    r#"
                    leaf = await runtime.spawn('leaf', prompt='ROLE leaf: answer n')
                    below = await leaf.submit({'n': n}, result=Total)
                    await runtime.complete(Total(total=below.total + 10))
                    "#,
                ),
            ),
            (
                "ROLE adder",
                serving("await runtime.complete(Total(total=sum(xs)))"),
            ),
            (
                "",
                primary_waits(
                    r#"
                    import __main__
                    @outrig.agent
                    async def add(xs: list[int]) -> Total:
                        "ROLE adder: add them up"
                    @outrig.agent
                    async def nested(n: int) -> Total:
                        "ROLE nested: ask a leaf for n"
                    totals = await asyncio.gather(add([1]), add([2, 3]), add([4, 5, 6]))
                    print([t.total for t in totals])
                    print((await nested(1)).total)
                    print(len(__main__._launching), sorted(c for c in __main__._kernels if c != 'primary'))
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "[1, 5, 15]\n11\n0 []\n");
    assert_eq!(
        kind(&events, "agent.spawned", None).len(),
        5,
        "three, the nested one, its leaf"
    );
    assert_eq!(kind(&events, "agent.call.started", None).len(), 4);
    assert_eq!(kind(&events, "agent.call.settled", None).len(), 4);
    let releases = kind(&events, "agent.released", Some("agent/primary"));
    assert_eq!(releases.len(), 4, "one release per call");
    let ended: Vec<usize> = releases
        .iter()
        .map(|event| data(event)["ended"].as_array().map_or(0, Vec::len))
        .collect();
    assert_eq!(
        ended.iter().sum::<usize>(),
        5,
        "the nested call's release ended its leaf too: {ended:?}"
    );
    for n in 1..=5 {
        assert_eq!(
            kind(&events, "agent.stopped", Some(&format!("agent/c{n}"))).len(),
            1,
            "c{n} is gone"
        );
    }
}

/// A call releases its child as its submission is accepted, while the round
/// that completed it is still running: whichever ends first, the call's
/// execution or the release, the round ends rather than being dropped. Five
/// calls, since the two race. Each child records its round as completed,
/// stopped on the accepted submission, with the call that completed it
/// committed as it returned, and sends no model call after the answer.
#[tokio::test]
async fn a_typed_calls_round_ends_when_its_submission_is_accepted() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_ROUND_ENDS",
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
                    @outrig.agent
                    async def add(xs: list[int]) -> Total:
                        "ROLE adder: add them up"
                    totals = await asyncio.gather(*[add([n]) for n in range(5)])
                    print([t.total for t in totals])
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "[0, 1, 2, 3, 4]\n");
    assert_eq!(
        of_agent(&requests, "ROLE adder").len(),
        5,
        "one model call per child: none to speak after completing"
    );
    for n in 1..=5 {
        let subject = format!("agent/c{n}");
        let subject = subject.as_str();
        let rounds = kind(&events, "model.round.completed", Some(subject));
        assert_eq!(rounds.len(), 1, "{subject}");
        let stopped = data(rounds[0])["stopped"].clone();
        assert!(
            stopped
                .as_str()
                .is_some_and(|why| why.contains("was accepted")),
            "{subject} stopped on {stopped}"
        );
        assert!(
            kind(&events, "model.round.dropped", Some(subject)).is_empty(),
            "{subject}'s round was not dropped"
        );
        for turn in kind(&events, "turn.committed", Some(subject)) {
            let turn = data(turn);
            assert_eq!(turn["incomplete"], false, "{subject}: {turn}");
            assert!(
                !turn["messages"].to_string().contains("had not returned"),
                "{subject}: {turn}"
            );
        }
        let settled = position(&events, "agent.request.settled", |event| {
            event.subject.as_ref().map(|s| s.as_str()) == Some(subject)
        });
        let calls = position(&events, "model.call", |event| {
            event.subject.as_ref().map(|s| s.as_str()) == Some(subject)
        });
        assert!(
            calls.iter().all(|call| *call < settled[0]),
            "{subject} made a model call after its answer: {calls:?} after {settled:?}"
        );
    }
}

/// A child that ends its submission's round and the one more round asking
/// for the result without completing fails the call `CompletionRejected`,
/// saying so; the mock gets no third call, no answer was counted, and the
/// child is released.
#[tokio::test]
async fn a_child_that_ends_two_rounds_without_completing_fails_the_call() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_ONE_SHOT",
        "",
        vec![
            (
                "ROLE stubborn",
                scripted(vec![
                    text_reply("I will not complete it"),
                    text_reply("Still not"),
                ]),
            ),
            (
                "",
                primary_waits(
                    r#"
                    @outrig.agent
                    async def stubborn(n: int) -> Total:
                        "ROLE stubborn: never completes"
                    h = stubborn(1)
                    try:
                        await h
                    except outrig.AgentError as e:
                        print(type(e).__name__, 'ended without completing' in str(e), h.status)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (events, requests) =
        one_call(tree, "c1", "outrig_session_primary", "stubborn", "rejected").await;
    assert_eq!(
        printed(&requests, "p1"),
        "CompletionRejected True failed: rejected (child released)\n"
    );
    assert_eq!(
        of_agent(&requests, "ROLE stubborn").len(),
        2,
        "no third call"
    );
    assert!(kind(&events, "agent.request.invalid", None).is_empty());
    assert_eq!(
        data(kind(&events, "agent.request.settled", None)[0])["outcome"],
        "rejected"
    );
}

/// With the session at `children-max`, a call's handle settles
/// `AgentLimitReached` at once with no spawn for it; releasing a child lets
/// the next call run.
#[tokio::test]
async fn a_call_past_children_max_fails_at_once_and_a_release_makes_room() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_LIMIT",
        "children-max = 1",
        vec![
            (
                "ROLE adder",
                serving("await runtime.complete(Total(total=sum(xs)))"),
            ),
            (
                "",
                primary_waits(
                    r#"
                    @outrig.agent
                    async def add(xs: list[int]) -> Total:
                        "ROLE adder: add them up"
                    holder = await runtime.spawn('holder', prompt='ROLE holder: wait')
                    h = add([1])
                    try:
                        await h
                    except outrig.AgentLimitReached as e:
                        print('limit', h.status, 'children-max' in str(e))
                    await runtime.release([holder])
                    print((await add([1, 2])).total)
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
        "limit failed: limit (child released) True\n3\n"
    );
    assert_eq!(
        kind(&events, "agent.spawned", None).len(),
        2,
        "the holder and the second call's"
    );
    assert_eq!(
        kind(&events, "agent.call.started", None).len(),
        1,
        "the refused call began nothing"
    );
}

/// `model=` names a configured model or alias, which the child's calls go to;
/// an unknown name fails the call with the configured names listed.
#[tokio::test]
async fn model_names_a_configured_model_and_an_unknown_one_fails_the_call() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_MODEL",
        "[models.haiku]\nprovider = \"claude\"\nidentifier = \"mock-haiku\"\n\n\
         [models.quick]\nalias = [\"haiku\"]\n",
        vec![
            (
                "ROLE quick",
                serving("await runtime.complete(Total(total=n))"),
            ),
            (
                "",
                primary_waits(
                    r#"
                    @outrig.agent(model='quick')
                    async def quick(n: int) -> Total:
                        "ROLE quick: answer with n"
                    @outrig.agent(model='nope')
                    async def nowhere(n: int) -> Total:
                        "ROLE nowhere"
                    print((await quick(7)).total)
                    h = nowhere(1)
                    try:
                        await h
                    except ValueError as e:
                        print(e)
                    print(h.status)
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
        said.starts_with("7\n")
            && said.contains("\"nope\"")
            && said.contains("Configured models: fast, haiku, quick, sonnet")
            && said.ends_with("failed: failed (child released)\n"),
        "{said}"
    );
    assert!(
        of_agent(&requests, "ROLE quick")
            .iter()
            .all(|request| request.body["model"] == "mock-haiku")
    );
    let spawned = kind(&events, "agent.spawned", None);
    assert_eq!(spawned.len(), 1);
    assert_eq!(data(spawned[0])["model"], "quick");
}

/// A waiter cancelled while it awaits a call's handle leaves the work to
/// settle, and a new await gives the result; `h.cancel()` cancels the call,
/// whose handle reads cancelled at once, and releases its child.
#[tokio::test]
async fn a_cancelled_waiter_leaves_the_call_running_and_cancel_releases_the_child() {
    let flag = Flag::new();
    let started = Flag::new();
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_CANCEL",
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
                "ROLE sleeper",
                scripted(child_runs(
                    "s1",
                    &format!(
                        "open({started}, 'w').close()\nawait asyncio.sleep(3600)",
                        started = started.py()
                    ),
                )),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    @outrig.agent
                    async def patient(xs: list[int]) -> Total:
                        "ROLE patient: wait, then add"
                    @outrig.agent
                    async def sleeper(n: int) -> Total:
                        "ROLE sleeper: never answers"
                    h = patient([1, 2])
                    await until(lambda: h.status.state == 'running')
                    waiter = asyncio.ensure_future(asyncio.wait({{h.future}}))
                    await asyncio.sleep(0.05)
                    waiter.cancel()
                    try:
                        await asyncio.wait_for(h, 0.05)
                    except asyncio.TimeoutError:
                        print('gave up', h.done(), h.status)
                    open({flag}, 'w').close()
                    print((await h).total, h.status)
                    s = sleeper(1)
                    await until(lambda: os.path.exists({started}))
                    print(s.cancel(), s.done(), s.status)
                    try:
                        await s
                    except outrig.RequestCancelled:
                        print('cancelled', s.status)
                    "#,
                    flag = flag.py(),
                    started = started.py(),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "gave up False running (child running)\n\
         3 accepted (child released)\n\
         True True failed: cancelled (child running)\n\
         cancelled failed: cancelled (child running)\n",
        "a status is fixed as the call ends"
    );
    let cancelled = kind(&events, "agent.request.cancelled", Some("agent/c2"));
    assert_eq!(cancelled.len(), 1);
    assert_eq!(data(cancelled[0])["received"], true);
    let settled: Vec<Value> = kind(&events, "agent.call.settled", None)
        .into_iter()
        .map(|event| data(event)["outcome"].clone())
        .collect();
    assert_eq!(settled, ["accepted", "cancelled"]);
    assert_eq!(
        kind(&events, "agent.released", Some("agent/primary")).len(),
        2
    );
}

/// A call nobody awaits still runs: after the round that made it has ended,
/// its child completes, the call settles, and the child is released, with
/// no await anywhere.
#[tokio::test]
async fn a_call_never_awaited_still_runs_and_releases_its_child() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_UNAWAITED",
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
                    @outrig.agent
                    async def add(xs: list[int]) -> Total:
                        "ROLE adder: add them up"
                    add([1, 2, 3])
                    print('sent')
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    tree.seen("agent.call.settled", 1).await;
    let (events, requests) =
        one_call(tree, "c1", "outrig_session_primary", "add", "accepted").await;
    assert_eq!(printed(&requests, "p1"), "sent\n");
    assert_eq!(kind(&events, "agent.spawned", None).len(), 1);
    assert_eq!(
        data(kind(&events, "agent.request.settled", None)[0])["outcome"],
        "accepted"
    );
    assert_eq!(
        kind(&events, "agent.released", Some("agent/primary")).len(),
        1
    );
    assert_eq!(kind(&events, "agent.stopped", Some("agent/c1")).len(), 1);
}

/// `validate=` runs once per answer: one that rejects the first completion is
/// called twice, the second completion is accepted, and the child's execution
/// before the first attempt ran once.
#[tokio::test]
async fn the_validator_runs_once_per_attempt() {
    let mut tree = tree(
        "OUTRIG_TEST_TYPED_VALIDATE",
        "",
        vec![
            (
                "ROLE checked",
                scripted(child_runs(
                    "c1",
                    &py(r#"
                        n = sum(xs)
                        try:
                            await runtime.complete(Total(total=n + 1))
                        except ValueError as e:
                            print('rejected:', '$: total 4 is not the sum of [1, 2]' in str(e))
                        await runtime.complete(Total(total=n))
                        "#),
                )),
            ),
            (
                "",
                primary_waits(
                    r#"
                    checks = []
                    def policy(result, xs):
                        checks.append(result.total)
                        return [] if result.total == sum(xs) else [f'total {result.total} is not the sum of {xs}']
                    @outrig.agent(validate=policy)
                    async def checked(xs: list[int]) -> Total:
                        "ROLE checked: add them up"
                    print((await checked([1, 2])).total, checks)
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (events, requests) =
        one_call(tree, "c1", "outrig_session_primary", "checked", "accepted").await;
    assert_eq!(printed(&requests, "p1"), "3 [4, 3]\n");
    let invalid = kind(&events, "agent.request.invalid", None);
    assert_eq!(invalid.len(), 1);
    assert_eq!(
        data(invalid[0])["problems"],
        json!(["$: total 4 is not the sum of [1, 2]"])
    );
    assert_eq!(kind(&events, "agent.request.replied", None).len(), 2);
    assert_eq!(
        kind(&events, "exec.submitted", Some("agent/c1")).len(),
        1,
        "the child's one execution ran once"
    );
    let outputs: Vec<String> = kind(&events, "exec.completed", Some("agent/c1"))
        .into_iter()
        .map(|event| {
            data(event)["output"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(outputs, ["rejected: True\n"]);
    assert!(
        primary(&requests).len() >= 2,
        "the primary's call and the result after"
    );
}
