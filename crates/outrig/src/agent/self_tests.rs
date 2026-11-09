//! The instance is the child's `self`: an agent class instance constructed in
//! a kernel is bound in its child's kernel as `self`, the same object the
//! parent's code holds. Its ordinary methods run in whichever agent calls
//! them; its request methods are its maker's to call; it is never collected
//! while its child lives, and ends only as it is released. On the host's
//! interpreter, with the routed mock provider. The interpreter's own half --
//! the names `self` and an alias keep from others, and `help()` -- is
//! `interpreter_tests`'; an instance dropped unreleased is `instance_tests`'.

use super::agent_tests::round;
use super::children_tests::{
    child_runs, data, exec_outputs, kind, of_agent, primary_waits, printed, serving, tree,
};
use super::instance_tests::{
    ANSWER_ONE, child_of, fizzing, indented, launching, ready_then, subject,
};
use super::mock_http::{scripted, system_text, text_reply};

/// The module two instances' class comes from, a file the primary writes and
/// imports: a list every agent that imports it shares, and what each child
/// replies with.
const TWINS: &str = r#"
from dataclasses import dataclass
import outrig

LOG = []

@dataclass
class Seen:
    x: int
    module: int

class Twin(outrig.Agent):
    """ROLE twin. Answer requests on look."""
    @outrig.agent
    async def look(self, x: int) -> Seen:
        """Set `self.x` to `x`, and say what you see."""
"#;

/// What a twin runs for a request: set its own `self.x`, log it in the module
/// both import, give the other twin time to do the same, and reply with what
/// it then reads, and which module its class is from.
const LOOK: &str = r#"
import sys, twins
d = await runtime.channels['look'].receive()
self.x = d.body
twins.LOG.append(d.body)
await asyncio.sleep(0.2)
await d.reply(Seen(x=self.x, module=id(sys.modules[type(self).__module__])))
"#;

/// Two instances of one class each have their own `self` in their own child:
/// each child sets `self.x` and reads its own back, which the parent then
/// reads on the instances it holds; a list in the class's module, appended to
/// from both children, holds both entries; and both children's class is from
/// the one module object the parent imported.
#[tokio::test]
async fn two_instances_have_their_own_self_and_share_one_module() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_TWINS",
        "",
        vec![
            ("ROLE twin", ready_then(serving(LOOK))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import os, shutil, sys, tempfile
                    where = tempfile.mkdtemp()
                    with open(os.path.join(where, 'twins.py'), 'w') as f:
                        f.write({TWINS:?})
                    sys.path.insert(0, where)
                    import twins
                    a, b = twins.Twin(), twins.Twin()
                    seen = await asyncio.gather(a.look(1), b.look(2))
                    print([s.x for s in seen], a.x, b.x, sorted(twins.LOG))
                    print(all(s.module == id(twins) for s in seen), sys.modules['twins'] is twins)
                    await a.release()
                    await b.release()
                    sys.path.remove(where)
                    shutil.rmtree(where)
                    "#
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "[1, 2] 1 2 [1, 2]\nTrue True\n");
    let started = kind(&events, "agent.instance.started", None);
    assert_eq!(started.len(), 2);
    assert_ne!(subject(started[0]), subject(started[1]), "a child each");
    assert!(started.iter().all(|event| data(event)["self"] == true));
}

/// What a reviewer's launch round runs: it notes `self`'s identity where the
/// parent can read it, answers the request sent before it was ready with the
/// task its constructor stored, and finds `self` left out of
/// `runtime.names()` until it is bound to something else.
const REVIEWER: &str = r#"
import shared
shared.ids.append(id(self))
d = await runtime.channels['review'].receive()
await d.reply(self._task.title)
print('self' in runtime.names(), 'task' in globals())
me, self = self, 'rebound'
print('self' in runtime.names())
self = me
print('self' in runtime.names())
"#;

