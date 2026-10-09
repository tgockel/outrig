# 0003-27 -- A decorated function is typed agent work

## Context

`0003-26` gives agent code a child, a submission and a handle. `typed-agents.md` adds the form most
workflows want, a declaration that reads as a typed async function:

```python
@outrig.agent
async def analyze_code(section: str) -> ReviewResult:
    """Review one section of a unified diff for correctness and safety. ..."""
    ...
```

The parameters are the inputs, the docstring is the instructions, and the return annotation is the
result type. Planning settled the rest. Each call gets a new child, released once the call
settles, so nothing carries over between calls and `asyncio.gather` over many calls runs them in
parallel, bounded by `0003-26`'s limits: depth, the token budget, `children-max` on resident
children and `model-concurrency-max` on model requests in flight. Calling sends the work at once
and returns `0003-26`'s handle: `await analyze_code(s)` reads as one line, and `h =
analyze_code(s)` is the same call with the handle kept for progress, status or cancellation. An
agent class's method returns a handle the same way (`0003-30`), so every typed call follows one
rule. A call whose child ends two rounds without completing settles `CompletionRejected`
(`0003-26`'s one-shot end, decided 2026-10-05), so a `gather` never waits forever on a child that
stopped.

The body is never run, and it is never checked either: `...` is the convention, and a body with
statements in it is ignored. Decided on 2026-10-02 after the design critique, replacing a check
that parsed the body with `ast` and refused anything but a docstring and `...`. That check needed
source, which a function declared inside a submission does not have -- `_compile` in
`interpreter.py` compiles every submission under the one name `"<execution>"` -- and the decorator
already says what a call does. What the decorator reads is the signature and the docstring. The
same decorator marks a request method on an agent class (`0003-30`), where `model=` is refused.

Validation has two layers. Shape is always checked, by `0003-25`'s decoder. An application check,
`validate=`, is optional, runs in the parent, and must be pure, because each repair runs it again.

## Goal

A typed agent call is declared as a function and called as one, and its handle awaited; its result
is validated before the caller sees it.

## Deliverables

- **`@outrig.agent`**, bare or with keywords: `validate=` and `model=`.
- **The body is ignored.** Nothing reads the function's source: the body is never run and never
  checked, so a function declared in a submission is declared as one in a module is. A missing
  docstring is refused with `TypeError` naming the function, since the docstring is the child's
  instructions. `help(analyze_code)` says that the body is not run.
- **Declaration checks.** Hints are resolved in the function's module namespace, and each
  parameter's annotation and the return annotation are declared through `0003-25`, so a type
  outside the subset fails at decoration.
- **Instructions**: the docstring, then a generated manifest of the inputs -- each name, its type
  and a bounded preview -- then the result's schema text. The docstring is never passed through
  `str.format`, so its braces stay braces and no argument text is put into it; the child reads each
  value from its variable.
- **One call form.** `analyze_code(...)` binds its arguments to the signature, checks each against
  its declared type, spawns a new work child, submits the inputs, and returns `0003-26`'s handle
  with the work under way; `await` on the handle gives the result or raises the failure. The child
  is released however the call settles, whether the handle was awaited or not -- a release that
  closes the child's subtree, its own children included, as `0003-26`'s does.
  `inspect.iscoroutinefunction(analyze_code)` is false, and `help(analyze_code)` states that a
  call returns a handle.
- **`validate=`**: a function called in the parent with the decoded result and the call's
  arguments, returning a list of problems, an empty list accepting. It is called once per attempt,
  and its problems reach the child as decoding problems do, through `0003-26`'s completion call,
  counting against the same attempt limit. It is documented as pure -- no hosted calls, writes or
  model calls -- because it runs again on every attempt; nothing enforces that.
- **`model=`** names a configured model or alias, resolved as `0003-26`'s `spawn` resolves one; an
  unknown name fails with the configured names listed.
- **A child that ends two rounds without completing fails the call**, by `0003-26`'s one-shot
  end, which the wrapper inherits: a round that ends without a completion that passed earns one
  round asking for the result by id, and if that round ends the same way the handle settles with
  `CompletionRejected` whose message says the child ended without completing, and the child is
  released as it is for every settlement. The invalid-completion attempt limit, 3, is separate.
- **Events**: `agent.call.started` and `agent.call.settled` around each call, the wrapper over
  `0003-26`'s request events, with the declaration's qualified name -- its `__module__` and
  `__qualname__` -- and its module's digest on the start event, as `typed-agents.md` lists them.
  The digest is a hash of the declaring module's source, or of the submission's for a function
  declared in one.
- What a validator that raises does, per fork 1.

## Acceptance

- **The maintainer's `analyze_code` example from `typed-agents.md` returns a `ReviewResult`**
  equal to the decoded example JSON, against a mock model whose child completes with it.
- **The validator runs once per attempt.** One that rejects the first attempt is called twice, the
  second attempt is accepted, and the child's executions before the first attempt ran once each.
- **The body is ignored.** Declarations whose bodies are a docstring and `...`, a docstring and
  `pass`, and a docstring followed by `raise AssertionError` are all accepted, and a call on each
  spawns a child and never runs the body: no `AssertionError` is raised anywhere, and the host's
  record shows no execution of it. A function with no docstring is refused at decoration with
  `TypeError` naming it.
- **A function declared inside a submission is declared like one in a module**: decorated in a
  submission, with no source on disk, it is accepted, a call on it returns a handle that settles
  with the result, and its start event carries the submission's digest.
- **A `gather` of three calls creates three children, each gone once settled**: the host's record
  shows three spawns and three releases, and the parent holds no child afterward. A call whose
  child spawned a child of its own leaves neither once it settles.
- **The start event names the declaration**: a call's start event carries `analyze_code`'s
  `__module__` and `__qualname__` and the digest of its module's source, and the digest differs
  after that source changes.
- **The docstring reaches the child as written.** A docstring holding `{section}` and `{}` appears
  in the child's instructions unchanged, and an argument whose text contains braces appears only in
  the input manifest's preview, never inside the docstring.
- **A call never awaited still runs, and its child is released when it settles**: `analyze_code(s)`
  with its handle dropped shows one spawn, the settled event and one release in the host's record,
  with no await anywhere.
- `analyze_code(s)` returns a handle with `0003-26`'s rules,
  `inspect.iscoroutinefunction(analyze_code)` is false, and `help(analyze_code)` says a call returns
  a handle.
- **An argument of the wrong type is refused before anything is sent**: `analyze_code(1)` raises
  `TypeError` in the parent, and the host's record shows no spawn.
- **Cancelling a waiter leaves the work running; `h.cancel()` cancels the call and releases its
  child.** A task cancelled while it awaits `analyze_code(s)`'s handle leaves the work to settle
  later, and a new await returns its result; after `h.cancel()` the handle reads cancelled and the
  host's record shows the child released.
- A validator that raises settles the call with its exception, and the child is not prompted
  again (with fork 1's recommendation).
- **A child that ends two rounds without completing fails the call.** Against a mock that ends
  the submission's round and the prompted round without completing, the handle settles with
  `CompletionRejected` whose message says the child ended without completing, the mock receives
  no third call, the record shows the child released and no `agent.request.invalid`, and
  `except outrig.AgentError` around the await catches it.
- **A call past `children-max` fails at once.** With the session at the cap, `analyze_code(s)`
  returns a handle that settles with `AgentLimitReached`, the record shows no spawn for it, and
  releasing one child lets the next call run.
- An unknown `model=` fails with the configured names.
- `crates/outrig/public-api.txt` changes only by the start event's fields for the declaration's
  qualified name and its module's digest.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What a validator that raises does -- Recommended: the call settles with that exception, and
   no attempt is counted against the child.** A validator that raises instead of returning
   problems is a defect in the parent's code, which another attempt by the child cannot fix.

The fork this task had on 2026-10-01 -- how to read the source of a function declared inside a
submission, with registering each submission in `linecache` recommended -- is gone with the body
check; no source is read.

## Dependencies

- **Hard: `0003-26`.** Each call is that task's spawn, submission and release, and the handle a
  call returns is its handle.

## See also

- `plan/phase/0003-python/typed-agents.md` -- the declaration, its inputs, the call form, and
  validation and repair.
- `plan/phase/0003-python/work.md` -- the handle's rules, and the session-wide limits.
- `plan/phase/0003-python/agent-classes.md` -- the same decorator on a method of an agent class.
