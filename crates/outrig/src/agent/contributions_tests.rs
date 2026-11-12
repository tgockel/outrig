//! An agent class's contributions to its child's instructions, end to end: the sections its
//! `__outrig_instructions__` returns are composed after its docstring in the documented order,
//! verbatim, once, with nothing else of the instance read; what the hook returns wrongly or past
//! a bound, and a hook that raises, fail the constructor call before anything is launched. On the
//! host's interpreter, with the routed mock provider. The interpreter's own half -- the checks,
//! the lines it sends -- is `interpreter_tests`'; a root's contributions are `root_tests`', a host
//! program's `owned_tests`'.

use serde_json::json;

use super::agent_tests::round;
use super::children_tests::{
    data, kind, of_agent, primary_waits, printed, serving, tree, tree_built,
};
use super::instance_tests::{child_of, launching, ready_then};
use super::mock_http::system_text;
use crate::harness::skills::DirectorySource;

/// A class whose hook returns three sections from what its constructor stored and the mapping it
/// was given, an instance that holds a secret and a counter, and a facility whose `repr` writes a
/// file.
const REVIEWER: &str = r#"
class Task:
    def __init__(self, title):
        self.title = title

class Probe:
    def __repr__(self):
        open('@MARK@', 'w').write('ran')
        return 'Probe()'

class Reviewer(outrig.Agent):
    """ROLE reviewer. Answer requests on review; {braces} stay."""
    def __init__(self, task, probe):
        super().__init__(facilities={'task': task, 'probe': probe})
        self._task = task
        self.secret = 'S3CR3T-VALUE'
        self.calls = 0
    def __outrig_instructions__(self):
        self.calls += 1
        return [
            outrig.Section('Task', f'Review {self._task.title}; {{x}} and {{}} stay.'),
            outrig.Section('Facilities', ', '.join(sorted(self.facilities))),
            outrig.Section('Note', 'café'),
        ]
    @outrig.agent
    async def review(self, path: str) -> str:
        """Review the file at `path`."""

reviewer = Reviewer(Task('the parser'), Probe())
print([await reviewer.review(f'p{n}') for n in range(3)])
print(reviewer.calls)
await reviewer.release()
"#;

/// What the reviewer's child runs to answer one request with its body.
const ECHO: &str = "d = await runtime.channels['review'].receive()\nawait d.reply(d.body)";

/// A class's sections reach its child's instructions in the documented order, each under its
/// title and as returned, braces and all, after the docstring and before what the child was given
/// and its channels; the hook reads `self` and its mapping, and nothing else of the instance is
/// read -- not the attribute it set, and no `repr` -- and it runs once, so every round's
/// instructions are the launch round's and are recorded once. The start event lists each title
/// and the length of its text in bytes. A child is told no skills, the session's having some.
#[tokio::test]
async fn a_class_contributes_sections_composed_in_order_and_once() {
    let marks = tempfile::tempdir().expect("a directory");
    let mark = marks.path().join("repr-ran");
    let skills = tempfile::tempdir().expect("a skills directory");
    std::fs::create_dir(skills.path().join("review")).expect("a skill");
    std::fs::write(
        skills.path().join("review/SKILL.md"),
        "---\nname: review\ndescription: Reviews a change.\n---\nSteps.\n",
    )
    .expect("its instructions");
    let source = skills.path().to_path_buf();
    let mut tree = tree_built(
        "OUTRIG_TEST_CONTRIBUTIONS_COMPOSED",
        "",
        vec![
            ("ROLE reviewer", ready_then(serving(ECHO))),
            (
                "",
                primary_waits(&REVIEWER.replace("@MARK@", mark.to_str().expect("a UTF-8 path"))),
            ),
        ],
        None,
        move |builder| builder.skill_source("test", DirectorySource::new(source)),
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(printed(&requests, "p1"), "['p0', 'p1', 'p2']\n1\n");

    let made = of_agent(&requests, "ROLE reviewer");
    assert!(
        launching(made[0]) && made.len() > 3,
        "a launch round and three"
    );
    let told = system_text(&made[0].body);
    let sections = "## Task\n\nReview the parser; {x} and {} stay.\n\n\
                    ## Facilities\n\nprobe, task\n\n## Note\n\ncaf\u{e9}\n\n";
    let positions: Vec<usize> = [
        "You act on this project by writing Python",
        "You are a child agent",
        "ROLE reviewer. Answer requests on review; {braces} stay.",
        sections,
        "`self` is the `Reviewer` instance your parent's code constructed",
        "You were given these to act through, by name:",
        "- `task`, a `",
        "- `probe`, a `",
        "You answer requests, which arrive on these channels:",
        "`runtime.channels[\"review\"]`: Review the file at `path`.",
        "Your whole conversation is in `runtime.history.turns`",
    ]
    .map(|needle| {
        told.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {told}"))
    })
    .to_vec();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?} in {told}"
    );
    assert!(!told.contains("S3CR3T-VALUE"), "{told}");
    assert!(
        !told.contains("These skills"),
        "a child is told none: {told}"
    );
    assert!(!mark.exists(), "the facility's repr ran");
    assert!(
        made.iter()
            .all(|request| system_text(&request.body) == told),
        "composed once, each round's instructions the launch round's"
    );

    let child = child_of(&events, "Reviewer");
    assert_eq!(
        kind(&events, "model.instructions", Some(&child)).len(),
        1,
        "recorded once"
    );
    let started = data(kind(&events, "agent.instance.started", None)[0]);
    assert_eq!(
        started["contributions"],
        json!([
            {"title": "Task", "length": "Review the parser; {x} and {} stay.".len()},
            {"title": "Facilities", "length": "probe, task".len()},
            {"title": "Note", "length": 5},
        ])
    );
}

