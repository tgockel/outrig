//! Roots the session's owner constructs, end to end: a Rust owner of a session with no primary
//! agent names a class of its application code, the arguments to construct it with and a
//! model; the class is constructed in a kernel of its own, where the instance is `self`, and
//! the owner holds a handle. Each root's rounds run against the routed mock provider, on the
//! host's interpreter, and are opened by its own runner, never by the owner.
//!
//! The mock routes each root by a marker its class's docstring carries. A root's first round is
//! its orienting round, which the mock answers with text unless a test says otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use super::children_tests::{
    Hold, Tree, data, exec_outputs, held, kind, of_agent, opening, primary_runs, printed, serving,
    tree_built,
};
use super::instance_tests::{launching, ready_then};
use super::mock_http::{RecordedRequest, Responder, scripted, submit, system_text, text_reply};
use super::{orientation, runner};
use crate::harness::event::RequestEnd;
use crate::harness::services_tests::Fixture;
use crate::harness::skills::DirectorySource;
use crate::harness::{
    ApplicationSpec, Argument, ConstructSpec, Facility, RootError, RootHandle, Service,
    SessionBuilder, SessionError, Verdict,
};
use crate::python::testing::{pip_env, py, wheel_links, within};

/// The application's `review.agents`: the classes the tests construct.
const AGENTS: &str = r#"
import asyncio
import dataclasses
import time
import outrig


@dataclasses.dataclass(frozen=True)
class Task:
    title: str
    priority: int


@dataclasses.dataclass(frozen=True)
class Verdict:
    path: str
    ok: bool


class Reviewer(outrig.Agent):
    """ROLE reviewer. You review the files your facilities reach."""

    def __init__(self, task, facilities=None, *, model=None):
        super().__init__(facilities=facilities, model=model)
        self._task = task
        print("constructing", type(self).__name__)

    def describe(self):
        return "a reviewer"

    @outrig.agent
    async def review(self, path: str) -> Verdict:
        """Review the file at `path`."""
        ...


class Holder(outrig.Agent):
    """ROLE holder. You hold a task, and no request reaches you."""

    def __init__(self, task):
        super().__init__()
        self.task = task

    def summary(self):
        return f"holding {self.task.title} at {self.task.priority}"


class Briefed(outrig.Agent):
    """ROLE briefed. You review the task you hold."""

    def __init__(self, task):
        super().__init__()
        self._task = task

    def __outrig_instructions__(self):
        return [outrig.Section(
            "Task", f"Review {self._task.title} at priority {self._task.priority}.")]


class Advised(Reviewer):
    """ROLE advised. You review the files your task names."""

    def __outrig_instructions__(self):
        return [outrig.Section("Task", f"Review {self._task.title}.")]


class Raising(outrig.Agent):
    """ROLE raising. You are never constructed."""

    def __init__(self):
        super().__init__()
        raise ValueError("no task today")

    @outrig.agent
    async def review(self, path: str) -> str:
        """Review it."""
        ...


class Slow(outrig.Agent):
    """ROLE slow. Your constructor waits on the task."""

    def __init__(self, svc, secs=30):
        super().__init__()
        svc.sleep_honoring(secs=secs)

    @outrig.agent
    async def review(self, path: str) -> str:
        """Review it."""
        ...


class Big(outrig.Agent):
    """ROLE big. You answer with large numbers."""

    def __init__(self):
        super().__init__()

    @outrig.agent
    async def count(self, n: int) -> int:
        """Count past it."""
        ...


class Tally(outrig.Agent):
    """ROLE tally. You answer with tallies."""

    @outrig.agent
    async def tally(self, n: int) -> dict[str, list[int]]:
        """Tally it."""
        ...


class Nested(outrig.Agent):
    """ROLE nested. A constructor that failed made you."""

    @outrig.agent
    async def ping(self, x: str) -> str:
        """Answer it."""
        ...


class Nesting(outrig.Agent):
    """ROLE nesting. You are never constructed."""

    def __init__(self):
        super().__init__()
        self.nested = Nested()
        # Once the nested agent's launch has asked for its child -- its first step, which runs
        # after this construction answers -- hold this kernel's loop past the host's wait for
        # the kernel to end, so the child's kernel opens before this one has ended.
        loop = asyncio.get_running_loop()
        loop.call_soon(loop.call_soon, time.sleep, 4)
        raise ValueError("after launching another")

    @outrig.agent
    async def review(self, path: str) -> str:
        """Review it."""
        ...
"#;

/// The application holding [`AGENTS`] as `review.agents`, imported at start, in a directory
/// kept while the test runs.
fn application() -> (tempfile::TempDir, ApplicationSpec) {
    let dir = tempfile::tempdir().expect("an application directory");
    let package = dir.path().join("review");
    std::fs::create_dir_all(&package).expect("the package's directory");
    std::fs::write(package.join("__init__.py"), "").expect("write __init__.py");
    std::fs::write(package.join("agents.py"), AGENTS).expect("write agents.py");
    let spec = ApplicationSpec::directory(dir.path()).imports(["review.agents"]);
    (dir, spec)
}