/// The child's `self` is the object its parent constructed, not a copy: the
/// task its `__init__` stored after `super().__init__()` is the parent's task
/// object, read in the child's first round and given back as its reply; the
/// identity it notes is the parent's `id(reviewer)`; and the child's kernel
/// holds the parent's very object. `self` is out of `runtime.names()` until
/// rebound, the event says the child has its `self`, and its instructions say
/// what `self` is.
#[tokio::test]
async fn the_childs_self_is_the_object_its_parent_constructed() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_SAME",
        "",
        vec![
            ("ROLE reviewer", scripted(child_runs("c1", REVIEWER))),
            (
                "",
                primary_waits(
                    r#"
                    import __main__, sys, types
                    shared = sys.modules['shared'] = types.ModuleType('shared')
                    shared.ids = []
                    class Task:
                        def __init__(self, title):
                            self.title = title
                    class Reviewer(outrig.Agent):
                        """ROLE reviewer. Answer requests on review."""
                        def __init__(self, task):
                            super().__init__()
                            self._task = task
                        @outrig.agent
                        async def review(self, path: str) -> str:
                            """Review the file at `path`."""
                    task = Task('a task the parent made')
                    reviewer = Reviewer(task)
                    title = await reviewer.review('src/main.py')
                    await reviewer.ready()
                    held = __main__._kernels[reviewer._outrig.child.id].globals['self']
                    print(title == task.title, shared.ids == [id(reviewer)], held is reviewer,
                          held._task is task)
                    await reviewer.release()
                    "#,
                ),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True True True True\n");
    let child = child_of(&events, "Reviewer");
    assert_eq!(
        exec_outputs(&events, &child)[0],
        "False False\nTrue\nFalse\n"
    );
    let started = kind(&events, "agent.instance.started", None);
    assert_eq!(data(started[0])["self"], true);
    let system = system_text(&of_agent(&requests, "ROLE reviewer")[0].body);
    assert!(
        system.contains("`self` is the `Reviewer` instance your parent's code constructed"),
        "{system}"
    );
}

/// A class with an ordinary method that notes the thread it runs on, an
/// ordinary coroutine method, and one request method.
const HELPER: &str = r#"
import threading
class Helper(outrig.Agent):
    """ROLE helper. Answer requests on fizz."""
    def __init__(self):
        super().__init__()
        self.threads = []
    def where(self):
        self.threads.append(threading.get_ident())
    async def later(self):
        return 'ran'
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Reply with a Thing."""
"#;

/// A helper's launch round: each kind of method, called on `self`.
const HELPER_LAUNCH: &str = r#"
import inspect, threading
self.where()
later = self.later()
print(inspect.iscoroutine(later), await later)
try:
    self.fizz('to myself')
except TypeError as e:
    print("'fizz'" in str(e), 'nothing was sent' in str(e))
print(threading.current_thread().name)
"#;

/// An ordinary method runs where it is called: on the child kernel's thread
/// from the child's round, and on the parent's from the parent; an ordinary
/// `async def` called in the child gives a coroutine, which runs there; a
/// request method called on `self` in the child raises `TypeError` naming the
/// channel and sends nothing, while the parent's call of it is a request,
/// the only one the record shows.
#[tokio::test]
async fn ordinary_methods_run_where_they_are_called() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_METHODS",
        "",
        vec![
            (
                "ROLE helper",
                scripted(
                    [
                        child_runs("c1", HELPER_LAUNCH),
                        child_runs("c2", ANSWER_ONE),
                    ]
                    .concat(),
                ),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import __main__, threading
                    {helper}
                    helper = Helper()
                    await helper.ready()
                    helper.where()
                    kernel = __main__._kernels[helper._outrig.child.id]
                    print(helper.threads == [kernel.thread.ident, threading.get_ident()],
                          threading.get_ident() == threading.main_thread().ident)
                    print(type(await helper.fizz('hello')).__name__)
                    await helper.release()
                    "#,
                    helper = indented(HELPER, 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "True True\nThing\n");
    let child = child_of(&events, "Helper");
    assert_eq!(
        exec_outputs(&events, &child)[0],
        format!("True ran\nTrue True\nkernel-{}\n", &child["agent/".len()..])
    );
    let sent = kind(&events, "agent.request.sent", None);
    assert_eq!(sent.len(), 1, "the parent's request alone");
    assert_eq!(data(sent[0])["sender"], "agent/primary");
}

/// An outer instance's launch round makes an inner one, kept in a module the
/// primary can read, and waits for it to be ready.
const OUTER: &str = r#"
import shared
shared.inner = shared.Inner()
await shared.inner.ready()
"#;

/// Releasing an agent releases its subtree: the instance it made ends with
/// it, its child listed among those the release ended, with no
/// `agent.instance.released` of its own; and a call on it from then on raises
/// `AgentReleased`, sending nothing.
#[tokio::test]
async fn an_instance_ends_with_the_agent_that_made_it() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_SUBTREE",
        "",
        vec![
            ("ROLE outer", scripted(child_runs("c1", OUTER))),
            ("ROLE inner", ready_then(scripted(vec![text_reply("idle")]))),
            (
                "",
                primary_waits(&format!(
                    r#"
                    import sys, types
                    shared = sys.modules['shared'] = types.ModuleType('shared')
                    {inner}
                    {outer}
                    shared.Inner = Inner
                    outer = Outer()
                    await outer.ready()
                    inner = shared.inner
                    await outer.release()
                    try:
                        inner.fizz('x')
                    except outrig.AgentReleased:
                        print('released')
                    "#,
                    inner = indented(&fizzing("Inner", "inner"), 20),
                    outer = indented(&fizzing("Outer", "outer"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "released\n");
    let (outer, inner) = (child_of(&events, "Outer"), child_of(&events, "Inner"));
    let released = kind(&events, "agent.instance.released", None);
    assert_eq!(released.len(), 1);
    assert_eq!(subject(released[0]), outer);
    assert_eq!(data(released[0])["how"], "explicit");
    let ended = kind(&events, "agent.released", None)
        .into_iter()
        .map(|event| data(event)["ended"].clone())
        .collect::<Vec<_>>();
    assert!(
        ended.iter().any(|ended| ended
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| *id == inner["agent/".len()..]))),
        "{ended:?}"
    );
    assert!(kind(&events, "agent.request.sent", None).is_empty());
}

/// Two classes whose `__init__` stores its `task` argument on `self._task`.
const TASKED: &str = r#"
class Task:
    def __init__(self, title):
        self.title = title
class Aliased(outrig.Agent):
    """ROLE aliased. Answer requests on fizz."""
    def __init__(self, task, *, title):
        super().__init__(title=title)
        self._task = task
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Reply with a Thing."""
class Plain(outrig.Agent):
    """ROLE plain. Answer requests on fizz."""
    def __init__(self, task):
        super().__init__()
        self._task = task
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Reply with a Thing."""
"#;

/// The aliased child's launch round: `task` is its constructor's argument, the
/// object `self._task` holds, and is out of `runtime.names()` until rebound.
const ALIASED: &str = r#"
print('task' in globals(), task is self._task, 'task' in runtime.names())
task = 'rebound'
print('task' in runtime.names())
task = self._task
"#;

/// An alias a construction names is bound beside `self` as the constructor's
/// argument itself -- `task is self._task` -- left out of `runtime.names()`
/// until rebound, and named in the child's instructions; one named like an
/// input, like `self` or like a class bound in the child is refused, as is one
/// named once the launch has begun; and an instance whose construction names
/// none has no such global. No construction
/// in a kernel names an alias, so this names one through the instance's
/// record, as an owner's construction will.
#[tokio::test]
async fn an_alias_is_the_constructor_argument_itself() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_ALIAS",
        "",
        vec![
            ("ROLE aliased", scripted(child_runs("c1", ALIASED))),
            (
                "ROLE plain",
                scripted(child_runs("c2", "print('task' in globals())")),
            ),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {tasked}
                    task = Task('t')
                    aliased = Aliased(task, title='T')
                    for name in ('title', 'self', 'Thing'):
                        try:
                            aliased._outrig.alias({{name: task}})
                        except (ValueError, TypeError) as e:
                            print(name, type(e).__name__, repr(name) in str(e))
                    aliased._outrig.alias({{'task': task}})
                    plain = Plain(task)
                    await aliased.ready()
                    await plain.ready()
                    try:
                        aliased._outrig.alias({{'late': task}})
                    except RuntimeError as e:
                        print('late', 'launching' in str(e))
                    await aliased.release()
                    await plain.release()
                    "#,
                    tasked = indented(TASKED, 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "title ValueError True\nself ValueError True\nThing TypeError True\nlate True\n"
    );
    let (aliased, plain) = (child_of(&events, "Aliased"), child_of(&events, "Plain"));
    assert_eq!(
        exec_outputs(&events, &aliased)[0],
        "True True False\nTrue\n"
    );
    assert_eq!(exec_outputs(&events, &plain)[0], "False\n");
    let told = |marker: &str| system_text(&of_agent(&requests, marker)[0].body);
    let (aliased, plain) = (told("ROLE aliased"), told("ROLE plain"));
    assert!(
        aliased.contains("`task` is the `task` its constructor was given, the same object."),
        "{aliased}"
    );
    assert!(!plain.contains("its constructor was given"), "{plain}");
}

/// An instance constructed and left alone has its launch round, which needs
/// no request, and nothing more: no request is made of it or recorded, its
/// child reads idle after that round, and the provider is called once.
#[tokio::test]
async fn an_instance_left_alone_has_its_launch_round_and_goes_idle() {
    let mut tree = tree(
        "OUTRIG_TEST_SELF_ALONE",
        "",
        vec![
            ("ROLE alone", scripted(vec![text_reply("ready")])),
            (
                "",
                primary_waits(&format!(
                    r#"
                    {alone}
                    alone = Alone()
                    await alone.ready()
                    await until(lambda: alone._outrig.child.status == 'idle')
                    print('idle')
                    await alone.release()
                    "#,
                    alone = indented(&fizzing("Alone", "alone"), 20),
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "idle\n");
    assert!(
        events
            .iter()
            .all(|event| !event.kind().starts_with("agent.request.")),
        "no request"
    );
    let called = of_agent(&requests, "ROLE alone");
    assert_eq!(called.len(), 1);
    assert!(launching(called[0]));
    assert_eq!(kind(&events, "agent.instance.ready", None).len(), 1);
}
