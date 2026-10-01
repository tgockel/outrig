# Agent classes

An **agent class** declares a long-lived child whose request channels are typed methods. Where
`@outrig.agent` (`typed-agents.md`) gives each call a fresh child, released when the call settles,
a subclass of `outrig.Agent` gives each instance one child that answers many requests and keeps its
namespace and history between them. The two share one request machinery -- ids, reply types,
handles, strict decoding, repair, events (`work.md`) -- and differ in lifetime, which is the
distinction `work.md` opens with: an agent and a piece of work do not live equally long.

It is built on `work.md`'s explicit child API. `0003-24` provides `outrig.schema`; `0003-25` the
request child, its handles, release and limits; `0003-26` the decorator; and `0003-29` the class.
The design was settled in planning on 2026-10-01, and revised on 2026-10-02 after the design
critique: a request method is marked with `@outrig.agent` rather than by its body, a trailing
round counts no attempt, and an instance counts against `children-max`. On 2026-10-05, after the
follow-up review, the single trailing round became the end-of-round summary rule, with the state
at the last host-opened round as its baseline ("The child's rounds"), and `requests-max` bounds
the requests unsettled on one child ("Limits and placement").

The maintainer's example, which the sections below explain line by line:

```python
class FooAgent(outrig.Agent):
    """Class docstring -> the first part of the child's instructions."""
    _x: int                                    # ordinary caller-side state
    def bar(self) -> int: return self._x       # ordinary method, runs in the caller
    @outrig.agent
    async def fizz(self, message: str) -> Thing:
        """Request channel "fizz": str in, Thing reply; this text describes the channel."""
        ...
    @outrig.agent
    async def buzz(self, foo: int, bar: str) -> SomethingElse:
        """Several parameters -> generated dataclass FooAgentBuzzMessage(foo, bar)."""
        ...

foo = FooAgent()                               # launches the child at once
fizz = foo.fizz("Hello"); b1 = foo.buzz(1, "one"); b2 = foo.buzz(2, "two")
done, more = await runtime.wait({fizz, b1, b2})
```

