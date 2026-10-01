# Typed agents

`@outrig.agent` declares a piece of model-driven work as an `async def`. Its parameters are the
inputs, its docstring is the instructions, and its return annotation is the type of the result.
Calling it sends the work to a fresh child agent and returns a handle; awaiting the handle gives a
result checked against that type, so ordinary Python can fan such calls out, gather them, and
combine what they return -- usually from a skill (`skills.md`).

It is built on `work.md`'s explicit child API, which ships beside it. `0003-24` provides
`import outrig` and `outrig.schema`; `0003-25` the children, their handles, completion and limits;
and `0003-26` the decorator. The design was settled in planning on 2026-09-30, and the call form
was changed on 2026-10-01 when the second declared form was added: `agent-classes.md`, a class
whose instance is one long-lived child answering typed requests. On 2026-10-02, after the design
critique, the body check was dropped in favor of ignoring the body, the trailing round stopped
counting an attempt, and two session-wide limits were added ("Placement and limits"). This page is
the function form, one fresh child per call.

## The declaration

```python
@outrig.agent
async def analyze_code(section: str) -> ReviewResult:
    """Review one section of a unified diff for correctness and safety.

    The section is available to you as the variable `section`. Report each
    problem you find under a short problem-forward kebab-case id.
    """
    ...
```

**The body is ignored: never run and never checked.** `...` is the convention for it, and the
decorator reads the signature and the docstring and nothing else, so a declaration needs no
source: a function defined in a submission, which is compiled under `<execution>` and has no
file, is declared exactly as one in a module or a skill is. A body with statements in it does
nothing, and the runtime says nothing about it; the decorator is what says that a call sends work
to a child, and `help()` on the declaration repeats it. A missing docstring is refused with
`TypeError`, since the docstring is the instructions. Decided on 2026-10-02, replacing a check
that parsed the body with `ast` and refused anything but a docstring and `...`: the check needed
source, which a submission's functions did not have, and it guarded against a mistake the
decorator's presence already makes visible. Ignoring the body is not a decision that it may never
mean anything ("Open questions").

**Annotations resolve where the function was defined**, in its module's namespace and inside the
interpreter that defined the classes, and a name that does not resolve fails the declaration
rather than the first call. Every parameter's type, and the result's, must be one `outrig.schema`
takes: the serializable subset of `messages.md`, plus `Literal` and `Annotated`. The additions are
`0003-24`'s, and the maintainer's schema below needs both. The subset as `0003-08` built it does not
cover `Literal`: checked against the payload, its contract check refuses `ReviewResult` at
`Verdict.status`.

## Inputs are values

Each argument is checked against its parameter's type before any child starts, and is bound into
the child's namespace as a variable of the parameter's name. The child's instructions are the
docstring, a generated **input manifest** -- each input's name, type and a bounded preview -- and
the result type's schema text.

Nothing is interpolated into the docstring, which is not passed through `str.format`. Braces are
common in code-review prose and would break it, and putting untrusted diff text straight into
instructions makes prompt injection easier. The child reads an input's exact value in code, so a
50 KB section never has to fit into a prompt.

A hosted object is not in the subset, so it cannot be an input. It does not need to be one: a
child has the session's bindings already ("Placement and limits").

## The maintainer's result type

The maintainer's schema has four top-level JSON keys: `verdict`, `notes`, `pre-existing-issues` and
`questions`. `notes` and `pre-existing-issues` both map ids to the same `Note` shape; `questions`
maps ids to `Question`, and a question id need not match any note. Ids are problem-forward kebab
case: `unbounded-retry-loop`, not `fix-retry`. The descriptive text below is the maintainer's
wording, and the requirement is that it reaches the child's schema intact.

