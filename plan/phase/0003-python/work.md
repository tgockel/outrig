# Work and subagents

The subagent tools are MCP-shaped, and until this page there was no Python equivalent designed for
them -- `harness-components.md` listed them as deferred for exactly that reason, and
`agent-placement.md` answers where a child runs without answering how one is made. This page is
the missing half: how a parent creates a child, gives it work, and finds out what happened.

Nothing here is in the first milestone, which runs one agent. It is written now because the
protocol and the interpreter are being shaped around agent identity anyway, and because the 0.2.x
subagent system already solved several of these problems in ways worth carrying rather than
rediscovering.

## Two lifetimes, not one

The distinction the MCP surface got right and the obvious Python translation would lose: an
**agent** and a **work item** do not have the same lifetime. Today `outrig__subagent_release` is a
separate act from a subagent finishing a round, and `doc/concepts/subagents.md` is explicit that
"finishing a round does not end a subagent. It goes idle with its history intact."

Keep that. A child that answers a question still has its interpreter, its namespace, and whatever
it bound. Destroying it to signal completion throws away the expensive part -- and under
`agent-placement.md` the expensive part is a thread and a session module, not a process, which
makes reuse cheaper still.

So: creating an agent and giving it work are different operations, and a convenience that does
both is a convenience rather than the model.

## The handle

A child is an ordinary Python object. Work on it is awaitable, because everything else here is:

```python
child = await runtime.spawn(
    "audit-config",
    prompt="...",
    model="fast",            # a name from the operator's config, not a wire identifier
)
job = child.submit(AnalyzeETF(symbol="VTI", ...))   # returns a handle, not a result
result = await job                                   # typed, validated
```

What a handle has to expose, and why each one is on the list rather than inferred:

- **A stable id** per *submission*, distinct from the name a parent chose, so a late result is
  attributed to the request that produced it rather than to whatever is current.
- **One terminal result per submission, and it does not move.** `await job` resolves once and
  re-awaiting returns the same accepted value, because that is what a future does and pretending
  otherwise makes `runtime.wait` unusable on it. The 0.2.x inbox keeps only the latest value and
  lets a later publication supersede an earlier one; that behavior is worth keeping, but it is a
  *revision stream* and belongs on its own feed with its own cursor rather than inside the
  result. A late publication against a finished submission is recorded as an update or refused
  outright -- it never rewrites a settled result.
- **A documented path to something `asyncio.wait` accepts.** Being awaitable through `__await__`
  is not the same as being a future, and `runtime.wait` mirrors `asyncio.wait`, which takes
  futures. Either the handle exposes one or an adapter is named.
- **Status** separate from result -- running, idle, failed, released -- because "no result yet"
  and "failed without publishing" are different answers and today's system already distinguishes
  them.
- **Progress**, as a channel rather than a return value. `messages.md` already carries a worked
  example with a `progress` channel that flows one way; this is that.
- **Cancellation**, which is one of the four different things `execution-and-rounds.md` warns are
  routinely conflated. Cancelling a work item is not killing the child.
- **Explicit release**, all-or-nothing across a list. The existing release refuses a batch
  containing an unknown or duplicated name rather than half-applying it, because releasing is
  unrecoverable.

## Completion is typed, and it is not a message

A channel contract says what shape a message has. It does not say a task is finished, that a
result belongs to the request that asked for it, or that any claim in it was checked.
`messages.md` is deliberate that sending on the user channel "is not a lifecycle operation," and
that stays true: ordinary chat must not be forced into a result schema.

So a work item ends at its own boundary, which:

1. validates the declared result shape;
2. optionally runs deterministic acceptance checks the application supplied;
3. returns bounded repair feedback when the result is invalid, so the child can fix it;
4. repairs only the final answer -- a rejected result never replays the side effects that
   produced it.

The existing system's shape is worth preserving here. `outrig__set_result` takes a required
`status` enum and a required `body` precisely because an earlier design with two optional fields
let models call it empty, and `required` is the one constraint providers enforce. Publishing
explicitly is also what makes failure honest: a child that runs out of budget never publishes, so
its parent is told it stopped rather than handed a status string dressed as an answer.

Cancelling a waiter does not cancel the submission's canonical result or the work item itself;
explicit work cancellation is a separate operation. This needs saying because the obvious
implementation -- delegating `job.__await__` straight to the shared result future -- gives it away
for free: `execution-and-rounds.md` measures cancellation propagating through a direct await, so
one parent execution being cancelled would take the shared result with it while the child carried
on working. Shielding inside the handle is one way to hold the line, not a mandated one, and
whatever adapter exposes the future to `runtime.wait` has to follow the same ownership.

Every definitive terminal outcome settles the submission's future exactly once: with the accepted
value, or with a documented exception for failure, release, or a budget exhausted before the child
ever published. A parent sitting in an ordinary `await job` is released by all of them -- it does
not have to be watching a status field to learn that its work died. Re-awaiting a settled failure
is as stable as re-awaiting a settled success. An idle child is not by itself a terminal outcome
for the work it was given; that distinction wants deciding when the API is built.

Duplicate completion needs a stated answer rather than an emergent one, and it is the same answer
as above: the terminal result settles once, and anything after it is an update on the revision
feed or a refusal. Which of those it should be is open; that it must be one of them is not.

## Limits belong outside generated code

Depth, fan-out, concurrency, and cumulative model spend are enforced by the host. They already are:
`subagent-depth-max` defaults to 3, `subagent-width-max` to 8, a launch past the width is refused
rather than queued, and every child counts against the budget until released -- including a
finished one whose result was collected. An agent cannot raise its own ceiling, and a child cannot
reach a tool its parent was not granted.

Cumulative model spend is the one that does not exist yet and should. With `history.md` reading
usage that the loop currently discards, a per-tree token budget becomes expressible for the first
time, and a supervision tree that can spawn children is exactly where an unbounded one hurts.

## What the parent sees

Reading is edge-triggered today -- a collect blocks until there is something newer than what the
parent last saw, and reading twice with nothing new in between blocks rather than repeating
itself. That property is worth keeping because it is what makes a wait loop terminate.

Waiting on children is ordinary waiting: a parent awaits its work items, and
`asyncio.FIRST_COMPLETED` is how it reacts to whichever finishes first rather than to all of them.
`execution-and-rounds.md` carries the signature.

## Open questions

- Whether a child's history is visible to its parent at all. `history.md` gives every agent a
  readable store; nothing says a parent may read its child's, and the session-level trust model
  means it would not be a violation so much as a choice. Sibling visibility is a separate question
  with a probably-different answer.
- What a parent's exit does to running children. Today releasing tears down the subtree; with
  reusable agents and long-lived work items that is one policy among several.
- Whether acceptance checks are worth having before a real caller wants one. A validated shape is
  cheap; an evidence check is a small rule engine.
- How a work item's result relates to the child's own `user` channel, given the user can address a
  subagent directly.

## Unverified

- Everything about the existing subagent system cited here is read from `doc/concepts/subagents.md`
  and the 0.2.x implementation, which this phase does not port. It is prior art, not a foundation.
- No part of this has been prototyped. The first milestone runs one agent, so the first honest
  evidence about whether these are the right operations is still ahead.
