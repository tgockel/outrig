# Messages

How agents address each other and the user. This is the layer generated Python sees: channels,
endpoints, the contracts they carry, and the rules delivery follows.

Framing is deliberately not visible here, and it is not uniform. A message to or from the host --
the `user` channel, today the only one -- is carried as NDJSON. A message between two agents never
leaves the interpreter process, because agents are co-hosted one per thread
(`agent-placement.md`): it moves between two event loops through `loop.call_soon_threadsafe`,
since an `asyncio.Queue` is not thread-safe. Generated Python cannot tell the two apart, and that
is the point of stating it once here rather than in the operations below.

It has one consequence outside this page. A message that never reaches the host is a message
nothing can observe, so the interpreter emits an observation for it separately -- see
`observability.md`, which records that as the one cost co-hosting adds rather than removes.

Only the `user` channel is built in the first milestone. The rest is designed now so that
adding a second relationship later does not change the shape of the first. Between agents, this
phase builds what a work item needs -- its inputs, a one-way progress channel, and its result
(`work.md`) -- and request channels, on which a long-lived child answers typed requests
(`agent-classes.md`); the rest of the layer stays design.

## Channels and endpoints

A **channel** connects two endpoints and carries typed messages. It is created by one party --
the **creator** -- which also fixes what may travel in each direction. A channel's contracts
are immutable for its lifetime: changing one means creating a new channel, and messages already
queued are never reinterpreted under a new contract.

An **endpoint** is one end of a channel, named locally. The same channel can be `work` to a
parent and `jobs` to its child; neither name is authoritative, and neither agent can see the
other's. An agent holds a collection of endpoints and discovers them by name:

```python
runtime.channels["user"]
runtime.channels["work"]
```

