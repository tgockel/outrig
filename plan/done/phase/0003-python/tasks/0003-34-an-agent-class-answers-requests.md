# 0003-34 -- An agent class answers requests

## Context

By this task a parent has two ways to make a child do typed work, and both are built. `0003-30`
gives it `runtime.spawn`, and with `requests=` a *request child*: a kernel whose parent sends
requests on named channels with `child.channels[name].request(body)` and gets a handle for each,
and whose own code takes each request with `receive()` and answers by its delivery --
`await d.reply(value)`, checked strictly against the channel's reply type, or `await d.fail(msg)`.
The same task gives the handle its rules (`await`, `cancel()`, `done()`, `result()`, `status`,
`future`), the per-request attempt limit, the end-of-round summary rule that opens a round naming
unanswered requests, the request event family, release with `AgentReleased`, the token budget,
`children-max`, `model-concurrency-max` and `requests-max`, and the close rows. `0003-31` gives
the decorator -- the mark on a declaration, whose body is ignored, with the docstring required and
the types declared through `0003-26` -- and the composition of a child's instructions from a
docstring, an input manifest and schema text. `0003-26` declares types, decodes strictly and
renders schema text.

What is missing is the form the maintainer wants to write: a class whose instance is one
long-lived child, whose methods marked `@outrig.agent` are its request channels, and whose context
-- its namespace and its history -- is kept from one request to the next. `agent-classes.md`
designs it around this example:

```python
class FooAgent(outrig.Agent):
    """Class docstring -> the first part of the child's instructions."""
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Request channel "fizz": str in, Thing reply; this text describes the channel."""
        ...
    @outrig.agent
    async def buzz(self, foo: int, bar: str) -> SomethingElse:
        """Several parameters -> generated dataclass FooAgentBuzzMessage(foo, bar)."""
        ...

foo = FooAgent()
fizz = foo.fizz("Hello"); b1 = foo.buzz(1, "one"); b2 = foo.buzz(2, "two")
done, more = await runtime.wait({fizz, b1, b2})
```

This task is a thin layer over `spawn(requests=...)`: the class derives the `requests=` dict from
its methods, the constructor schedules the spawn in the background, a method call is `request()`
on the named endpoint with the message built from its arguments, and `release()` is `0003-30`'s
release. Nothing about how a request is carried, checked, repaired, limited or evented is new
here; what is new is the declaration, the instance's lifetime, and the readiness between them.

The host program as a caller is `0003-35`'s: it reuses the declaration checks, the type
derivation and the request machinery over the owner connection, with the instance's child a root
child of a session that has no primary. Nothing here assumes the caller is a kernel beyond where
the constructor schedules the spawn, and `__init_subclass__` runs the same on the host.

## Goal

An agent class declares a long-lived child whose methods marked `@outrig.agent` are typed request
channels; calling one sends a request and returns a handle; one child answers many requests with
its context kept; releasing the instance ends the child.

## Deliverables

- **`outrig.Agent` and the declaration check.** A subclass is checked when it is created, through
  `__init_subclass__`, and a violation raises `TypeError` naming the class and the member: no class
  docstring; a method marked `@outrig.agent` without a docstring; `@outrig.agent(model=...)` on a
  method, with a message pointing at the constructor's `model=`, and `validate=` on one (fork 2);
  a marked method with a parameter
  without an annotation, or with a type `0003-26` refuses; one with no return annotation; one with
  `*args` or `**kwargs`; a marked method named `user`, or named as one of `outrig.Agent`'s own
  members; a subclass restating a request method with another signature, or replacing one with an
  unmarked method or an unmarked method with one; two distinct classes with one `__name__` among a
  class's types. A marked method's body is never run and never checked, so a class declared in a
  submission is checked exactly as one in a module is, with no source read. Unmarked methods,
  `def` or `async def` and whatever their bodies, properties and annotated attributes are left
  alone.
- **Message and reply types.** No parameter: `None`; one: its type; several: a frozen dataclass
  `<Class><Method>Message` with fields in parameter order and defaults kept, built with
  `make_dataclass` and declared through `0003-26`, reachable on the class under its name. The reply
  type is the return annotation. Both, with the dataclasses they use, are bound by name in the
  child's namespace as `0003-30` binds a result type.