```python
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Annotated, Literal, Optional

import outrig
from outrig.schema import alias, doc

Status = Literal["approve", "comment", "reject-changes"]
Severity = Literal["blocker", "high", "medium", "low"]
DuplicateStatus = Literal["duplicate", "not-found", "unknown"]


@dataclass(frozen=True, kw_only=True)
class Verdict:
    status: Annotated[Status, doc(
        "Overall outcome. approve: the change can merge as is. comment: "
        "remarks the author should read, none of which block merging. "
        "reject-changes: the change should not merge in its current form.")]
    message: Annotated[str, doc(
        "Brief rationale based only on actionable PR findings.")]


@dataclass(frozen=True, kw_only=True)
class Note:
    location: Annotated[str, doc(
        "`path/to/file.ext:start-end`. The location or range of locations "
        "this note refers to.")]
    severity: Annotated[Severity, doc(
        "blocker: likely data loss/corruption, critical security issue, "
        "widespread outage, or a change that fundamentally cannot work. "
        "high: serious correctness, compatibility, concurrency, or "
        "reliability failure in a normal supported scenario. "
        "medium: real defect with narrower conditions or limited blast "
        "radius. "
        "low: minor but concrete defect worth fixing; not a stylistic "
        "preference.")]
    message: Annotated[str, doc(
        "Self-contained defect explanation and correction. For "
        "pre-existing issues, explicitly state that base behavior is "
        "unchanged.")]
    duplicate_status: Annotated[DuplicateStatus, alias("duplicate-status"), doc(
        "Whether this is already tracked elsewhere. duplicate: an existing "
        "report matches it. not-found: you searched and found none. "
        "unknown: you did not or could not search.")]
    duplicate_reference: Annotated[Optional[str], alias("duplicate-reference"), doc(
        "URL or stable identifier, or null.")]


@dataclass(frozen=True, kw_only=True)
class Question:
    location: Annotated[Optional[str], doc(
        "`path/to/file.ext:start-end` or None for a question not tied to "
        "a specific source location.")]
    message: Annotated[str, doc(
        "The unresolved question and why its answer matters.")]


@dataclass(frozen=True, kw_only=True)
class ReviewResult:
    verdict: Annotated[Verdict, doc("The overall review decision.")]
    notes: Annotated[dict[str, Note], doc(
        "Problems introduced or exposed by this change. Keys in the map "
        "are problem-forward kebab-case IDs. Titles and identifiers must "
        "describe the defect, not an imagined solution. Prefer "
        "failed-probe-overwrites-working-credential and avoid "
        "keep-old-credential-after-failed-probe.")]
    pre_existing_issues: Annotated[dict[str, Note], alias("pre-existing-issues"), doc(
        "Problems noticed in surrounding code that this change did not "
        "introduce. Same map structure as notes: keys are problem-forward "
        "kebab-case IDs that name the defect, not a fix.")]
    questions: Annotated[dict[str, Question], doc(
        "Questions for the author, keyed by a problem-forward kebab-case "
        "id. Question IDs are independent of notes and need not match "
        "any of them.")] = field(default_factory=dict)
```

All four classes are `@dataclass(frozen=True, kw_only=True)`. `kw_only=True` is what lets
`Note.duplicate_status`, `Note.duplicate_reference`, `Question.location` and `Question.message` be
required without Python's field-ordering rule forcing a default onto any of them, and
`field(default_factory=dict)` keeps `ReviewResult.questions` from sharing one dict between
instances. Every field is required except `ReviewResult.questions`, whose default matches the
maintainer's schema treating an absent `questions` map as empty.

**Two fields are required-but-nullable**: `Note.duplicate_reference` and `Question.location`.
Their type includes `None`, and they are still not optional: the JSON key must be present, and its
value may be a string or `null`. `Note.duplicate_status` is different, a required three-way enum
that is never nullable. A child that has not searched for duplicates emits the string `"unknown"`;
the runtime never infers `"unknown"` from a missing key, and never treats `"unknown"` and `null`
as the same. Only a field's type says whether `null` is legal for it, and only a default says it
may be omitted.

An example completion:

```json
{"verdict": {"status": "reject-changes",
             "message": "The retry loop can spin forever on a permanent error."},
 "notes": {"unbounded-retry-loop": {
    "location": "src/net/retry.py:42-42", "severity": "high",
    "message": "retry() never gives up on a permanent 4xx, so callers hang.",
    "duplicate-status": "not-found", "duplicate-reference": null}},
 "pre-existing-issues": {"missing-client-timeout": {
    "location": "src/net/client.py:10-10", "severity": "medium",
    "message": "The HTTP client has no default timeout. Base behavior is unchanged by this change.",
    "duplicate-status": "duplicate", "duplicate-reference": "#123"}},
 "questions": {"intended-retry-budget": {
    "location": null,
    "message": "Is there a product requirement for the total retry time?"}}}
```

Every `Note` carries all five fields, with `duplicate-reference` an explicit `null` on the
`not-found` note rather than omitted. The question's location is an explicit `null`. The
pre-existing note's message says that base behavior is unchanged, as `Note.message`'s text asks,
and locations use a `start-end` range even for one line. `reject-changes` with only a `high` note
is valid: the schema does not tie a verdict to a severity.