/// A session with no primary agent, holding the application, `top` among its configuration's
/// top-level keys, its model calls answered by `routes`, and built further by `build`.
async fn owned(
    var: &str,
    top: &str,
    routes: Vec<(&'static str, Responder)>,
    build: impl FnOnce(SessionBuilder) -> SessionBuilder,
) -> (Tree, tempfile::TempDir) {
    let (dir, spec) = application();
    let tree = tree_built(var, top, routes, None, move |builder| {
        build(builder.no_primary().application(spec))
    })
    .await;
    (tree, dir)
}

/// A reply the tests decode with serde, as an embedder would.
#[derive(Debug, Deserialize, PartialEq)]
struct Reviewed {
    path: String,
    ok: bool,
}

/// What a root runs to answer one request on `review` well.
const REVIEWS: &str =
    "d = await runtime.channels['review'].receive()\nawait d.reply(Verdict(path=d.body, ok=True))";

/// A root's rounds: its orienting round ends at once, and each later one runs `source`.
fn reviewing(source: &str) -> Responder {
    ready_then(serving(source))
}

/// A root's rounds: its orienting round ends at once, and every later model call is never
/// answered, so a request it was given stays open.
fn stalling() -> Responder {
    ready_then(Arc::new(|_: &RecordedRequest| {
        Box::pin(std::future::pending())
    }))
}

/// A `Reviewer`, its task a data argument.
fn reviewer() -> ConstructSpec {
    ConstructSpec::new("review.agents:Reviewer", "fast")
        .arg("task", Argument::data(json!({"title": "t", "priority": 1})))
}

/// The root's agent id.
fn agent_of(root: &RootHandle) -> String {
    root.agent().expect("a launched root").to_string()
}

/// What a root's launch failed with.
async fn not_ready(root: &RootHandle) -> RootError {
    within(root.ready())
        .await
        .expect_err("the root is not ready")
}

/// With one declared channel, a request's reply crosses as JSON the owner decodes with serde,
/// and the root's rounds are opened by its runner: on its orienting round, and on the request.
/// The owner never opens one: `Session::round` is `NoPrimary` throughout. What the constructor
/// printed reaches the root with its first result, as its construction's.
#[tokio::test]
async fn a_request_on_a_roots_channel_is_answered_as_json() {
    let (mut tree, _app) = owned(
        "OUTRIG_TEST_ROOT_REQUEST_KEY",
        "",
        vec![("ROLE reviewer", reviewing(REVIEWS))],
        |builder| builder,
    )
    .await;
    assert!(matches!(
        tree.session.round().await,
        Err(SessionError::NoPrimary)
    ));
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("a session with no primary agent constructs");
    within(root.ready()).await.expect("ready");
    assert_eq!(
        tree.session.root(root.id().expect("an id")).map(|r| r.id()),
        Some(root.id())
    );
    let channels: Vec<&str> = root.channels().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(channels, ["review"]);
    assert!(
        root.channels()[0].reply.contains("\"ok\""),
        "{:?}",
        root.channels()
    );
    let request = root.request("review", json!("src/main.py")).expect("sent");
    let reply = within(request.outcome()).await.expect("answered");
    let reviewed: Reviewed = serde_json::from_str(&reply).expect("decodes");
    assert_eq!(
        reviewed,
        Reviewed {
            path: "src/main.py".into(),
            ok: true,
        }
    );
    assert!(matches!(
        tree.session.round().await,
        Err(SessionError::NoPrimary)
    ));
    let child = agent_of(&root);
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let started = kind(&events, "agent.instance.started", None);
    assert_eq!(started.len(), 1);
    let started = data(started[0]);
    assert_eq!(started["parent"], "owner");
    assert_eq!(started["module"], "review.agents");
    assert_eq!(started["qualname"], "Reviewer");
    assert_eq!(started["self"], true);
    assert_eq!(started["channels"][0]["name"], "review");
    // Its orienting round, then the request's.
    let made = of_agent(&requests, "ROLE reviewer");
    assert!(made.len() >= 2, "{made:?}");
    assert!(launching(made[0]));
    assert!(made[1..].iter().all(|request| !launching(request)));
    let completed = kind(&events, "exec.completed", Some(&format!("agent/{child}")));
    assert_eq!(completed.len(), 1, "one execution, the request's");
    assert_eq!(
        data(completed[0])["background"],
        json!([{"id": null, "output": "constructing Reviewer\n", "dropped": 0}])
    );
}

/// A root's instructions say first that the program that owns the session constructed it, then its
/// docstring and the sections its hook returned -- which its construct answer carried -- `self` as
/// that program's instance, what it does with no request channel or with one, and, ahead of what
/// every agent is told last, the session's skills. Its first round opens on the lead alone, with
/// the task's sentence when its class contributed a section, and announces nothing: no request
/// waits, and nothing is invented. A request body is never in the instructions. The start event
/// lists each section.
#[tokio::test]
async fn a_roots_instructions_carry_the_programs_line_its_sections_and_the_skills() {
    let skills = tempfile::tempdir().expect("a skills directory");
    std::fs::create_dir(skills.path().join("review")).expect("a skill");
    std::fs::write(
        skills.path().join("review/SKILL.md"),
        "---\nname: review\ndescription: Reviews a change.\n---\nSteps.\n",
    )
    .expect("its instructions");
    let source = skills.path().to_path_buf();
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_CONTRIBUTIONS_KEY",
        "",
        vec![
            ("ROLE briefed", stalling()),
            ("ROLE advised", reviewing(REVIEWS)),
            ("ROLE reviewer", reviewing(REVIEWS)),
        ],
        move |builder| builder.skill_source("test", DirectorySource::new(source)),
    )
    .await;
    let mut roots = Vec::new();
    for class in ["Briefed", "Advised", "Reviewer"] {
        let spec = ConstructSpec::new(format!("review.agents:{class}"), "fast").arg(
            "task",
            Argument::data_as(
                json!({"title": "the parser", "priority": 2}),
                "review.agents:Task",
            ),
        );
        let root = within(tree.session.construct(spec))
            .await
            .expect("constructed");
        within(root.ready()).await.expect("ready");
        roots.push(root);
    }
    // A request's body is a part of no root's instructions.
    let request = roots[1]
        .request("review", json!("src/secret-path.py"))
        .expect("sent");
    within(request.outcome()).await.expect("answered");
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");

    let told = |role: &str| {
        let made = of_agent(&requests, role);
        assert!(launching(made[0]), "{}", opening(made[0]));
        (system_text(&made[0].body), opening(made[0]))
    };
    let before = |text: &str, needles: &[&str]| {
        let at: Vec<usize> = needles
            .iter()
            .map(|needle| {
                text.find(needle)
                    .unwrap_or_else(|| panic!("{needle:?} in {text}"))
            })
            .collect();
        assert!(
            at.windows(2).all(|pair| pair[0] < pair[1]),
            "{at:?} in {text}"
        );
    };
    let line = "You are a root agent: the program that owns this session constructed you; no user \
                reaches you, and that program is who you answer.";
    let section = "## Task\n\nReview the parser at priority 2.";

    // No request channel: its one round, with the task's sentence in its lead.
    let (briefed, lead) = told("ROLE briefed");
    let recorded = |role: &str| {
        let child = agent_of(&roots[usize::from(role != "briefed")]);
        let events = kind(
            &events,
            "model.instructions",
            Some(&format!("agent/{child}")),
        );
        assert_eq!(events.len(), 1, "recorded once");
        data(events[0])["preamble"].clone()
    };
    assert_eq!(
        recorded("briefed"),
        json!(briefed),
        "the event holds what the model was sent"
    );
    before(
        &briefed,
        &[
            line,
            "ROLE briefed. You review the task you hold.",
            section,
            "`self` is the `Briefed` instance that program constructed in this interpreter for you",
            "Your class declares no request channel, so no request arrives",
            "These skills come with the session",
            "- `review`: Reviews a change.",
            "Your whole conversation is in",
        ],
    );
    assert!(!briefed.contains("another agent's code"), "{briefed}");
    assert!(!briefed.contains("`/name`"), "{briefed}");
    assert_eq!(
        lead,
        orientation::opening(&orientation::orienting(true, ""), 0),
        "nothing waits and nothing is invented"
    );

    // A channel and a section: told how to answer, the skills, and the same lead.
    let (advised, lead) = told("ROLE advised");
    before(
        &advised,
        &[
            line,
            "## Task\n\nReview the parser.",
            "`self` is the `Advised` instance that program constructed",
            "You answer requests, which arrive on these channels:",
            "These skills come with the session",
            "Your whole conversation is in",
        ],
    );
    assert!(!advised.contains("secret-path"), "{advised}");
    assert_eq!(
        lead,
        orientation::opening(&orientation::orienting(true, ""), 0)
    );

    // No section: no heading, and the lead without the task's sentence.
    let (reviewer, lead) = told("ROLE reviewer");
    assert!(
        reviewer.contains(line) && !reviewer.contains("## "),
        "{reviewer}"
    );
    assert_eq!(
        lead,
        orientation::opening(&orientation::orienting(false, ""), 0)
    );

    let started = kind(&events, "agent.instance.started", None);
    let by = |qualname: &str| {
        data(
            started
                .iter()
                .find(|event| data(event)["qualname"] == qualname)
                .unwrap_or_else(|| panic!("{qualname} started")),
        )
    };
    assert_eq!(
        by("Briefed")["contributions"],
        json!([{"title": "Task", "length": "Review the parser at priority 2.".len()}])
    );
    assert_eq!(by("Reviewer")["contributions"], json!([]));
}