/// What a hook returns wrongly, or past a bound, or a hook that raises, fails the constructor
/// call with what is wrong -- `TypeError` for what is not a `Section`s' iterable, `ValueError`
/// naming the section for a bound, the hook's own exception for one it raises -- and launches
/// nothing: no spawn, no instance, no model call of the class's child.
#[tokio::test]
async fn a_hook_that_returns_wrongly_or_raises_launches_nothing() {
    let mut tree = tree(
        "OUTRIG_TEST_CONTRIBUTIONS_REFUSED",
        "",
        vec![(
            "",
            primary_waits(
                r#"
                class Bad(outrig.Agent):
                    """ROLE bad. Answer requests on fizz."""
                    def __init__(self, what):
                        super().__init__()
                        self.what = what
                    def __outrig_instructions__(self):
                        what = self.what
                        if what == 'raises':
                            raise RuntimeError('the hook raised')
                        if what == 'str':
                            return 'a string'
                        if what == 'tuple':
                            return [('Title', 'text')]
                        if what == 'big':
                            return [outrig.Section('Big', 'x' * (16 * 1024 + 1))]
                        if what == 'together':
                            return [outrig.Section(name, 'x' * 10923) for name in 'ABC']
                        return [outrig.Section('t' * 65, 'text')]
                    @outrig.agent
                    async def fizz(self, message: str) -> Thing:
                        """Reply with a Thing."""
                for what, said in [('raises', 'the hook raised'), ('str', 'not str'),
                                   ('tuple', 'item 1 is tuple'),
                                   ('big', "section 1, 'Big', has 16385 bytes of text"),
                                   ('together', "section 3, 'C', brings the sections to"),
                                   ('title', 'has a title of 65 characters')]:
                    try:
                        Bad(what)
                    except (TypeError, ValueError, RuntimeError) as e:
                        print(what, type(e).__name__, said in str(e))
                "#,
            ),
        )],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "raises RuntimeError True\nstr TypeError True\ntuple TypeError True\n\
         big ValueError True\ntogether ValueError True\ntitle ValueError True\n"
    );
    assert!(kind(&events, "agent.spawned", None).is_empty(), "no spawn");
    assert!(
        events
            .iter()
            .all(|event| !event.kind().starts_with("agent.instance")),
        "no instance"
    );
    assert!(of_agent(&requests, "ROLE bad").is_empty(), "no model call");
}
