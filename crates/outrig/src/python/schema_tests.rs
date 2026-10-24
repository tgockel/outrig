//! `outrig.schema`, and the package outside a kernel, run as a program on the
//! host runs it: the payload's interpreter, with OutRig's package directory on
//! `sys.path` after the standard library and no kernel anywhere.
//!
//! The maintainer's `ReviewResult` and its example completion are copied from
//! `typed-agents.md` unchanged, as `review_fixture.py` and
//! `review_fixture.json`: the decoder has to take them as written.

use std::process::Command;

use super::testing::{package, py, python};

const REVIEW: &str = include_str!("review_fixture.py");
const EXAMPLE: &str = include_str!("review_fixture.json");

/// What every program here starts with: `outrig.schema` imported, the
/// maintainer's classes declared as `review`, their example as `EXAMPLE`,
/// `example()` a fresh copy of it, and `problems(value, schema)` the problems
/// a decode finds, joined, or `decoded`.
const PRELUDE: &str = r#"
import copy, dataclasses, json, os, sys, types, typing
from dataclasses import dataclass
from typing import Annotated, Literal, Optional
sys.path.append(sys.argv[1])
sys.path.append(sys.argv[2])
import outrig
from outrig import schema
from outrig.schema import DecodeError, alias, declare, doc
import review_fixture as rf
from review_fixture import Note, Question, ReviewResult, Verdict
review = declare(ReviewResult)
with open(os.path.join(sys.argv[2], 'review_fixture.json')) as f:
    EXAMPLE = json.load(f)

def example():
    return copy.deepcopy(EXAMPLE)

def problems(value, against=review):
    try:
        against.decode(value)
    except DecodeError as e:
        return '; '.join(map(str, e.problems))
    return 'decoded'

def module(name, source):
    """A module made from `source` and registered, as one imported from a file is."""
    made = sys.modules[name] = types.ModuleType(name)
    exec(source, made.__dict__)
    return made
"#;

