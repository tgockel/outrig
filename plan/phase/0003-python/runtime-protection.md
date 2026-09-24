# Runtime protection

Generated code is arbitrary code. Most of what it does wrong is accidental -- a loop that never
terminates, a print in a loop, a recursion that never bottoms out -- and none of that is an
attack. It still has to be survivable, because an agent that can brick its own session on a bad
comprehension is not usable.

Part of this is already designed and proven on the prototype. That part is recorded here so it
is ported deliberately rather than rediscovered. The rest is a later milestone.

## The wedge, and why it is the hard one

Synchronous Python that never yields blocks the event loop of the agent running it. Everything
that agent could do goes with it: the loop is what would report the problem, accept new work, or
deliver a message. Agents are co-hosted one per thread (`agent-placement.md`), so the damage stops
at the agent -- its siblings keep running, at roughly half throughput and with a tail of one GIL
switch interval. Reproduced against the prototype with `while True: pass`:

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
come, so the round hangs rather than failing.

Measured again on the port, before it has any interrupt handling of its own, two of those rows
change. **Closing stdin now exits**: the reader thread ends the process itself, so a wedged loop no
longer outlives the host that owned it. And the port runs its loops with `run_forever` rather than
`asyncio.run`, so nothing defers a plain SIGINT:

| state when SIGINT arrives        | on the port, measured                                    |
|----------------------------------|----------------------------------------------------------|
| no execution running             | escapes the loop, which is re-entered; interpreter lives |
| executing, not yielding          | `KeyboardInterrupt` in agent code; error result, freed   |
| executing, suspended on an await | escapes the loop, which is re-entered; slot still held   |

The third row no longer destroys the session, but neither does it recover the execution. Where the
interrupt lands inside the loop's own bookkeeping -- mid-callback, or mid-way through writing a
protocol line -- was not measured, and the handler described below is still what makes delivery
deliberate rather than lucky.

## What the prototype established

**An `interrupt` message handled on the reader thread.** The interpreter's stdin reader is a
separate thread and keeps running throughout. Handling the message *there*, rather than handing it
to the loop with `call_soon_threadsafe`, is the whole trick -- that queue is precisely what a wedged
loop is not draining. The handler raises SIGINT; a handler installed from inside the loop overrides
the one `asyncio.run` set, and turns it into a `KeyboardInterrupt` inside the running execution. The
execution then comes back as an ordinary error result with a traceback.

Measured: 5 of 5 trials recovered with the session still usable afterwards.

**Two alternatives rejected with evidence, not on taste.**

- `ctypes.pythonapi.PyThreadState_SetAsyncExc` -- `ctypes.pythonapi` is `None` in a statically
  linked build, so the attribute lookup fails and the call never happens. Confirmed since against
  the pinned payload, along with the mechanism, which is worth recording because it is version
  dependent: a fully static musl binary has no working `dlopen`, `PyDLL(None)` raises
  `OSError: Dynamic loading not supported`, and **Python 3.13** catches that and assigns `None`.
  Python 3.12 has no such fallback and would instead fail at `import ctypes`. If the payload's
  version moves, re-check which of those two happens.
- `_thread.interrupt_main()` -- killed the process in 9 of 10 runs.

**A liveness probe on the host, not a deadline.** A timeout alone would abandon legitimately
slow work; a build or a download can hold the foreground for minutes. But a healthy interpreter
answers an inventory request *while* its foreground execution runs, because its loop is still
turning. So the host probes before it interrupts, and only an interpreter that has gone quiet gets
interrupted. A long execution keeps its slot indefinitely.

**Interrupting an idle interpreter does nothing.** Verified separately: the handler is gated on
whether an execution is running, so with none in flight the signal produces no result and the
interpreter survives untouched.

That is the only unconditional case, and the rest depends on where the main thread is when the
signal lands:

| state                                  | SIGINT does                                  |
|----------------------------------------|----------------------------------------------|
| no execution running                   | nothing -- the handler returns. Measured     |
| executing, not yielding (a wedge)      | `KeyboardInterrupt` into agent code. 5 of 5  |
| executing, suspended on an await       | `KeyboardInterrupt` into the loop; exit      |

