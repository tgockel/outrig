# Runtime protection

Generated code is arbitrary code. Most of what it does wrong is accidental -- a loop that never
terminates, a print in a loop, a recursion that never bottoms out -- and none of that is an
attack. It still has to be survivable, because an agent that can brick its own session on a bad
comprehension is not usable.

Part of this is already designed and proven on the prototype. That part is recorded here so it
is ported deliberately rather than rediscovered. The rest is a later milestone.

## The wedge, and why it is the hard one

Synchronous Python that never yields blocks the interpreter's event loop. Everything else goes
with it: the loop is what would report the problem, accept new work, or deliver a message.
Reproduced against the prototype with `while True: pass`:

```text
  result within 3s?      none
  inventory answers?     none
  later exec answers?    none
  closing stdin exits?   no -- still running
  plain SIGINT frees it? no
```

Two things worth drawing out. Closing stdin does not help, because the clean-exit path runs on
the loop. And a plain SIGINT to the process does not help either, because `asyncio.run` installs
a handler that defers the interrupt until the loop next runs a callback, which is exactly what a
wedged loop never does.

The host side is no better placed by default: its request is waiting on a reply that will never
come, so the turn hangs rather than failing.

## What the prototype established

**An `interrupt` message handled on the reader thread.** The kernel's stdin reader is a separate
thread and keeps running throughout. Handling the message *there*, rather than handing it to the
loop with `call_soon_threadsafe`, is the whole trick -- that queue is precisely what a wedged
loop is not draining. The handler raises SIGINT; a handler installed from inside the loop
overrides the one `asyncio.run` set, and turns it into a `KeyboardInterrupt` inside the running
execution. The execution then comes back as an ordinary error result with a traceback.

Measured: 5 of 5 trials recovered with the session still usable afterwards.

**Two alternatives rejected with evidence, not on taste.**

- `ctypes.pythonapi.PyThreadState_SetAsyncExc` -- `ctypes.pythonapi` is `None` in a statically
  linked build. It would have silently done nothing.
- `_thread.interrupt_main()` -- killed the process in 9 of 10 runs.

**A liveness probe on the host, not a deadline.** A timeout alone would abandon legitimately
slow work; a build or a download can hold the foreground for minutes. But a healthy kernel
answers an inventory request *while* its foreground execution runs, because its loop is still
turning. So the host probes before it interrupts, and only a kernel that has gone quiet gets
interrupted. A long execution keeps its slot indefinitely.

**Interrupting a healthy kernel is a safe no-op.** Verified separately: no result is produced,
the kernel survives, and the pending execution continues. That is what makes the probe-then-
interrupt sequence safe to run on suspicion.

## What is still unsolved

**A thread parked in a call Python cannot break into.** `time.sleep`, a blocking read, a long
C call in a built-in module. Python takes an asynchronous exception at a bytecode boundary, and
there is no bytecode boundary coming. Nothing short of stopping the container frees it, and the
host learns of it only by the absence of a result. This is the documented final containment
action and it stays that way.

**Resource limits.** Nothing constrains memory, CPU, or process count. A `[x] * 10**12` exhausts
the container, and the container's cgroups are shared with every other tool in it, so the
consequence is not confined to the interpreter. Operator-controlled limits are a later
milestone; they are also the thing that makes the failure legible rather than mysterious, since
an OOM kill currently arrives as a dead interpreter with no explanation.

**Descendant processes.** Generated code may start subprocesses. Interrupting the execution that
started one does not stop it, and it may hold the capture pipe open after its parent execution
has been reported.

**Output flooding as a denial of the session.** Output is bounded per execution, and
between-execution output is bounded separately so a chatty background task cannot displace the
result the model asked for. Both were prototype fixes. What is not bounded is the *rate*: code
that writes continuously keeps the drain thread busy indefinitely.

## For the first milestone

The interrupt path and the liveness probe come across with the port. They are not optional
polish: without them the first `while True:` ends the session, and an agent that cannot survive
its own mistakes cannot be evaluated.

Everything else on this page is a later milestone, and the acceptance criterion for that
milestone is worth stating now: a session survives a runaway execution without the operator
stopping the container, and every failure it cannot survive says which one it was.