/// Run `body`, after [`PRELUDE`], in the payload's interpreter on the host,
/// and return what it printed. A program that fails fails the test.
fn run(body: &str) -> String {
    let fixtures = tempfile::tempdir().expect("tempdir");
    std::fs::write(fixtures.path().join("review_fixture.py"), REVIEW).expect("write");
    std::fs::write(fixtures.path().join("review_fixture.json"), EXAMPLE).expect("write");
    let program = format!("{PRELUDE}\n{}", py(body));
    let output = Command::new(python())
        .args(["-I", "-c", &program])
        .arg(package())
        .arg(fixtures.path())
        .output()
        .expect("the payload's python runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{}\n{stdout}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// On the host, `outrig.schema` imports and works, and what only a kernel has
/// says so rather than being missing: an `AttributeError` would read as a
/// typo, and `hasattr` would hide it.
#[test]
fn outrig_schema_imports_on_the_host_and_runtime_says_it_is_a_kernels() {
    let out = run(r#"
        print(declare(int))
        for name in ('runtime', 'bindings'):
            try:
                getattr(outrig, name)
            except Exception as e:
                print(type(e).__name__, e)
        try:
            from outrig import runtime
        except Exception as e:
            print('from-import', type(e).__name__)
        print(hasattr(outrig, 'config'))
        print([name for name in ('Session', 'Mount', 'Event', 'Missed', 'ShutdownReport',
                                 'ShutdownException', 'current_session') if hasattr(outrig, name)])
        "#);
    assert_eq!(
        out,
        "<schema of int>\n\
         RuntimeError outrig.runtime exists only in an agent's kernel, where OutRig runs agent \
         code, and this Python is not one\n\
         RuntimeError outrig.bindings exists only in an agent's kernel, where OutRig runs agent \
         code, and this Python is not one\n\
         from-import RuntimeError\n\
         False\n\
         []\n"
    );
}

/// The maintainer's classes declare as written, and the example decodes to
/// the instance it describes -- and encodes back to the same JSON.
#[test]
fn the_maintainers_review_result_decodes_its_example() {
    let out = run(r#"
        result = review.decode(example())
        expected = ReviewResult(
            verdict=Verdict(
                status='reject-changes',
                message='The retry loop can spin forever on a permanent error.',
            ),
            notes={'unbounded-retry-loop': Note(
                location='src/net/retry.py:42-42', severity='high',
                message='retry() never gives up on a permanent 4xx, so callers hang.',
                duplicate_status='not-found', duplicate_reference=None,
            )},
            pre_existing_issues={'missing-client-timeout': Note(
                location='src/net/client.py:10-10', severity='medium',
                message='The HTTP client has no default timeout. Base behavior is unchanged by '
                        'this change.',
                duplicate_status='duplicate', duplicate_reference='#123',
            )},
            questions={'intended-retry-budget': Question(
                location=None,
                message='Is there a product requirement for the total retry time?',
            )},
        )
        print(result == expected, type(result.verdict) is Verdict,
              type(result.questions['intended-retry-budget']) is Question)
        print(review.encode(result) == EXAMPLE)
        "#);
    assert_eq!(out, "True True True\nTrue\n");
}

/// The schema text carries every `doc()` text verbatim, each as its field's
/// description, with the shape `typed-agents.md` gives: `Literal` an `enum`,
/// a map an object whose `additionalProperties` is its value's schema, and a
/// dataclass a closed object requiring every field without a default.
#[test]
fn the_review_schema_text_carries_every_doc_verbatim() {
    let out = run(r#"
        text = review.text()
        rendered = json.loads(text)
        docs = []
        for cls in (Verdict, Note, Question, ReviewResult):
            for hint in typing.get_type_hints(cls, include_extras=True).values():
                docs += [m.text for m in hint.__metadata__ if isinstance(m, type(doc('')))]
        print(len(docs), all(text_ in text for text_ in docs))
        defs = rendered['$defs']
        described = [p['description'] for p in rendered['properties'].values()]
        described += [p['description'] for c in defs.values() for p in c['properties'].values()]
        print(sorted(described) == sorted(docs))
        print(rendered['required'], rendered['additionalProperties'], sorted(defs))
        print(list(rendered['properties']))
        print(defs['Note']['required'], defs['Note']['additionalProperties'])
        print(defs['Note']['properties']['severity']['enum'])
        print(rendered['properties']['notes']['additionalProperties'])
        print(defs['Note']['properties']['duplicate-reference']['anyOf'])
        print(defs['Question']['required'])
        "#);
    assert_eq!(
        out,
        "13 True\n\
         True\n\
         ['verdict', 'notes', 'pre-existing-issues'] False ['Note', 'Question', 'Verdict']\n\
         ['verdict', 'notes', 'pre-existing-issues', 'questions']\n\
         ['location', 'severity', 'message', 'duplicate-status', 'duplicate-reference'] False\n\
         ['blocker', 'high', 'medium', 'low']\n\
         {'$ref': '#/$defs/Note'}\n\
         [{'type': 'string'}, {'type': 'null'}]\n\
         ['location', 'message']\n"
    );
}

/// Every `Literal` value decodes as itself and encodes back, the hyphenated
/// ones included.
#[test]
fn every_literal_value_round_trips() {
    let out = run(r#"
        def set_status(v, s): v['verdict']['status'] = s
        def set_severity(v, s): v['notes']['unbounded-retry-loop']['severity'] = s
        def set_duplicate(v, s): v['notes']['unbounded-retry-loop']['duplicate-status'] = s
        def get_status(r): return r.verdict.status
        def get_severity(r): return r.notes['unbounded-retry-loop'].severity
        def get_duplicate(r): return r.notes['unbounded-retry-loop'].duplicate_status
        for alias_, put, get in ((rf.Status, set_status, get_status),
                                 (rf.Severity, set_severity, get_severity),
                                 (rf.DuplicateStatus, set_duplicate, get_duplicate)):
            for value in typing.get_args(alias_):
                v = example()
                put(v, value)
                decoded = review.decode(v)
                print(value, get(decoded) == value, review.encode(decoded) == v)
        "#);
    assert_eq!(
        out,
        "approve True True\n\
         comment True True\n\
         reject-changes True True\n\
         blocker True True\n\
         high True True\n\
         medium True True\n\
         low True True\n\
         duplicate True True\n\
         not-found True True\n\
         unknown True True\n"
    );
}

/// `pre_existing_issues` is the field's name, not its key, and so as a key
/// it is unknown -- beside the real key or in its place.
#[test]
fn a_fields_own_name_is_unknown_when_it_has_an_alias() {
    let out = run(r#"
        v = example()
        v['pre_existing_issues'] = v.pop('pre-existing-issues')
        print(problems(v))
        v = example()
        v['pre_existing_issues'] = {}
        print(problems(v))
        "#);
    assert_eq!(
        out,
        "$.pre_existing_issues: unknown key; that field's key is \"pre-existing-issues\"; \
         $.pre-existing-issues: missing required key\n\
         $.pre_existing_issues: unknown key; that field's key is \"pre-existing-issues\"\n"
    );
}

/// `duplicate-status` and `duplicate-reference` are both required: the one
/// never nullable, the other nullable and still required.
#[test]
fn a_notes_duplicate_fields_are_required() {
    let out = run(r#"
        for key in ('duplicate-status', 'duplicate-reference'):
            v = example()
            del v['notes']['unbounded-retry-loop'][key]
            print(problems(v))
        "#);
    assert_eq!(
        out,
        "$.notes['unbounded-retry-loop'].duplicate-status: missing required key\n\
         $.notes['unbounded-retry-loop'].duplicate-reference: missing required key\n"
    );
}

/// `null` is a value only where the type includes `None`.
#[test]
fn null_is_accepted_only_where_the_type_includes_none() {
    let out = run(r#"
        v = example()
        v['pre-existing-issues']['missing-client-timeout']['duplicate-reference'] = None
        print(problems(v))
        for key in ('location', 'severity'):
            v = example()
            v['notes']['unbounded-retry-loop'][key] = None
            print(problems(v))
        "#);
    assert_eq!(
        out,
        "decoded\n\
         $.notes['unbounded-retry-loop'].location: expected a string, not null\n\
         $.notes['unbounded-retry-loop'].severity: expected one of \"blocker\", \"high\", \
         \"medium\", \"low\", not null\n"
    );
}

/// A question's location may be `null`, and may not be left out: the field
/// is required-but-nullable, not optional.
#[test]
fn a_questions_location_is_required_but_nullable() {
    let out = run(r#"
        v = example()
        print(problems(v), review.decode(v).questions['intended-retry-budget'].location)
        del v['questions']['intended-retry-budget']['location']
        print(problems(v))
        "#);
    assert_eq!(
        out,
        "decoded None\n\
         $.questions['intended-retry-budget'].location: missing required key\n"
    );
}

/// Of every key anywhere in the example, `questions` is the only one whose
/// omission decodes, and it gives an empty map; a missing `verdict` is a
/// missing required key.
#[test]
fn only_questions_may_be_omitted() {
    let out = run(r#"
        places = [(), ('verdict',), ('notes', 'unbounded-retry-loop'),
                  ('pre-existing-issues', 'missing-client-timeout'),
                  ('questions', 'intended-retry-budget')]
        accepted = []
        for place in places:
            container = EXAMPLE
            for step in place:
                container = container[step]
            for key in container:
                v = example()
                target = v
                for step in place:
                    target = target[step]
                del target[key]
                if problems(v) == 'decoded':
                    accepted.append((*place, key))
        print(accepted)
        v = example()
        del v['questions']
        print(review.decode(v).questions)
        v = example()
        del v['verdict']
        print(problems(v))
        "#);
    assert_eq!(
        out,
        "[('questions',)]\n\
         {}\n\
         $.verdict: missing required key\n"
    );
}

/// An omitted `questions` takes its `default_factory`, once per instance.
#[test]
fn decoded_instances_never_share_a_default() {
    let out = run(r#"
        v = example()
        del v['questions']
        a, b = review.decode(v), review.decode(v)
        print(a.questions is not b.questions, a.questions == b.questions == {})
        a.questions['added'] = None
        print(b.questions)
        "#);
    assert_eq!(out, "True True\n{}\n");
}

/// The decoder asserts the shape and nothing more: no verdict is tied to a
/// severity, and a question's id need match no note.
#[test]
fn the_decoder_invents_no_policy() {
    let out = run(r#"
        v = example()
        v['notes']['unbounded-retry-loop']['severity'] = 'low'
        v['pre-existing-issues'] = {}
        print(problems(v), review.decode(v).verdict.status)
        v = example()
        v['questions'] = {'matches-no-note': {'location': None, 'message': 'Why?'}}
        print(problems(v))
        "#);
    assert_eq!(out, "decoded reject-changes\ndecoded\n");
}

/// A value with several problems reports each with its path, in the order
/// the value holds them, and a value with more than `PROBLEMS_MAX` lists that
/// many and counts the rest.
#[test]
fn every_problem_is_reported_with_its_path() {
    let out = run(r#"
        v = example()
        del v['verdict']['message']
        v['notes']['unbounded-retry-loop']['location'] = 42
        v['notes']['unbounded-retry-loop']['severity'] = 'critical'
        v['pre-existing-issues']['missing-client-timeout']['duplicate-status'] = None
        v['questions']['intended-retry-budget']['extra'] = True
        try:
            review.decode(v)
        except DecodeError as e:
            print(e)
        many = example()
        many['notes'] = {f'n{i}': {} for i in range(30)}
        try:
            review.decode(many)
        except DecodeError as e:
            lines = str(e).splitlines()
            print(len(e.problems), e.more, schema.PROBLEMS_MAX, lines[0], '|', lines[-1])
        "#);
    assert_eq!(
        out,
        "5 problems decoding ReviewResult:\n  \
         $.verdict.message: missing required key\n  \
         $.notes['unbounded-retry-loop'].location: expected a string, not an integer\n  \
         $.notes['unbounded-retry-loop'].severity: expected one of \"blocker\", \"high\", \
         \"medium\", \"low\", not \"critical\"\n  \
         $.pre-existing-issues['missing-client-timeout'].duplicate-status: expected one of \
         \"duplicate\", \"not-found\", \"unknown\", not null\n  \
         $.questions['intended-retry-budget'].extra: unknown key\n\
         20 130 20 150 problems decoding ReviewResult: |   ... and 130 more\n"
    );
}

/// A type outside the set fails when it is declared, naming the field it is
/// in -- one nested in another dataclass by the innermost -- and so does a
/// hint that does not resolve in its module.
#[test]
fn a_type_outside_the_set_fails_at_declaration_naming_the_field() {
    let out = run(r#"
        @dataclass
        class Blob:
            data: bytes
        @dataclass
        class Tags:
            tags: set[str]
        class Plain:
            pass
        @dataclass
        class Holder:
            thing: Plain
        @dataclass
        class Outer:
            inner: list[Tags]
        late = module('late', 'from __future__ import annotations\n'
                              'from dataclasses import dataclass\n'
                              '@dataclass\n'
                              'class Late:\n'
                              '    fine: int\n'
                              '    thing: NoSuchType\n')
        for t in (Blob, Tags, Holder, Outer, late.Late):
            try:
                declare(t)
                print('declared', t)
            except TypeError as e:
                print(str(e).split(', which is outside')[0])
        "#);
    assert_eq!(
        out,
        "outrig.schema cannot take bytes (field 'data' of Blob)\n\
         outrig.schema cannot take set[str] (field 'tags' of Tags)\n\
         outrig.schema cannot take Plain (field 'thing' of Holder)\n\
         outrig.schema cannot take set[str] (field 'tags' of Tags)\n\
         outrig.schema cannot take Late, whose field types cannot be resolved: NameError: name \
         'NoSuchType' is not defined (field 'thing')\n"
    );
}

/// The markers are refused where they cannot mean anything, and so is a
/// dataclass decoding could not build.
#[test]
fn a_declaration_the_decoder_could_not_honor_is_refused() {
    let out = run(r#"
        @dataclass
        class Nullable:
            kind: Literal['a', None]
        @dataclass
        class Fractional:
            kind: Literal[1.5]
        @dataclass
        class Nested:
            names: list[Annotated[str, alias('name')]]
        @dataclass
        class Clash:
            first: Annotated[str, alias('second')]
            second: str
        @dataclass
        class Twice:
            text: Annotated[str, doc('one'), doc('two')]
        @dataclass
        class Unset:
            counted: int = dataclasses.field(default=0, init=False)
        for t in (Nullable, Fractional, Nested, Clash, Twice, Unset):
            try:
                declare(t)
                print('declared', t)
            except TypeError as e:
                print(e)
        "#);
    assert_eq!(
        out,
        "outrig.schema cannot take Literal['a', None] (field 'kind' of Nullable), but a Literal \
         takes only str, int and bool values; for null, write Literal[...] | None\n\
         outrig.schema cannot take Literal[1.5] (field 'kind' of Fractional), but a Literal \
         takes only str, int and bool values\n\
         outrig.schema cannot take Annotated[str, alias('name')] (field 'names' of Nested), but \
         alias() names a field's key, and is only allowed in the annotation of a dataclass \
         field itself\n\
         outrig.schema cannot take Clash (field 'second' of Clash), whose JSON key 'second' is \
         already field 'first''s\n\
         outrig.schema cannot take Annotated[str, doc('one'), doc('two')] (field 'text' of \
         Twice), which has more than one doc()\n\
         outrig.schema cannot take Unset (field 'counted' of Unset), which is init=False, so \
         decoding cannot set it\n"
    );
}

/// A dataclass made at runtime has no source; it declares from its fields
/// alone, decodes, and renders.
#[test]
fn a_dataclass_built_at_runtime_declares_decodes_and_renders() {
    let out = run(r#"
        import inspect
        Made = dataclasses.make_dataclass('Made', [
            ('kind', Annotated[Literal['a', 'b'], doc('Which one.')]),
            ('tags', Annotated[list[str], alias('the-tags')]),
            ('count', int, dataclasses.field(default=0)),
        ], frozen=True)
        try:
            inspect.getsource(Made)
        except (OSError, TypeError):
            print('no source')
        made = declare(Made)
        print(made.decode({'kind': 'a', 'the-tags': ['x']}))
        print(problems({'kind': 'c', 'tags': []}, made))
        print(json.dumps(made.json_schema()))
        "#);
    assert_eq!(
        out,
        "no source\n\
         Made(kind='a', tags=['x'], count=0)\n\
         $.kind: expected one of \"a\", \"b\", not \"c\"; $.tags: unknown key; that field's key \
         is \"the-tags\"; $.the-tags: missing required key\n\
         {\"type\": \"object\", \"properties\": {\"kind\": {\"description\": \"Which one.\", \
         \"enum\": [\"a\", \"b\"]}, \"the-tags\": {\"type\": \"array\", \"items\": {\"type\": \
         \"string\"}}, \"count\": {\"type\": \"integer\"}}, \"required\": [\"kind\", \
         \"the-tags\"], \"additionalProperties\": false}\n"
    );
}

/// A value decodes against a union when exactly one member takes it, or when
/// every member that does builds the same value; otherwise the problem says
/// which. A refusal is worded by the one member of the value's kind, when
/// there is one.
#[test]
fn a_union_decodes_only_what_one_member_takes() {
    let out = run(r#"
        @dataclass(frozen=True)
        class A:
            x: int
        @dataclass(frozen=True)
        class B:
            x: int
        @dataclass(frozen=True)
        class C:
            y: str
        print(problems({'x': 1}, declare(A | B)))
        print(declare(A | C).decode({'y': 'z'}), declare(A | C).decode({'x': 1}))
        either = declare(int | float)
        print(repr(either.decode(3)), repr(either.decode(3.5)))
        print(problems({'x': 'one'}, declare(Optional[A])))
        print(problems([1], declare(str | int)))
        print(problems({'z': 1}, declare(A | C)))
        "#);
    assert_eq!(
        out,
        "$: is ambiguous: it decodes as each of A, B\n\
         C(y='z') A(x=1)\n\
         3 3.5\n\
         $.x: expected an integer, not a string\n\
         $: expected a string or an integer, not an array\n\
         $: matches none of A | C; $.z: as A: unknown key; $.x: as A: missing required key; \
         $.z: as C: unknown key; $.y: as C: missing required key\n"
    );
}

/// A value only one member of a union takes is that member's, without
/// comparing it to anything: a dataclass whose `__eq__` raises, or never
/// answers equal, decodes as `T | None` as it does as `T`.
#[test]
fn a_union_takes_its_one_members_value_uncompared() {
    let out = run(r#"
        @dataclass(eq=False)
        class Touchy:
            x: int
            def __eq__(self, other):
                raise AssertionError('compared')
        @dataclass(eq=False)
        class Unequal:
            x: int
            def __eq__(self, other):
                return False
        for t in (Touchy, Unequal):
            print(declare(t).decode({'x': 1}).x, declare(t | None).decode({'x': 1}).x,
                  declare(Optional[t]).decode(None))
        "#);
    assert_eq!(out, "1 1 None\n1 1 None\n");
}

/// Scalars match by JSON kind exactly: `true` is not `1` and `1` is not
/// `true`, while an integer passes as a float and stays an integer.
#[test]
fn scalars_and_literals_match_by_kind() {
    let out = run(r#"
        print(problems(True, declare(int)))
        print(problems(1, declare(bool)))
        print(repr(declare(float).decode(2)), problems(float('nan'), declare(float)))
        print(problems(True, declare(Literal[1])))
        print(problems(1, declare(Literal[True])))
        print(problems('1', declare(Literal[1])))
        "#);
    assert_eq!(
        out,
        "$: expected an integer, not true\n\
         $: expected true or false, not an integer\n\
         2 $: expected a finite number\n\
         $: expected one of 1, not true\n\
         $: expected one of true, not 1\n\
         $: expected one of 1, not \"1\"\n"
    );
}

/// `encode` is the inverse of `decode`, under the same keys, and refuses an
/// instance that is not of its type, naming where.
#[test]
fn encode_gives_what_decode_takes_back() {
    let out = run(r#"
        result = review.decode(example())
        print(review.encode(result) == EXAMPLE, review.decode(review.encode(result)) == result)
        bad = dataclasses.replace(result, verdict=Verdict(status='maybe', message='m'))
        try:
            review.encode(bad)
        except TypeError as e:
            print(e)
        "#);
    assert_eq!(
        out,
        "True True\n\
         1 problem encoding ReviewResult:\n  \
         $.verdict.status: expected Literal['approve', 'comment', 'reject-changes'], not \
         'maybe'\n"
    );
}

/// `help(outrig.schema)` documents the way to declare, decode, encode and
/// render, within what a kernel's `help()` shows.
#[test]
fn help_documents_the_schema() {
    let out = run(r#"
        import pydoc
        text = pydoc.render_doc(schema, renderer=pydoc.plaintext)
        names = ('declare(t)', 'decode(self, value)', 'encode(self, value)', 'json_schema(self)',
                 'text(self)', 'doc(text)', 'alias(name)', 'class DecodeError', 'class Problem',
                 'PROBLEMS_MAX = 20')
        print([name for name in names if name not in text], len(text) <= 8 * 1024)
        "#);
    assert_eq!(out, "[] True\n");
}

/// `help(outrig.skills)` says what a skill is to the agent within what a kernel's `help()` shows,
/// and on the host, where no session serves skills, each function says so.
#[test]
fn help_documents_skills_and_the_host_has_none() {
    let out = run(r#"
        import pydoc
        text = pydoc.render_doc(outrig.skills, renderer=pydoc.plaintext)
        names = ('read(name)', 'reload(name)', 'catalog()', 'class SkillError', 'class Origin',
                 'class Skill', 'importlib.resources.files', 'requires-python')
        print([name for name in names if name not in text], len(text) <= 8 * 1024)
        for call in (lambda: outrig.skills.read("a"), outrig.skills.catalog):
            try:
                call()
            except RuntimeError as e:
                print(e)
        "#);
    assert_eq!(
        out,
        "[] True\n\
         outrig.skills reaches the session's skills, which only an agent's kernel has, and this \
         Python is not one\n\
         outrig.skills reaches the session's skills, which only an agent's kernel has, and this \
         Python is not one\n"
    );
}

/// `@outrig.agent` declares on the host as it does in a kernel -- the
/// maintainer's `analyze_code` against `ReviewResult`, checked as written --
/// and a call there says that it needs an agent's kernel, sending nothing.
#[test]
fn a_declared_function_declares_on_the_host_and_a_call_says_it_needs_a_kernel() {
    let out = run(r#"
        import inspect
        @outrig.agent
        async def analyze_code(section: str) -> ReviewResult:
            """Review one section of a unified diff for correctness and safety."""
            ...
        print(inspect.iscoroutinefunction(analyze_code), analyze_code.__outrig_agent__.qualname,
              analyze_code.__outrig_agent__.digest is None)
        try:
            analyze_code('diff')
        except RuntimeError as e:
            print(e)
        try:
            @outrig.agent
            async def nameless(section: str) -> ReviewResult:
                ...
        except TypeError as e:
            print(e)
        "#);
    assert_eq!(
        out,
        "False analyze_code True\n\
         a call to __main__.analyze_code runs in an agent's kernel, where OutRig runs agent \
         code, and this Python is not one\n\
         __main__.nameless has no docstring, which is what a declaration tells its child\n"
    );
}