An endpoint offers exactly three operations (a request channel's ends differ; see below):

- `await endpoint.receive()` -- take the next message, waiting if there is none, as a
  `Delivery`. Removes it from the queue. See "Routing and attribution" for its shape.
- `await endpoint.send(message)` -- hand a message to the channel. Success means accepted for
  delivery, not processed by the recipient.
- `endpoint.pending()` -- how many messages are queued. Consumes nothing.

`pending()` is what makes notification possible without disclosure: the host can tell a model that
three messages are waiting on `control` without putting any of them in its context. The model reads
them by writing code that does so. What the host tells it is rendered from live queue state -- on
each plain channel its delivered and unread counts, and on a request channel the ids of the
requests unread and of those received and not yet answered -- as
`2 messages are waiting on runtime.channels["user"]`, with "requests" as the noun on a request
channel (`agent-classes.md`). It is said in two places that do different things. A round the host
opens on queue state opens with it, and the state that round opened on is the baseline: when a
round ends, the host compares the live state with the state at the last such round, and any change
since -- an arrival, a receive, a reply, a cancellation -- opens the next round when the live state
is not empty, while an unchanged state opens nothing, and so does an empty one, nothing unread and
nothing unanswered. An announcement during a round, a prefix on the next result for what arrived
since the round opened or last announced, reports the counts and moves that baseline nowhere
(`execution-and-rounds.md`, "A round").

## Request channels

A **request channel** is a channel whose two directions are tied. One carries requests of one
type; the other carries replies, each naming the id of the request it answers, and nothing else.
It is how a parent asks a long-lived child a typed question and gets a typed answer back
(`work.md`). A parent declares one per name at spawn, `requests={"fizz": outrig.Request(str,
Thing, doc="...")}`, and an agent class declares one per `async def` method (`agent-classes.md`).

The two ends differ, because each offers what its direction's contract allows:

- The parent's end, `child.channels["fizz"]`, has `request(body) -> handle` and `pending()`.
  `request` checks the body against the request type, hands it to the channel, and returns a
  handle (`work.md`) that settles when the reply arrives; `pending()` counts requests the child
  has not yet taken.
- The child's end, `runtime.channels["fizz"]`, has `receive()` and `pending()`. A received
  delivery carries `id` and `body`, and `await reply(value)` or `await fail(message)` on it is how
  the answer
  goes back ("Routing and attribution"). `send` raises, because that direction's message contract
  is empty: a reply is addressed to a request, and a message that names none has nowhere to go.

Queues are bounded here as on every channel, and a request channel's bound is `requests-max`
itself (default 256, per child, `0003-26`): it counts every request of the child's that has not
settled -- queued, or received and not yet answered -- and a `request()` past it raises
`AgentLimitReached` at once. Nothing waits on the sender's side; cancelling, settling or releasing
frees the count (`agent-classes.md`).

## Contracts

A direction's contract is a set of types. Anything with an obvious JSON form qualifies: `str`,
`int`, `float`, `bool`, `None`, lists of them, string-keyed dicts of them -- and dataclasses
built from the same. A contract of `{str}` is as legitimate as a contract of `{AnalyzeETF}`, so
the `user` channel carrying text is an ordinary channel and not a special case.

Dataclasses are what a non-trivial contract wants, because a named type is what makes a union
discriminable and `help()` informative. They are not a requirement.

```python
@dataclass
class AnalyzeETF:
    symbol: str
    start_date: str
    end_date: str

@dataclass
class ETFResult:
    symbol: str
    annualized_return: float
```

A dataclass here describes data, not behavior. What crosses a channel is field values and an
identifier for the type; each side constructs its own local representation from the agreed
schema. Constructors, `__post_init__`, properties, and object identity do not cross. Decoding
never runs code the sending side chose.

This holds when the peer runs in another thread as much as when it runs in another process. A
co-hosted channel could hand over the object itself, and does not: the two agents would then
share mutable state that the contract says they do not, and code written against a co-hosted peer
would break on one that is not. The serializable subset is the contract, not an artifact of the
transport.

**The serializable subset**, which channel construction validates and outside which it raises:
strings, booleans, integers, finite floats, `None`, typed lists, string-keyed dicts, optionals,
declared unions, and dataclasses whose every field is drawn from the same subset. Infinities and
NaN are excluded because not every encoding carries them faithfully. A declaration outside the
subset is an error at construction, not a failure at first send.

A typed result needs two more, `Literal` and `Annotated`, which `0003-25` adds for
`outrig.schema` (`typed-agents.md`). Whether channel contracts take them too is that task's fork,
recommended yes, so that a channel and a result type never accept different types.

The subset is the same whether a type appears at the top level of a contract or nested inside a
dataclass. There is no rule that the outermost thing must be a dataclass.

A request method on an agent class with several parameters gets a generated message type,
`<Class><Method>Message`: a frozen dataclass whose fields are the parameters, defaults kept
(`agent-classes.md`). It is an ordinary member of the subset with no rule of its own, and the rule
every dataclass in a contract already has applies to it: the type must exist in both kernels by
name, because the parent's end constructs it and the child's end decodes into it. The runtime
binds the generated class in the child under that name, beside the reply type.

A direction may accept a union of types, and the two directions of one channel are unrelated to
each other. A channel may also carry nothing in one direction, which is how a progress feed is
declared.

## Routing and attribution

Sender and destination identity come from the application, not from fields inside the message.
A body that claims to be from another agent is a body with a field in it.

So `receive()` hands back an envelope, always:

```python
delivery = await endpoint.receive()
delivery.id           # the delivery's id; a reply names it
delivery.body         # the message itself
delivery.sender       # who sent it -- "user" on the user channel
delivery.received_at  # when it arrived, in UTC
delivery.skill        # the skill a /name directive resolved to, else None (0003-29)
```

**Decided in `0003-08`: one method returning a `Delivery`, over two.** An earlier draft proposed a
body-only `receive()` beside a `receive_delivery()` for attribution, which kept
`print(await ch.receive())` down to the message. The envelope was chosen for uniformity: one
operation with one shape, where the pair would have been two shapes of the same take, and code
that starts needing the sender does not have to change the call it makes. The cost is `.body` on
every simple use.

`id` is on every delivery, the user channel's included: the host already assigns each queued `msg`
an id beside the queue, and the envelope exposes it rather than inventing a second one. On a request
channel the envelope is a `RequestDelivery`, a `Delivery` subclass that adds `await reply(value)`
and `await fail(message)`, so code that handles deliveries from any channel sees one shape and the
two methods exist only where a reply can go. Putting them on every `Delivery` and raising on a
channel that is not a request channel is `0003-26`'s fork.

## Delivery rules

- Ordering holds within a channel. There is no ordering between channels.
- A successful send means accepted for delivery.
- Receiving removes a message. Notification does not.
- A reply names its request. A reply to a request that has settled -- answered, cancelled, or
  released -- is refused: `reply` raises in the child, and an event records the attempt
  (`work.md`, "A request settles once").
- Queues are bounded; overload is observable rather than silent.
- Closure is observable.

The runtime is one machine, one container, and one interpreter process. It does not promise
recovery across an interpreter crash, and it does not promise exactly-once processing: a receive
can complete immediately before the agent that took the message fails. Co-hosting narrows neither
promise and widens one consequence -- an interpreter crash fails every agent's endpoints together,
where separate interpreters would have failed one.

## Failure

An agent can fail without its interpreter exiting. The interpreter hosts every agent in one process,
so the failures worth naming are an agent whose thread has wedged past recovery
(`runtime-protection.md`), an agent the application has torn down, and the interpreter process
itself going away -- which fails every agent at once. In all three the application marks the
affected endpoints failed, and the surviving side sees one behavior rather than three. From that
side:

- sending to a failed endpoint raises;
- receiving from one raises rather than waiting for a message that cannot arrive;
- `runtime.wait()` reports the failure once, and not again on later waits;
- naming the failed endpoint directly keeps raising, because that is a question with an answer.

Reporting a channel failure does not cancel whatever unrelated operation the surviving agent
was waiting on. What to do about a dead peer is that agent's decision.

## Waiting

`runtime.wait(fs, *, timeout=None, return_when=asyncio.ALL_COMPLETED)` awaits operations while
watching every incoming channel. It mirrors `asyncio.wait` deliberately -- an iterable first,
keyword-only after, `(done, pending)` back, and the `asyncio` constants accepted directly rather
than respelled, `timeout` included. The rule for which way it comes back is that it returns for
asyncio's reasons -- operations satisfying `return_when`, or the timeout expiring -- and raises
for the runtime's, which is input on a channel and any other wake the host delivers. Otherwise it
awaits, for as long as that takes. A completion resumes the awaiting code; it does not call the
model. `execution-and-rounds.md` draws that distinction out, because running the two together is
easy and produces a design for a problem that does not exist.

Three properties make it worth having:

- **Input wins.** If a message is queued and the operation has also completed, it still raises.
  The result is not lost -- it stays in the operation. A request arriving on a request child's
  channel is input like any other (`agent-classes.md`): it ends the wait, and the child's code
  reads it.
- **The operation is not cancelled.** It keeps running, and a later execution awaits it again to
  collect the result. An interruption costs a decision, not the work. This is a property of
  `runtime.wait`, which preserves what it is given as `asyncio.wait` does -- not of binding a
  name. A task reached by a bare `await` does receive a cancellation aimed at the execution;
  `execution-and-rounds.md` measures it and gives the shielding idiom.
- **It does not consume.** The message is still queued afterwards, so unread input makes the
  next wait raise again immediately rather than blocking.

Operations must be futures or handles, not bare coroutines. This was already true because a
coroutine cannot be awaited twice, and asyncio now enforces it outright: `TypeError: Passing
coroutines is forbidden, use tasks explicitly`. A handle (`work.md`) is accepted as it is and
comes back in `done` or `pending` as itself; `runtime.wait` watches its `future`, which is also
what `asyncio.wait`, knowing nothing of handles, takes. Prefer `asyncio.create_task(coro,
name="...")` over `ensure_future`, because `(done, pending)` comes back as sets of tasks and
`Task.get_name()` is what makes them legible. `Task-7` is a worse answer than `ci_run` in a
result, a traceback, and the variable inventory alike. A hosted path is not a coroutine; it goes
in as `path._resolve()` (`hosted-objects.md`, "Calls are awaited").

Other pages depend on the second property. Because the operation survives under its name and
the agent's state survives in its namespace, a round can end with nothing important left in the
conversation -- which is what makes discarding conversation safe at all. `history.md` builds on
it, and `execution-and-rounds.md` carries the surrounding contract.

## The user channel

The primary agent has one, supplied by the application, carrying text in both directions.
**Decided in planning (2026-09-30): children have none in 0.3.** The user addresses the primary
agent only, and what a child is to receive comes through its parent (`work.md`). An earlier draft
let the user address a subagent directly, with nothing relayed through its parent;
`plan/next/children-have-a-user-channel.md` records what that would take.

The name is reserved. An agent class may not declare a request method named `user`
(`agent-classes.md`): its channels take their names from its methods, and `user` is the one name
every agent's endpoint collection keeps for the application, whether or not a child has one in
0.3.

It is not the only way an agent reaches the user, and the distinction matters:

- The model's **own text** is running commentary -- what it is doing and why. Models produce it
  whether or not they are asked to.
- A **send on the user channel** is a deliberate message: a result, an answer, or a
  notification from work that finished after the model stopped writing.

The second is the only one available to code running after a round has ended, which is what
makes it more than a second way of printing. Sending is not a lifecycle operation: it does not
cancel work, destroy the interpreter, or end the round.

In the other direction, a message the user sends is data the agent's code reads: it is announced
by the summary above, by channel and count, and only what the execution that read it observed
enters the model's history (`history.md`, "Why this is newly safe"); a bounded, attributed preview
of a waiting message, shown beside the count without consuming the message, is
`potential/user-input-preview.md`, a feature to test.

### A skill directive

A line the user types as `/name text`, where `name` is a skill (`skills.md`), arrives on this
channel as ordinary text. For a skill with a `skill.py` the body is the line as typed; for an
instruction-only skill it is the skill's `SKILL.md` body followed by the line. Either way `.body`
stays a `str` and the channel's contract does not change. What marks it as a directive is a new
optional field on the `Delivery`, `skill`, naming the skill the application resolved; on every
other message it is `None`. The field comes from the application for the same reason `sender`
does: a body that begins `/review-diff` is a body with some text in it, and only the application
knows that it resolved a directive. The preamble tells the agent that such a message asks it to
run the skill. The field's name is `0003-29`'s.

## A worked relationship

One parent, one child, four channels, contracts differing per direction:

```text
  channel     parent sends        child sends
  --------    ----------------    ------------------
  work        AnalyzeETF          ETFResult
  control     ChangeScope         ScopeAcknowledged
  progress    (nothing)           ProgressUpdate
  fizz        str (request)       Thing (reply)
```

The child sees four endpoints under its own names and reads their contracts by reflection. It
does not register one agent-wide message type and does not have to know what the parent expects
beyond what the endpoints it was handed say. A parent creating several unlike children gives
each whatever channels that relationship needs.

A work handle (`work.md`) has one of these rows built in: its progress is the third, a channel
that carries nothing from parent to child, read on the parent's side as `h.progress`. The result
that settles a submission is on none of the rows. A child gives it with `runtime.complete`, and an
`ETFResult` sent on `work` is a message like any other, which settles nothing.

The fourth row is a request channel. The parent's `request("Hello")` returns a handle, and the
child's `reply(thing)` on the delivery that carried the request settles that handle; a `send` of a
`Thing`, on this channel or any other, would settle nothing, which is why the child's end has no
`send`. The rows illustrate shapes rather than one child's full set: a work child has no request
channels, and a request child's handles have no progress in 0.3 (`work.md`).
