# 0003-41 -- Construction completes before attachment

## Context

`outrig.Agent` has no metaclass (`crates/outrig/src/python/outrig/_agents.py:490`). Its
`__init_subclass__` checks the declaration (`:527-529`, `_declare_class` `:581-631`), and its
`__init__` checks the class has a channel and `model` is a `str`, then runs `self._outrig =
_launch(self, inputs, model)` (`:531-542`). `_launch` is `_Instance`, bound as the interpreter
loads the package (`crates/outrig/src/python/interpreter.py:2439`), whose `__init__` validates
and copies the inputs, builds the child's record and schedules the launch on the constructing
kernel's loop with `call_soon_threadsafe` before it returns (`:5061-5095`). On the host, the
runner is spawned inside `make`, before the spawn is answered
(`crates/outrig/src/agent/children.rs:642`, `:374-384`), and the runner opens the orienting round
at once, since `orienting` bypasses the
summary rule (`crates/outrig/src/agent/runner.rs:69`, `:101`, `:130`). A subclass whose
`__init__` skips `super().__init__` is caught at its first call by `_runs` (`_agents.py:573-578`).

So a subclass `__init__` that raises after `super().__init__()` has already launched a child
whose orienting round can spend a model call before anything releases it, and nothing post-dates
the most-derived constructor: no hook runs after `__init__` returns.

OR-AG2 asks that no inference, orientation or request processing observes a half-initialized
most-derived instance; that a constructor or post-construction failure prevents inference and
produces startup diagnostics; and that mandatory attachment cannot depend on an application
override calling `super()`. Its accepted direction is a metaclass whose synchronous `__call__`
completes `super().__call__(*args, **kwargs)`, then runs an optional application hook, then
attaches; construction stays synchronous, and asynchronous startup surfaces through readiness.
OR-A1-3 names the landed line, `_agents.py:542`, as what must change, and asks that
`self._task = task` set after `super().__init__()` be present when the kernel first sees `self`
and that a constructor exception leave nothing launched and no kernel reserved.

