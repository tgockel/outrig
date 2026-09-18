# Executions and rounds

Two units the phase leaned on without defining. An **execution** is one submission of Python to an
agent's kernel. A **round** is one prompt or message, the agent working through however many model
calls it takes, and yielding control back. Neither is a turn, and the places where the difference
matters are the places the design was vague.

This is a contract rather than a discovery. Most of it describes behavior the ported interpreter
already has and never wrote down.

## An execution

One `exec` message in, one result out, carrying a stable id the host assigns. Source is compiled
under `<execution>` so a traceback names something the model recognizes, and with top-level
`await` permitted, so a submission is a coroutine body rather than a script.

### Outcomes

Three, and they are distinguishable because a model that cannot tell them apart cannot respond to
them differently.

| outcome   | meaning                                                              |
|-----------|----------------------------------------------------------------------|
| `ok`      | ran to completion; a trailing expression's value was echoed           |
| `error`   | raised; the traceback is the result, bounded                          |
| `unknown` | the host cannot say; the interpreter stopped answering or died        |

`unknown` is the one worth insisting on. A result that never arrives is not a failure, and
recording it as one invites the caller to retry something that may well have happened.

It covers two different situations and the host should not collapse them. A confirmed interpreter
exit means the execution is over, however it ended. An execution that simply has not answered may
still be running, and its slot is not free. The second wants a state of its own -- unresolved,
holding the id -- so that a late reply resolves the uncertainty as a new observation rather than
by rewriting the old one. Neither implies crash recovery, and neither licenses a retry.

A compile failure is an `error` like any other, but worth distinguishing in the message because
nothing ran: there is no partial state to reason about, and the fix is textual.

The echo on `ok` calls `repr()`, which is agent-written code, and that needs reconciling with
`discovery.md`'s rule that automatic observation never runs it. The line is where the rendering
happens: the echo runs **inside** the agent's own execution, so a hostile or looping `__repr__`
wedges that execution and nothing else, and the interrupt path already covers it. The inventory
runs **outside** any execution, at the host's request, and must therefore stay to names and types.
Rendering that cannot be attributed to an execution cannot be allowed to run agent code, and a
byte cap on the output does not bound the work of producing it.

### No rollback, ever

If a submission writes a file and then raises, the write happened. The interpreter has no
transaction and this design will not pretend otherwise. Two consequences, because the alternative
is an agent that quietly does things twice:

- A failed execution may have completed some of its effects. The traceback says where it stopped,
  not what it undid.
- A **lost** result -- `unknown` -- must never be retried automatically. Re-running a submission
  whose effects are unknown is how one `git push` becomes two.

### One at a time

The kernel holds a single foreground slot and **rejects** a second submission rather than queueing
it: an execution is already running in that agent's kernel. The slot is per kernel rather than
per session, so one agent running does not stop another. Rejection is the right default -- a queue
would let a model stack up work it can no longer reason about, and the rejection is itself
information. What the rejection must carry is which execution holds the slot, so the caller can
decide between waiting and interrupting.

Background tasks the agent started with `create_task` are not in the slot, and nothing cancels
them at a round boundary. But they do not own themselves, which an earlier draft claimed: asyncio
tracks tasks weakly, and a task whose last reference is dropped may be collected before it
finishes. The documented contract is to keep a reference.

The ordinary idiom already satisfies it -- `ci_run = asyncio.create_task(...)` stays bound in the
session namespace, which is the same habit that makes the namespace the durable state. What is
not safe is a detached `create_task(...)` whose handle is never bound or is later overwritten.
Whether the runtime should retain such tasks itself, and report their exceptions when nobody
awaits them, is the open decision; promising unconditional survival is the one option that is
simply wrong.

### Four different cancellations

Routinely confused, and not interchangeable. Cancelling a **wait** stops watching and leaves the
operation running. Cancelling a **task** raises `CancelledError` inside it and it may refuse.
Killing a **subprocess** is a signal to something with its own opinion about dying. Cancelling an
**accepted remote operation** may be impossible, and its effect may already have happened. A
design that says only "cancel" has not said anything.

## A round

One prompt or message, the agent working, and yielding control back. It yields when the model
stops emitting or when a limit fires.

A round has no fixed duration, and this is the point most easily misread. An execution may await
something slow -- a build, a deployment, CI, a human review -- and the round simply lasts that
long. The model is not called during the wait. Nothing is burned. When the operation completes the
agent's own code resumes and the round continues, and when the agent finally stops writing, the
round ends.