## `outrig.schema`

`outrig.schema` provides two markers for `typing.Annotated`: `doc(text)`, a field's description,
and `alias(name)`, its JSON key.

**Field documentation comes from `Annotated`**, read with
`typing.get_type_hints(cls, include_extras=True)`. A string literal after a field cannot serve:
Python evaluates it and discards it, and recovering it means parsing source, which a dynamically
built class does not have. `field(metadata=...)` would work too, and was not chosen because it
forces a `field()` call onto every documented field.

**A JSON key differs from its field's name only through `alias()`.** There is no conversion
between hyphens and underscores in either direction.

**Decoding is strict.**

- A key that matches no declared JSON key is rejected, not dropped -- including
  `pre_existing_issues` where `alias("pre-existing-issues")` is declared.
- `null` is accepted only where the field's type includes `None`.
- A required-but-nullable field must be present; omitting it is a missing required field.
- An omitted field takes its dataclass default, from `default` or `default_factory`, and from
  nowhere else. Only `ReviewResult.questions` has one here.
- Every problem carries the JSON path it was found at, such as
  `$.notes['unbounded-retry-loop'].severity`, so a repair message can name it.

**The schema text** comes from the same declarations. A `Literal` becomes an `enum`; a dataclass
an object with `additionalProperties: false` and every field without a default in `required`;
`dict[str, Note]` an object whose `additionalProperties` is `Note`'s schema; and a `doc` text the
field's `description`.

## Completion is a Python call

**Decided in planning (2026-09-30): the child completes by calling
`await runtime.complete(value)` in its own code**, the spelling being `0003-25`'s. The maintainer's
reason: the result can be built from objects that live only in Python and never pass through the
model. A child that found its problems with code completes with the structure that code built,
instead of the model writing it out a second time as the arguments of a tool call. The child's
only model tool stays `submit_python`.

A completion tool would not have provided enforcement either. A provider's strict mode cannot
express this schema: `notes` is a map, whose JSON Schema is an object with `additionalProperties`
set to a schema, and Anthropic's and OpenAI's strict modes both reject that. Checking the result
is OutRig's job whichever way the child hands it over.

Completion is not a message (`work.md`), and nothing a child sends on a channel is read as its
result. The value crosses as every value between agents does (`messages.md`): as data, decoded
strictly into new instances on the parent's side.

This is how a *work child* answers, and a decorated call's child is one. The other kind, a
*request child* -- an instance of an agent class (`agent-classes.md`) -- has no `runtime.complete`:
it receives each request as a delivery and answers it with `await d.reply(value)` or
`await d.fail(message)`.
A completion and a reply are decoded, repaired and limited by one path, and settle their handle
the same way (`work.md`, "A request settles once").

## Validation and repair

Validation has two steps.

1. **Shape**, always: strict decoding into the declared type, as above. It asserts what the type
   says and nothing more. The review schema does not require `reject-changes` to carry a
   `blocker` or `high` note, and does not require question ids to match note ids.
2. **Application policy**, optional: a validator the declaration names with `validate=`, which
   runs in the parent with the decoded result and the call's inputs, and returns repair messages,
   an empty list meaning accepted.

```python
def review_policy(result: ReviewResult, section: str) -> list[str]:
    """Repair messages; empty means accepted. No hosted calls, writes or model calls."""
    problems = []
    for table, notes in (("notes", result.notes),
                         ("pre-existing-issues", result.pre_existing_issues)):
        for key, note in notes.items():
            if note.duplicate_status == "duplicate" and not note.duplicate_reference:
                problems.append(f"{table}.{key}: duplicate-status is duplicate "
                                "but duplicate-reference is missing")
            if note.duplicate_status != "duplicate" and note.duplicate_reference:
                problems.append(f"{table}.{key}: duplicate-reference given "
                                "but duplicate-status is not duplicate")
    reviewed = changed_paths(section)                 # a diff helper ("A workflow")
    for key, note in result.notes.items():            # notes only
        if note.location.rsplit(":", 1)[0] not in reviewed:
            problems.append(f"notes.{key}.location {note.location!r} is not a "
                            "file in the reviewed section")
    return problems
```