/// A request already waiting as a root's first round opens is announced in its opening line, by
/// channel and count, after the lead, and its body is in no instructions: the round reads it in
/// code, and the next opens on nothing more. The runner is held until the request is made, since
/// the registry starts it before it answers the owner.
#[tokio::test]
async fn a_request_already_waiting_is_announced_after_the_roots_lead() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_WAITING_KEY",
        "",
        vec![("ROLE reviewer", reviewing(REVIEWS))],
        |builder| builder,
    )
    .await;
    let hold = Arc::new(tokio::sync::Notify::new());
    runner::HOLD.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&hold)));
    let root = within(tree.session.construct(reviewer())).await;
    // Whatever happens, no later runner on this thread waits for a hold nobody lifts.
    runner::HOLD.with(|slot| slot.borrow_mut().take());
    let root = root.expect("constructed");
    let request = root
        .request("review", json!("src/secret-path.py"))
        .expect("sent");
    // It has an id once the registry has recorded it, which the runner's lead then counts.
    within(async {
        while request.id().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    hold.notify_one();
    within(root.ready()).await.expect("ready");
    within(request.outcome()).await.expect("answered");
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let made = of_agent(&requests, "ROLE reviewer");
    assert!(launching(made[0]), "{}", opening(made[0]));
    let waiting = "1 request is waiting on runtime.channels[\"review\"]";
    assert_eq!(
        opening(made[0]),
        orientation::opening(&orientation::orienting(false, waiting), 0)
    );
    let told = system_text(&made[0].body);
    assert!(!told.contains("secret-path"), "{told}");
    // What the model was sent as its instructions is what the event records, once.
    let recorded = kind(
        &events,
        "model.instructions",
        Some(&format!("agent/{}", agent_of(&root))),
    );
    assert_eq!(recorded.len(), 1);
    assert_eq!(data(recorded[0])["preamble"], told);
}

/// The fixture library bound as `files`, where no approval is asked for.
const FILES: &str = r#"
[bindings.files]
description = "The fixture library."
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make"
"#;

/// Where the fixture library is installed for these tests, kept between runs.
fn packages() -> PathBuf {
    std::env::temp_dir().join(format!(
        "outrig-test-root-packages-{}",
        nix::unistd::getuid()
    ))
}

/// OR-A2-1: a root constructed with its task a service bound by name, a dict holding a binding,
/// `task` bound beside `self`, and an alias for its model. Its round's code calls the task as
/// itself -- the service's call attributed to the root -- and `task is self._task`; it sees the
/// binding it was given and no other.
#[tokio::test]
async fn a_root_is_given_a_service_a_binding_and_an_alias() {
    let approvals = tempfile::tempdir().expect("an approvals directory");
    let store = approvals.path().to_path_buf();
    let wheels =
        std::thread::scope(|scope| scope.spawn(wheel_links).join()).expect("the wheels build");
    let task = Fixture::task();
    let served = Arc::clone(&task) as Arc<dyn Service>;
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_FACILITIES_KEY",
        FILES,
        vec![(
            "ROLE reviewer",
            reviewing(&format!(
                "import outrig\nself._task.update_progress(note='x')\n\
                 print(task is self._task, self.describe(), list(outrig.bindings))\n{REVIEWS}"
            )),
        )],
        move |builder| {
            builder
                .serve("task", served)
                .binding_places(&packages(), pip_env(wheels), &store)
        },
    )
    .await;
    let spec = ConstructSpec::new("review.agents:Reviewer", "fast")
        .arg("task", Argument::Facility(Facility::service("task")))
        .arg(
            "facilities",
            Argument::Facilities([("files".to_string(), Facility::binding("files"))].into()),
        )
        .alias("task");
    let root = within(tree.session.construct(spec))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    let request = root.request("review", json!("a.py")).expect("sent");
    within(request.outcome()).await.expect("answered");
    let child = agent_of(&root);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &format!("agent/{child}")),
        ["True a reviewer ['files']\n"]
    );
    assert_eq!(
        task.seen(),
        [(
            "update_progress".to_string(),
            json!({"note": "x"}),
            child.clone()
        )]
    );
    let started = data(kind(&events, "agent.instance.started", None)[0]);
    assert_eq!(started["parent"], "owner");
    assert_eq!(started["self"], true);
    assert_eq!(started["facilities"][0]["name"], "files");
    assert_eq!(started["facilities"][0]["kind"], "binding");
    let spawned = data(kind(&events, "agent.spawned", None)[0]);
    assert_eq!(spawned["model"], "fast");
}

/// OR-A1-1: a class with a docstring, an `__init__` and ordinary methods, and no request
/// method, is constructed as a root, its data argument decoded into the application's own
/// dataclass, and reaches `ready()`; it has no channel, and a request of it is refused.
#[tokio::test]
async fn a_class_with_no_channel_is_constructed_as_a_root() {
    // No request reaches it, so its orienting round is its one round.
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_CHANNELLESS_KEY",
        "",
        vec![(
            "ROLE holder",
            serving("print(self.summary(), type(self.task).__name__)"),
        )],
        |builder| builder,
    )
    .await;
    let spec = ConstructSpec::new("review.agents:Holder", "sonnet").arg(
        "task",
        Argument::data_as(json!({"title": "t", "priority": 2}), "review.agents:Task"),
    );
    let root = within(tree.session.construct(spec))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    assert!(root.channels().is_empty());
    let refused = root.request("review", json!("x")).expect_err("no channel");
    assert!(refused.to_string().contains("none"), "{refused}");
    let child = agent_of(&root);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &format!("agent/{child}")),
        ["holding t at 2 Task\n"]
    );
    let started = data(kind(&events, "agent.instance.started", None)[0]);
    assert_eq!(started["channels"], json!([]));
}

