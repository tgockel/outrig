# Work and subagents

The subagent tools are MCP-shaped, and the phase began without a Python equivalent for them --
`harness-components.md` listed them as deferred for that reason, and `agent-placement.md` answers
where a child runs without answering how one is made. This page is the other half: how a parent
creates a child, gives it work, and finds out what happened.

**Decided in planning (2026-09-30, extended 2026-10-01): every way in ships in this phase, the
declared forms built on the explicit API.** The explicit API -- `runtime.spawn`, `child.submit`,
request channels, the handle, release -- is `0003-25`. `@outrig.agent` (`typed-agents.md`) is
`0003-26`, implemented on it: each decorated call spawns a fresh child, submits one piece of work,
and releases the child when the result settles. `outrig.Agent` (`agent-classes.md`) is `0003-29`,
implemented on it too: an instance is one long-lived child, each `async def` method a typed
request channel, released with the instance. The 0.2.x subagent system already solved several of
these problems in ways worth keeping rather than rediscovering, and the sections below say where.

## Two lifetimes, not one

The distinction the MCP surface got right and the obvious Python translation would lose: an
**agent** and a **work item** do not have the same lifetime. Today `outrig__subagent_release` is a
separate act from a subagent finishing a round, and `doc/concepts/subagents.md` is explicit that
"finishing a round does not end a subagent. It goes idle with its history intact."

Keep that. A child that answers a question still has its namespace, its history, and whatever it
bound. Destroying it to signal completion throws away the expensive part -- and under
`agent-placement.md` the expensive part is a thread and a session module, not a process, which
makes reuse cheaper still.

So creating an agent and giving it work are different operations, and a convenience that does
both is a convenience rather than the model. `@outrig.agent` is that convenience: it does both,
and releases the child when the work settles. An agent class keeps the two apart as two operations:
constructing the instance creates the child, each method call gives it work, and releasing the
instance ends it.

## Three ways in

```python
# Explicit, a work child: one child, several submissions in turn.
child = await runtime.spawn(
    "audit-config",
    prompt="...",
    model="fast",            # a name from the operator's config, not a wire identifier
)
job = child.submit({"etf": AnalyzeETF(symbol="VTI", ...)},   # each input bound as a variable
                   result=ETFResult)                         # returns a handle, not a result
result = await job                                           # an ETFResult, decoded strictly
job = child.submit({"etf": AnalyzeETF(symbol="BND", ...)},   # the same child, its names
                   result=ETFResult)                         # still bound
...
await runtime.release([child])

# Explicit, a request child: one child, typed request channels, many requests in flight.
child = await runtime.spawn(
    "fizzer",
    prompt="...",
    requests={"fizz": outrig.Request(str, Thing, doc="...")},   # (str, Thing) is shorthand
)
h = child.channels["fizz"].request("Hello")                  # sent now; h is the handle
thing = await h                                              # a Thing, decoded strictly
...
await runtime.release([child])

# Declared, a work child per call: fresh, released when the call settles.
review = await analyze_code(section)

# Declared, a request child per instance: released with it (agent-classes.md).
async with FooAgent() as foo:
    thing = await foo.fizz("Hello")
```

`submit` takes the work's inputs as a dict of named values and binds each one as a variable of
that name in the child's namespace -- here `etf`, an `AnalyzeETF` (`messages.md`). `result=` is
the type the child's completion is decoded into, strictly (`typed-agents.md`), so `await job`
gives an `ETFResult` or raises.

`requests=` declares a request channel per name (`messages.md`, "Request channels"): the type a
request carries, the type its reply carries, and a description the child's instructions include.
`child.channels["fizz"].request(body)` checks the body against the request type, sends it, and
returns a handle; `pending()` counts the requests the child has not yet taken. In the child, `await
runtime.channels["fizz"].receive()` gives a delivery with an `id` and a `body`, and `await
d.reply(value)` or `await d.fail(message)` on it settles the handle that sent it. The spellings are
`0003-25`'s, `0003-26`'s and `0003-29`'s.

**Why all of them.** A fresh child per call is the right default for independent typed work.
Nothing from one call is present in the next, a `gather` over a hundred sections needs no lifetime
code, and a failure ends with its call. What it cannot do is keep a child. A work child kept
through the explicit API takes submissions in turn, and a second submission reaches the names the
first one bound and the history it built, which no fresh child has. A request child keeps its
context the same way and takes many requests at once, each answered on the delivery that carried
it; an agent class is that child declared as a Python class, one method per channel. Each form
covers what the others do not, and the declared forms add little code beside the API they are
built on.

## The handle