`@outrig.agent(validate=review_policy)` on the declaration attaches it; the spelling is
`0003-26`'s. A validator receives the inputs so that it can check evidence against them: here,
that each note's location names a file the section changes. **The location check covers only
`notes`**, because pre-existing issues and questions may legitimately point outside the diff.

**A validator must be pure over its inputs.** Purity is what lets a repair redo only the final
answer: a validator with effects would repeat them on every attempt. It is a documented contract,
not an enforced sandbox.

**Repair.** When either step finds problems, `runtime.complete` raises them in the child's code,
bounded: each problem's JSON path, what is wrong, and the expected shape. The child corrects the
value and calls `complete` again. Nothing it ran before is run again -- its earlier executions,
hosted calls and model turns stand, and only the final answer is redone.

A child whose round ends without a completion that passed is prompted once more, in a trailing
round that asks for the result by the request's id. If that round ends without one too, the child
goes idle and the call stays open, its child resident, until `h.cancel()` ends it, a
`runtime.wait` timeout ends the caller's wait, or the session closes. Only a completion that
failed a check counts toward the per-request attempt limit, 3 by default (`work.md`); past it,
the call settles with `CompletionRejected`, which carries the last value submitted and its
problems. The trailing round counts nothing, and there is no wait budget: spending a round is not
a cost the limit exists to bound, since the token budget bounds spend, and a child's own long
waits happen inside an execution, where no round ends (`work.md`, "A request settles once"). A
workflow that fans out bounds its own waiting for that reason ("A workflow").

## Calling

```python
h = analyze_code(section)                 # the work is sent now; h is the handle
...
result = await h                          # an accepted ReviewResult, or the failure raised

result = await analyze_code(section)      # the same, on one line
```

**`analyze_code(section)` sends the work at once and returns the handle.** There is one call form.
The handle is `work.md`'s: `await h` gives the result or raises the failure, and re-awaiting gives
the same answer; `h.done()`, `h.result()` and `h.status` answer without waiting; `h.cancel()`
cancels the call and releases its child, while cancelling a waiter does not; `h.progress` is a
one-way endpoint yielding `Delivery` envelopes of what the child reports as progress; and
`h.future` is the shielded view `asyncio.wait` takes. `runtime.wait` accepts the handle directly,
and `asyncio.gather` accepts it as it accepts any awaitable, so `await analyze_code(section)`
still reads as one line and a `gather` over calls needs nothing else.

**The wrapper is not a coroutine function.** `inspect.iscoroutinefunction(analyze_code)` is
`False`, and `help(analyze_code)` states that a call returns a handle. A call nobody awaits still
runs: the child does its work, the result settles in the handle, and the child is released when
the call settles, awaited or not. A handle collected before it settled is recorded as an event and
not cancelled (`work.md`), and cancelling a bare `await analyze_code(section)` cancels only the
waiter; in both cases the work runs on to settlement, and the child is released then. Code that
wants an interrupted workflow to stop its children keeps the handles and cancels them
("A workflow").

**Each call gets a new child, which is released when the call's result settles** -- accepted,
rejected, cancelled, out of budget, or ended by the session closing (`lifecycle.md`). Underneath,
a call is `work.md`'s explicit API: spawn a child, submit one piece of work, and release the child
when the work settles. Releasing it closes the child's subtree the way a session close does: its
own children are released, its pending escalations are cancelled, its running execution is
interrupted, and its dispatched hosted calls finish or are reported `unknown` (`lifecycle.md`,
"Releasing a child"). An agent that wants one child to take several submissions in turn, keeping
its namespace and history between them, uses that API directly; one that wants a child to answer
many typed requests declares an agent class (`agent-classes.md`).

## Placement and limits

A child is a kernel in the same interpreter as its parent (`agent-placement.md`): a thread, a
session module, an event loop and an execution slot of its own. The host runs a round loop for
each child, as it does for the primary, and makes the child's model calls. A child has the same
bindings, since every kernel gets a stub for each (`hosted-objects.md`), and the same skill
catalog. That is sharing, not isolation: code in a child can reach its parent's objects, and
giving co-hosted agents different grants would be presentation only, so none is offered.

- **Model.** The parent's, unless `model=` names a model from the operator's configuration, an
  alias included -- a name, not a wire identifier. `plan/next/subagent-model-allowlist.md` covers
  constraining that choice.
- **Tools.** `submit_python`, as for every agent.
- **No user channel.** The user addresses the primary agent only (`messages.md`), so a child's
  messages come from its parent.