/// OR-A1-1's other half: the same class constructed in an agent's kernel, as that agent's child,
/// is refused as it always was; and the session with a primary agent constructs no root.
#[tokio::test]
async fn a_class_with_no_channel_is_refused_as_a_kernels_child() {
    let (dir, spec) = application();
    let tree = tree_built(
        "OUTRIG_TEST_ROOT_CHILD_REFUSED_KEY",
        "",
        vec![(
            "",
            primary_runs(
                "import review.agents\n\
                 try:\n    review.agents.Holder(task=None)\n\
                 except TypeError as e:\n    print(e)",
            ),
        )],
        None,
        move |builder| builder.application(spec),
    )
    .await;
    let mut tree = tree;
    assert_eq!(
        super::agent_tests::round(&mut tree.session, "go").await,
        "done"
    );
    let refused = tree.session.construct(reviewer()).await;
    assert!(matches!(refused, Err(SessionError::Primary)), "{refused:?}");
    let (_, _, requests) = tree.end().await;
    drop(dir);
    assert_eq!(
        printed(&requests, "p1"),
        "review.agents.Holder declares no request channel -- no method marked @outrig.agent -- \
         so an instance of it would answer nothing\n"
    );
}

/// OR-A2-2: a constructor that raises fails `ready()` with the exception rendered and its
/// traceback, records `agent.construct.failed` and no `agent.instance.started`, calls no
/// model, and frees its place: with `children-max = 1`, a second root is constructed. The
/// session's shutdown is clean.
#[tokio::test]
async fn a_constructor_that_raises_spends_no_model_round() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_RAISES_KEY",
        "children-max = 1",
        vec![
            ("ROLE raising", ready_then(serving("print('never')"))),
            ("ROLE reviewer", reviewing(REVIEWS)),
        ],
        |builder| builder,
    )
    .await;
    let raising = ConstructSpec::new("review.agents:Raising", "sonnet");
    let root = within(tree.session.construct(raising))
        .await
        .expect("asked");
    assert_eq!(root.agent(), None);
    assert_eq!(root.id(), None);
    let RootError::Construct {
        code,
        message,
        traceback,
    } = not_ready(&root).await
    else {
        panic!("not a construction's failure");
    };
    assert_eq!(code, "raised");
    assert_eq!(message, "ValueError: no task today");
    assert!(
        traceback.contains("raise ValueError(\"no task today\")"),
        "{traceback}"
    );
    let second = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    within(second.ready()).await.expect("the place was freed");
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert!(of_agent(&requests, "ROLE raising").is_empty());
    let failed = kind(&events, "agent.construct.failed", None);
    assert_eq!(failed.len(), 1);
    let failed = data(failed[0]);
    assert_eq!(failed["class"], "review.agents:Raising");
    assert_eq!(failed["module"], "review.agents");
    assert_eq!(failed["qualname"], "Raising");
    assert_eq!(failed["code"], "raised");
    assert!(
        failed["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("ValueError: no task today\nTraceback")),
        "{failed}"
    );
    let started: Vec<_> = kind(&events, "agent.instance.started", None)
        .into_iter()
        .map(|event| data(event)["qualname"].clone())
        .collect();
    assert_eq!(started, ["Reviewer"]);
}

/// OR-A2-2's last part: a release while the root is launching -- its orienting round held at
/// the provider -- releases it, and `ready()` then fails `Released`.
#[tokio::test]
async fn a_release_while_the_root_launches_releases_it() {
    let hold = Arc::new(Hold::default());
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_RELEASED_LAUNCHING_KEY",
        "",
        vec![(
            "ROLE reviewer",
            held(vec![text_reply("ready")], Arc::clone(&hold)),
        )],
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(hold.arrived.notified()).await;
    within(root.release()).await;
    assert!(
        matches!(not_ready(&root).await, RootError::Released(_)),
        "released"
    );
    assert!(tree.session.root(root.id().expect("an id")).is_none());
    within(root.release()).await;
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let released = kind(&events, "agent.instance.released", None);
    assert_eq!(released.len(), 1);
    assert_eq!(data(released[0])["how"], "explicit");
}