A child is an ordinary Python object, and work on it is awaitable, because everything else here
is. `child.submit(...)` returns a handle; so do `child.channels["fizz"].request(...)`, a decorated
call, and a request method on an agent class. Each sends at once and there is no separate start:
the handle is what a caller holds from the moment the request exists.

What a handle has to expose, and why each one is on the list rather than inferred:

- **A stable id** per *request* -- a submission or a channel request -- distinct from the name a
  parent chose, so a late result is attributed to the request that produced it rather than to
  whatever is current.
- **One terminal result per request, and it does not move.** `await job` resolves once and
  re-awaiting returns the same accepted value, because that is what a future does and pretending
  otherwise makes `runtime.wait` unusable on it. The 0.2.x inbox keeps only the latest value and
  lets a later publication supersede an earlier one; that behavior is worth keeping, but it is a
  *revision stream* and belongs on its own feed with its own cursor rather than inside the
  result. A late completion or reply against a settled request is refused and recorded as an
  event -- it never rewrites a settled result.
- **`done()` and `result()`**, as a future has them: `done()` says whether the request has
  settled, and `result()` returns the accepted value or raises the failure, and raises if nothing
  has settled yet. They are what code that is not awaiting asks.
- **`future`, a shielded view the `asyncio` machinery accepts.** Being awaitable through
  `__await__` is not the same as being a future, and `asyncio.wait` takes futures. `runtime.wait`
  accepts a handle directly (`messages.md`); `asyncio.wait` takes `h.future`. Cancelling the view
  cancels a waiter, never the request.
- **Status** separate from result -- a submission queued behind another, running, accepted or
  failed; a request waiting for the child to take it, past the queue bound included, received,
  accepted or failed; a child running, idle, wedged or released; the spellings are `0003-25`'s --
  because "no result yet" and "failed without publishing" are different answers and today's
  system already distinguishes them. For a channel request it also says whether the child has
  taken the request yet, which decides what cancelling it does.
- **Progress**, as a channel rather than a return value: a one-way endpoint the parent reads as
  `h.progress`, yielding `Delivery` envelopes. It is the `progress` row of `messages.md`'s worked
  example. In 0.3 only a work child's handle has one;
  `plan/next/progress-channels-on-agent-classes.md` is the request child's.
- **Cancellation**, which is one of the four different things `execution-and-rounds.md` warns are
  routinely conflated. Cancelling a work item is not killing the child. For a decorated call,
  whose child exists for that one item, it releases the child as well. For a channel request,
  what it does depends on whether the child has received it: before, the request is removed from
  the queue and the child never sees it; after, the delivery stays with the child, and its `reply`
  or `fail` raises `RequestCancelled`. Either way the handle settles as cancelled.
- **Explicit release**, all-or-nothing across a list. The existing release refuses a batch
  containing an unknown or duplicated name rather than half-applying it, because releasing is
  unrecoverable: it closes the child's subtree the way a session close does (`lifecycle.md`,
  "Releasing a child"). A decorated call's child needs no explicit release: it is released when
  its call settles. An agent class instance is released with `await foo.release()`, or on leaving
  `async with FooAgent() as foo:`, and every request outstanding on it settles with
  `AgentReleased`.

## A request settles once

Everything a parent asks of a child is a **request** underneath: a submission to a work child, or
a body sent on a request channel. Each has an id, a reply type, and a handle in the parent, and
each is answered once. The two kinds of child answer differently, and never mix:

- A **work child** -- a decorated call's, or `spawn` with `submit` -- has its inputs bound as
  variables, one active submission at a time, and answers with `await runtime.complete(value)`.
- A **request child** -- an agent class instance, or `spawn(..., requests=...)` -- receives each
  request explicitly as a delivery and answers it with `await d.reply(value)` or
  `await d.fail(message)`. It has no `runtime.complete`, and a work child has no request channels.
  The kind is fixed at spawn.

`runtime.complete`, `reply` and `fail` are all awaited: each returns once the request has settled,
or raises its problems (`0003-25`, fork 3).

**Rejected: `runtime.complete` answering "the one outstanding request" in a request child.** It
would have let the class form reuse the work child's completion, and it is racy and confusing:
with two requests received, which one `complete` answers depends on an arrival order the child's
code never saw, and a reply that names its request has no such question.