The class docstring is the first part of the child's instructions, and `_x` and `bar` are the
caller's: they live in the parent and never reach the child ("The declaration"). `fizz` and `buzz`
are request channels, marked `@outrig.agent`, whose message and reply types come from their
signatures ("Message and reply types"). `FooAgent()` returns at once and launches the child in the
background ("Construction and
readiness"). Each of the three calls sends its request now and returns a handle, and
`runtime.wait` takes the handles as it takes futures ("Calling"). One child answers all three, in
rounds its runner starts as requests arrive ("The child's rounds"), until `foo` is released or
collected ("Release").

## The declaration

Every rule below is checked when the class is created, through `__init_subclass__`, and a
violation raises `TypeError` naming the class and the member. A module that declares a wrong class
fails to import, and a submission that declares one fails before anything is sent.

**The class docstring is required, and it is the first part of the child's instructions.** A class
without one has nothing to tell its child, so it is refused rather than launched with an empty
prompt. Nothing is interpolated into it: it is not passed through `str.format`, for the reasons
`typed-agents.md` gives for a function's docstring, and the inputs reach the child as variables
with a manifest.

**A method marked `@outrig.agent` is a request channel; an unmarked method is an ordinary
method.** The decorator is the mark, and nothing is inferred from a body. An unmarked `async def`
is an ordinary coroutine method, whatever its body holds: it runs in the caller as `bar` does, and
can await several requests itself. A marked method's body is ignored -- never run and never
checked -- and `...` is the convention for it, as for a decorated function (`typed-agents.md`).
Decided on 2026-10-02, replacing the rule that marked a channel by a body of a docstring and `...`.
Under that rule one stray statement after the docstring silently turned a request method into an
ordinary method that sent nothing, and the check needed each method's source, which a class
declared inside a submission did not have. With the decorator, a class declared in a module, a
skill or a submission is checked the same way, and a mistake in the mark is visible in the
declaration rather than in a body. A marked method without a docstring is refused, because a
channel needs its description.

**`@outrig.agent(model=...)` on a method is refused**, with a message pointing at the class: an
instance is one child, and one child has one model, which is the constructor's `model=`
("Construction and readiness"). The decorator takes no other argument on a method in 0.3;
`validate=` is open ("Open questions").

**Ordinary methods, properties and annotated state are the caller's.** `_x: int` and `bar` are
Python: they run in the parent, and the child never sees them; so does an unmarked `async def`.
The child's namespace holds the inputs the constructor was given and the message and reply types,
nothing of the instance. This is `messages.md`'s rule that object identity does not cross, applied
to the instance itself.

**Channel names are method names.** `fizz` is `runtime.channels["fizz"]` in the child, which is
what makes the method's docstring a description of that channel. `user` is refused: it is the user
channel's name, which a child does not have in 0.3 and may have later
(`plan/next/children-have-a-user-channel.md`). The names `outrig.Agent` defines -- `ready`,
`release`, `__aenter__`, `__aexit__` -- cannot be request channels either, since the method would
replace them.

**Subclassing adds channels, and the instructions are the most-derived class's docstring.** A
subclass has a docstring of its own or is refused like any class without one; the base's is not
prepended, and a subclass that wants it writes it. A subclass may add request methods. It may
restate a request method, marked again, with the same signature, which changes only the channel's
description. One with another signature is refused, because the channel's contract is derived from
the signature and code written against the base would send the wrong shape. An unmarked method
replacing a request method, or a request method replacing an unmarked one, is refused the same
way.

## Message and reply types

**The request type comes from the parameters.** No parameter beyond `self`: the channel carries
`None`. One: its annotated type, so `fizz` carries `str`. Several: a generated frozen dataclass
named `<Class><Method>Message` -- `FooAgentBuzzMessage(foo, bar)` -- with fields in parameter
order and each parameter's default kept as its field's default. The signature already says what
the message holds, so asking for a dataclass beside it would be a second declaration of the same
thing. Every parameter is annotated, with a type `outrig.schema` takes (`0003-24`), and a missing
annotation is refused. `*args` and `**kwargs` are refused, since a message has one fixed shape.

The generated class is declared through `0003-24` as any dataclass is -- `make_dataclass`, with no
source of its own -- so the type is checked at class creation, the request is decoded strictly in
the child, and the schema text in the child's instructions comes from the same renderer. It is
reachable on the class under its own name, so parent code can construct one by hand.

**The reply type is the return annotation**, required, and a reply is decoded against it strictly
on the parent's side (`0003-24`), as a work child's completion is. `-> None` declares a request
answered with no value; the child still replies, because the reply is what settles the handle.

**Both are bound by name in the child.** `Thing`, `SomethingElse` and `FooAgentBuzzMessage`, with
the dataclasses they use, are bound into the child's namespace under their class names, as
`0003-25` binds a result type, so `d.body.foo` reads and `Thing(...)` constructs. Two distinct
classes with one `__name__` -- `a.Thing` on one method and `b.Thing` on another -- are refused at
class creation naming both methods, because one name holds one class in the child. The crossing
is `messages.md`'s: field values and a type identifier, decoded into the child's own instances.

## Construction and readiness

```python
foo = FooAgent()                                        # the example: no inputs
bar = BarAgent(repo_path="/workspace", model="fast")    # two inputs, a configured model
```

**`FooAgent(**inputs, model=...)` returns at once, and the child launches in the background.**
`outrig.Agent.__init__` does two things. Synchronously, it checks each input against the
serializable subset with the check channel contracts use (`_check_subset`,
`interpreter.py:1442`), so a hosted reference or an open file is refused in the constructor with
nothing launched. Then it schedules `runtime.spawn(name, prompt=<the class docstring>,
model=model, inputs=inputs, requests=<the channels>)` as a task on the constructing kernel's event
loop and returns the instance. The spawn is `0003-25`'s, with the channels derived from the
declaration ("Explicit plumbing"). A subclass with an `__init__` of its own calls
`super().__init__(**inputs, model=...)`; one without passes its keywords through. `model` is the
one keyword the base keeps for itself, so no input is named `model`.

**Inputs reach the child as variables.** Each is bound under its name in the child's namespace,
and the instructions carry a manifest -- name, type, bounded preview -- as a submission's do
(`typed-agents.md`). Everything else on the instance stays in the parent.

**The child's instructions** are the class docstring; the input manifest; one entry per channel,
with its name, the method's docstring, and the schema text of its request and reply types; and how
to answer -- receive a delivery, reply or fail by it -- with the statement that this child has no
`runtime.complete`.

**The orienting round.** The launch starts the child's first round, in which the model reads its
instructions and may prepare before any request arrives: import what it will need, open the
repository, bind helpers. A request arriving during it is announced on the next tool result like
any other and can be answered in the same round ("The child's rounds"). The round costs one model
call for an instance that may never receive a request, which is the cost of a child that is
oriented when the first one arrives; a caller that does not want to pay it before it has a request
constructs the instance when it has one.

**`await foo.ready()`** resolves once the orienting round has ended, and raises if the launch
failed. It settles once: a second await returns at once. A request made before the child exists
waits in the instance and is sent when the spawn returns, with its handle pending meanwhile, so the
three calls on the line after the constructor in the example are ordinary and nothing about them
waits for `ready()`.

**Where each error surfaces**, because the split is what lets the constructor return without a
host call:

- At class creation: every rule of "The declaration" and "Message and reply types".
- In the constructor, synchronously: an input outside the subset, and a constructor called outside
  any execution's context -- on a raw thread -- which raises as `outrig.runtime` does there
  (`0003-24`).
- At `ready()` and at the first handle, because they are the host's answers and the host is asked
  in the background: an unknown `model=`, with the configured names listed; a parent already at
  `subagent-depth-max`; the session at `children-max`, which is `AgentLimitReached` ("Limits and
  placement"); admission closed. A failed launch settles `ready()` and every held handle with the
  same error, and a later call raises it too.

**`async with FooAgent() as foo:`** awaits `ready()` on entry and releases on exit, whether the
body returned or raised. The cost is a model round before the body starts; the gain is that a
launch failure raises at the `async with` line rather than at each request.

## Calling

```python
h = foo.fizz("Hello")                     # bound to the signature, checked, sent now
thing = await h                           # a Thing, or the request's failure
thing = await foo.fizz("Hello")           # the same, in one line
done, more = await runtime.wait({foo.fizz("a"), foo.buzz(1, "one")},
                                return_when=asyncio.FIRST_COMPLETED)
done, more = await asyncio.wait({h.future for h in handles})
things = await asyncio.gather(foo.fizz("a"), foo.fizz("b"))
```

**One rule: a call sends the request and returns the handle.** `foo.fizz("Hello")` binds its
arguments to the signature, checks each against its parameter's type before anything is sent --
`foo.fizz(1)` raises `TypeError` and the child never receives it -- builds the message, posts it on
the channel, and returns `work.md`'s handle. There is no coroutine form and no `.start()`, so
`inspect.iscoroutinefunction(FooAgent.fizz)` is false and `help(foo.fizz)` says that the call
returns a handle. A forgotten `await` still sends the request; its handle is evented when it is
collected unsettled, and the request runs to settlement ("Release").

**The handle** is `work.md`'s: awaitable, settling once with the decoded reply or a documented
exception, and re-awaiting gives the same answer; `cancel()`, `done()`, `result()`, `status`, and
`future`, a shielded view for `asyncio.wait`. `runtime.wait` accepts handles directly, which is
what the example's last line relies on, and `asyncio.gather` takes them as awaitables. A request
has no progress endpoint in 0.3 (`plan/next/progress-channels-on-agent-classes.md`). Cancelling a
waiter leaves the request standing; `h.cancel()` cancels the request.

**What the child sees:**

```python
d = await runtime.channels["fizz"].receive()
d.id                                 # the request's id, which the reply names
d.body                               # "Hello", a str; on buzz, a FooAgentBuzzMessage
d.sender                             # the parent
await d.reply(Thing(...))            # checked against Thing; raises the problems
await d.fail("no such file")         # settles the handle with AgentRequestFailed
```

`reply` is the request child's `complete`. Its value is checked against the reply type, strictly,
and when it fails the problems are raised in the child's code with their JSON paths, bounded, so the
child can correct the value and reply again; each failed reply counts one attempt toward the
request's limit ("The child's rounds"). It returns once the value is accepted, as `0003-25`'s
`complete` does. `fail(message)` settles the handle with `AgentRequestFailed` carrying the message,
for a request the child cannot answer -- a file that is not there, an instruction it cannot follow
-- so that the parent learns why instead of holding an open request until it cancels it. A
reply to a request that
has settled -- answered already, cancelled, released -- is refused and evented, and after
`h.cancel()` it raises `RequestCancelled`. `reply` and `fail` are methods of request deliveries,
`0003-25`'s `RequestDelivery` subclass of `Delivery`; `id` is on every delivery, the user channel's
included, since the interpreter already posts each message under an id (`_deliver`,
`interpreter.py:1725`).

**A request child has no `runtime.complete`**, and the parent has no `submit` on it. The two child
kinds never mix: a work child has inputs bound as variables, one active submission, and
`complete`; a request child receives each request and answers by its delivery. Letting `complete`
answer "the one outstanding request" was rejected ("Rejected alternatives"), so a request child's
`help(runtime)` does not list `complete`, and calling it raises with a message naming `reply`.

## The child's rounds

A request child is driven by the host as every child is: a Rust runner calls the model and
submits the child's Python (`0003-25`). What the request machinery adds is when a round starts and
what the model is told.

**A request arriving while the child is idle starts a round.** `PythonAgent::round`
(`crates/outrig/src/agent/mod.rs:239-241`) already returns at once when nothing was delivered
since the last round that ended well, so a runner looping on rounds makes no model call while its
child is idle; a request posted on a request channel is a delivery, a change since the last nudge
(below), and the next round opens on it. The opening is the announcement the primary gets for a
user message, with the noun changed: `1 request is waiting on runtime.channels["fizz"]`.

**During a round, a request is announced on the next tool result**, by the announcer the primary
already has (`crates/outrig/src/agent/channel.rs:51-115`), whose text today reads
`2 messages are waiting on runtime.channels["user"]`; for a request channel the noun is
"requests". The announcement is made when something has arrived since the round opened or since
it last announced, and it reports the unread counts and nothing else: it is information for the
model, and it moves no baseline, since what the host compares when the round ends is the state
the round opened on (below). The model reads the requests by writing code that does, as
`messages.md` says of every channel: notification without disclosure.

**`runtime.wait` in the child is ended by an arriving request.** A request is input on a channel,
and `runtime.wait` (`interpreter.py:1815`) watches every channel, so a child waiting on a
subprocess or a timer with `runtime.wait` has the wait ended by `MessageAvailable` naming the
channel, with the operation still running. Input wins, as it does for the primary
(`execution-and-rounds.md`). A bare `await` is not watched, so a child in one is unreachable
until it returns and the request waits in its queue; the preamble's rule of thumb about duration
applies to a child as it applies to the primary.

**A round that ends with requests unanswered is followed by a host-initiated round when, and
only when, the child's queue state has changed since the last round the host opened on it.**
Decided on 2026-10-05, after the follow-up review, replacing one trailing round followed by
idling, and restated the same day, after the review's interleaving (the second trace below), with
its baseline fixed where the maintainer put it: no change to the queues since the last nudge, no
nudge again. This is the one rule for every child; `work.md`, `typed-agents.md` and the tasks
state it by reference. A round the host opens on queue state is a nudge. When it opens one, it
keeps the summary it opened on -- on each channel the ids of the requests unread, and the id of
each request received and neither replied to nor failed -- and that summary is the baseline. At
the end of every round, the host computes the same summary from the child's live queue state and
compares it with the baseline. Any difference -- an arrival, a receive, a reply, a cancellation
-- opens the next round, and the opening is rendered from the live summary: the unread counts in
the announcer's words, and each received request by id and channel with how long it has waited.
An unchanged summary opens nothing, and so does one with nothing in it, nothing unread and
nothing unanswered. An announcement made during a round, in a result's prefix, moves the baseline
nowhere: it tells the model what has arrived, and the comparison is still with the summary the
round opened on. The baseline moves only when the nudge's round ends well; a round that ended in
an error leaves it where it was, as `Announcer::keep` does today, so its opening is made again.
The rule generalizes the `Announcer` (`channel.rs`), which keeps a baseline across rounds and
opens a round on the difference; the summary adds the received-and-unanswered requests, which the
host knows because it delivers and settles each one, and compares ids where `Told` compares
counts (`execution-and-rounds.md`, "A round", for what else changes). Such a round counts no
attempt against any request (below).

Five consequences, which `0003-25` tests:

- **A child that answered anything in a round gets the next round, whatever arrived meanwhile.**
  Every reply removes a request's id from the summary, so a round that answers one of several
  ends with a summary that differs from the baseline, and an arrival during the round, announced
  or not, cannot make the two equal again, since the baseline holds ids and not a count.
- **A request the child received and left unanswered earns exactly one further round**, its
  nudge: receiving it moved it from unread to received, a change since the baseline. If the nudge
  round ends with it still unanswered, the summary is the one that round opened on, and no round
  opens.
- **A request never received earns one round at most.** One that arrives while the child is idle
  opens that round itself; one that arrives during a round, announced in a result, is a change
  since the baseline when the round ends, and opens the round after it. Either way it is then
  unread in the baseline, and it earns nothing more while it stays so.
- **A child that ends a round with nothing changed gets no further round** until an arrival, a
  cancel or a release changes the summary.
- **A new arrival opens a round that names the old requests too**, since the opening is rendered
  from the whole summary and not from the arrival.

The trace the rule was decided on. Five requests, #1 through #5, arrive on `fizz` while the child
is idle, and the child serves one per round:

1. The arrival opens a round, a nudge: `5 requests are waiting on runtime.channels["fizz"]`, and
   the baseline is #1 through #5 unread. The child receives #1, replies, and ends the round. The
   live summary is #2 through #5 unread, which differs from the baseline. A round opens:
   `4 requests are waiting on runtime.channels["fizz"]`, and the baseline is now #2 through #5.
2. The same, down to one. After the fifth reply the summary is empty and the child is idle. All
   five settled, with no arrival after the five.
3. Had the child received all five in the first round and answered one, the summary would be four
   received and unanswered against a baseline of five unread; the round that opens names the four
   by id, and each later reply changes the summary again until none is left.
4. Had the child received #1 and ended the round without answering it, the summary is #1 received
   and four unread, against five unread: one round opens naming #1 and counting four, and that is
   the new baseline. If that round ends the same way, the summary is unchanged, and the child is
   idle with #1 open and four unread; nothing further is spent until an arrival, a cancel or a
   release changes it.

The trace the baseline was fixed on, the follow-up review's interleaving. The same five arrive
while the child is idle, and a sixth arrives during the first round:

1. The arrival opens a round, the nudge: five waiting, and the baseline is #1 through #5 unread.
2. The child receives #1 and replies. Before that execution's result returns, #6 arrives, so the
   result carries `5 requests are waiting on runtime.channels["fizz"]` -- #2 through #6. The
   announcement moves nothing.
3. The model ends the round. The live summary is #2 through #6 unread; the baseline is #1 through
   #5. They differ -- #1 settled, #6 arrived -- so a round opens, `5 requests are waiting on
   runtime.channels["fizz"]`, with #2 through #6 as the baseline. Had the announcement moved the
   baseline to five unread, five would have matched five and nothing would have opened, with five
   requests open and no arrival to come.
4. The child goes on serving one per round, and each round's reply opens the next, until the
   summary is empty.

A work child has one more step. Its summary is its one submission, and where this rule would
leave the submission unanswered and the child idle, the submission settles with
`CompletionRejected` instead; a decorated call's child is then released, while an explicit work
child stays idle and a submission queued behind the failed one opens its own round next
(`work.md`, "A request settles once").

**What an idle child's open requests wait for.** A request the rule leaves open stays open -- its
handle unsettled, its status saying the child is idle -- until the caller cancels it with
`h.cancel()`, the child answers it in a round a later arrival opened, or the instance is released.
A `runtime.wait(timeout=...)` that expires ends the caller's wait and nothing else: the request is
still open, and it still holds its instance ("Release"). A caller that wants to be rid of it
inspects what is pending -- `h.done()`, `h.status` -- and cancels or releases, and neither undoes
an effect the child has already dispatched. `requests-max` ("Limits and placement") bounds how
many such requests one child can accumulate, abandoned ones included.

**Only an invalid reply counts toward the attempt limit, 3 by default.** A reply that fails the
reply type's check is raised in the child's code with its problems and counts one attempt; past
the limit the request settles with `CompletionRejected`, carrying the last value replied and its
problems, and a reply to it afterward is refused. The child goes on serving its other requests:
the limit ends one request, not the instance. A host-initiated round counts nothing, and no count
of rounds ends a request child's request. Decided on 2026-10-02, replacing a rule under which each
trailing round counted an attempt against every request it listed, so that a child serving a
batch in order across rounds spent the later requests' allowance while answering the earlier ones
correctly, and rejecting a separate budget of rounds in its place. The maintainer's reasons:
spending rounds carries no real penalty of its own -- the token budget bounds spend -- and a long
wait may be legitimate, since a request can wait on a build or a test run; and the child's own
long waits happen inside an execution, where no round ends, so a round that ends with a request
open is not evidence that the child has stopped working on it. The alternative, no limit on
invalid replies either, is `0003-25`'s fork 13.

**The requests unsettled on one child are bounded by `requests-max`**, default 256, counting
unread and received-and-unanswered alike, and a call past it raises `AgentLimitReached` at the
call with nothing sent ("Limits and placement"). Nothing waits outside the child's queue.

**`h.cancel()` before receipt removes the request**, so the child never sees it; after receipt the
child's `reply` or `fail` raises `RequestCancelled`, and the handle settles cancelled. Cancelling a
request never interrupts the child's execution: it is cancelling work, not interrupting
(`execution-and-rounds.md`'s four cancellations), and the child learns of it at its reply.

## Release

```python
await foo.release()                    # idempotent

async with FooAgent() as foo:          # ready() on entry, release() on exit
    thing = await foo.fizz("Hello")
```

**Release is explicit.** `await foo.release()` ends the child, and a second call returns at once.
`async with` releases on exit however the body ended. Every outstanding request -- held before the
child existed, waiting unread, or received and unanswered -- settles with `AgentReleased`, and a
reply the child makes during or after the release is refused and evented, as a late reply to a
settled request is. A call on a released instance raises `AgentReleased` at the call, and sends
nothing. A release before `ready()` has settled settles it with `AgentReleased`; once settled,
by either, it stays settled.

**Releasing the instance closes the child's subtree** the way `lifecycle.md`'s "Releasing a child"
says: its own children -- instances it constructed, decorated calls in flight -- are released the
same way, its pending escalations are cancelled, its running execution is interrupted, and its
forwarded hosted calls finish, or are reported `unknown` if their binding dies. `0003-25` builds
and tests that; the class adds nothing to it beyond the requests that settle.

**The GC backstop.** An instance collected unreleased is released by the runtime. `outrig.Agent`
holds a finalizer that cannot await and runs on whichever thread dropped the last reference, so
it does one thing: it schedules `release()` on the constructing kernel's event loop, and the
release is evented as a collection. Three consequences:

- **An unsettled request holds its instance.** The runtime's record of each request that has not
  settled holds a reference to the instance, so a child is never released while a request is in
  flight, even when agent code has dropped both the instance and the handle: `FooAgent().fizz("x")`
  with neither reference kept runs to settlement, then the child is released and
  `agent.instance.collected` is recorded. The request holds the reference rather than the handle
  because the handle wrapper is collectable -- a handle collected unsettled is evented ("Calling")
  -- and a child released while a caller still awaits it would settle that caller's request with
  `AgentReleased`. When the last request settles and no reference to the instance remains, the
  finalizer releases the child.
- **A reloaded skill's module-level instance lives until it is collected.** `outrig.skills.reload`
  removes the module from `sys.modules` and nothing else (`skills.md`): a function taken from the
  old module holds its globals, and the instance among them. A skill that constructs an instance at
  import releases it itself, or accepts that the child lives until the old namespace goes.
- **Timing is the collector's.** CPython frees an unreferenced instance at once unless it is in a
  reference cycle, and the instance's bookkeeping of its handles is one, so the backstop runs when
  the cyclic collector does, not at the last `del`. It is a backstop, and explicit release is the
  rule.

**At the session's close**, a launching instance is never ready -- `ready()` and the first handle
raise the closing error -- and an idle or working one is released with its requests settled, as
`lifecycle.md`'s rows for agent instances and requests say. The shutdown report lists the
outstanding requests.

## Limits and placement

An instance's child is a kernel in the same interpreter as its parent (`agent-placement.md`): a
thread, a session module, an event loop and an execution slot, with the same bindings and the same
skill catalog, and no user channel. While idle it costs that thread and its memory, about 16 MiB
measured, and no model calls.

- **Depth.** The child is one level below the agent that constructed it, and an instance
  constructed by an agent at `subagent-depth-max` fails at `ready()` and the first handle.
- **Tokens.** The per-tree token budget (`0003-25`) covers the child's rounds. A child that has
  spent it starts no further model call, and each of its outstanding requests settles with the
  budget error; the instance stays until it is released.
- **Children.** An instance's child counts against `children-max` (default 64; `0003-25`) while
  it is resident -- launching, working, idle, or wedged -- because what that limit bounds is the
  thread and the memory, which an idle instance holds too. Construction past the cap fails from
  the constructor's background launch: `AgentLimitReached`, an `outrig.AgentError`, surfaces at
  `ready()` and at the first handle, as every host-side error does ("Construction and
  readiness"), and nothing waits for a place; `release()` is what frees one. `run-new` does not
  read `subagent-width-max`, which is `run`'s key. A cap on children *working* at once, which an
  idle instance would not count against, is `potential/resource-scheduling.md` ("Rejected
  alternatives").
- **Model requests.** The child's model calls take permits from `model-concurrency-max` (default
  8; `0003-25`), one per provider request, held only while the request is in flight and never
  while the child's code runs, so an instance whose child awaits children of its own holds no
  permit while it waits. A round that would be the ninth request in flight waits in the queue for
  a permit, and releasing the instance while it waits removes it from the queue.
- **Requests.** `requests-max` (default 256; `0003-25`) bounds the requests unsettled on one
  child: held before the child exists, waiting unread, or received and unanswered. A method call
  past it raises `AgentLimitReached` at the call and sends nothing, as `foo.fizz(1)` raises
  `TypeError` there; a reply, a `fail`, `h.cancel()` or the release frees the place. Nothing
  waits outside the child's queue, so a parent that sends faster than the child answers is
  refused rather than slowed, and a request its caller abandoned after a `runtime.wait` timeout
  counts until it is cancelled. Decided on 2026-10-05, after the follow-up review, replacing a
  request past the queue bound held on the sending side, which bounded the child's unread queue
  and nothing else. A sender that wants to wait for room is
  `plan/next/wait-for-request-capacity.md`.
- **Spend.** A child's model usage is attributed to the child's round. A round that answers
  several requests is attributed to the round and not divided among them, because a division
  would be invented; each request's settled event names the rounds it spanned, so a reader can
  see what was spent while it was open. Upward, the child's usage is added to the instance's total,
  which its released event carries, and to the skill invocation and the main agent's round under
  which the instance was constructed, with spend after that round yielded published as usage
  events naming it (`0003-25`, fork 8).

A child's thread is not the main thread, so a child whose code wedges is contained and not
recoverable (`agent-placement.md`); its requests wait until the instance is released, and the
release settles them. Its kernel is still a thread and its memory, so it counts against
`children-max` until the session ends, not until the release marks it gone.

## Events

All execution diagnostics (`observability.md`), with bodies as bounded previews: a request's body
and its reply are values between agents that the parent's model may never see, so they are
recorded as messages between agents are, never as model view.

- `agent.instance.started` -- the instance's id, the class's `__module__` and `__qualname__` and
  its module's digest, the child's id, the input manifest, and the channels declared with their
  types;
- `agent.instance.ready` -- the orienting round ended, or the launch failed and why;
- `agent.instance.released` -- how: explicit, `async with`, collected, or the session closing; the
  requests settled by it; and the instance's usage total;
- `agent.instance.collected` -- the finalizer ran, before the release it scheduled;
- the request family, shared with submissions (`work.md`): `agent.request.sent`, with the id, the
  channel, a bounded body preview and the parent execution; `received`; `replied`; `invalid`, with
  the problems and the attempt number; `failed`, with the message; `cancelled`; and `settled`, with
  the outcome, the attempts, and the rounds spanned;
- the child's own execution and model events, under its own `subject`.

A method call produces no `agent.call.*` event: those are the decorated-call wrapper's, and a call
on an instance is a request.

## Explicit plumbing

The class is a thin layer over `0003-25`'s request child, and the layer underneath is reachable:

```python
child = await runtime.spawn(
    "foo", prompt="...", model="fast", inputs={"repo_path": "/workspace"},
    requests={"fizz": outrig.Request(str, Thing, doc="..."),
              "buzz": (FooAgentBuzzMessage, SomethingElse)})    # a tuple: no description
h = child.channels["fizz"].request("Hello")                      # -> handle
child.channels["fizz"].pending()                                 # not yet taken by the child
await runtime.release([child])
```

`outrig.Request(request_type, reply_type, doc="...")` declares one channel with the description
the child's instructions carry; a bare `(request_type, reply_type)` tuple is the same without one.
The parent's endpoint has `request(body) -> handle` and `pending()`; the child's end has
`receive()`, and its `send` raises, because the contract in that direction is empty: the child
replies, it does not send. The direction is fixed in 0.3 -- the parent requests, the child
replies -- and a child's own requests go to children of its own.

In those terms the class does four things. `__init_subclass__` derives the `requests=` dict from
the methods marked `@outrig.agent`. `__init__` schedules the spawn. A method call builds the
message from its arguments and calls `request()` on the named endpoint. `release()` is
`runtime.release([child])`.
Agent code that wants another shape -- channels decided at run time, a child shared by several
objects -- builds it from the same calls.

## Rejected alternatives

**Rejected: `runtime.complete` answering "the one outstanding request".** It would have kept one
completion call for both child kinds. It is racy: between the model deciding what to complete with
and the call, a second request can arrive, and "the one outstanding" is then two. It is also
unrecorded: nothing says which request the value answered, so the attribution the handle depends
on would be a guess. A reply names its delivery, and a request child has no `complete`.

**Rejected: a coroutine call form, with `.start()` for the handle.** `await foo.fizz("x")` would
read as an ordinary await. But it is two conventions where one does, and the common case is the
fan-out in the example, which `runtime.wait` takes as futures and not as coroutines: a method
has to return a handle for that anyway. A call that returns a handle still reads as one line when
awaited, and `help()` says what it returns.

**Rejected: a cap on live children that a launch waits for.** The width cap of 2026-09-30 had a
launch past it wait for a slot. An idle instance costs a thread and its memory, not model spend,
and under that cap a long-lived helper would have held a slot for the whole session while doing
nothing, with the launch that wanted the slot waiting on a release only the helper's owner could
perform. The cap was dropped on 2026-10-01, which left the number of kernels in a session
unbounded, and `children-max` took its place on 2026-10-02 with the opposite choices on both
points: an idle instance counts, because the limit bounds memory and threads and an idle instance
holds both; and a launch past it fails at once instead of waiting. A cap on children *working* at
once, which an idle instance would not count against, is `potential/resource-scheduling.md`.

**Rejected: marking a request method by its body.** The design of 2026-10-01 made an `async def`
whose body was a docstring and `...` a channel, and any other `async def` an ordinary method. It
needed no decorator, and it made one stray statement change what a call did without a word from
the runtime, and needed each method's source for the check. The decorator says the same thing
where it can be read, and the body is then free to be ignored ("The declaration").

**Rejected: an awaitable constructor.** `foo = await FooAgent.create()` would make readiness
explicit at construction. It would also block the caller for the child's orienting round -- a
model call -- before a single request could be sent, where the example sends three on the next
line; and since `__init__` cannot be `async`, it would be a second spelling beside the class
call. `ready()` and `async with` give the same guarantee to the code that wants it.

## Open questions

- `validate=` on a request method. The method's `@outrig.agent` is the place it would attach;
  what it receives beyond the decoded reply is open, since a method has the instance's inputs as
  well as the call's arguments. Deferred; strict decoding is the only check on a reply in
  `0003-29`, and the decorator refuses the keyword on a method until then.
- Progress from an agent class. A submission has `h.progress`; a class request has nothing between
  sent and settled. `plan/next/progress-channels-on-agent-classes.md` records the shapes.
- Whether every long wait in a request child should be ended by a new request. `runtime.wait` is,
  a bare `await` is not, and the child's preamble is the only place that says to prefer the first
  for anything long. Interrupting a child's execution for a request would be the stronger rule,
  and it is the one the primary does not have for a user message.
- Whether the instance exposes its child object, for `pending()` and the child's status. The
  explicit API has both, and the class hides the child entirely.

## Unverified

- Nothing on this page has run. The class machinery -- `__init_subclass__` collecting the marked
  methods, the generated dataclass, the background spawn -- has not been prototyped, and the first
  evidence is `0003-29`'s acceptance.
- The GC backstop's timing under asyncio was reasoned from CPython's reference counting and cyclic
  collector, not measured: a pending task that references the instance keeps it alive, a cycle
  defers collection to the collector's next pass, and a finalizer that runs at interpreter exit
  may find the loop closed. The acceptance uses `gc.collect()`, which says nothing about when the
  collector runs unprompted.
- That `make_dataclass` output passes `0003-24`'s declaration and schema path -- `get_type_hints`
  resolves a generated class's annotations in the namespace of its `__module__`, which the
  generator has to set -- is `0003-24`'s added acceptance item, not a result.
- The announcer generalization. `announcement` (`channel.rs:98-115`) knows a channel by name and
  count only; saying "requests" for a request channel needs the `pending` protocol message to
  carry each channel's kind, or the host to remember the kinds it declared. Neither exists. The
  end-of-round summary also needs the received-and-unanswered requests, which are the host's own
  record; a comparison of whole summaries, by id, where `Told` compares two counts; and a
  baseline that is the summary the round opened on, where `Told::kept` takes what the round
  announced, a mid-round announcement included.