/// OR-A2-5: a reply of the wrong shape is refused in the root's kernel, recorded as
/// `agent.request.invalid`, and repaired by the next execution, the earlier one not run again; a
/// request whose third reply is still wrong ends `Rejected`, with that reply and its problems.
#[tokio::test]
async fn a_wrong_reply_is_refused_in_the_kernel_and_repaired() {
    let script = scripted(vec![
        text_reply("ready"),
        submit(
            "a1",
            &py("d = await runtime.channels['review'].receive()\nawait d.reply({'path': d.body})"),
        ),
        submit("a2", "await d.reply(Verdict(path=d.body, ok=False))"),
        text_reply("ended"),
        submit(
            "b1",
            &py("d = await runtime.channels['review'].receive()\nawait d.reply({'path': 1})"),
        ),
        submit("b2", "await d.reply({})"),
        submit("b3", "await d.reply({'path': 'p', 'ok': 'no'})"),
        text_reply("ended"),
    ]);
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_REPAIRED_KEY",
        "",
        vec![("ROLE reviewer", script)],
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    let first = root.request("review", json!("a.py")).expect("sent");
    let reply = within(first.outcome()).await.expect("repaired");
    assert_eq!(reply, r#"{"path": "a.py", "ok": false}"#);
    let second = root.request("review", json!("b.py")).expect("sent");
    let RootError::Rejected { last, problems, .. } =
        within(second.outcome()).await.expect_err("rejected")
    else {
        panic!("not rejected");
    };
    assert_eq!(last, r#"{"path": "p", "ok": "no"}"#);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, "$.ok");
    let (first_id, second_id) = (first.id().expect("an id"), second.id().expect("an id"));
    let child = agent_of(&root);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let invalid = |id| {
        kind(&events, "agent.request.invalid", None)
            .into_iter()
            .filter(|event| data(event)["request"] == json!(id))
            .count()
    };
    assert_eq!(invalid(first_id), 1);
    assert_eq!(invalid(second_id), 3);
    let submitted: Vec<String> = kind(&events, "exec.submitted", Some(&format!("agent/{child}")))
        .into_iter()
        .map(|event| {
            data(event)["source"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        submitted
            .iter()
            .filter(|source| source.contains("{'path': d.body}"))
            .count(),
        1,
        "{submitted:?}"
    );
}

/// What the root's kernel refuses to construct fails `ready()` naming it, calls no model, and
/// records `agent.construct.failed`: a class or a module that is not there, a facility the
/// session does not give, data that does not decode into its class -- naming each problem's
/// path -- and an alias naming no argument.
#[tokio::test]
async fn what_cannot_be_constructed_is_refused_naming_it() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_REFUSALS_KEY",
        "",
        vec![("ROLE", reviewing(REVIEWS))],
        |builder| builder,
    )
    .await;
    let cases: [(ConstructSpec, &str, &[&str]); 6] = [
        (
            ConstructSpec::new("review.agents:Missing", "sonnet"),
            "class-missing",
            &["review.agents:Missing: module 'review.agents' has no 'Missing'"],
        ),
        (
            ConstructSpec::new("nowhere:Thing", "sonnet"),
            "module-missing",
            &["module 'nowhere' did not import"],
        ),
        (
            ConstructSpec::new("review.agents:Task", "sonnet"),
            "not-an-agent-class",
            &["review.agents:Task is not a subclass of outrig.Agent"],
        ),
        (
            ConstructSpec::new("review.agents:Reviewer", "sonnet")
                .arg("task", Argument::Facility(Facility::service("absent"))),
            "invalid-argument",
            &["facility 'task' names service 'absent', which the session does not serve"],
        ),
        (
            ConstructSpec::new("review.agents:Holder", "sonnet").arg(
                "task",
                Argument::data_as(json!({"title": 1}), "review.agents:Task"),
            ),
            "invalid-argument",
            &[
                "argument 'task' does not decode into review.agents:Task",
                "$.title",
                "$.priority",
            ],
        ),
        (
            reviewer().alias("missing"),
            "invalid-argument",
            &["alias 'missing' names no argument"],
        ),
    ];
    for (spec, code, says) in cases {
        let class = spec.class().to_string();
        let root = within(tree.session.construct(spec)).await.expect("asked");
        let RootError::Construct {
            code: got, message, ..
        } = not_ready(&root).await
        else {
            panic!("{class} not refused as a construction");
        };
        assert_eq!(got, code, "{class}: {message}");
        for said in says {
            assert!(message.contains(said), "{class}: {said:?} in {message}");
        }
    }
    let unknown = within(
        tree.session
            .construct(ConstructSpec::new("review.agents:Reviewer", "nope")),
    )
    .await
    .expect("asked");
    let RootError::Model(said) = not_ready(&unknown).await else {
        panic!("not refused for its model");
    };
    assert!(said.contains("sonnet"), "{said}");
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert!(requests.is_empty(), "no model was called: {requests:?}");
    assert_eq!(kind(&events, "agent.construct.failed", None).len(), 6);
    assert!(kind(&events, "agent.instance.started", None).is_empty());
}

/// Release and exit: a release ends the root and settles its open request `Released`, and the
/// root takes no request after; one alive at the session's exit is released by the close, and
/// the report lists its open request, which ends `Closed`; a construction after admission
/// closed is refused as closed.
#[tokio::test]
async fn a_root_ends_by_its_release_or_the_sessions_close() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_RELEASE_KEY",
        "",
        vec![("ROLE reviewer", stalling())],
        |builder| builder,
    )
    .await;
    let released = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    let kept = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(released.ready()).await.expect("ready");
    within(kept.ready()).await.expect("ready");
    let open = released.request("review", json!("a.py")).expect("sent");
    let left = kept.request("review", json!("b.py")).expect("sent");
    tree.seen("round.started", 4).await;
    within(released.release()).await;
    assert!(
        matches!(within(open.outcome()).await, Err(RootError::Released(_))),
        "released"
    );
    assert!(matches!(
        released.request("review", json!("c.py")),
        Err(RootError::Released(_))
    ));
    tree.session.close_admission();
    let late = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    assert!(matches!(not_ready(&late).await, RootError::Closed(_)));
    let left_id = left.id();
    let (report, events, _) = tree.end().await;
    assert!(
        matches!(within(left.outcome()).await, Err(RootError::Closed(_))),
        "closed"
    );
    assert_eq!(
        report
            .requests
            .iter()
            .map(|request| (Some(request.request), request.outcome))
            .collect::<Vec<_>>(),
        [(left_id, RequestEnd::Closed)]
    );
    let how: Vec<_> = kind(&events, "agent.instance.released", None)
        .into_iter()
        .map(|event| data(event)["how"].clone())
        .collect();
    assert_eq!(how, ["explicit", "closing"]);
}

/// A request the owner cancels ends `Cancelled`, and the cancel says it ended it; a second
/// cancel, and one of a request that has ended, say it did not.
#[tokio::test]
async fn a_request_the_owner_cancels_ends_cancelled() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_CANCEL_KEY",
        "",
        vec![("ROLE reviewer", stalling())],
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    let request = root.request("review", json!("a.py")).expect("sent");
    tree.seen("agent.request.sent", 1).await;
    assert!(
        within(request.cancel()).await,
        "the owner's cancel ended it"
    );
    assert!(matches!(
        within(request.outcome()).await,
        Err(RootError::Cancelled(_))
    ));
    assert!(!within(request.cancel()).await, "it had ended");
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let settled = kind(&events, "agent.request.settled", None);
    assert_eq!(data(settled[0])["outcome"], "cancelled");
}

/// A construction that fails after its constructor launched an agent of its own releases that
/// agent with it: its kernel ended and its place freed. The constructor holds its kernel's loop
/// once the agent's launch has asked for its child, so the child's kernel opens while the
/// failed root's is still ending; with `children-max = 2`, two roots fit afterward only if the
/// child's place was given back.
#[tokio::test]
async fn a_failed_construction_releases_what_it_launched() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_NESTED_KEY",
        "children-max = 2",
        vec![
            ("ROLE nested", ready_then(serving("print('never')"))),
            ("ROLE reviewer", reviewing(REVIEWS)),
        ],
        |builder| builder,
    )
    .await;
    let failed = within(
        tree.session
            .construct(ConstructSpec::new("review.agents:Nesting", "sonnet")),
    )
    .await
    .expect("asked");
    let RootError::Construct { code, message, .. } = not_ready(&failed).await else {
        panic!("not a construction's failure");
    };
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("raised", "ValueError: after launching another")
    );
    let first = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    let second = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    within(first.ready()).await.expect("the first fits");
    within(second.ready())
        .await
        .expect("the second fits: the child's place was freed");
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let nested = kind(&events, "agent.instance.started", None)
        .into_iter()
        .find(|event| data(event)["qualname"] == "Nested")
        .map(|event| super::instance_tests::subject(event).to_string())
        .expect("the nested agent launched");
    let released: Vec<_> = kind(&events, "agent.released", None)
        .into_iter()
        .map(|event| data(event)["ended"].clone())
        .collect();
    let child = nested.strip_prefix("agent/").expect("an agent's subject");
    assert_eq!(released, [json!([child])]);
}