- **Children and model requests.** Two session-wide limits, both `0003-25`'s (`work.md`).
  `children-max` (default 64) bounds the children resident in the session, idle ones included; a
  call past it fails at once -- its handle settles with `AgentLimitReached` and no child is made
  -- rather than waiting for a place. `model-concurrency-max` (default 8) bounds the model
  requests in flight across the session: a permit is held per provider request and never while a
  parent's code awaits a child, so a call made with every permit held waits for one and a tree
  deeper than the permit count still completes. `run-new` does not read `subagent-width-max`,
  which stays `run`'s key at its default of 8; a scheduler for active work is
  `potential/resource-scheduling.md`.
- **Depth.** `subagent-depth-max` (default 3) bounds nesting, and a call from an agent at the limit
  fails at once, since waiting would not end.
- **Tokens.** A per-tree token budget bounds what a tree of children spends. A call whose child
  runs out before completing settles with a budget error, never with an empty result.
- **Spend.** A child's model usage is attributed to the child and added to its call, to the skill
  invocation the call ran under, and to the main agent's round. The boundary evaluator's usage is
  attributed separately (`boundary-policy.md`).

A child's thread is not the main thread, so the interrupt, a signal aimed at the main thread,
cannot reach it. A child whose code wedges is contained and not recoverable: its parent is told,
and the thread runs until the session ends (`runtime-protection.md`).

## A workflow

The review skill whose metadata `skills.md` shows, trimmed. The dataclasses, `analyze_code`
(declared with `validate=review_policy`) and `review_policy` above are in the same module, and the
diff helpers are summarized rather than shown.

```python
import asyncio
from dataclasses import dataclass, field
from typing import Literal

# split_sections(diff) -> list[str]: per-file sections, split at `diff --git` headers.
# changed_paths(section) -> set[str]: the paths its `+++ b/...` lines name.
# merge(results) -> ReviewResult: all four maps kept, colliding ids given a single-hyphen
#     suffix, the most severe status, and every section's verdict message.


def _classify_failure(exc: BaseException) -> str:
    """A fixed, bounded diagnostic label for a section failure.

    Deliberately never includes the exception's own message or args: those
    may quote section content, file paths or, for some hosted-call failures,
    credentials. Only the exception's type is reported, which is bounded and
    safe to log and to return to a caller.
    """
    return f"{type(exc).__module__}.{type(exc).__qualname__}"


@dataclass(frozen=True)
class SectionFailure:
    index: int
    paths: tuple[str, ...]
    error: str                                  # see _classify_failure; never repr(exc)


@dataclass(frozen=True)
class ReviewOutcome:
    """Typed workflow outcome. `result` is set only when every section succeeded."""
    kind: Literal["complete", "no-changes", "partial"]
    sections: int
    result: ReviewResult | None = None
    accepted: dict[int, ReviewResult] = field(default_factory=dict)
    failures: tuple[SectionFailure, ...] = ()


class IncompleteReviewError(RuntimeError):
    """Raised when some section reviews failed; carries accepted partial results."""
    def __init__(self, outcome: ReviewOutcome):
        super().__init__(f"{len(outcome.failures)} of {outcome.sections} "
                         "section review(s) did not produce an accepted result")
        self.outcome = outcome


async def main(repo, base: str = "origin/main", *paths: str,
               max_parallel: int = 4, allow_partial: bool = False,
               timeout: float | None = 1800.0) -> ReviewOutcome:
    if max_parallel <= 0:
        raise ValueError("max_parallel must be >= 1")   # Semaphore(0) never releases
    diff = repo.git.diff(base, "--", *paths)          # hosted, intercepted call
    sections = split_sections(diff)
    if not sections:
        return ReviewOutcome(kind="no-changes", sections=0)

    gate = asyncio.Semaphore(max_parallel)   # this workflow's bound on children at once
    handles = []

    async def review(section: str) -> ReviewResult:
        async with gate:
            h = analyze_code(section)         # sent now; the child is running
            handles.append(h)
            return await h

    try:
        async with asyncio.timeout(timeout):  # a child that never answers stays open
            outcomes = await asyncio.gather(*(review(s) for s in sections),
                                            return_exceptions=True)
    finally:
        for h in handles:                     # an interrupted gather cancels no child
            if not h.done():
                h.cancel()
    accepted = {i: o for i, o in enumerate(outcomes) if isinstance(o, ReviewResult)}
    failures = tuple(
        SectionFailure(i, tuple(sorted(changed_paths(sections[i]))), _classify_failure(o))
        for i, o in enumerate(outcomes) if not isinstance(o, ReviewResult))

    if not failures:
        return ReviewOutcome(kind="complete", sections=len(sections),
                             result=merge([accepted[i] for i in sorted(accepted)]),
                             accepted=accepted)
    partial = ReviewOutcome(kind="partial", sections=len(sections),
                            accepted=accepted, failures=failures)
    if allow_partial:
        return partial                        # no combined verdict is issued
    raise IncompleteReviewError(partial)
```