**A round is a label, not a state transition.** It is a boundary this design drew for accounting --
what a stretch of work cost, what to show in a record, where a history window can cut -- and
drawing it discards nothing. The conversation does not restart at one. A round that resumes after
a twenty-minute wait opens on the tool call that waited and the result it returned, sitting in the
transcript where every other tool result sits:

```text
assistant  submit_python("done, pending = await runtime.wait({ci_run})\n"
                         "[t.get_name() for t in done]")
tool       ['ci_run']
```

The trailing expression is doing work there and is not decoration. An assignment echoes nothing,
so a submission that only assigns comes back with an empty result -- correct, and useless as an
observation. The agent says what it wants to see.

That is enough to carry on from, so nothing has to be handed across the boundary and no mechanism
is needed to hand it. An agent is not told which condition woke it, because the tool result already
says. This is why there is no terminal act, no wake payload, and no registration call in this
design: they would all be machinery for reconstructing something that was never lost.

It does put one requirement on `history.md`, and it is already there: the trailing tool call and
its result are not ordinary eviction candidates. Evicting them is the one way to make a round
boundary lose information, and the budget section protects the current turn and its protocol data
for exactly this reason.

### What a completion does, precisely

The rule that "a background operation's completion does not independently trigger a model
invocation" is about **the model**, not about the agent's code, and running the two together
produces a design for a problem that does not exist.

- A completion **does** resume code that is already awaiting it. `runtime.wait` is built on
  `asyncio.wait(..., return_when=FIRST_COMPLETED)`; when the operation finishes, the await
  returns and the next line runs. The event loop does this and nothing has to arrange it.
- A completion **does not** call the model. An operation nobody is awaiting finishes quietly, its
  result sitting in the future until something collects it. The model is not summoned by work
  finishing.

So an agent that wants to wait for CI writes the obvious thing and it works:

```python
ci_run = asyncio.create_task(watch_ci(pr), name="ci_run")
review = asyncio.create_task(watch_review(pr), name="review")

done, pending = await runtime.wait({ci_run, review},
                                   return_when=asyncio.FIRST_COMPLETED)
```

The execution runs until one of them finishes -- minutes, hours -- and then continues. It reports
`ok` like any other.

### A bare `await` is fine, and gives up two things

Nothing requires `runtime.wait`. `res = await thing` is ordinary Python, top-level await is
enabled, and for a single operation it is the better spelling: it hands back the value, where
`runtime.wait` hands back `(done, pending)` as sets of tasks and leaves the collecting to you.

Two things differ, and the second matters more than the first.

**It does not watch the channels.** No `MessageAvailable` can raise out of it, so a message
arriving while it blocks waits in the queue until the await finishes on its own.

**Cancelling the execution propagates into what it awaits.** Measured: with the execution
cancelled, a task reached by a bare `await` receives the cancellation, while the same task behind
`runtime.wait` or `asyncio.shield` keeps running.

| the execution awaits via        | the awaited task afterwards |
|---------------------------------|------------------------------|
| `await ci_run`                  | cancellation reaches it      |
| `await asyncio.shield(ci_run)`  | still running                |
| `runtime.wait({ci_run})`        | still running                |

Binding `ci_run` to a name keeps a *reference*; it does not shield it. So `messages.md`'s promise
that an interrupted wait leaves the operation alive is a property of `runtime.wait`, not of
awaiting generally, and an agent that wants both a bare await and a surviving task writes
`await asyncio.shield(ci_run)`.

Both differences are agent choices -- whether to be reachable, and whether the work outlives a
cancelled execution -- and neither is a thing either name suggests, which is why `discovery.md`
puts this in the preamble rather than a docstring.

The rule of thumb is duration, not correctness. A bare await on something quick is what anyone
would write. A bare await on something that may take an hour is a decision to be unreachable for
an hour -- and on something that never resolves, it is an execution that holds its slot until
someone cancels it. `runtime-protection.md` records what cancelling it takes, which is not the
same mechanism a wedge takes.

### Being redirected mid-wait

The agent is not stranded during a long wait, which is what makes blocking acceptable rather than
merely tolerable. `runtime.wait` watches every channel as well as the operations, and a message
arriving raises `MessageAvailable`: the execution ends, the slot frees, and the user's new
instruction is handled. The operation is not cancelled -- it keeps running under its name, and a
later execution awaits it again.

That is `messages.md`'s "input wins" property, and it is the answer to "what if I change my mind
while it waits."

**A redirection stays in the same round.** Since a round is a label, this is a convention rather
than a discovery, and it is worth picking deliberately because limits hang off it. The agent never
stopped being awake: a tool call returned, the model was called again, and the transcript runs
straight through. Counting that as a new round would also reset the per-round turn allowance, which
would let a chatty user -- or an agent that provoked messages -- inflate its own budget without
limit. A round ends when the model stops emitting, and a message arriving mid-wait is not that.