/// A construction the owner stops waiting for, its constructor still running, launches its root
/// all the same, which nothing could then reach: it is released, its place given back.
#[tokio::test]
async fn a_construction_nobody_waits_for_is_released() {
    let fixture = Fixture::new();
    let served = Arc::clone(&fixture) as Arc<dyn Service>;
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_DROPPED_KEY",
        "children-max = 1",
        vec![
            ("ROLE slow", reviewing(REVIEWS)),
            ("ROLE reviewer", reviewing(REVIEWS)),
        ],
        move |builder| builder.serve("svc", served),
    )
    .await;
    let spec = ConstructSpec::new("review.agents:Slow", "sonnet")
        .arg("svc", Argument::Facility(Facility::service("svc")))
        .arg("secs", Argument::data(json!(1)));
    let wait = std::time::Duration::from_millis(300);
    let dropped = tokio::time::timeout(wait, tree.session.construct(spec)).await;
    assert!(dropped.is_err(), "the construction outlasted the wait");
    tree.seen("agent.instance.released", 1).await;
    let next = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    within(next.ready())
        .await
        .expect("its place was given back");
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let released = kind(&events, "agent.instance.released", None);
    assert_eq!(data(released[0])["how"], "explicit");
}

/// A construction whose answer has arrived, unread, when the owner stops waiting launched a root
/// nothing holds: it is released, its place given back. The construction is polled once, with a
/// waker that says when the answer lands, and dropped then, never polled again.
#[tokio::test]
async fn a_construction_answered_but_unread_is_released() {
    #[derive(Default)]
    struct Woken(tokio::sync::Notify);
    impl std::task::Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_UNREAD_KEY",
        "children-max = 1",
        vec![("ROLE reviewer", reviewing(REVIEWS))],
        |builder| builder,
    )
    .await;
    let woken = Arc::new(Woken::default());
    let waker = std::task::Waker::from(Arc::clone(&woken));
    let mut construct = Box::pin(tree.session.construct(reviewer()));
    let polled = construct
        .as_mut()
        .poll(&mut std::task::Context::from_waker(&waker));
    assert!(polled.is_pending(), "the construction waits for its answer");
    within(woken.0.notified()).await;
    drop(construct);
    tree.seen("agent.instance.released", 1).await;
    let next = within(tree.session.construct(reviewer()))
        .await
        .expect("asked");
    within(next.ready())
        .await
        .expect("its place was given back");
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let released = kind(&events, "agent.instance.released", None);
    assert_eq!(data(released[0])["how"], "explicit");
}

/// A release the owner stops waiting for, while the root is launching, runs to its end: the
/// root's readiness settles `Released` and the session holds it no more; a second release
/// waits for the first.
#[tokio::test]
async fn a_release_nobody_waits_for_finishes() {
    let hold = Arc::new(Hold::default());
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_RELEASE_DROPPED_KEY",
        "",
        vec![(
            "ROLE reviewer",
            held(vec![text_reply("ready")], Arc::clone(&hold)),
        )],
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(hold.arrived.notified()).await;
    // The held round keeps the release waiting well past this.
    let wait = std::time::Duration::from_millis(300);
    assert!(tokio::time::timeout(wait, root.release()).await.is_err());
    assert!(
        matches!(not_ready(&root).await, RootError::Released(_)),
        "released"
    );
    within(root.release()).await;
    assert!(tree.session.root(root.id().expect("an id")).is_none());
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let released = kind(&events, "agent.instance.released", None);
    assert_eq!(released.len(), 1);
    assert_eq!(data(released[0])["how"], "explicit");
}

/// A reply too large for the line the owner is told on is refused as the root makes it, saying
/// to reply with less, whether or not it would be accepted: it counts no attempt, and the request
/// stays open for the next. An accepted reply reaches the owner as JSON text, an integer past 64
/// bits intact; a rejected one, as the last reply counted.
#[tokio::test]
async fn a_reply_reaches_the_owner_whole_or_not_at_all() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_REPLY_BOUNDS_KEY",
        "",
        vec![
            (
                "ROLE reviewer",
                reviewing(
                    r#"
                    d = await runtime.channels['review'].receive()
                    try:
                        await d.reply(Verdict(path='x' * (2 << 20), ok=True))
                    except ValueError as e:
                        print('refused:', 'reply with less' in str(e))
                    await d.reply(Verdict(path=d.body, ok=True))
                    "#,
                ),
            ),
            (
                "ROLE big",
                reviewing(
                    r#"
                    d = await runtime.channels['count'].receive()
                    if d.body >= 0:
                        await d.reply(2**64 + d.body)
                    else:
                        for reply in ('one', 'two', 'x' * (2 << 20), 'three'):
                            try:
                                await d.reply(reply)
                            except Exception as e:
                                print(len(reply), type(e).__name__, 'reply with less' in str(e))
                    "#,
                ),
            ),
        ],
        |builder| builder,
    )
    .await;
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    let reply = within(
        root.request("review", json!("a.py"))
            .expect("sent")
            .outcome(),
    )
    .await
    .expect("answered");
    let reviewed: Reviewed = serde_json::from_str(&reply).expect("decodes");
    assert_eq!(reviewed.path, "a.py");
    let big = within(
        tree.session
            .construct(ConstructSpec::new("review.agents:Big", "sonnet")),
    )
    .await
    .expect("constructed");
    let count = within(big.request("count", json!(1)).expect("sent").outcome())
        .await
        .expect("answered");
    assert_eq!(count, "18446744073709551617");
    let count: u128 = serde_json::from_str(&count).expect("a u128");
    assert_eq!(count, (1u128 << 64) + 1);
    let rejected = within(big.request("count", json!(-1)).expect("sent").outcome()).await;
    let Err(RootError::Rejected { last, problems, .. }) = rejected else {
        panic!("not rejected: {rejected:?}");
    };
    assert_eq!(last, "\"three\"", "the last reply counted");
    assert_eq!(problems.len(), 1, "{problems:?}");
    let child = agent_of(&root);
    let counter = agent_of(&big);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &format!("agent/{child}")),
        ["refused: True\n"]
    );
    let refused = format!("{} ValueError True", 2 << 20);
    assert_eq!(
        exec_outputs(&events, &format!("agent/{counter}")).last(),
        Some(
            &[
                "3 DecodeError False",
                "3 DecodeError False",
                &refused,
                "5 CompletionRejected False\n",
            ]
            .join("\n")
        ),
    );
}