- **Construction.** `FooAgent(**inputs, model=...)` checks each input against the serializable
  subset synchronously and raises in the constructor on a failure; then schedules
  `runtime.spawn(name, prompt=<the class docstring>, model=, inputs=, requests=)` as a task on the
  constructing kernel's event loop and returns. `model` is the base's keyword, so no input is named
  `model`. The child's name is derived from the class, distinguishable per instance; its spelling
  is this task's. `await foo.ready()` settles once the orienting round has ended, or with the
  launch's failure; a request made before the child exists waits in the instance and is sent when
  the spawn returns. An unknown `model=`, a parent at `subagent-depth-max`, the session at
  `children-max` -- `AgentLimitReached`, from the background launch -- and closed admission surface
  at `ready()` and at the first handle, never in the constructor. The instance's child counts
  against `children-max` while it is resident, idle included, and `release()` frees its place.
  `async with` awaits `ready()` on entry and releases on exit.
- **The child's instructions**: the class docstring, unformatted; the input manifest; one entry
  per channel with its name, the method's docstring, and the request and reply schema text; how to
  receive a delivery and answer by `reply` or `fail`; and that this child has no
  `runtime.complete`, which is absent from its runtime and whose call raises naming `reply`.
- **Calling.** A method call binds its arguments to the signature, checks each against its
  parameter's type, builds the message, sends it on the channel, and returns `0003-30`'s handle.
  `inspect.iscoroutinefunction` on the method is false, and `help(foo)` and `help(foo.fizz)` say
  that a call sends a request and returns a handle. `runtime.wait` takes the handles directly. A
  call with `requests-max` requests unsettled on the instance raises `AgentLimitReached` at the
  call and sends nothing.
- **The child's loop**, which is `0003-30`'s with the announcer's noun generalized: a request
  arriving while the child is idle starts a round; one arriving during a round is announced on
  the next tool result as `N requests are waiting on runtime.channels["fizz"]`, which moves no
  baseline; at the end of every round the host compares the child's live queue state -- unread
  per channel by id, and each request received and unanswered -- with the state at the last round
  it opened on that state, and opens a round rendered from the live state only when they differ
  and the live state is not empty, so a child that answered anything in a round and still has a
  request unread or unanswered gets the next round, whatever arrived meanwhile, a received
  request left unanswered earns one further round, and an unread one earns none beyond the first
  round that opens with it counted (`agent-classes.md`, "The child's rounds"); a
  request the child leaves open stays open until
  `h.cancel()`, a later round answers it, or release, a `runtime.wait` timeout in the caller
  ending that caller's wait only (`0003-37` replaces this ending with a warning round and then a
  cancel); only an invalid reply counts an attempt, 3 by default, and past
  the limit the request settles `CompletionRejected` while the child goes on serving the others;
  `fail` settles `AgentRequestFailed`; `h.cancel()` removes a request before receipt and makes
  `reply` raise `RequestCancelled` after it.
- **Release.** `await foo.release()`, idempotent, and `async with`. Every outstanding request
  settles `AgentReleased`; a reply during or after the release is refused and evented; the subtree
  is closed as `0003-30`'s release closes one; a call on a released instance raises
  `AgentReleased` and sends nothing.
- **The GC backstop.** A finalizer on the instance schedules `release()` on the constructing
  kernel's event loop when the instance is collected unreleased, evented as a collection. The
  runtime's record of each unsettled request holds the instance, so an in-flight request keeps its
  child even when agent code has dropped both the instance and the handle; the handle wrapper is
  collectable, and a handle collected unsettled is evented.
- **Events**: `agent.instance.started`, with the instance's id, the class's `__module__`,
  `__qualname__` and module digest, the child's id, the input manifest and the declared channels;
  `agent.instance.ready`, with the launch's outcome; `agent.instance.released`, with how, the
  requests it settled and the instance's usage total; `agent.instance.collected`. A method call
  produces `0003-30`'s request events and no `agent.call.*` event. All are execution diagnostics
  with bounded body previews (`observability.md`).