What it guarantees:

- **No changes is not approval.** An empty diff returns `kind="no-changes"` with no verdict; it
  does not invent an `approve`. When every section's review fails, the result is
  `IncompleteReviewError`, or with `allow_partial=True` a `partial` outcome whose `accepted` is
  empty.
- **A partial failure issues no combined verdict.** A combined `ReviewResult` is produced only
  when every section was accepted. A partial outcome keeps each accepted section's own result,
  verdict included, unchanged.
- **Nothing is dropped.** `merge` keeps all four maps, gives colliding ids a deterministic
  single-hyphen suffix rather than overwriting them, keeps `duplicate` notes where they are, takes
  the most severe status, and keeps every section's verdict message.
- **Bad input fails rather than hangs.** `max_parallel <= 0` is rejected before the semaphore is
  built, since a semaphore with no permits is never acquired.
- **No exception text escapes.** A failed section is reported by its exception's type alone. The
  exception's message may quote section text, a path or a credential.
- **One bound of its own.** `max_parallel` is this workflow's only limit on how many children run
  at once. The host's limits are the session's (`work.md`): without the semaphore a diff of 65
  sections would launch 64 children and fail the 65th with `AgentLimitReached`, and however many
  children run, at most `model-concurrency-max` of their model requests are in flight.
- **A child that never answers does not hold the review open.** A call whose child goes idle
  without completing stays open until something ends it ("Validation and repair"). `timeout`
  bounds the whole gather; when it ends the wait, the `finally` cancels every open handle, which
  releases each child, and `main` raises `TimeoutError`. `None` removes the bound.
- **An interruption stops the children.** Cancelling `main` cancels the gather's waiters, which
  cancels no child; the `finally` cancels every handle not yet settled, and each cancelled call
  releases its child.
- **The hosted call blocks.** `repo.git.diff` is a synchronous hosted call, so it holds this
  kernel's thread, and with it the agent's event loop, while it runs (`hosted-objects.md`).
  `await asyncio.to_thread(repo.git.diff, ...)` keeps the loop turning during a long one.
- **Nothing replays.** Aggregation is pure Python, so running `merge` again is safe. Running `main`
  again makes new model calls, and nothing runs it again on its own.

`return_exceptions=True` keeps one failed child from discarding the others' accepted results. Each
failure is a typed exception: `CompletionRejected`, the budget exhausted, the call cancelled, or
the session closing.

## States and events

The states one call passes through, as explanation rather than enum names:

- created and sent, then running, then completion submitted and validating, then accepted;
- back from validating to running when a check fails and repair is requested; from running to
  running again when a round ends without a completion and the child is prompted once more; and
  to open with the child idle when that trailing round ends without one, until the caller ends it;
- ending instead in rejected (the attempt limit reached), budget exhausted, cancelled, or closed
  with the session (`lifecycle.md`).

The runtime's events for a call, as candidates. Two are the wrapper's own; the rest are the
request family that every submission and channel request shares (`work.md`):

- `agent.call.started` -- the call's id, the child's id, the declaration's name and its module's
  digest, and the parent execution or skill invocation;
- `agent.request.sent` and `agent.request.received` -- the submission's id, and the round that
  took it up;
- `agent.request.replied` -- a completion was called, with a bounded preview of the value;
- `agent.request.invalid` -- the problems and the attempt's number;
- `agent.request.cancelled` -- `h.cancel()` ended it;
- `agent.request.settled` -- the outcome, and the rounds the request spanned;
- `agent.call.settled` -- the outcome, and the usage attributed to the child;
- the child's own model events, and its progress deliveries observed as every message between
  agents is (`observability.md`).