/// What was wrong with a rejected reply reaches the owner bounded: a path quoting a long key is
/// shortened in its middle, so twenty problems under one key of 850,000 characters, in a reply
/// that fits its line, still fit the line the rejection is told on.
#[tokio::test]
async fn a_rejected_replys_problems_reach_the_owner_shortened() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_PROBLEMS_KEY",
        "",
        vec![(
            "ROLE tally",
            reviewing(
                r#"
                d = await runtime.channels['tally'].receive()
                for _ in range(3):
                    try:
                        await d.reply({'x' * 850000: ['bad'] * 20})
                    except Exception as e:
                        print(type(e).__name__)
                "#,
            ),
        )],
        |builder| builder,
    )
    .await;
    let root = within(
        tree.session
            .construct(ConstructSpec::new("review.agents:Tally", "sonnet")),
    )
    .await
    .expect("constructed");
    let rejected = within(root.request("tally", json!(1)).expect("sent").outcome()).await;
    let Err(RootError::Rejected { last, problems, .. }) = rejected else {
        panic!("not rejected: {rejected:?}");
    };
    assert!(
        last.starts_with("{\"xxx") && last.len() > 850_000,
        "the reply, whole"
    );
    assert_eq!(problems.len(), 20);
    for (i, problem) in problems.iter().enumerate() {
        assert!(
            problem.path.len() < 1_100,
            "{} characters",
            problem.path.len()
        );
        assert!(
            problem.path.starts_with("$['xxx"),
            "{}",
            &problem.path[..20]
        );
        assert!(problem.path.ends_with(&format!("xx'][{i}]")), "where it is");
    }
    let child = agent_of(&root);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &format!("agent/{child}")),
        ["DecodeError\nDecodeError\nCompletionRejected\n"]
    );
}

/// The close table's *Constructing* row: a construction the session's close comes to first --
/// its constructor waiting on a service -- is given up, its kernel ended, and recorded as
/// `closing`; `ready()` fails with the closing error.
#[tokio::test]
async fn a_construction_the_close_comes_to_is_given_up() {
    let fixture = Fixture::new();
    let served = Arc::clone(&fixture) as Arc<dyn Service>;
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_CONSTRUCTING_KEY",
        "",
        vec![("ROLE slow", reviewing(REVIEWS))],
        move |builder| builder.serve("svc", served),
    )
    .await;
    let spec = ConstructSpec::new("review.agents:Slow", "sonnet")
        .arg("svc", Argument::Facility(Facility::service("svc")));
    let closer = tree.session.closer();
    let watching = Arc::clone(&fixture);
    let (root, ()) = within(async {
        tokio::join!(tree.session.construct(spec), async move {
            while watching.seen().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            closer.close_admission();
        })
    })
    .await;
    let root = root.expect("asked");
    assert!(matches!(not_ready(&root).await, RootError::Closed(_)));
    let (report, events, requests) = tree.end().await;
    assert!(requests.is_empty(), "{requests:?}");
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let failed = kind(&events, "agent.construct.failed", None);
    assert_eq!(failed.len(), 1);
    assert_eq!(data(failed[0])["code"], "closing");
}

/// The builder form: the root constructed as the session starts, its handle the session's; a
/// root that is not constructed fails the start; and `check()` refuses, before any container, a
/// model the configuration lacks and a key its secrets cannot give.
#[tokio::test]
async fn the_builder_constructs_the_root_as_the_session_starts() {
    let (tree, _app) = owned(
        "OUTRIG_TEST_ROOT_BUILDER_KEY",
        "",
        vec![("ROLE reviewer", reviewing(REVIEWS))],
        |builder| builder.construct(reviewer()),
    )
    .await;
    let root = tree.session.constructed().expect("the builder's root");
    within(root.ready()).await.expect("ready");
    assert!(
        tree.session.root(root.id().expect("an id")).is_some(),
        "reachable by its id"
    );
    let reply = within(
        root.request("review", json!("a.py"))
            .expect("sent")
            .outcome(),
    )
    .await
    .expect("answered");
    let reviewed: Reviewed = serde_json::from_str(&reply).expect("decodes");
    assert!(reviewed.ok);
    let (report, _, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");

    let (_dir, spec) = application();
    let (addr, _requests, _) = super::mock_http::routed(vec![], None).await;
    let var = "OUTRIG_TEST_ROOT_BUILDER_CHECK_KEY";
    let cfg = || super::children_tests::config(addr, var, "");
    let builder = |spec: ConstructSpec| {
        SessionBuilder::new(cfg(), None, None)
            .no_primary()
            .secrets(super::agent_tests::keyed(var))
            .construct(spec)
    };
    let unknown = builder(ConstructSpec::new("review.agents:Reviewer", "nope")).check();
    assert!(
        matches!(&unknown, Err(SessionError::Resolve(e)) if e.to_string().contains("sonnet")),
        "{unknown:?}"
    );
    let keyless = SessionBuilder::new(cfg(), None, None)
        .no_primary()
        .secrets(|_: &str| None::<String>)
        .construct(reviewer())
        .check();
    assert!(
        matches!(&keyless, Err(SessionError::Resolve(e)) if e.to_string().contains(var)),
        "{keyless:?}"
    );
    let primary = SessionBuilder::new(cfg(), Some("coding"), None)
        .secrets(super::agent_tests::keyed(var))
        .construct(reviewer())
        .check();
    assert!(matches!(primary, Err(SessionError::Primary)), "{primary:?}");
    let failing = builder(ConstructSpec::new("review.agents:Raising", "sonnet"))
        .application(spec)
        .start_on_host()
        .await;
    let Err(SessionError::Construct { class, error }) = failing else {
        panic!("the start did not fail for its root");
    };
    assert_eq!(class, "review.agents:Raising");
    let RootError::Construct { code, message, .. } = *error else {
        panic!("not the construction's failure: {error}");
    };
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("raised", "ValueError: no task today")
    );
}