- **`lifecycle.md`'s rows for agent instances**, tested here: a launching instance at the close is
  never ready, and `ready()` and the first handle raise the closing error; an idle or working one
  is released, with its requests settled as `0003-30`'s request row says; a collection during the
  close is recorded and does nothing further; the report lists the outstanding requests.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

- **The maintainer's example runs against a mock model**: `foo = FooAgent()` returns before any
  model call; the three requests, all sent before `ready()` settles, settle with a `Thing` and two
  `SomethingElse` instances equal to what the mock replied with; `runtime.wait({fizz, b1, b2})`
  returns all three in `done`; and the host's record shows one spawn.
- **Context persists across requests.** A mock whose first round binds a variable and whose
  second round's reply is computed from it: the second request settles with the value that only
  the first round's binding can produce, and the record shows one child.
- **Each declaration error is a `TypeError` naming the member**, one assertion per rule in the
  first deliverable, for a class declared in a module and for the same class declared in a
  submission; `@outrig.agent(model="fast")` on a method raises with a message naming the
  constructor's `model=`.
- **The mark decides, not the body.** A marked method whose body is `raise AssertionError` is
  accepted at class creation, and a call on it sends a request -- the record shows
  `agent.request.sent` -- and raises nothing. An unmarked `async def` whose body is a docstring
  and `...` is an ordinary method: a call on it returns a coroutine, and the record shows no
  `agent.request.sent`.
- **Types are derived as declared.** `buzz(1, "one")` arrives in the child as a frozen
  `FooAgentBuzzMessage` with `foo == 1` and `bar == "one"`, and assigning to a field raises;
  `fizz("Hello")` arrives as `str`; `fizz(1)` raises `TypeError` in the parent, and the record
  shows no `agent.request.sent` for it.
- **The child's instructions are composed as declared.** They contain the class docstring
  unchanged -- one holding `{message}` and `{}` appears with its braces -- the input manifest,
  each channel's docstring with its request and reply schema text, and a statement that this child
  has no `runtime.complete`.
- **Re-prompting follows the summary rule.** A mock that reads a request and ends its round
  without answering is called again with a round whose opening names the request's id; a valid
  reply in that round settles the handle with the value. A mock that never answers is called
  twice for one request -- the round that received it and the one that named it -- after which
  the mock receives no further call, the child reads idle, and the handle stays open with no
  `agent.request.invalid` recorded; `h.cancel()` settles it cancelled, and a new request sent
  before that starts a round whose opening names both. (`0003-37` replaces this ending: a third
  round warns, and the request is then cancelled.) Three requests sent together to a mock
  that answers one per round all settle, with a round opened by the host after each of the first
  two replies and no arrival after the three. A request on which the mock replies with a wrong
  shape three times settles `CompletionRejected` and the mock receives no further call for it;
  one with a wrong shape twice and a right one third settles with the value, and the record shows
  two `agent.request.invalid` events with attempts 1 and 2. `await d.fail("why")` settles the
  handle with `AgentRequestFailed` whose message is `"why"`.
- **Release settles and closes.** With one request outstanding, `await foo.release()` settles it
  with `AgentReleased`, and a grandchild the child had made is released with its waiters settled;
  a second `release()` returns at once. `async with FooAgent() as foo:` whose body raises releases
  the instance and the exception propagates. An instance dropped with a request in flight, its
  handle dropped too, stays until the request settles and is then released, with
  `agent.instance.collected` then `agent.instance.released` in the record; one dropped idle is
  released the same way after `gc.collect()`. `foo.fizz("x")` after release raises
  `AgentReleased`, and the record shows no `agent.request.sent` for it.
- **Errors surface where the design says.** `FooAgent(x=object())` raises in the constructor and
  the record shows no spawn. `FooAgent(model="nope")` returns; `await foo.ready()` raises with the
  configured names listed, and so does `await foo.fizz("x")`. An instance constructed by a child
  at `subagent-depth-max` fails the same way, at `ready()`. So does one constructed with the
  session at `children-max`: `ready()` and the first handle raise `AgentLimitReached`, the record
  shows no spawn, and after a `release()` elsewhere the next construction succeeds. An idle
  instance counts: with `children-max = 1` and one idle instance, a second construction fails
  until the first is released.