The validation found this the concrete defect to cure (its reversal 1, fourth consequence; the
conflict check's C5) and noted the metaclass is compatible with `__init_subclass__` and
`_Marked.__set_name__`, which run at class creation, and should derive from `abc.ABCMeta` or
document the conflict. **This task reverses a recorded decision.** `agent-classes.md`,
"Construction and readiness", records: "`outrig.Agent.__init__` does two things. Synchronously,
it checks each input against the serializable subset ... Then it schedules `runtime.spawn(name,
prompt=<the class docstring>, model=model, inputs=inputs, requests=<the channels>)` as a task on
the constructing kernel's event loop and returns the instance." And `0003-34`'s `## Decisions`:
"A subclass whose `__init__` skips `super().__init__` raises `TypeError` saying so at its first
call." `task-agents.md`, "Construction completes before attachment", says what replaces them.

## Goal

An `outrig.Agent` subclass's most-derived constructor finishes, and its optional hook runs,
before OutRig attaches the instance; a constructor or hook that raises launches nothing and spends
no model round.

## Deliverables

- **The metaclass.** `outrig.Agent` gets a metaclass derived from `abc.ABCMeta` (or the task
  records why not). Its `__call__` runs `super().__call__(*args, **kwargs)`, which runs the
  most-derived `__init__` to its end; then the attach hook, when the class defines one, called on
  the type and given the instance, with no `super()` needed and nothing returned; then attachment:
  `instance._outrig = _launch(instance, inputs, model, facilities)`, where `facilities` is
  recorded here and given meaning by `0003-42`. On the host, `_launch` is `0003-35`'s, so a
  host-declared class constructs through the same sequence. `__init_subclass__` and
  `_Marked.__set_name__` are unchanged.
- **`Agent.__init__` records and launches nothing.** `__init__(self, *, model=None,
  facilities=None, **inputs)` keeps the checks it makes today -- a class with no channel, a
  `model` that is not a `str` -- and records the keywords for the metaclass. The `TypeError` for a
  skipped `super().__init__` is relaxed or kept per fork 1.
- **Attachment keeps `_Instance.__init__`'s synchronous checks.** An input outside the subset, a
  key `0003-42` refuses, a construction outside every execution: each still raises from the
  constructor call, with nothing launched (`agent-classes.md`, "Construction and readiness").
- **A request method called before attachment raises `TypeError`** saying construction has not
  finished: `_runs` finds `_outrig` unset during `__init__` and the hook.
- **Constructing an agent inside a constructor or hook** is refused or allowed per fork 2.
- **Failure leaves nothing.** A constructor or hook that raises has nothing of OutRig's to clean,
  since attachment has not happened; the exception is the constructor call's, and what the
  constructor did to the application's objects stands. `help(outrig.Agent)` documents the
  sequence, the hook, and that OutRig rolls nothing back.
- **The hook's spelling** per fork 3, documented in `help(outrig.Agent)` and `agent-classes.md`'s
  pointer.
- **Events and the close table.** No new event: a construction that raises in a kernel is in its
  execution's traceback (`exec.completed`), and launches nothing to event, as a refused launch
  records none (`agent-classes.md`, "Events"); `doc/reference/events.md` says so under
  `agent.instance.started`. No new close row: a construction is synchronous Python inside an
  execution, which the **Executions** rows cover. The task's `## Decisions` says both.
- **`## Decisions` records**: the reversal of the two quoted sentences and what replaces them;
  fork 1's fate of the `TypeError` rule; fork 2; the hook's name; the `abc.ABCMeta` base, or why
  not; and that the requirement's "without `super()`" is about the hook, which OutRig calls
  directly, while `super().__init__` is how a subclass passes the base its keywords.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test.

- A subclass whose `__init__` sets `self._task` after `super().__init__()` and then raises: the
  constructor call raises that exception; the record shows no `agent.spawn` line, no
  `agent.instance.started` and no model call. The same for a hook that raises after `__init__`
  returned, and for a hook that raises while `__init__` left state half set.
- A subclass whose hook sets state without `super()`: the hook runs after `__init__` returned
  (asserted by an attribute `__init__` set being readable in the hook) and before attachment
  (asserted by `self._outrig` unset in the hook); `agent.instance.started` follows; `ready()`
  settles.
- A request method called in `__init__` raises `TypeError` with no `agent.request.sent`.
- A subclass that skips `super().__init__`: per fork 1, constructs with no base keyword and
  `agent.instance.started` is recorded; or raises `TypeError` before attachment. `0003-34`'s test
  of the skipped call is rewritten to the fork taken.
- `class Base(outrig.Agent)` with an `abc.abstractmethod`: a subclass that implements it
  constructs; one that does not raises `TypeError` at the class call, before `__init__`, as `abc`
  raises it; a class that mixes in an unrelated metaclass raises `TypeError` at class creation.
- Constructing an agent inside another's `__init__`: per fork 2, refused with `RuntimeError`
  naming the rule and nothing launched; or allowed, with its child recorded.
- Every `0003-34` test passes unchanged but the one fork 1 rewrites; `0003-31`'s decorated calls
  are untouched (`agent` on a function takes no metaclass path).
- `help(outrig.Agent)` describes the sequence and the hook; `inspect.signature(Agent.__init__)`
  shows `facilities`.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **A subclass that skips `super().__init__` -- Recommended: allowed.** The metaclass attaches
   regardless, and the instance gets no base keyword: the parent's model, no `facilities=` (so its
   maker's view, `facilities.md`: the session-wide one under the CLI's primary, the parent's mapping
   under a dict-constructed one), no inputs. OR-AG2's sentence is then literally true, and the view
   is presentation, so nothing silent is widened in authority. The alternative keeps `0003-34`'s
   `TypeError`, raised by the metaclass before attachment when `Agent.__init__` did not run, which
   still spends no round and tells the author at once.
2. **Constructing an agent inside a constructor or hook -- Recommended: refused.** A context
   variable the metaclass sets around the call makes `_Instance.__init__` raise `RuntimeError`
   saying to construct children after attachment, in a method or in the child's own code. The
   alternative allows it and documents the consequence: under `0003-45` a child made in a
   constructor that then raises is held by its own kernel's `self`, so it lives until the session
   ends, which the refusal prevents.
3. **The hook's spelling -- Open.** Candidates: `__outrig_attach__`, a dunder OutRig owns that no
   request method or helper can collide with; `attach`, which would join the names a channel
   cannot take. Recommended: `__outrig_attach__`.

## Dependencies

- **Hard: `0003-39`.** The metaclass, the context variable and `_Instance`'s new signature are
  interpreter and package code.
- **Hard: `0003-34`.** The class this changes.
- **Soft: `0003-35`.** Its host-side `_launch` is what the metaclass calls on the host; the
  sequence must leave its construction working.

## See also

- `plan/phase/0003-python/task-agents.md` -- "The shape" and "Construction completes
  before attachment".
- `plan/phase/0003-python/agent-classes.md` -- "The declaration", "Construction and readiness".
- `plan/done/phase/0003-python/tasks/0003-34-an-agent-class-answers-requests.md` -- its
  `## Decisions` on the skipped `super().__init__` and on `_Instance`.
- `crates/outrig/src/python/outrig/_agents.py`, `crates/outrig/src/python/interpreter.py`
  (`_Instance`), `crates/outrig/src/agent/children.rs` (`make`), `runner.rs`.