The interrupted tool call and its result stay in the transcript either way, so nothing about the
convention risks losing the continuation. It decides what a round id spans, what a token total
attributes to, and where a limit resets -- and nothing else.

## `runtime.wait` mirrors `asyncio.wait`

The current shape takes a single operation and offers no way to say "whichever finishes first",
which the CI-and-review case needs. Mirroring asyncio fixes it without inventing anything:

```python
runtime.wait(fs, *, timeout=None, return_when=asyncio.ALL_COMPLETED)
```

An iterable first, keyword-only after, returning `(done, pending)`. `asyncio.FIRST_COMPLETED`,
`FIRST_EXCEPTION`, and `ALL_COMPLETED` are accepted directly -- in CPython they are ordinary
strings, so taking the constants costs nothing and respelling them would buy nothing.

What it is, in one sentence: `asyncio.wait`, which additionally watches the agent's input channels
and is tied to the round. Everything else follows from that, including the one rule worth
memorizing.

That sentence belongs in the preamble rather than in a docstring an agent has to think to read.
The signature is identical to asyncio's on purpose, so it discloses nothing about the channel
watching -- `discovery.md` uses this as the case that sets where the preamble's edge is.

**It returns for asyncio's reasons and raises for the runtime's.** The operations satisfying
`return_when`, and the timeout expiring, are things a plain `asyncio.wait` would have returned
from, so they come back as `(done, pending)`. Input arriving on a channel is something only the
runtime knows about, so it raises. If you could have written it with `asyncio.wait`, it returns.

That extends to runtime wakes generally, not just user input. A bump the host delivers for its own
reasons -- an operator checking in, a limit coming into view, a condition it is tracking on the
agent's behalf -- is the same category and takes the same path. The agent does not have to
enumerate them, and adding one later does not change a signature.

Three consequences, all improvements:

**The return shape changes.** Today `runtime.wait(operation)` returns that operation's result or
raises its exception. With N operations "the result" is not well defined, and `(done, pending)` is
what a Python programmer already expects. The three properties the design depends on are
untouched: input still wins over a completed operation, the operation is still not cancelled, and
a pending message is still not consumed.

**Bare coroutines are refused, for a better reason.** `messages.md` already required a future
rather than a coroutine because a coroutine cannot be awaited twice. Asyncio now refuses them
outright -- `TypeError: Passing coroutines is forbidden, use tasks explicitly` -- so the rule is
the language's rather than OutRig's.

**`timeout` is honored, and means what asyncio means by it.** A model will reach for it without
being told -- it is in the signature it already knows -- so the semantics have to be asyncio's
rather than the stronger thing the English suggests. It returns `(done, pending)` when it expires,
with `done` possibly empty. It does **not** cancel what is still pending and does **not** raise
`TimeoutError`. So it says "come back to this in an hour", not "give up on it"; giving up is a
separate act on the tasks in `pending`. For the same reason a task that failed is a *completed*
task sitting in `done` with its exception retrievable, not an exception raised out of the wait.

It earns its place beyond convenience. An execution awaiting something that will never resolve is
an execution that holds its slot forever while the liveness probe reports a healthy interpreter --
the loop is turning, so nothing is wrong by any measure the probe has. A user message still frees
it, which is the backstop. A timeout is the agent's own guard against needing one.

Name the tasks. `asyncio.create_task(watch_ci(pr), name="ci_run")` costs one keyword and makes
`done` legible when it comes back, in a traceback, and in the variable inventory. An unnamed task
is `Task-7` everywhere it appears.

## Open questions

- Whether rejecting a second execution stays right once agents are co-hosted, or whether the
  rejection should name the holder's agent as well as its id.
- What `unknown` does to the round. It is neither a completion nor a failure, and the host has to
  decide something.
- Whether a very long round wants any operator visibility of its own. Nothing is wrong with a
  round that lasts an hour, but nothing currently says it is happening either.
- Whether `MessageAvailable` should derive from `BaseException` rather than `Exception`. A stray
  `except Exception:` around a wait swallows it today and lets the agent run on past a redirection
  it never saw.

## Unverified

- `asyncio.wait`'s signature, the string-valued `return_when` constants, `Task.get_name()`, and
  the coroutine `TypeError` were checked against the pinned payload.
- That a long await behaves as described is read from the prototype's `Runtime.wait`, which loops
  on `asyncio.wait(..., FIRST_COMPLETED)` over the operation and a message event. It was not
  exercised against a genuinely long-running operation.