- **`requests-max` counts an instance's unsettled requests.** With `requests-max = 2`, one request
  received and unanswered and one unread, a third `foo.fizz("x")` raises `AgentLimitReached` at
  the call, with no `agent.request.sent` for it; after `h.cancel()` on the unread one, or the
  child's reply to the received one, the next call succeeds. Requests held before `ready()` count
  the same way.
- **Cancellation.** `h.cancel()` before the child receives the request removes it: the record
  shows `agent.request.cancelled` and no `agent.request.received` for it. After receipt, the
  child's `reply` raises `RequestCancelled` and the handle reads cancelled.
- **Shutdown with a launching instance and outstanding requests**: the session closes while one
  instance is in its orienting round and another has two requests unanswered; every handle and
  both `ready()` calls settle with the closing error within the test's deadline, and the
  `ShutdownReport` lists the two requests.
- `help(foo)` lists `fizz` and `buzz` and says each returns a handle;
  `inspect.iscoroutinefunction(FooAgent.fizz)` is false.
- **The instance events are in order** for the example: `agent.instance.started` before the three
  `agent.request.sent`, and `agent.instance.ready` after it; for each request `received`, `replied`
  and `settled` naming the rounds it spanned, in that order after its `sent`; then
  `agent.instance.released`. (Corrected in execution: the requests made before the child exists
  are sent as the spawn answers, so `ready` falls wherever the orienting round ends; see
  Decisions.)
- `crates/outrig/public-api.txt` regenerated, its additions limited to the instance events; the
  request family is `0003-30`'s.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether `async with` awaits `ready()` on entry -- Recommended: yes.** A body that runs before
   the launch is known to have succeeded would have its requests fail one by one instead of the
   `async with` line raising once. The cost is a model round, the orienting round, before the body
   starts, which the form makes visible.
2. **`validate=` on a request method -- Open; deferred.** For a decorated function the shape is
   settled: one declaration, one validator, called with the result and the call's arguments. For a
   method the place is clear -- the method's `@outrig.agent` -- but what it receives is not,
   since a method has the instance's inputs as well as the call's arguments. This task checks a
   reply by strict decoding only, refuses `validate=` on a method as it refuses `model=`, and
   records the question in `agent-classes.md`.
3. **A subclass overriding a request method with another signature -- Recommended: refused.** The
   channel's contract is derived from the signature, and a subclass with another contract under
   the same name would break every caller written against the base. Restating with the same
   signature is allowed, since it changes only the description.

The shape of a channel declaration (`outrig.Request`, or a tuple without a description), where
`reply` and `fail` live (`RequestDelivery`), the spend of a round that answers several requests,
the end-of-round summary rule, `requests-max`, and the attempt limit that counts invalid replies
only are `0003-30`'s decisions, and this task does not reopen them.

## Dependencies

- **Hard: `0003-30`.** The request child, `request()` and `pending()`, the handle, `reply` and
  `fail`, the attempt limit and the end-of-round summary rule, the request events, release and
  `AgentReleased`, the token budget, `children-max`, `requests-max` and `AgentLimitReached`,
  `model-concurrency-max`, and the close rows this task's instance rows are listed with.
- **Hard: `0003-31`.** The decorator as the mark, the instruction composition from a docstring,
  an input manifest and schema text, and `model=` resolution through config.
- **Hard: `0003-26`.** Declaring a type, the generated dataclass through `make_dataclass`, strict
  decoding, and schema text.

## See also

- `plan/phase/0003-python/agent-classes.md` -- the declaration, type derivation, construction and
  readiness, the child's rounds, release, events, and the alternatives rejected.
- `plan/phase/0003-python/work.md` -- the request machinery the class is a layer over, the handle,
  and the two child kinds.
- `plan/phase/0003-python/messages.md` -- request channels, `Delivery.id`, and `runtime.wait`
  accepting handles.
- `plan/phase/0003-python/typed-agents.md` -- the decorator and the instruction composition the
  class shares with a decorated function.
