# 0003-45 -- The instance is the child's `self`

## Context

A child's kernel is built by `_open` with its globals `__builtins__`, `asyncio` and `runtime`
(`Kernel.__init__`, `crates/outrig/src/python/interpreter.py:5378-5403`), a stub per binding
(`:5439-5445`), and -- for a spawn waiting on it -- the parent's inputs and the classes under their
names (`:6276-6282`); no `self` is bound. `_Instance` is "what the runtime keeps of an agent class
instance, apart from the instance, which nothing here holds: its being collected is what releases
its child" (`:5049-5051`): a `weakref.finalize` on the instance schedules the release (`:5093`,
`_collected` `:5275-5282`), each request holds the instance until it ends (`_Work.instance`,
`:4475-4479`), and `0003-34`'s `## Decisions` records "`_Instance` keeps no reference to the
instance". The spawn's record carries inputs and classes to `_open` (`_spawning`, `:3947-3949`).
Every kernel's module is registered in the interpreter's `sys.modules` (`:5404`), so modules a
class imports are one object for every agent (`agent-placement.md`, "What agents share
regardless"). `agent.instance.collected` is the event of the finalizer running
(`doc/reference/events.md`).

OR-AG1 asks that a Session host an actual task-bound instance, constructed for its one task,
whose base and application initialization establish the task reference and instance state; that
generated Python has that instance as `self`; that agents have separate attributes, namespaces,
histories and loops while imports follow normal module sharing; and that no synthetic request
represents the task. OR-AG3 asks that ordinary methods run their bodies where called and
`@agent` methods are requests, with neither silently treated as the other.

**This task reverses a recorded decision.** `agent-classes.md`, "The declaration": "Ordinary
methods, properties and annotated state are the caller's. `_x: int` and `bar` are Python: they
run in the parent, and the child never sees them; so does an unmarked `async def`. The child's
namespace holds the inputs the constructor was given and the message and reply types, nothing of
the instance. This is `messages.md`'s rule that object identity does not cross, applied to the
instance itself." The validation found the reversal physically sound -- kernels are threads of
one process sharing one heap, and `security.md` already says agents are not isolated from each
other -- and named its consequences (its reversal 1; the conflict check's C1 and its tension 1):
the GC backstop stops working for a shared `self`, shared mutable state between two threads joins
the model, and there are two construction sites with one contract. `task-agents.md`, "The
instance is the child's `self`", records what replaces the sentence. The decision on the backstop
is made: explicit release is the rule for an instance with a kernel-side `self`, and the backstop
stays for instances without one; `weakref.proxy` is rejected.

## Goal

In a kernel, the instance the parent constructed is bound as `self` in its child's namespace --
one object, its ordinary methods running where they are called and its `@outrig.agent` methods
sending requests -- and an instance with a kernel-side `self` is released explicitly.

## Deliverables

- **`self` in the child.** `_Instance` keeps the instance from attachment; the spawn's record
  carries it as a Python object, and `_open` binds `kernel.globals["self"]` to it beside the
  inputs and classes. `self` is a boot name, hidden from the inventory until rebound (fork 2),
  and is refused as an input name, a facility key and a bound class's name.
- **Aliases beside `self`** (OR-A1-4). The record a kernel is opened with may name aliases: each
  is bound as a global to the same object the named constructor argument was, so `task is
  self._task` holds when `__init__` stored the argument unchanged. The mechanism is this task's;
  which names are bound is the construction's to say -- an owner's construct record names them
  (`0003-46`), and an in-VM child's dict keys are already its convenience globals (`0003-42`) --
  and fork 3 decides the default. An alias is a boot name, refused as an input name, a facility
  key and a bound class's name, and the orientation's `self` sentence names it when one is bound.
- **One object.** `self._task` set in `__init__` is read in the child as the same object;
  `type(self)`'s module is one object in `sys.modules` for every agent; two instances have their
  own attributes, kernels, histories and loops. `help(outrig.Agent)` says that a module-global is
  shared state and per-agent state belongs on `self`.
- **The release rule.** For an instance with a kernel-side `self` no finalizer is registered,
  `agent.instance.collected` is never recorded, and `release()`, `async with`, the release of the
  parent's subtree and the session's close end it; `help(outrig.Agent)` and the orientation
  sentence say explicit release is the rule. For an instance without one -- a class the owner
  declares in its own process and does not provision (`0003-35`), or an instance whose launch
  failed -- the backstop stays as `0003-34` built it. `_Work.instance` stays harmless.
- **Shared mutable state** is documented in `help(outrig.Agent)` and in the orientation sentence
  (`0003-47` composes the whole; this task writes the sentence): `self` is also the parent's
  object, one attribute access is atomic and a compound update is not, and a request on a channel
  is how the parent asks for something.
- **Helpers and requests.** An ordinary method runs where it is called, in either kernel, and
  produces no `agent.request.*`; a request method called in the parent sends a request; a request
  method called on `self` in the child's own kernel is refused or allowed per fork 1.
- **Two sites, one contract.** The deliverables above hold for a child constructed in a kernel;
  `0003-46` constructs in the root's kernel through the same `_Instance`, and this task leaves
  the seam: `_Instance` takes the instance and binds it wherever the child's kernel opens.
- **Events and the close table.** `agent.instance.started` gains `self`, `true` when the child
  has the instance as its `self` and `false` for a host-declared class not provisioned;
  `agent.instance.collected`'s entry in `doc/reference/events.md` says it is never recorded for an
  instance with a kernel-side `self`. In `lifecycle.md`'s close table the **Agent instances** row
  *Collected during the close* is scoped to instances without one; no new row.
- **`## Decisions` records**: the reversal of the quoted sentence, with `messages.md`'s rule
  left standing for what crosses a channel; the GC-backstop decision as made, with `weakref.proxy`
  rejected and its cost -- `self is reviewer` false, `type(self)` the proxy's type, `isinstance`
  false, `ReferenceError` in a round once the referent is gone; that shared mutable state is
  documented, not policed; the two construction sites under one contract; that `0003-34`'s
  "`_Instance` keeps no reference to the instance" is superseded for an instance with a
  kernel-side `self`; the alias mechanism and that an alias is a reference, never a copy; and
  forks 1 through 3.
- `agent-classes.md` gains the pointers the report proposes; `crates/outrig/public-api.txt`
  regenerated.

## Acceptance

Each is a mock-driven test.

- Two instances of one class, each `self` in its child: round code sets `self.x = 1` in one and
  `self.x = 2` in the other, and each reads its own; a module-global list appended from both
  kernels holds both entries; `sys.modules` holds one module for the class.
- The child writes `id(self)` to a module global; the parent's `id(reviewer)` equals it. A test
  helper asserts `reviewer is` the object the child's kernel holds.
- `__init__` sets `self._task` after `super().__init__()`; the first round's code reads
  `self._task.title` and replies with it; the reply equals the task's title.
- An ordinary method records `threading.get_ident()` on the instance: called from the child's
  round it is the child kernel's thread; called from the parent it is the parent's; neither
  produces `agent.request.sent`. An unmarked `async def` called in the child returns a coroutine.
- A request method called in the parent sends a request; called on `self` in the child: per fork
  1, `TypeError` naming the channel with no `agent.request.sent`, or a request the child
  receives on its own channel.
- The GC: `del reviewer; gc.collect()` while the child's kernel lives releases nothing and
  records no `agent.instance.collected`; the child stays resident, counted by `children-max`;
  `await reviewer.release()` records `agent.instance.released` with `how` `explicit`; after it,
  a weak reference to the instance dies on `gc.collect()`. `0003-34`'s tests that an instance
  dropped with a request in flight "stays until the request settles and is then released, with
  `agent.instance.collected`" and that one dropped idle "is released the same way after
  `gc.collect()`" are rewritten: with a kernel-side `self` neither collection happens, and the
  host-declared case keeps them (`0003-35`'s tests).
- A release of the parent's subtree ends the child and the instance with it, as `lifecycle.md`,
  "Releasing a child", says; the instance's methods then raise `AgentReleased`.
- An input named `self`, a facility key `self` and a bound class named `self` are each refused
  at construction.
- A record naming the alias `task` for an argument `__init__` stored as `self._task`: in the
  child, `task is self._task` holds and `task` is hidden from `runtime.names()` until rebound;
  with no alias named, `task` is unbound; an alias named like an input is refused.
- An instance constructed and left alone: the record shows the orienting round and no
  `agent.request.*`; the child reads idle after it.
- `agent.instance.started` carries `self` `true`; a `0003-35` host-declared instance's carries
  `false`.
- `runtime.names()` in the child omits `self` until the round rebinds it (fork 2).
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **A request method called on `self` in the child's own kernel -- Recommended: refused with
   `TypeError`** naming the channel and saying a request method is how the parent asks; a request
   to itself would open a round on its own arrival, be answered, and repeat until the token
   budget ends it. The alternative allows it as an ordinary request, bounded by `requests-max`
   and the budget.
2. **`self` in `runtime.names()` -- Recommended: hidden as a boot name**, since it is
   infrastructure the orientation names; the alternative lists it with its type.
3. **An alias bound by default -- Recommended: none.** An alias is bound only when the record
   names it; `task` for an argument so named is the owner's to ask for (`0003-46`'s fork 3). The
   alternative binds `task` whenever a constructor argument has that name, which puts a global in
   the kernel the application did not ask for and collides with a `task` input or key.

## Dependencies

- **Hard: `0003-41`.** Attachment after the most-derived constructor is what makes a complete
  instance the one the child gets.
- **Hard: `0003-34`.** The class, `_Instance`, the backstop this scopes.
- **Soft: `0003-42`.** `self.facilities` resolving per kernel is what makes a shared `self` the
  right place for facilities; the test of a facility through `self` needs it.
- **Soft: `0003-35`.** The host-declared class is the degenerate case whose backstop stays.

## See also

- `plan/phase/0003-python/task-agents.md` -- "The instance is the child's `self`",
  "Helpers and requests", "The task is not a request".
- `plan/phase/0003-python/agent-classes.md` -- "The declaration", "Release".
- `plan/phase/0003-python/agent-placement.md` -- "The shape", "What agents share regardless".
- `plan/phase/0003-python/security.md` -- "Agents are not isolated from each other, and it does
  not matter".
- `plan/phase/0003-python/messages.md` -- "Contracts", whose rule stands for channels.
- `plan/done/phase/0003-python/tasks/0003-34-an-agent-class-answers-requests.md` -- its
  `## Decisions` on `_Instance` and the finalizer.
- `crates/outrig/src/python/interpreter.py` (`_Instance`, `_collected`, `_open`, `Kernel`),
  `crates/outrig/src/python/outrig/_agents.py`.