/// OR-A2-3: two sessions in one process, with different keys and models, run a root each: each
/// root's model calls carry its own session's key and model, and an `[agents.<name>]` profile
/// named for the class, with a model and preamble of its own, changes nothing its root receives.
#[tokio::test]
async fn two_sessions_run_a_root_each_on_their_own_keys_and_models() {
    let haiku = r#"
[models.haiku]
provider   = "claude"
identifier = "claude-haiku-test"

[agents.Reviewer]
model    = "haiku"
preamble = "PROFILE PREAMBLE"
"#;
    let mut ends = Vec::new();
    for (var, key, model) in [
        ("OUTRIG_TEST_ROOT_TWO_A_KEY", "sk-a", "fast"),
        ("OUTRIG_TEST_ROOT_TWO_B_KEY", "sk-b", "haiku"),
    ] {
        let (tree, app) = owned(
            var,
            haiku,
            vec![("ROLE reviewer", reviewing(REVIEWS))],
            move |builder| {
                builder.secrets(move |name: &str| (name == var).then(|| key.to_string()))
            },
        )
        .await;
        let spec = ConstructSpec::new("review.agents:Reviewer", model)
            .arg("task", Argument::data(json!(null)));
        let root = within(tree.session.construct(spec))
            .await
            .expect("constructed");
        within(root.ready()).await.expect("ready");
        ends.push((tree, app, key, model));
    }
    for (tree, _app, key, model) in ends {
        let (report, _, requests) = tree.end().await;
        assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
        assert!(!requests.is_empty());
        let identifier = match model {
            "haiku" => "claude-haiku-test",
            _ => super::mock_http::MODEL,
        };
        for request in &requests {
            assert_eq!(request.header("x-api-key"), Some(key), "{key}");
            assert_eq!(request.body["model"], identifier, "{key}");
            assert!(!system_text(&request.body).contains("PROFILE PREAMBLE"));
        }
    }
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::config::{Config, MountAccess};
    use crate::python::testing::{ALPINE, PYTHON_ALPINE, pull};
    use crate::{LaunchSpec, MountSpec};

    /// OR-A2-3's images: two sessions in one process, each in its own container from its own
    /// image and with its own key, run a root each, and each root sees its own image alone.
    #[tokio::test]
    async fn two_roots_run_in_their_own_sessions_containers() {
        pull(ALPINE).await;
        pull(PYTHON_ALPINE).await;
        let mut ends = Vec::new();
        for (image, var, key, has_python) in [
            (ALPINE, "OUTRIG_TEST_ROOT_IMAGE_A_KEY", "sk-a", "False"),
            (
                PYTHON_ALPINE,
                "OUTRIG_TEST_ROOT_IMAGE_B_KEY",
                "sk-b",
                "True",
            ),
        ] {
            let logs = tempfile::tempdir().expect("a logs dir");
            let (dir, spec) = application();
            // Its orienting round, its one round, says whether its image has a Python of its own.
            let probe = serving("import os\nprint(os.path.exists('/usr/local/bin/python3'))");
            let (addr, requests, _) =
                super::super::mock_http::routed(vec![("ROLE reviewer", probe)], None).await;
            let session = SessionBuilder::new(
                super::super::children_tests::config(addr, var, ""),
                None,
                None,
            )
            .no_primary()
            .application(spec)
            .secrets(move |name: &str| (name == var).then(|| key.to_string()))
            .container(LaunchSpec::from_image(
                image,
                BTreeMap::new(),
                logs.path().to_path_buf(),
            ))
            .start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
            let root = within(session.construct(reviewer()))
                .await
                .expect("constructed");
            within(root.ready()).await.expect("ready");
            ends.push((session, requests, root, key, has_python, logs, dir));
        }
        for (session, mut requests, root, key, has_python, _logs, _dir) in ends {
            let child = agent_of(&root);
            let report = super::super::agent_tests::shut_down(session).await;
            assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
            let made = super::super::mock_http::drain(&mut requests);
            assert!(
                made.iter()
                    .all(|request| request.header("x-api-key") == Some(key))
            );
            // Its probe's line, then what its construction printed, which its first result
            // carries.
            let printed =
                super::super::agent_tests::tool_result(made.last().expect("a call"), "s0");
            assert_eq!(
                printed.lines().next(),
                Some(has_python),
                "{child} on its own image: {printed}"
            );
        }
    }

    /// A `ProjectReader` the application holds, whose one channel answers a question about the
    /// project under `root`.
    const READER: &str = r#"
import outrig


class ProjectReader(outrig.Agent):
    """You read the project under the directory `root` names, and answer questions about it from
    what its files say. Start with its README and its manifests."""

    def __init__(self, root):
        super().__init__(root=root)

    @outrig.agent
    async def explain(self, question: str) -> str:
        """Answer `question` about the project in a few sentences of plain text."""
        ...
"#;

    /// Against a real model, opted into with `OUTRIG_E2E_REAL_API=1` and
    /// `OUTRIG_E2E_GLOBAL_CONFIG` naming a global configuration whose providers and
    /// `default-model` it runs on, their keys in the environment: a Rust owner provisions an
    /// application whose root answers "What does this project do?" about this repository,
    /// mounted read-only so the model cannot change it, and the session's report is clean.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_constructed_root_answers_about_this_repository() {
        if std::env::var("OUTRIG_E2E_REAL_API").ok().as_deref() != Some("1") {
            eprintln!(
                "[outrig-e2e] skipping a_constructed_root_answers_about_this_repository (set \
                 OUTRIG_E2E_REAL_API=1 and OUTRIG_E2E_GLOBAL_CONFIG to enable)"
            );
            return;
        }
        let global = std::env::var("OUTRIG_E2E_GLOBAL_CONFIG").expect(
            "OUTRIG_E2E_REAL_API=1 set without OUTRIG_E2E_GLOBAL_CONFIG -- name the global \
             configuration that declares the model",
        );
        pull(ALPINE).await;
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("this repository");
        let app = tempfile::tempdir().expect("an application directory");
        std::fs::create_dir_all(app.path().join("reader")).expect("the package");
        std::fs::write(app.path().join("reader/__init__.py"), "").expect("write");
        std::fs::write(app.path().join("reader/agents.py"), READER).expect("write");
        let logs = tempfile::tempdir().expect("a logs dir");
        let config =
            Config::load(app.path(), Some(Path::new(&global))).unwrap_or_else(|e| panic!("{e}"));
        let model = config.default_model.clone().expect("a default-model");
        let session = SessionBuilder::new(config, None, None)
            .no_primary()
            .application(ApplicationSpec::directory(app.path()).imports(["reader.agents"]))
            .container(
                LaunchSpec::from_image(ALPINE, BTreeMap::new(), logs.path().to_path_buf())
                    .with_mount(MountSpec::new(
                        &repository,
                        &repository,
                        MountAccess::ReadOnly,
                    )),
            )
            .construct(
                ConstructSpec::new("reader.agents:ProjectReader", model).arg(
                    "root",
                    Argument::data(json!(repository.to_str().expect("a UTF-8 path"))),
                ),
            )
            .start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let root = session.constructed().expect("the builder's root");
        let wait = std::time::Duration::from_secs(600);
        tokio::time::timeout(wait, root.ready())
            .await
            .expect("ready in time")
            .expect("ready");
        let request = root
            .request("explain", json!("What does this project do?"))
            .expect("sent");
        let answer = tokio::time::timeout(wait, request.outcome())
            .await
            .expect("answered in time")
            .expect("answered");
        let answer: String = serde_json::from_str(&answer).expect("a str");
        assert!(!answer.trim().is_empty(), "{answer}");
        let report = super::super::agent_tests::shut_down(session).await;
        assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    }
}
