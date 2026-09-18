# Messages

How agents address each other and the user. This is the layer generated Python sees: channels,
endpoints, the contracts they carry, and the rules delivery follows. Messages are carried as
NDJSON; that is all this page has to say about the framing.

Only the `user` channel is built in the first milestone. The rest is designed now so that
adding a second relationship later does not change the shape of the first.

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

An endpoint offers exactly three operations:

- `await endpoint.receive()` -- take the next message, waiting if there is none. Removes it
  from the queue. See "Routing and attribution" for the form that also carries the sender.
- `await endpoint.send(message)` -- hand a message to the channel. Success means accepted for
  delivery, not processed by the recipient.
- `endpoint.pending()` -- how many messages are queued. Consumes nothing.

`pending()` is what makes notification possible without disclosure: the host can tell a model
that three messages are waiting on `control` without putting any of them in its context. The
model reads them by writing code that does so.

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

**The serializable subset**, which channel construction validates and outside which it raises:
strings, booleans, integers, finite floats, `None`, typed lists, string-keyed dicts, optionals,
declared unions, and dataclasses whose every field is drawn from the same subset. Infinities and
NaN are excluded because not every encoding carries them faithfully. A declaration outside the
subset is an error at construction, not a failure at first send.

The subset is the same whether a type appears at the top level of a contract or nested inside a
dataclass. There is no rule that the outermost thing must be a dataclass.

A direction may accept a union of types, and the two directions of one channel are unrelated to
each other. A channel may also carry nothing in one direction, which is how a progress feed is
declared.

## Routing and attribution

Sender and destination identity come from the application, not from fields inside the message.
A body that claims to be from another agent is a body with a field in it.

Which raises the question of what `receive()` hands back. Two calls rather than an envelope on
every message:

```python
body     = await endpoint.receive()           # the message itself
delivery = await endpoint.receive_delivery()  # .body, plus .sender and .received_at
```

The common case stays readable -- `print(await ch.receive())` shows the message, not a wrapper
around it -- and code that needs attribution asks for it. Both consume; they are two shapes of
the same operation, not a peek and a take.

**This is a proposal, not a settled call.** The alternative is one method returning an envelope
always, which is more uniform and makes every simple use noisier. It should be decided before
the first channel task is written, because it is in the signature.

## Delivery rules

- Ordering holds within a channel. There is no ordering between channels.
- A successful send means accepted for delivery.
- Receiving removes a message. Notification does not.
- Queues are bounded; overload is observable rather than silent.
- Closure is observable.

The runtime is one machine and one container. It does not promise recovery across an
interpreter crash, and it does not promise exactly-once processing: a receive can complete
immediately before the interpreter that took the message fails.

## Failure

When an interpreter exits, the application marks its endpoints failed. From the surviving side:

- sending to a failed endpoint raises;
- receiving from one raises rather than waiting for a message that cannot arrive;
- `runtime.wait()` reports the failure once, and not again on later waits;
- naming the failed endpoint directly keeps raising, because that is a question with an answer.

Reporting a channel failure does not cancel whatever unrelated operation the surviving agent
was waiting on. What to do about a dead peer is that agent's decision.

## Waiting

`runtime.wait(operation, label=None)` awaits an operation while watching every incoming
channel. It returns the operation's result, raises the operation's exception, or raises
`MessageAvailable` naming the channels that have input.

Three properties make it worth having:

- **Input wins.** If a message is queued and the operation has also completed, it still raises.
  The result is not lost -- it stays in the operation.
- **The operation is not cancelled.** It keeps running under whatever name it is bound to, and
  a later execution awaits it again to collect the result. An interruption costs a decision,
  not the work.
- **It does not consume.** The message is still queued afterwards, so unread input makes the
  next wait raise again immediately rather than blocking.

The operation must be a future -- `asyncio.gather(...)` or `asyncio.ensure_future(...)` -- and
not a bare coroutine, which cannot be awaited twice.

## The user channel

Every agent has one, supplied by the application, carrying text in both directions. The user
can address a subagent directly; nothing has to relay through its parent.

It is not the only way an agent reaches the user, and the distinction matters:

- The model's **own text** is running commentary -- what it is doing and why. Models produce it
  whether or not they are asked to.
- A **send on the user channel** is a deliberate message: a result, an answer, or a
  notification from work that finished after the model stopped writing.

The second is the only one available to code running after a turn has ended, which is what
makes it more than a second way of printing. Sending is not a lifecycle operation: it does not
cancel work, destroy the interpreter, or end the turn.

## A worked relationship

One parent, one child, three channels, contracts differing per direction:

```text
  channel     parent sends        child sends
  --------    ----------------    ------------------
  work        AnalyzeETF          ETFResult
  control     ChangeScope         ScopeAcknowledged
  progress    (nothing)           ProgressUpdate
```

The child sees three endpoints under its own names and reads their contracts by reflection. It
does not register one agent-wide message type and does not have to know what the parent expects
beyond what the endpoints it was handed say. A parent creating several unlike children gives
each whatever channels that relationship needs.
