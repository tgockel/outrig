# 0003-45 -- The instance is the child's `self`

## Context

A child's kernel is built by `_open` with its globals `__builtins__`, `asyncio` and `runtime`
(`Kernel.__init__`, `crates/outrig/src/python/outrig/_interpreter.py:5378-5403`), a stub per binding
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
- `crates/outrig/src/python/outrig/_interpreter.py` (`_Instance`, `_collected`, `_open`, `Kernel`),
  `crates/outrig/src/python/outrig/_agents.py`.

## Decisions

Forks 1 through 3 were taken as recommended. What the task left to execution is recorded here.

- **The reversal.** `agent-classes.md`, "The declaration" -- "The child's namespace holds the
  inputs the constructor was given and the message and reply types, nothing of the instance" --
  no longer holds for an instance a kernel constructs: `_open` binds it as `self` in the child's
  kernel, the object the parent holds, so its ordinary methods, properties and state are reachable
  from both kernels. `messages.md`'s rule stands for what crosses a channel:
  the instance is not a message, and inputs still cross as deep copies. A class a host program
  declares keeps the sentence (`0003-35`): its instance stays in the program, and
  `agent.instance.started.self` is `false` for it.
- **Explicit release, and the interpreter's finalizer removed outright.** `_Instance` keeps the
  instance from attachment, since the launch hands it to the child's kernel, and `_instances` keeps
  the `_Instance` until a release, the session's close or the end of the kernel that made it. A
  finalizer on the instance could therefore never run while there was anything to release, so
  none is registered, and what only it used went: `_collected`, `_Instance.collected`, the
  `collected` branch of the release and the collection half of the close's notice. The task kept
  the backstop for "an instance whose launch failed": under `0003-34` that instance's `retire()`
  detached its finalizer and a refused launch records nothing, so the backstop had nothing to do
  for it, and dropping it changes nothing observable. A host program's instance keeps `0003-35`'s
  backstop in `outrig._host`, untouched. `0003-34`'s "`_Instance` keeps no reference to the
  instance" is superseded for an instance a kernel constructs. `weakref.proxy` is rejected as
  `task-agents.md` records: it would keep the backstop and break identity -- `self is reviewer`
  false, `type(self)` the proxy's type, `isinstance` false -- and raise `ReferenceError` in a round
  once the referent was gone. An instance dropped by everything but its child lives until it is
  released, its maker is, or the session closes; after its release, the instance, its record and
  the ended kernel form a cycle the collector frees, which a weak reference in
  `a_dropped_instance_lives_until_it_is_released` shows. `_Work.instance` is unchanged and
  harmless: it holds what is held anyway.
- **Shared mutable state is documented, not policed**, in `help(outrig.Agent)`, the concepts page
  and the orientation's `self` sentence: one attribute read or write is atomic, a compound update
  is not, a request is how the parent asks, and a module's globals are shared by every agent that
  imports it, so per-agent state belongs on `self`.
- **Two construction sites, one contract.** The seam `0003-46` builds on is what a child binds
  at boot: a spawn's record carries `boot`, the instance as `self` and its aliases, which `_open`
  binds into the kernel's globals and `_boot`; an owner-constructed root's kernel will bind the
  same names on itself, the one assignment `_open` makes, or a helper once it is a second caller.
- **Fork 1: refused.** `_Instance.request` raises `TypeError` when the calling kernel is the
  instance's own child's, naming `Class.channel()` and the channel and saying request methods are
  how the agent that made it asks, before anything is counted or sent. Each request method's
  `help()` says so too.
- **Fork 2: hidden.** `self` and each alias go into the kernel's `_boot`, so `runtime.names()` and
  the inventory leave them out until agent code binds the name to something else.
- **Fork 3: no default alias, and the mechanism with no driver.** `_Instance.alias({name:
  object})` names aliases before the launch runs, checked as they are named: a name an input's
  could not be, `outrig`, `self`, an input's name, a key of the instance's mapping, given or
  inherited, or a bound class's name is refused, naming it; and once the launch has begun, naming
  one raises `RuntimeError`, since the spawn has already carried what it binds. The spawn's record
  carries them to `_open`, each the object itself, never a copy, and the spawn line's `instance`
  names them for the orientation's `self` sentence, one sentence each. Nothing in a kernel calls
  `alias`: an in-VM child's dict keys are already its convenience globals, and `0003-46`'s
  construct record is the first caller. The acceptance test names one through
  `aliased._outrig.alias(...)`, standing in for that record.
- **`self` is refused as a name the child binds otherwise.** A facility key `self` was refused
  already (`0003-42`). A class the child would bind under `self` -- a channel's type or an input's
  class -- is refused at attachment with `TypeError`, as two classes of one name are. An input
  named `self` never reaches `Agent.__init__`'s `**inputs`: Python binds `self=` to its first
  parameter and raises `TypeError` ("got multiple values for argument 'self'"), which the test
  pins; no code was added for it.
- **The event.** `agent.instance.started` gains `self` -- `has_self` in Rust -- read from the spawn
  line's `instance.self`, which the interpreter's instances send; a host program's line, which
  carries none, reads `false`.
- **The orientation's sentence** comes after the input list and before the channels, where
  `0003-47`'s order puts it, for an instance whose child has it as `self`: what `self` is, that
  ordinary methods run here, that a request method on `self` raises, each alias, the shared-state
  rule, that its own state belongs on `self`, and that it runs until released.
- **`help(outrig.Agent)`** was 7,927 characters of `help()`'s 8,192 before this task. To fit the
  `self` paragraph, the facilities paragraph and the `facilities` property's docstring were
  tightened, and `__init_subclass__` has a docstring of its own in place of the `object` one pydoc
  showed. With the title `help()` adds, the answer is 8,019 characters.
- **Tests that changed.** `0003-34`'s `a_dropped_instance_is_released_once_its_requests_end`
  became `a_dropped_instance_lives_until_it_is_released`, and
  `the_close_ends_launching_and_working_instances` asserts no collection. The interpreter's
  `an_instance_collected_after_the_close_is_recorded_once_its_child_exists` became
  `the_close_releases_each_instance_once_its_child_exists`; and
  `an_instance_dropped_as_its_kernel_ends_records_nothing`, whose finalizer race is gone, became
  `an_instance_released_as_its_kernel_ends_records_nothing`, a release begun in the cleanup as the
  kernel ends. `0003-35`'s test of a program's collected instance keeps its body, renamed so that
  it no longer claims a kernel's instance is collected the same way. `0003-42`'s facilities test
  reads the reviewer's mapping through `self.facilities` in place of its stand-in.
- **`/simplify` adopted part of an independent implementation.** Built from the same requirement,
  it carried `self` and the aliases to `_open` as one `boot` mapping with no method on `Kernel`;
  folded the close's release into the general one, since a release `closing` begins only once
  `_closing` is set, so it asks the host nothing and says the same notice; and composed the `self`
  sentence inline in `child_preamble`, one sentence per alias, with no struct and no plural form.
  Those were adopted, about half the logic the first version had. This version's guard against
  naming an alias once the launch has begun, its event and `_Work` comments, and its fuller ending
  of the `self` sentence were kept. It also found two `_Work` comments still saying a request
  holds its instance so that the instance is not collected; `_Work.instance` stays, since a
  handle collected while its request is open is said through it, and the comments now say that.
