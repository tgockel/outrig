# Task agents

A **task agent** is an `outrig.Agent` subclass an application constructs for one task. The
instance holds the task and the agent's facilities as ordinary Python state, and the Python the
model writes in the agent's kernel has that instance as `self`. This page decides how such an
instance is constructed and attached, where it lives, what its ordinary methods and its request
methods are, how the owner of a session constructs one in a kernel the owner does not share, and
how its instructions are composed. `0003-41` builds construction and attachment; `0003-45` makes
the instance the child's `self`; `0003-46` builds the owner-constructed root; `0003-47` composes
the instructions. `0003-39` comes first because each of them adds interpreter code
(`application-code.md`, "What the mounted package carries"). The facilities an agent is given
and the view they make are `facilities.md`; the code an application supplies is
`application-code.md`; an agent's channels to the application's own messaging are
`external-channels.md`; what a Rust owner holds for an instance it constructed -- the control
handle and its reads -- is `rust-owner.md`.

Everything `agent-classes.md` decided about the class stands unless a section below quotes a
sentence of it and names the task that reverses it: the declaration checks in
`__init_subclass__`, the mark that makes a method a request channel, the message and reply types,
the handle, the child's rounds, release and the limits. The running example is a review
application; nothing below is specific to it.

```python
class Reviewer(outrig.Agent):
    """You review changes in the repository your facilities reach."""

    def __init__(self, task, facilities, *, model=None):
        super().__init__(facilities=facilities, model=model)   # records; launches nothing
        self._task = task                                       # ordinary instance state

    def describe(self) -> str:                                  # ordinary: runs where called
        return f"{type(self).__name__} on {self._task.title}"

    @outrig.agent
    async def review(self, path: str) -> Verdict:
        """Review the file at `path` and return a verdict."""
        ...

reviewer = Reviewer(task, {"files": self.facilities["files"]})  # in a kernel: a child launches
```

In the child's kernel, `self` is `reviewer`: `self._task.title` reads the task the parent
constructed it with, `self.describe()` runs there, `self.facilities["files"]` is the child's own
reference to the `files` facility, and `d = await runtime.channels["review"].receive()` takes a
request the parent's `reviewer.review("src/main.py")` sent. A task agent an owner constructs as
a root may declare no request method at all: its work arrives on the channels the application
backs, and its task is on `self`. The spellings of `facilities` and of the hooks below are their
tasks' forks; the sequence is this page's.

## The shape

Construction in a kernel, as `0003-41`, `0003-42`, `0003-45` and `0003-47` build it:

1. `Reviewer(...)` runs the most-derived `__init__` to its end, as any class call does. The base's
   `__init__` records `model`, `facilities` and the data inputs and launches nothing.