They describe the lifecycle the runtime owns, not the child's local Python. The child's hosted
calls produce ordinary boundary events (`boundary-policy.md`), with the call's correlation attached
as diagnostic context and not as a principal.

When the interpreter dies, the death is reported as an event and the session goes through closing
to its report, as it does in this phase whatever the agents were doing. No Python is left in which
to settle a call, so the owner reports every open call terminated, in its events and its shutdown
report (`lifecycle.md`). A later interpreter would
start with none of them (`plan/next/interpreter-restart-with-a-reset-notice.md`).

## Rejected alternatives

**Rejected: reusing children by default.** An earlier proposal left fresh against reused open, with
the choice on the declaration, the call site, or a pool. A reused child keeps its history and
namespace and saves a spawn, but its one execution slot makes concurrent calls queue or fail,
earlier sections affect how it reads later ones, and a failure can leave state the next call
inherits. A fresh child per call makes `gather` correct by construction and keeps a failure within
one call. Reuse stays available through the explicit API (`work.md`).

**Rejected: completing through a model tool.** The child would call a `complete` tool with the
result as its arguments, as a 0.2.x subagent calls `outrig__set_result`. The result could then be
only what the model writes out, never a structure the child's code built, and the provider could
not enforce this schema anyway.

**Rejected: a call that returns a coroutine, with `.start()` for the handle.** This page's first
design: `analyze_code(section)` returned a plain coroutine, so nothing ran until it was awaited
and a forgotten `await` spent nothing, and `.start(section)` returned the handle when one was
wanted. It was reversed on 2026-10-01 with the agent class (`agent-classes.md`). A class method
has to return a handle in any case -- its request goes to a child already running, and several
are in flight at once -- and keeping the coroutine form for functions would have meant teaching
two conventions for the same act. One rule instead: a call sends now and returns the handle. What
the coroutine form prevented -- a forgotten `await` spending tokens -- is now stated rather than
prevented: `help()` says the call returns a handle, and a call never awaited still runs and
releases its child when it settles.

**Rejected: `outrig.work(analyze_code, s)`.** It makes creating work explicit, and is verbose for
the common case. The explicit API is already the explicit form.

**Rejected: a cap on live children that a launch waits for.** The decision of 2026-09-30 had a
call past `subagent-width-max` wait for a slot. It was dropped on 2026-10-01 with the agent class:
an idle instance, kept for its context, would have held a slot for as long as its owner kept it,
and a `gather` would have waited on a limit unrelated to its work. A session with no bound on
resident children followed, and the design critique of 2026-10-02 named it; `children-max` and
`model-concurrency-max` are the replacement, counting idle children and refusing rather than
waiting (`work.md`, "Limits belong outside generated code").

## Open questions

- Whether a body may eventually mean something: a parent-side step before the call, or a fallback
  the body supplies. Ignoring it now does not decide against them.
- Explicit interpolation into the instructions: an opt-in such as `template=True`, or a marker the
  runtime delimits and escapes.
- A pattern for map keys -- the problem-forward kebab ids, `^[a-z0-9]+(-[a-z0-9]+)*$`, as
  `propertyNames` -- declared through a marker such as `keys()`. Providers enforce `required` more
  reliably than other constraints, so the decoder would check the pattern again either way.
- How a child builds its result: whether the result type's classes are bound into its namespace
  beside the inputs, so that it can construct an instance, or it completes with the JSON form the
  schema text describes. `0003-25` decides.
- What a validator that raises, rather than returning problems, does to the call. It is a defect
  in the parent's code, which argues for settling the call with it rather than counting an attempt
  against the child.
- Whether a hosted reference other than a binding -- a commit object the parent holds, say -- may
  ever be an input. The subset refuses it, and the child can reach the same object through its own
  binding.

## Unverified

- The maintainer's classes were run as written under the payload's Python, 3.13.15, with stand-in
  `doc` and `alias` markers: they construct, `get_type_hints(..., include_extras=True)` returns the
  markers, omitting a required-but-nullable field raises `TypeError`, and two instances do not share
  a `questions` default. `0003-08`'s contract check refusing `ReviewResult` was measured the same
  way. Nothing else on this page has been run.
- That Anthropic's and OpenAI's strict modes reject map-valued schemas was read from their
  documentation in planning, not sent to either.
- The workflow has not run. Its diff helpers handle the common `diff --git` shape only: renames,
  binary diffs, deletions and quoted names need a tested parser before it can be trusted on a real
  history.
