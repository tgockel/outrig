# 0003-26 -- A decorated function is typed agent work

## Context

`0003-25` gives agent code a child, a submission and a handle. `typed-agents.md` adds the form most
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
parallel, bounded by `0003-25`'s depth limit and token budget. Calling sends the work at once and
returns `0003-25`'s handle: `await analyze_code(s)` reads as one line, and `h = analyze_code(s)`
is the same call with the handle kept for progress, status or cancellation. An agent class's
method returns a handle the same way (`0003-29`), so every typed call follows one rule.

The body is never run, and a body with logic in it would do nothing without saying so. The
declaration is therefore checked: a docstring and `...`, read with `ast` where the source is
available. It is not available for a function declared inside a submission: `_compile` in
`interpreter.py` compiles every submission under the one name `"<execution>"`, which
`inspect.getsource` cannot read back.

Validation has two layers. Shape is always checked, by `0003-24`'s decoder. An application check,
`validate=`, is optional, runs in the parent, and must be pure, because each repair runs it again.

## Goal

A typed agent call is declared as a function and called as one, and its handle awaited; its result
is validated before the caller sees it.

## Deliverables

- **`@outrig.agent`**, bare or with keywords: `validate=` and `model=`.
- **The body check.** At decoration the source is parsed with `ast`, and a body that is anything
  but a docstring followed by `...` raises `TypeError` naming the function. A missing docstring is
  refused too, since the docstring is the child's instructions. Where the source is unavailable,
  per fork 1.
- **Declaration checks.** Hints are resolved in the function's module namespace, and each
  parameter's annotation and the return annotation are declared through `0003-24`, so a type
  outside the subset fails at decoration.
- **Instructions**: the docstring, then a generated manifest of the inputs -- each name, its type
  and a bounded preview -- then the result's schema text. The docstring is never passed through
  `str.format`, so its braces stay braces and no argument text is put into it; the child reads each
  value from its variable.
- **One call form.** `analyze_code(...)` binds its arguments to the signature, checks each against
  its declared type, spawns a new work child, submits the inputs, and returns `0003-25`'s handle
  with the work under way; `await` on the handle gives the result or raises the failure. The child
  is released however the call settles, whether the handle was awaited or not -- a release that
  closes the child's subtree, its own children included, as `0003-25`'s does.
  `inspect.iscoroutinefunction(analyze_code)` is false, and `help(analyze_code)` states that a
  call returns a handle.
- **`validate=`**: a function called in the parent with the decoded result and the call's
  arguments, returning a list of problems, an empty list accepting. It is called once per attempt,
  and its problems reach the child as decoding problems do, through `0003-25`'s completion call,
  counting against the same attempt limit. It is documented as pure -- no hosted calls, writes or
  model calls -- because it runs again on every attempt; nothing enforces that.
- **`model=`** names a configured model or alias, resolved as `0003-25`'s `spawn` resolves one; an
  unknown name fails with the configured names listed.
- **Events**: `agent.call.started` and `agent.call.settled` around each call, the wrapper over
  `0003-25`'s request events, with the declaration's qualified name -- its `__module__` and
  `__qualname__` -- and its module's digest on the start event, as `typed-agents.md` lists them.
  The digest is a hash of the declaring module's source, or of the submission's for a function
  declared in one.
- What a validator that raises does, per fork 2.

## Acceptance

- **The maintainer's `analyze_code` example from `typed-agents.md` returns a `ReviewResult`**
  equal to the decoded example JSON, against a mock model whose child completes with it.
- **The validator runs once per attempt.** One that rejects the first attempt is called twice, the
  second attempt is accepted, and the child's executions before the first attempt ran once each.
- **A body with logic is refused** at decoration with `TypeError`: a statement before or after the
  docstring, a `return`, a body of `pass`, and no docstring.
- **A function declared inside a submission is accepted** when its body is a docstring and `...`,
  and refused otherwise, by the same check as a module's (with fork 1's recommendation).
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
- `analyze_code(s)` returns a handle with `0003-25`'s rules,
  `inspect.iscoroutinefunction(analyze_code)` is false, and `help(analyze_code)` says a call returns
  a handle.
- **An argument of the wrong type is refused before anything is sent**: `analyze_code(1)` raises
  `TypeError` in the parent, and the host's record shows no spawn.
- **Cancelling a waiter leaves the work running; `h.cancel()` cancels the call and releases its
  child.** A task cancelled while it awaits `analyze_code(s)`'s handle leaves the work to settle
  later, and a new await returns its result; after `h.cancel()` the handle reads cancelled and the
  host's record shows the child released.
- A validator that raises settles the call with its exception, and the child is not prompted
  again (with fork 2's recommendation).
- An unknown `model=` fails with the configured names.
- `crates/outrig/public-api.txt` changes only by the start event's fields for the declaration's
  qualified name and its module's digest.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Source for a function declared inside a submission -- Recommended: register each submission's
   source in `linecache`.** Compiling each execution under a name of its own, such as
   `<execution 7>`, and putting its lines in `linecache` lets `inspect.getsource` read a
   submission's functions, so one AST check covers modules and submissions alike; tracebacks gain
   the submitted lines they do not show today. `_format_error` finds a submission's frames by
   `co_filename == "<execution>"` and has to match the new names. The alternative, checking
   bytecode where source is missing, is weaker: a docstring followed by `...`, by `pass` or by
   `None` compiles to the same bytecode, and what a body compiles to differs between CPython
   versions.
2. **What a validator that raises does -- Recommended: the call settles with that exception, and
   the child is not charged an attempt.** A validator that raises instead of returning problems is
   a defect in the parent's code, which another attempt by the child cannot fix.

## Dependencies

- **Hard: `0003-25`.** Each call is that task's spawn, submission and release, and the handle a
  call returns is its handle.

## See also

- `plan/phase/0003-python/typed-agents.md` -- the declaration, its inputs, the call form, and
  validation and repair.
- `plan/phase/0003-python/work.md` -- the handle's rules.
- `crates/outrig/src/python/interpreter.py` -- `_compile` and `_format_error`, which fork 1
  changes.