- `plan/phase/0003-python/lifecycle.md` -- releasing a child, and the instance rows.
- `crates/outrig/src/agent/mod.rs` -- `round`, which runs only when something was delivered.
- `crates/outrig/src/agent/channel.rs` -- the announcer whose noun this task generalizes.
- `plan/phase/0003-python/potential/resource-scheduling.md` and
  `plan/next/progress-channels-on-agent-classes.md` -- what this task leaves out.

## Decisions

Forks 1 and 3 were taken as recommended, and fork 2 as the task states it. What no doc settled is
recorded here.

- **The spawn answers at launch, and the orienting round follows** (the maintainer's call, as the
  simpler of two readings). The deliverable, `agent-classes.md` and `lifecycle.md` have requests
  made before the child exists reach it as the spawn returns, during the orienting round; the
  acceptance line on the example's event order had them follow `agent.instance.ready`. Both
  cannot hold, and either way `a = SomeAgent(); b = await a.foo("bar")` works. Answering the spawn
  only once the round had ended would have tied the answer to a model round, and left a release
  during that round waiting for it, since the instance has no child to name until then. The
  acceptance line is corrected to the order this guarantees.
- **The orienting round** is the runner's own loop with one more state: it opens at once, on a
  fixed opening that says the round is for getting ready, and its baseline stays empty, so
  whatever is still open when it ends -- arrived before it opened or during it -- opens the next
  round. As it ends the host emits `agent.instance.ready` and sends the interpreter an `oriented`
  line, which settles `ready()`. A round that fails before its code ran is opened again, as every
  child round is; one that fails after counts as ended; a spent budget or a model that cannot
  answer makes `ready()` raise `BudgetExhausted` or `AgentRequestFailed` with the host's message.
  Only an instance's child has the round: its spawn line names the instance, and
  `runtime.spawn(..., requests=...)` is unchanged.
- **A held request is an ordinary request.** The instance makes its child's `_Child` record at
  construction, `holding`, and a call is `ChildChannel.request` on it: `requests-max` counts it,
  the handle, its status and `cancel()` work as for any request, and the close ends it with the
  rest. `_open` adopts the record when the child's kernel opens, and the launch sends what was held,
  in order, under the record's lock, which `withdraw` also takes, so a cancel never races the send.
  Until the kernel opens, the record's `requests-max` is the session's, which the host says in a
  `limits` line as the session starts (`Children::new`). A request that ends before it is sent is
  in no event, since the host never heard of it. A kernel that ends while its record still holds
  has opened before its spawn answered -- the host ends it so when the launch fails after
  `open_child`, building the child's budget say -- and leaves what is held to the launch, which
  ends it with the spawn's error, as `ready()` and each later call raise it. A failed launch takes
  its child's id off the record, so none of what was held is said to have ended.
- **The mark in a class body.** `@outrig.agent` tells a method by its qualified name: a function
  defined directly in a class body (`C.f`, not `f` or `g.<locals>.f`) is marked and left to the
  class, whose `__init_subclass__` checks it and replaces it with the method that sends a
  request; every other function is `0003-31`'s typed call. The mark's `__set_name__` refuses a
  class that is not an `outrig.Agent` subclass.
- **The checks.** Each names `module.Class.member`. A channel may not be named after anything
  `outrig.Agent` has. A restatement must match the base's parameter names, kinds and defaults and
  the rendered schema of each type -- not the annotations themselves, since `doc()` markers do not
  compare equal -- and keeps the base's types, taking only the new docstring. A plain member
  anywhere in the class's order that shadows a channel is refused, a mixin's included. A class
  with no channel is accepted at creation, as a base for subclasses, and refused at construction.
  The generated message is `<Class><Camel>Message`, named so in the child too; a default that
  cannot be hashed becomes a `default_factory` that deep-copies it, a keyword-only parameter a
  `kw_only` field, and a name the class already uses is refused. Each marked method becomes a
  plain `def` under `functools.wraps`, so `inspect.signature` follows `__wrapped__` and
  `inspect.iscoroutinefunction` is false. The class keeps its channels as `_outrig_channels`, out
  of `help()`'s way. A subclass whose `__init__` skips `super().__init__` raises `TypeError`
  saying so at its first call. A `staticmethod` or `classmethod` over a mark is refused: neither
  passes `__set_name__` on, so the mark would stay in the class as a member nothing can call.
