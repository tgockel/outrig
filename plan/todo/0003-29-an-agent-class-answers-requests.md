# 0003-29 -- An agent class answers requests

## Context

By this task a parent has two ways to make a child do typed work, and both are built. `0003-25`
gives it `runtime.spawn`, and with `requests=` a *request child*: a kernel whose parent sends
requests on named channels with `child.channels[name].request(body)` and gets a handle for each,
and whose own code takes each request with `receive()` and answers by its delivery --
`await d.reply(value)`, checked strictly against the channel's reply type, or `await d.fail(msg)`.
The same task gives the handle its rules (`await`, `cancel()`, `done()`, `result()`, `status`,
`future`), the per-request attempt limit, the trailing round that lists unanswered requests, the
request event family, release with `AgentReleased`, the token budget, `children-max` and
`model-concurrency-max`, and the close rows. `0003-26` gives the decorator -- the mark on a
declaration, whose body is ignored, with the docstring required and the types declared through
`0003-24` -- and the composition of a child's instructions from a docstring, an input manifest and
schema text. `0003-24` declares types, decodes strictly and renders schema text.

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
on the named endpoint with the message built from its arguments, and `release()` is `0003-25`'s
release. Nothing about how a request is carried, checked, repaired, limited or evented is new
here; what is new is the declaration, the instance's lifetime, and the readiness between them.

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
  without an annotation, or with a type `0003-24` refuses; one with no return annotation; one with
  `*args` or `**kwargs`; a marked method named `user`, or named as one of `outrig.Agent`'s own
  members; a subclass restating a request method with another signature, or replacing one with an
  unmarked method or an unmarked method with one; two distinct classes with one `__name__` among a
  class's types. A marked method's body is never run and never checked, so a class declared in a
  submission is checked exactly as one in a module is, with no source read. Unmarked methods,
  `def` or `async def` and whatever their bodies, properties and annotated attributes are left
  alone.
- **Message and reply types.** No parameter: `None`; one: its type; several: a frozen dataclass
  `<Class><Method>Message` with fields in parameter order and defaults kept, built with
  `make_dataclass` and declared through `0003-24`, reachable on the class under its name. The reply
  type is the return annotation. Both, with the dataclasses they use, are bound by name in the
  child's namespace as `0003-25` binds a result type.
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
  parameter's type, builds the message, sends it on the channel, and returns `0003-25`'s handle.
  `inspect.iscoroutinefunction` on the method is false, and `help(foo)` and `help(foo.fizz)` say
  that a call sends a request and returns a handle. `runtime.wait` takes the handles directly.
- **The child's loop**, which is `0003-25`'s with the announcer's noun generalized: a request
  arriving while the child is idle starts a round; one arriving during a round is announced on
  the next tool result as `N requests are waiting on runtime.channels["fizz"]`; a round that ends
  with requests announced and unread, or received and unanswered, is followed by one trailing
  round listing them by id, and if that round ends the same way the child goes idle, with those
  requests open until `h.cancel()`, a `runtime.wait` timeout in the caller, or release; only an
  invalid reply counts an attempt, 3 by default, and past the limit the request settles
  `CompletionRejected` while the child goes on serving the others; `fail` settles
  `AgentRequestFailed`; `h.cancel()` removes a request before receipt and makes `reply` raise
  `RequestCancelled` after it.
- **Release.** `await foo.release()`, idempotent, and `async with`. Every outstanding request
  settles `AgentReleased`; a reply during or after the release is refused and evented; the subtree
  is closed as `0003-25`'s release closes one; a call on a released instance raises
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
  produces `0003-25`'s request events and no `agent.call.*` event. All are execution diagnostics
  with bounded body previews (`observability.md`).
- **`lifecycle.md`'s rows for agent instances**, tested here: a launching instance at the close is
  never ready, and `ready()` and the first handle raise the closing error; an idle or working one
  is released, with its requests settled as `0003-25`'s request row says; a collection during the
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
- **Re-prompting works per request.** A mock that reads a request and ends its round without
  answering is called again with a round that lists the request's id; a valid reply in that round
  settles the handle with the value. A mock that never answers is called twice for one request --
  the round that received it and the trailing round -- after which the mock receives no further
  call, the child reads idle, and the handle stays open with no `agent.request.invalid` recorded;
  `h.cancel()` settles it cancelled, and a new request sent before that starts a round whose
  opening names both ids. A request on which the mock replies with a wrong shape three times
  settles `CompletionRejected` and the mock receives no further call for it; one with a wrong
  shape twice and a right one third settles with the value, and the record shows two
  `agent.request.invalid` events with attempts 1 and 2. `await d.fail("why")` settles the handle
  with `AgentRequestFailed` whose message is `"why"`.
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
- **Cancellation.** `h.cancel()` before the child receives the request removes it: the record
  shows `agent.request.cancelled` and no `agent.request.received` for it. After receipt, the
  child's `reply` raises `RequestCancelled` and the handle reads cancelled.
- **Shutdown with a launching instance and outstanding requests**: the session closes while one
  instance is in its orienting round and another has two requests unanswered; every handle and
  both `ready()` calls settle with the closing error within the test's deadline, and the
  `ShutdownReport` lists the two requests.
- `help(foo)` lists `fizz` and `buzz` and says each returns a handle;
  `inspect.iscoroutinefunction(FooAgent.fizz)` is false.
- **The instance events are in order** for the example: `agent.instance.started`,
  `agent.instance.ready`, three `agent.request.sent`, and for each request `received`, `replied`
  and `settled` naming the rounds it spanned, then `agent.instance.released`.
- `crates/outrig/public-api.txt` regenerated, its additions limited to the instance events; the
  request family is `0003-25`'s.
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
the single trailing round, and the attempt limit that counts invalid replies only are `0003-25`'s
decisions, and this task does not reopen them.

## Dependencies

- **Hard: `0003-25`.** The request child, `request()` and `pending()`, the handle, `reply` and
  `fail`, the attempt limit and trailing round, the request events, release and `AgentReleased`,
  the token budget, `children-max` and `AgentLimitReached`, `model-concurrency-max`, and the close
  rows this task's instance rows are listed with.
- **Hard: `0003-26`.** The decorator as the mark, the instruction composition from a docstring,
  an input manifest and schema text, and `model=` resolution through config.
- **Hard: `0003-24`.** Declaring a type, the generated dataclass through `make_dataclass`, strict
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