2. The optional attach hook runs, if the class defines one, on the instance, with no `super()`.
3. OutRig attaches the instance: it makes the runtime's record of it (`_Instance`), classifies the
   facility mapping (`facilities.md`, "What a dict may hold"), schedules the launch on the
   constructing kernel's loop, and returns the instance. A `TypeError` from a bad input or key is
   raised here, synchronously, with nothing launched (`agent-classes.md`, "Construction and
   readiness").
4. On the loop, before the spawn line is sent, the instruction hook runs, if the class defines
   one, and its sections go on the line as text (`0003-47`).
5. The host launches the child: it opens the kernel, which binds `self` (`0003-45`), the facility
   view (`0003-42`), the inputs and the classes; composes the orientation from OutRig's rules,
   the docstring, the sections, the facility manifest and the channels (`0003-47`); starts the
   runner; and the orienting round opens (`agent-classes.md`, "Construction and readiness").
6. `await reviewer.ready()` settles when that round ends (`0003-34`).

Construction by the owner, as `0003-46` builds it, differs in where steps 1 through 4 run: in a
new root kernel, on its own thread, after the owner's request. The owner holds a control handle,
never the object ("The owner-constructed root"; `rust-owner.md`).

## Construction completes before attachment

`agent-classes.md`, "Construction and readiness", records: "`outrig.Agent.__init__` does two
things. Synchronously, it checks each input against the serializable subset ... Then it schedules
`runtime.spawn(name, prompt=<the class docstring>, model=model, inputs=inputs, requests=<the
channels>)` as a task on the constructing kernel's event loop and returns the instance." And
`0003-34`'s `## Decisions`: "A subclass whose `__init__` skips `super().__init__` raises
`TypeError` saying so at its first call." `0003-41` reverses the first and decides the second.

What replaces it: `Agent.__init__` records the base's keywords and launches nothing, and a
metaclass on `outrig.Agent` -- derived from `abc.ABCMeta`, so an application's abstract mixins
work, or `0003-41` records why not -- runs the most-derived `__init__` to completion in its
`__call__`, then the optional attach hook, then attachment. `__init_subclass__` keeps the
declaration checks (`agent-classes.md`, "The declaration"); the metaclass adds the construction
sequence beside it and checks nothing of the declaration.

Why: today `Agent.__init__` calls `_launch` directly
(`crates/outrig/src/python/outrig/_agents.py:542`), `_Instance.__init__` schedules the spawn
before returning (`crates/outrig/src/python/interpreter.py:5095`), and the host spawns the
child's runner inside `make`, before it answers `spawned`
(`crates/outrig/src/agent/children.rs:642`, `:374-384`); the runner opens the orienting round at
once (`crates/outrig/src/agent/runner.rs:69`, `:101`, `:130`). So a subclass `__init__` that
raises after `super().__init__()` has already launched a child whose orienting round can spend a
model call before anything releases it. With attachment after the most-derived constructor,
"a constructor that raises spends no model round" is true rather than usual (OR-AG2; OR-A1-3
names the line at `:542` as what must change). The attach hook exists so that an application can
finish state that its `__init__` cannot -- state a mixin's `__init__` sets after the subclass's
own body, say -- without the base relying on an override calling `super()`: OutRig calls the hook
directly.

What a failure does: a constructor or hook that raises leaves nothing of OutRig's behind, since
attachment has not happened, and the exception is the constructor call's; what the constructor
did to the application's own objects stands, since OutRig rolls nothing back
(`execution-and-rounds.md`, "No rollback, ever"). A failure after attachment -- an unknown
`model=`, a limit, closed admission -- surfaces at `ready()` and the first handle as before. A
request method called inside `__init__` or the hook raises `TypeError`, since the instance is not
attached; `0003-41`'s fork decides whether constructing an agent inside another's constructor is
refused or allowed and documented.

## The instance is the child's `self`

`agent-classes.md`, "The declaration", records: "Ordinary methods, properties and annotated state
are the caller's. `_x: int` and `bar` are Python: they run in the parent, and the child never sees
them; so does an unmarked `async def`. The child's namespace holds the inputs the constructor was
given and the message and reply types, nothing of the instance. This is `messages.md`'s rule that
object identity does not cross, applied to the instance itself." `0003-34` built exactly that:
`_open` binds the inputs and the classes into the child's globals and nothing else
(`interpreter.py:6279-6282`), and `_Instance` "keeps no reference to the instance" (its
`## Decisions`). `0003-45` reverses the sentence about the child's namespace: **the instance the
parent constructed is bound as `self` in the child's kernel**, the same object, and ordinary
methods, properties and annotated state are reachable from both kernels.

Why it is sound: a child is a kernel in the same interpreter as its parent, a thread sharing one
heap (`agent-placement.md`, "The shape"), and `security.md` already says "Agents are not isolated
from each other, and it does not matter": the grant is the session's, and co-hosting is
interference, not disclosure. `messages.md`'s rule holds unchanged for what crosses a channel; the
instance is not a message, and OutRig installs it in the child's namespace as it installs the
inputs, through `_spawning` (`interpreter.py:3947-3949`) into `_open`. What the requirement wants
from this (OR-AG1) is one object: the task the subclass's `__init__` stored on `self._task`, read
in the child as the same object, with no hidden second instance and no attribute serialization.

Beside `self`, a kernel may bind **aliases**: globals that are the same object as an argument the
instance was constructed with, so `task is self._task` holds when `__init__` stored the argument
unchanged (OR-A1-4). The mechanism is `0003-45`'s; which names are bound is the construction's to
say -- the owner's construct record names them (`0003-46`), and a child's dict keys are already
its convenience globals (`facilities.md`) -- and `0003-46`'s fork decides whether any is bound by
default. An alias is a reference, never a copy, and nothing is dumped from the instance to make
one.

What follows, which `0003-45`'s `## Decisions` records:

- **Explicit release is the rule for an instance with a kernel-side `self`; the GC backstop stays
  for an instance without one.** `agent-classes.md`, "Release", releases an instance that is
  collected unreleased, and `0003-34`'s `## Decisions` made that collection happen "as its last
  request ends, or at its last reference". Once the child's globals hold `self`, the parent
  dropping its reference collects nothing: the child's kernel holds the instance until the child
  ends, `agent.instance.collected` never fires for it, and an in-VM instance nobody releases lives
  until the session's close releases it. So `release()`, `async with`, the parent's release of its
  subtree and the session's close end such an instance, and the orientation and
  `help(outrig.Agent)` say so. The backstop stays for a class the owner declares in its own
  process and does not provision (`0003-35`), whose instance has no kernel-side `self`. Rejected:
  binding `self` through `weakref.proxy`, which keeps attribute access and lets the parent's drop
  collect the instance, and breaks `is` and `isinstance` -- `self is reviewer` is false,
  `type(self)` is the proxy's type -- and raises `ReferenceError` in a round once the referent is
  gone.
- **Shared mutable state between two threads is part of the model.** The parent's thread and the
  child's both read and write `self`; one attribute read or write is atomic under the GIL, a
  compound update is not. Documented, not policed, as `agent-placement.md`, "What agents share
  regardless", documents the working directory and `sys.modules`. A parent that changes `self`
  while the child works has no way to tell the child; a request on a channel is how a parent
  asks for something.
- **Two construction sites, one contract.** For an in-VM child the subclass's `__init__` runs in
  the constructing kernel and OutRig installs the object into the new kernel's namespace; for the
  owner-constructed root it runs in the root's kernel ("The owner-constructed root"). The object
  model is the same either way. A class the owner holds in its own process cannot have a
  kernel-side `self`, which is why the owner-constructed root needs the class's code in the kernel
  (`application-code.md`).
- **Imports are shared, instances are separate.** Two instances of one class have their own
  attributes, kernels, histories and loops, and one module object: `sys.modules` is the
  interpreter's (`skills.md`, "A skill is a module": "One module object serves every kernel").
  A module-global list is shared mutable state; per-agent state belongs on `self`.
- **Inputs are unchanged.** Data inputs still cross as deep copies bound as variables, with a
  manifest (`agent-classes.md`, "Construction and readiness"), and the message and reply types
  are bound by name. A task-bound class carries its task on `self`, so it has no inputs to
  declare for it.

## Helpers and requests

`agent-classes.md`, "The declaration", decided this and `0003-45` restates nothing: "A method
marked `@outrig.agent` is a request channel; an unmarked method is an ordinary method", whatever
its body and whether it is `def` or `async def`. With the instance shared, the two surfaces read
as OR-AG3 asks:

- An ordinary method runs where it is called. `reviewer.describe()` in the parent runs in the
  parent's execution; `self.describe()` in the child runs in the child's. Calling one moves
  execution to no other kernel, and a hosted call it makes is attributed to the kernel it runs in
  through the facility reference it resolves (`facilities.md`, "Attribution"). It is neither
  ignored nor a kernel transfer.
- A request method sends a request and returns its handle (`agent-classes.md`, "Calling"); its
  body is never run. In the child, a call on `self`'s own request method would be a request to
  itself: `0003-45`'s fork decides whether it is refused at the call, which this page recommends,
  since a child answering its own requests opens a round on each arrival and ends only with the
  token budget.

The orientation says which is which in one sentence (`0003-47`), and `help(self)` describes both
surfaces from their docstrings, as `help()` describes anything (`discovery.md`).

## The task is not a request

Nothing in OutRig represents the task, and nothing keeps an agent "working on it". `0003-37`
guarantees that no child is idle while it holds a request it has not answered; an agent holding
none is idle, which is how `lifecycle.md` has it. A task agent begins its task in its orienting
round, which is also where its instructions, its facility manifest and the application's
contributions reach the model, and that round requires no request and no delivery to exist
(OR-A1-5): it opens as the instance launches, on an empty baseline (`agent-classes.md`, "The
child's rounds"; `runner.rs:69`, `:101`). A round lasts as long as its work does, and a long wait
inside `runtime.wait` costs no model call (`execution-and-rounds.md`). When a round ends with
nothing open, the agent is idle, and only an arrival opens another round: a request on one of its
channels, or a delivery on an external channel the application backs (`external-channels.md`).
A root with no request channel holds no requests, so `0003-37`'s rule has nothing to count on it;
its liveness obligations come from reply-owed requests inbound on an external channel
(`external-channels.md`, "The liveness rule covers an inbound request channel on a runner-driven
holder"). The owner observes the agent's state through the control handle (`rust-owner.md`, "The
stop read"), its open requests and the stream, and chooses to wait, to sleep, or to end the
session (`continuation.md`). This is what OR-AG1 asks -- "No synthetic execution request
represents the task" -- and it is a fixed fact the application plans around: idle does not
complete a task, and a task agent that should keep going is woken by something arriving.

## The owner-constructed root

An owner constructs a task agent whose `self` lives in a kernel the owner does not share. The
first application's owner is a Rust embedder of `outrig::harness` with no Python host program
(A-00's first decision), so `0003-46` builds the operation for a Rust owner: a `SessionBuilder`
or `Session` operation that takes a class reference into the application code, constructor
arguments and a model, constructs the instance in a kernel of a `no_primary` session as the
application's primary with the owner as its parent endpoint, and returns a control handle
(`rust-owner.md`, "Construction from Rust", "The control handle"). The instance is a root agent
in the 0003 README's sense: a child whose parent is the owner, at depth 1, counted by
`children-max` and `requests-max` as any child (`agent-classes.md`, "From the host program").
Its rounds open through its own runner, and the owner never calls `Session::round()` for it,
which stays `NoPrimary` (CC-AGT-10).

`0003-35` stays as queued: it is the phase's Python-host deliverable, which another program
depends on, and the phase README's Tasks paragraph keeps it so. Its kernel-side plumbing -- a
host-originated request into a root's channel, the reply crossing as JSON, the acceptance
handshake, the `owner` parent, root placement at depth 1, release at session exit -- is what
`0003-46` reuses when it has landed and builds when it has not. Parity for the Python host
program with the new operations is an open choice recorded as `0003-46`'s fork, not a
deliverable.

What is kept and what is added, which `0003-46`'s `## Decisions` records. `agent-classes.md`,
"From the host program", says "the types reach the child as descriptions and are rebuilt with
`make_dataclass`, since no code of the program's ever crosses", and `0003-35` delivers that "The
declaration crosses as data ... never code, pickle or a reference". Both stand for a class the
program declares in its own process and does not provision: its instance stays in the program,
its child has no `self`, and its replies decode into the program's classes. That is the
degenerate case. For a class the application code holds (`application-code.md`), the
construction site moves into the kernel, and the sentence "no code of the program's ever
crosses" stays true for a different reason: the code is provisioned by OutRig before the session's
first construction, and nothing of it travels from the owner. What travels is:

- **Owner to kernel**: the class's name, `module:qualname`; the constructor's arguments, each
  data in the serializable subset (`messages.md`), optionally named with the provisioned
  dataclass it decodes into strictly, or a facility binding named by its bound name -- a service
  (`services.md`), a binding, a channel (`external-channels.md`) -- singly or as a dict of them
  (`facilities.md`, "A facility reference"; OR-A1-2); the names of arguments to bind as aliases
  in the kernel (OR-A1-4); and `model=`. Never a Python object of the owner's, a pickle or a
  reference into the owner's process.
- **Kernel to owner**, when the class declares channels: the class's declaration as data -- each
  channel's name, docstring and request and reply types as schema text, in `0003-35`'s encoding
  with its direction reversed -- from which the handle gets `channels()` and `request()`; then
  each reply as JSON, checked in the kernel against the declared type as a parent kernel checks
  one today, and typed for the owner by the schema text, which a Rust embedder deserializes with
  serde (OR-A2-5). A class with no channel declares nothing and the owner requests nothing of it.

The kernel runs the construction sequence above on its own thread: it imports the class, decodes
the arguments, makes the references and the aliases, and calls the class, so `__init__`, the
attach hook, attachment and the instruction hook run there and `self` is bound in that kernel. A
class with no `@outrig.agent` channel is constructed as a root (OR-A1-1): the refusal that an
instance "would answer nothing" (`_agents.py:533-535`) is for a child a kernel constructs, whose
only way to be given work is a request, and `0003-46`'s fork decides whether the exception is a
construction-site flag or a subclass marker. A construction that fails -- a module that does not
import, a constructor or hook that raises, an argument that does not decode -- ends the kernel,
makes the handle's readiness fail with the exception rendered and a bounded traceback, is
evented, spends no model round and leaves the session clean. One that succeeds gives the owner
the control handle: `ready()`, `release()`, requests when channels exist, and the reads
`0003-48` and `0003-49` add.

This resolves the tension the conflict check named between OR-AG1, OR-AG5 and `0003-35`: if the
owner's process constructed the instance, the kernel's `self` could only be a proxy and every
helper a boundary request; the kernel constructs it, and the owner holds the end of its channels
and a handle on its state.

## Instructions are composed from contributions

A task agent's instructions are composed by OutRig in a fixed order, each part a section of its
own, none interpolated into another (`0003-47`):

1. OutRig's rules: how the agent acts, that it is a child -- or, for a root, that the program
   owning the session constructed it -- and that no user reaches it except through the channels
   it holds (`orientation.rs:45-47`, `:176-181`).
2. The class docstring, unformatted, as `agent-classes.md`, "The declaration", has it.
3. The application's contributions: ordered, titled sections the instruction hook returned.
4. The input manifest, as today; the `self` sentence and the aliases (`0003-45`); the facility
   manifest from the agent's mapping (`facilities.md`, "The view is the agent's"), each entry by
   name, kind and the bounded description its kind allows.
5. The channels: each request channel with its docstring and the schema text of its request and
   reply types, and how to answer (`orientation.rs:213-241`); and the external channels the agent
   holds, with their manifest (`external-channels.md`, "What the agent is told"; `0003-51`).
6. What every agent is told last: the history window, the working directory, the interpreter,
   and the hosted-call rule when a facility is a binding (`rest()`, `orientation.rs:111-142`).

That is the system prompt, and no delivery's body is ever in it. The orienting round then opens
on its fixed lead, that the round is for getting ready (`ORIENTING`, `orientation.rs:360-362`),
and, when a delivery already waits on an external channel the agent holds, the opening line
announces what waits by channel and count, after the instructions and never as a body in them, as
`external-channels.md`, "Initial input is a delivery", documents and `0003-51` records. The
delivery itself is read by code; empty initial input delivers nothing and the opening announces
nothing; and the round requires no request or delivery to exist. That is the one documented
order OR-A1-5 and OR-A7-3 ask for: `ACT` and the rules, the docstring, the contributions, the
manifests and the channels, then the opening lead, then the announcement, with the first delivery
privileged in nothing. `0003-47` states it and `0003-51` records each delivery.

The hook is called by OutRig once per instance, after attachment, in the kernel that constructed
the instance -- the constructing kernel for an in-VM child, the root's own kernel for an
owner-constructed root -- and its result crosses to the host as bounded text sections on the
spawn line or the root's construct answer. It may read `self`: the task the constructor stored,
the mapping's keys and kinds. OutRig reads nothing it did not return: no `__dict__`, no `repr`,
no property, which is `discovery.md`'s governing constraint applied to instructions. A section
past its bound fails the construction before any round, under `0003-47`'s recommended fork, as a
docstring the class lacks fails the declaration. The composition happens once, before the
orienting round, because the preamble is given to the model once
(`crates/outrig/src/agent/build.rs:95`) and recorded once as `model.instructions`
(`crates/outrig/src/agent/mod.rs:334`); an application whose task changes puts the change where
the model reads it in code, on the task object.

CocoClaw, or any application, supplies content through the hook and never assembles the prompt;
what it omits from a child's mapping is not advertised to that child (OR-AG7).

## Events

Each task adds its own to `doc/reference/events.md`, as execution diagnostics
(`observability.md`), and its rows to `lifecycle.md`'s close table:

- `0003-41`: no new event; a construction that raises in a kernel is in its execution's
  traceback, and launches nothing to event. No new close row: a construction is synchronous
  Python inside an execution.
- `0003-45`: `agent.instance.started` gains `self`, whether the child has the instance as its
  `self`; `agent.instance.collected` is documented as never recorded for such an instance. The
  **Agent instances** row *Collected during the close* is scoped to instances without one.
- `0003-46`: `agent.instance.started` with `parent` the owner (`0003-35`'s fork 3), `self` true
  and `channels` possibly empty; `agent.construct.failed`, with the class and why. The **Root
  agents** rows gain *Constructing*.
- `0003-47`: `agent.instance.started` gains `contributions`, each section's title and length;
  `model.instructions` already carries the composed text.
- `0003-48` and `0003-49` add no event: the reads are the authority, and the stream stays the
  diagnostic surface (`rust-owner.md`).

## Rejected alternatives

**Rejected: a second instance in the child.** A copy of the instance, or an object rebuilt from
its attributes, would give the child a `self` with the parent's state at construction and
nothing after, and `self._task` would not be the task object the application holds. One object is
what OR-AG1 asks for, and the shared heap makes it free.

**Rejected: `weakref.proxy` as the child's `self`.** Above, under "The instance is the child's
`self`": it keeps the GC backstop and breaks identity.

**Rejected: attachment from inside `Agent.__init__` with a post-init hook.** A hook the base's
`__init__` calls runs before the subclass's own body continues, which is the defect, not the
cure. Only a step after the most-derived constructor returns can know it completed.

**Rejected: an `execute(task) -> Result` service.** The requirements withdrew it
(`task-bound-agent-correction.md`): the task is bound once at construction, and generic
assessment, save and resume are OutRig's, not methods a subclass must write.

**Rejected: a synthetic first delivery or a privileged first message.** The orienting round
opens on launch and needs nothing to exist; initial input, when there is any, is a delivery like
every later one (`external-channels.md`, "Initial input is a delivery").

**Rejected: interpolating contributions into the docstring.** `typed-agents.md`, "Inputs are
values", gives the reasons for a docstring: braces in prose, and untrusted text put straight into
instructions. A contribution is a section of its own, verbatim.

**Rejected: discovering instructions from the instance.** Dumping `self.__dict__`, rendering
`repr(self)` or reading properties would run application code and disclose whatever the instance
holds, a credential-holding client among them; `discovery.md` rules it out for every automatic
observation, and the hook returns what the application chose to say.

**Rejected: recomposing the contributions every round.** The preamble is composed once, and a
system prompt that changes under a conversation costs a hook call per round and an instructions
event per round for a change the task object already carries where the model reads it.

**Rejected: the owner driving the root's rounds.** `rust-owner.md`, "Rejected alternatives": the
instance's runner is the one scheduler, and the application forbids itself a loop around it.

**Rejected: reworking `0003-35` so every host-declared class is provisioned.** The phase README's
Tasks paragraph keeps `0003-35` as queued: one task's worth of difference, a reordered queue, and
the loss of the declaration-as-data path for a class the owner does not want in the kernel.

## Open questions

- The spellings: the attach hook's name (`0003-41`), the instruction hook's name (`0003-47`),
  the mapping's attribute (`0003-42`), the owner's construct operation and the control handle
  (`0003-46`). Each is its task's fork, recommended there.
- Whether a dict-less child of a dict-constructed parent -- a `runtime.spawn` from a reviewer --
  should inherit the parent's view rather than the session's. The phase decided the session's
  view for an agent constructed without a dict; the consequence, a narrowed child widening by
  spawning, is presentation and not authority, and is stated in `facilities.md`.
- Whether an `outrig.Agent` instance given as a facility is advertised with its request methods
  as channels the child may use, or only as the plain object it also is.
- Whether an alias is ever bound by default -- `task` for an argument so named -- or only when
  the construction names it (`0003-46`'s fork).
- The orienting lead for a root with contributions: `ORIENTING`'s text (`orientation.rs:360-362`)
  says the round is for getting ready; a task agent also begins its task in it. `0003-47`'s fork.
- Python-host parity for construction and the reads (`0003-46`'s fork; `rust-owner.md`).
- Whether the owner's `ready()` should settle at the end of the orienting round, as the kernel
  caller's does, or when construction succeeded; `0003-46` recommends the first.

## Unverified

- That a metaclass derived from `abc.ABCMeta` leaves `__init_subclass__` and `_Marked.__set_name__`
  running as they do under `type` was reasoned from CPython's `type.__new__`, which calls both
  before `__call__` is ever involved; `0003-41` tests it.
- The claim that running `__init__` on a root kernel's thread, outside the execution slot, bills
  its output to the kernel's backlog was reasoned from `agent-placement.md`'s contextvar
  dispatcher and not run; `0003-46`'s fork decides the context it runs in.
- No run against a real model is recorded for a child whose orientation names `self`; whether a
  model reaches for `self._task` rather than asking for the task is `0003-47`'s e2e-less risk,
  and the exit criterion is met by mock-driven tests.
- The GC behavior under a kernel-side `self` -- that the child's kernel keeps the instance alive
  until the kernel ends -- is reasoned from `_open` binding it into the module's `__dict__` and
  that module's registration in `sys.modules`; `0003-45` tests it with `gc.collect()`.