Only the middle row is recovery. The first is the no-op the sequence relies on. The third is the
measurement below, and it is why an interrupt is not a general-purpose remedy.

**The interrupt reaches one thread, which decides where the primary agent runs.** CPython runs
Python-level signal handlers only on the main thread of the main interpreter. Measured against the
payload: `signal.raise_signal(SIGINT)` called *from* a worker thread still runs the handler on
`MainThread`, and the `KeyboardInterrupt` surfaces there. Since agents are one per thread, the
recovery designed above reaches exactly one of them -- so the primary agent takes the main thread.
The agent with a person in front of it keeps the interrupt path as the prototype proved it, and
`agent-placement.md` carries the reasoning.

**A thread's stack, found in the port.** musl gives a thread 128 KiB of stack. Deep but legal
recursion off the main thread -- `json.loads` of a nested document, `repr` of a nested list --
overflowed it and killed the interpreter with `SIGSEGV`, every agent with it, before CPython's
recursion limit could raise `RecursionError`. Measured on the payload: 1 MiB still crashed, 2 MiB
did not. The interpreter sets 8 MiB, the main thread's, for every thread started after it boots --
a subagent's, its reader, and any the agent starts -- so a subagent is no easier to crash than the
primary. The cost is address space rather than memory, which `RLIMIT_AS` counts.

## What is still unsolved

**A thread blocked in a call Python cannot break into.** `time.sleep`, a blocking read, a long
C call in a built-in module. Python takes an asynchronous exception at a bytecode boundary, and
there is no bytecode boundary coming. Nothing short of stopping the container frees it, and the
host learns of it only by the absence of a result. This is the documented final containment
action and it stays that way.

**A wedged subagent, which is contained but not recoverable.** The interrupt above reaches the
main thread, and subagents are not on it. A subagent that wedges is gone: the host learns of it
from the absence of a result, the application marks its endpoints failed, and its parent is told.
The thread keeps spinning until the session ends. Its siblings survive -- measured at 50% of
compute throughput and a worst-case event-loop tick of 5.1 ms beside one wedge -- but that is the
cost of *one*. N of them leave the rest 1/(N+1) of a core, so a long session degrades rather than
fails. Recovering a wedged subagent needs an interpreter per agent or subinterpreters, and
`agent-placement.md` records why neither is taken here.

**Resource limits, of which memory is now partly answered.** Nothing constrains CPU or process
count, and the container's cgroups are shared with every other tool in it, so those consequences are
not confined to the interpreter. Memory is different, because agents share a process: without a
ceiling, one agent's `[x] * 10**12` OOM-kills the interpreter and ends every agent's session.
`RLIMIT_AS`, set once at interpreter start, converts that into a `MemoryError` raised in the
allocating thread, which returns as that agent's error result with a traceback -- the same recovery
shape the interrupt path has. Measured against a 512 MiB ceiling: the outsized allocation raised
immediately, a gradual one raised after 484 MiB, and the interpreter was alive and usable afterwards
in both cases.

State its limits honestly. The ceiling is process-wide, so it degrades every agent while it is
pinned -- measured: an ordinary `import json` on another thread raises `MemoryError` too, until
the memory is released. It makes an OOM legible instead of arriving as a dead interpreter with no
explanation; it is not per-agent isolation. And `os._exit(0)` remains uncontained: one line of
generated code ends the session. Operator-controlled CPU and process limits stay a later
milestone.

**A second failure, needing a second mechanism.** Everything above recovers a *wedge*: a loop that
has stopped turning. An execution suspended on an await that never resolves is the opposite
failure and the interrupt does not recover it.

Measured on the payload. With the execution parked on a future and the loop turning normally, the
SIGINT does not land in the execution at all -- the main thread is inside the event loop rather
than inside agent code, so the `KeyboardInterrupt` unwinds `asyncio.run` and the interpreter
exits. Applying the wedge remedy to this failure destroys the session it was meant to save.

What does work is `task.cancel()`, delivered through `loop.call_soon_threadsafe` from the reader
thread. Measured: `CancelledError` is raised at the await point, the execution reports an ordinary
error result, the interpreter stays alive, and the next submission runs. The prototype cannot do
this yet for a mundane reason -- `_dispatch` starts the execution with
`asyncio.ensure_future(...)` and discards the handle, so there is nothing to cancel. Keeping it is
the whole change.