- **`_Instance` keeps no reference to the instance.** Each request made through an instance holds
  it (`_Work.instance`) until the request ends, and the finalizer holds only the `_Instance`, so an
  instance nothing else holds is released as its last request ends, or at its last reference --
  not at the cyclic collector's pass, as `agent-classes.md` had reasoned. An exception the
  `_Instance` keeps -- what `ready()` raises, a failed launch's -- is kept as a value and raised as
  a copy: a raised exception's traceback holds the frames that awaited it, the instance's among
  them, and through the finalizer that would have kept the instance for good. The close test found
  it.
- **Release runs once, on the constructing kernel's loop**, however it began and from whichever
  kernel; one while the launch is in flight waits for the spawn's answer, then releases the child
  it made. `__aenter__` releases when `ready()` raises, since `__aexit__` does not run then. The
  session's close says each instance released at once when its child is known, after the
  requests it ends, since the constructing kernel's loop may not turn again; one still launching
  is said once its launch answers, after its collection when that came first.
- **Events.** All five carry the child's subject, and started names the `parent`, as
  `agent.call.*` do. Started and ready are the host's: started once nothing can refuse the launch,
  before the runner starts; ready as the round ends. Released, collected and the handle's
  collection come from the interpreter's notices. Released's `requests` are the host's own record
  of the child's requests that ended `released` or `closed`, and its `usage` the child's own model
  requests', as `agent.call.settled`'s is. An instance whose launch was refused records none of
  them, as a refused typed call records no call events (`0003-31`). The handle's collection is a
  fifth instance event, `agent.instance.handle.collected`, since the request family is
  `0003-30`'s; a handle collected while its request is held is said once the request is sent.
  Started's digest hashes the source of a function compiled in the class's body -- told by its
  code's qualified name, which a `functools.wraps` wrapper does not take on, so a function bound
  there from elsewhere is not one -- or, with none, the class's module's file. A subclass with only
  a docstring is digested from its module, and one declared in a submission, whose module has no
  file, is `null`: nothing it holds leads back to the submission.
- **Finalizers only schedule.** The instance's finalizer and `Handle.__del__` hand their work to
  the constructing kernel's loop: a collection can happen anywhere, a protocol line's write
  included, and `_send_lock` is not reentrant.
- **An instance ends with the kernel that made it.** Once that kernel's loop has stopped, each
  instance it made is retired: forgotten, what it held ended, and its finalizer detached, which
  through the `_Instance` would otherwise hold the kernel's namespace, and the instance in it, for
  good. Retiring then rather than as the end begins takes in one that code still running made as
  the kernel ended, whose launch never runs. The instance's child ends in that kernel's subtree, so
  no `agent.instance.released` is recorded for it, as a typed call in flight there records no
  `agent.call.settled`. A release that begins once the kernel has ended -- from the finalizer of an
  instance its cancelled tasks' cleanup dropped, a `release()` in that cleanup, or the session's
  close while the kernel's loop has yet to stop -- releases and records nothing: it begins after
  `_stop` took the tasks it cancels, so only that check keeps it from running. A failed launch, or a release other than the close's, detaches the
  finalizer too, since a collection after either has nothing to say. A collection and the close's
  release, which come on different threads, decide under the instance's lock which of them says
  the collection.
- **`/simplify` replaced the first implementation.** An independent one built from the same
  requirement came to about two thirds of the logic: it held requests on the child's record made
  early, where the first kept a second holding path in `_Work` and the instance, and threaded the
  orienting round through the runner's loop, where the first ran it in a function of its own. It
  was adopted, and the first's fixes ported onto it: exceptions raised as copies, a dropped held
  handle said once sent, mutable defaults, the closing release said at once, the message name's
  check, the error for a skipped `super().__init__`, and a request that ends before its instance
  is attached to it letting the instance go. The fixes a later review asked for went through it
  too: the independent ones came to about half the logic and were adopted, with the first's lock
  between a collection and the close's release, and a retired instance's held requests ended.
- **The interpreter program** is about 118 KB of its 128 KiB argument cap after this task, from
  113 KB; `plan/next/interpreter-program-nears-the-argument-cap.md` says so.