**A work child's result is not a message.** A channel contract says what shape a message has. It
does not say a task is finished, that a result belongs to the request that asked for it, or that
any claim in it was checked. `messages.md` is deliberate that sending on the user channel "is not
a lifecycle operation," and that stays true: ordinary chat must not be forced into a result
schema. **Decided in planning (2026-09-30): a work child completes with a Python call,
`await runtime.complete(value)` in its own code** (the spelling is `0003-25`'s), not with a model
tool. The result can then be built from objects that live only in Python and never pass through
the model, and the child's only model tool stays `submit_python`. `typed-agents.md` has the
reasoning, including why a tool would not have provided enforcement by the provider for a
map-valued schema. A reply on a request channel is the same kind of value: it crosses as data,
decoded strictly into the reply type on the parent's side, and it is the `reply` call on the
delivery, not a `send` on the channel, that settles the handle.

A completion and a reply go through one path, which:

1. validates the declared result shape, decoding strictly (`typed-agents.md`);
2. runs the application's own checks, when the caller supplied a pure validator -- `validate=` on
   a declaration;
3. returns bounded repair feedback when the value is invalid -- raised in the child's code at its
   `complete` or `reply` call, with the JSON path of each problem -- so the child can fix it;
4. repairs only the final answer: a rejected value never replays the side effects that produced
   it.

**A child whose round ends with a request unanswered is prompted again.** For a work child that is a
round without a completion that passed; for a request child, a round that ends with requests waiting
or received-but-unanswered is followed by one listing them by id (`agent-classes.md`). Each failed
check and each such prompt counts toward a per-request attempt limit, 3 by default, and past it the
request settles with `CompletionRejected`, carrying the last value and its problems. That answers
what an earlier draft of this page left open, whether an idle child is a terminal outcome for the
work it was given: it is not, until the limit is reached. `await d.fail(message)` is the child's own
way to end a request without a value, and settles it with `AgentRequestFailed`.

The existing system's lesson still applies, in a different place. `outrig__set_result` takes a
required `status` enum and a required `body` because an earlier design with two optional fields
let models call it empty, and `required` was the one constraint providers enforced. A Python call
is not a schema a provider checks, so the strict decoder enforces `required` itself,
required-but-nullable fields included, and an empty completion or reply gets a repair message
rather than acceptance. Publishing explicitly is still what makes failure honest: a child that
runs out of budget never completes, so its parent is told it stopped rather than handed a status
string in place of an answer.

Cancelling a waiter does not cancel the request's canonical result or the request itself; explicit
cancellation is the handle's `cancel()`, a separate operation. This needs saying because the
obvious implementation -- delegating `job.__await__` straight to the shared result future --
breaks it: `execution-and-rounds.md` measures cancellation propagating through a direct await, so
one parent execution being cancelled would take the shared result with it while the child carried
on working. The handle's `future` is a shielded view for that reason, and whatever `runtime.wait`
does with a handle has to follow the same ownership.

Every definitive terminal outcome settles the request's future exactly once: with the accepted
value, or with a documented exception for a rejected completion or reply, a `fail`, a
cancellation, a release, a budget exhausted before the child answered, or the session closing.
The exceptions share one base, `outrig.AgentError`, over `CompletionRejected`, `AgentReleased`,
`AgentRequestFailed`, `RequestCancelled` and the budget and closing errors, so a caller that wants
"anything went wrong" has one name for it; the spellings are `0003-25`'s. A parent in an ordinary
`await job` is released by all of them -- it does not have to be watching a status field to learn
that its request ended. Re-awaiting a settled failure is as stable as re-awaiting a settled
success.

A second completion, or a reply naming a request that has already settled -- answered, cancelled,
or released -- is refused: the `complete` or `reply` call raises in the child's code, and the
runtime records the attempt as an event in the request family (`observability.md`). It is never a
new result, and it is not an edition on the revision feed either: a reply to a settled request is
a defect in the child's round, not a later version of the answer.

## Limits belong outside generated code

Depth and cumulative model spend are enforced by the host. An agent cannot raise its own ceiling,
and a child cannot reach a tool its parent was not granted: its only tool is `submit_python`, and
the bindings it reaches are the session's (`typed-agents.md`).

**Depth.** `subagent-depth-max` keeps its default of 3. The primary agent is at depth 1, an agent
at the limit cannot launch, and a launch from one fails at once rather than waiting.

**No width cap.** **Decided in planning (2026-10-01): `run-new` does not read
`subagent-width-max`**; the key stays `run`'s, at its default of 8 and ceiling of 16, and the depth
limit and the token budget are the bounds on a tree. An earlier decision had a launch past the cap
wait for a slot and raised the shared default to 16. It was revisited with the agent class: an
idle instance, kept for its context, would have held a slot for as long as its caller kept it,
and counting idle children against a cap is rejected (`agent-classes.md`). A `gather` over any
number of sections is then ordinary code, and a skill's own semaphore is the concurrency it wants
(`typed-agents.md`). What this leaves unbounded is how many kernels run at once -- every child is
a thread in one interpreter process, under one memory ceiling and one GIL (`agent-placement.md`),
and a 500-way gather is 500 of them. A cap on children *working* at once, counted per launching
agent, is `plan/next/width-cap-on-working-children.md`.

**Spend.** A per-tree token budget is new, built with the children in `0003-25`. With usage now
read rather than discarded (`observability.md`) it is expressible for the first time, and a tree
that can spawn children is where an unbounded one costs most. A request whose child runs out
before answering settles with a budget error. A child's model usage is attributed to the child's
round and added to the skill invocation the child ran under and to the main agent's round. A round
in a request child may answer several requests; its usage is attributed to the round and not
divided among them, and each request's settled event names the rounds it spanned, which is
`0003-25`'s fork.

## What the parent sees

Reading is edge-triggered in 0.2.x -- a collect blocks until there is something newer than what the
parent last saw, and reading twice with nothing new in between blocks rather than repeating itself
-- because that is what makes a wait loop terminate. In Python the same property comes from
`(done, pending)`: a loop that waits on `pending` and drops what is `done` terminates, and a
settled handle passed in again returns at once, as an uncollected 0.2.x subagent does.

Waiting on children is ordinary waiting: a parent awaits its work items, and
`asyncio.FIRST_COMPLETED` is how it reacts to whichever finishes first rather than to all of them.
`execution-and-rounds.md` carries the signature.

## Children and the user

**Decided in planning (2026-09-30): children have no user channel in 0.3.** The user addresses the
primary agent only, and anything meant for a child goes through its parent's code. This answers
the question an earlier draft of this page left open -- how a work item's result relates to the
child's own `user` channel, given the user could address a child directly: in 0.3 the user
cannot. The REPL has one prompt and sends every line to the primary, and addressing a child would
need a way to name it and a way to show which agent answered; a decorated call's child also exists
only as long as its call. `plan/next/children-have-a-user-channel.md` records what giving children
one would take.

## When a parent stops

What a close does to children -- their work cancelled, and every waiter on it settled with a
documented error rather than left pending -- is `lifecycle.md`'s. So is what releasing one child
does, explicitly or when a decorated call settles: it closes that child's subtree the way a
session close does. Its work is cancelled and every waiter settled, its own children are
released, its pending escalations are cancelled, its running execution is interrupted, and its
dispatched hosted calls finish or are reported `unknown` (`lifecycle.md`, "Releasing a child").

Releasing an agent class instance is the same release with three ways to reach it: `await
foo.release()`, which is idempotent; leaving `async with FooAgent() as foo:`; and the runtime's
backstop when an instance is collected unreleased. The backstop is a finalizer, which cannot await
and runs on whatever thread dropped the last reference, so it schedules the release on the
constructing kernel's event loop, and that release is recorded as `agent.instance.collected`. The
runtime's record of each unsettled request holds the instance, so a child is never released while
a request is in flight, even when agent code has dropped both the instance and the handle:
`FooAgent().fizz("x")` with neither reference kept runs to settlement, then the child is released
and `agent.instance.collected` is recorded. The handle wrapper itself is collectable, and a handle
collected unsettled is evented and not cancelled: nothing can read its result, but a cancelled
bare `await` is the common way to reach that state, and cancelling the request there would make a
cancelled waiter cancel the work after all, which the handle's shielding exists to prevent. When
the last request settles and no reference to the instance remains, the finalizer releases the
child. A module-level instance in a reloaded skill lives until it is collected. However the
release is reached, every request outstanding on the instance settles with `AgentReleased`.
`0003-25` builds and tests the release and `0003-29` the instance. This page's part is the rule
those depend on: every terminal outcome settles a request exactly once.

## Open questions

- Whether a child's history is visible to its parent at all. `history.md` gives every agent a
  readable store; nothing says a parent may read its child's, and the session-level trust model
  means it would not be a violation so much as a choice. Sibling visibility is a separate question
  with a probably-different answer.
- Whether `runtime.wait` watches a handle's progress. It watches every channel and input wins, so
  if progress endpoints were among them, a parent with sixteen children reporting progress would
  have every wait ended by them.
- Whether a request method on an agent class takes `validate=` as a declaration does: where it is
  named, since a method has no decorator argument of its own, and whether the validator receives
  the request body, as a declaration's validator receives the inputs. `0003-29` leaves it open.

## Unverified

- Everything about the existing subagent system cited here is read from `doc/concepts/subagents.md`
  and the 0.2.x implementation, which this phase does not port. It is prior art, not a foundation.
- No part of this has been built. The first milestone ran one agent, and `0003-25` is where the
  first evidence about whether these are the right operations will come from.