The two are mutually exclusive, and the probe already tells them apart:

| failure                      | loop        | remedy                        |
|------------------------------|-------------|-------------------------------|
| wedge -- `while True: pass`  | not turning | SIGINT on the reader thread   |
| suspended on a dead await    | turning     | `task.cancel()` on the loop   |

`call_soon_threadsafe` cannot run on a wedged loop, which is exactly why the SIGINT trick exists;
SIGINT tears down a turning one. So the probe's answer selects the remedy rather than merely
deciding whether to act.

**The probe is evidence, not a guarantee about the moment of delivery.** It reports that the loop
was turning a moment ago, and an execution can change state between the probe and the signal: a
finite computation can miss a probe and then yield onto an await, so a delayed SIGINT arrives in
the state that exits the interpreter. The converse holds too -- an execution can stop yielding
after a successful probe, leaving a queued cancellation undelivered. Targeted cancellation is
cheap and safe to attempt; escalating to a signal is the step that can cost the session, and it
should be described as best-effort with that outcome named rather than as a race-free selector.

The tempting simplification is to use `task.cancel()` for both, since the execution is a task
either way. Measured against a wedge: the reader thread stays alive and does successfully call
`loop.call_soon_threadsafe(task.cancel)`, and a second later the task is neither cancelled nor
done, because the loop never ran the callback. That is the same property this page already relies
on in the other direction -- the queue a wedged loop is not draining is precisely the one the
interrupt avoids. Cancellation is also delivered at a suspension point, and a wedge is defined by
never reaching one, so even an invoked cancel would have nowhere to land.

What *is* shared is the outcome. Both remedies end as an exception propagating out of the agent's
code into the execution wrapper, reported as an ordinary error result with a traceback. Two
deliveries, one contract -- which is why `execution-and-rounds.md` needs no extra outcome for
this.

Two details the wrapper has to get right. `asyncio.CancelledError` derives from `BaseException`,
not `Exception`, so it must be caught deliberately rather than swept up by an ordinary handler --
and deliberately rather than by catching `BaseException` indiscriminately, which would also
swallow the interrupt path's `KeyboardInterrupt` in cases that should end the interpreter.
Requesting a cancel is also not the same as it completing: generated code may suppress
`CancelledError`, so the slot is freed by a terminal reply, never by having sent the request.

The user's path inherits that. Ctrl-C targets a stable execution id and keeps the host's
correlation until a terminal reply arrives or the execution is explicitly recorded unresolved.
Dropping the host's future *and* sending a cancel loses the reply that says whether the slot came
back, and a request that arrives late must not land on whatever execution is running by then.

**In the prototype the user can trigger neither**, which the first milestone changes. As ported,
the interrupt message has one sender -- the probe -- and the REPL's Ctrl-C never reaches the
interpreter at all; it drops the host's side of the turn. That is fine
for a wedge, which the probe detects on its own. It is not fine for a suspended await, which the
probe correctly reports as healthy: from outside, a dead wait and a legitimately long one are
identical, and only the user knows which. So Ctrl-C has to reach the interpreter and request a
cancel. `runtime.wait` escapes this on its own, since a message raises `MessageAvailable`, but a
bare `await` does not watch channels and is a reasonable thing to write
(`execution-and-rounds.md`).

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

Retaining the foreground execution's task handle joins them, and so does the Ctrl-C path that
uses it. They are small -- a binding instead of a discarded future, and a request the REPL already
has a key for -- and without them an ordinary bare `await` on something that never resolves ends
the session's usefulness while every diagnostic reports health. Deferring them would mean shipping
a first interactive milestone where that is true, which is worth saying out loud if it is what
gets chosen.

`RLIMIT_AS` joins them, for the same reason rather than as an early start on resource limits.
Co-hosting makes memory the one resource an agent can exhaust on everyone else's behalf, and the
ceiling is a single call at boot. The rest of the limits work stays where it was.

Everything else on this page is a later milestone, and the acceptance criterion for that
milestone is worth stating now: a session survives a runaway execution without the operator
stopping the container, and every failure it cannot survive says which one it was.
