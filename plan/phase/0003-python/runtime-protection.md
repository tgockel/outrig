# Runtime protection

Generated code is arbitrary code. Most of what it does wrong is accidental -- a loop that never
terminates, a print in a loop, a recursion that never bottoms out -- and none of that is an
attack. It still has to be survivable, because an agent that can brick its own session on a bad
comprehension is not usable.

Part of this was designed and proven on the prototype, and is recorded here so it was ported
deliberately rather than rediscovered. The interrupt, the probe, and the cancel have since been
built (`0003-06`); "What the port does" records how, and where the port departs from the
prototype. The memory ceiling followed (`0003-07`), and "The memory ceiling, as built" does the
same for it. The rest is a later milestone.

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
primary. The cost is address space rather than resident memory, though the memory ceiling
counts all of it.

## Two failures, two remedies

Everything above recovers a *wedge*: a loop that has stopped turning. An execution suspended on an
await that never resolves is the opposite failure and the interrupt does not recover it.

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

**In the prototype the user could trigger neither**, which the first milestone changes. There,
the interrupt message had one sender -- the probe -- and the REPL's Ctrl-C never reached the
interpreter at all; it dropped the host's side of the turn. That is fine for a wedge, which the
probe detects on its own. It is not fine for a suspended await, which the
probe correctly reports as healthy: from outside, a dead wait and a legitimately long one are
identical, and only the user knows which. So Ctrl-C has to reach the interpreter and request a
cancel. `runtime.wait` escapes this on its own, since a message raises `MessageAvailable`, but a
bare `await` does not watch channels and is a reasonable thing to write
(`execution-and-rounds.md`).

## What the port does

Built in `0003-06`. It keeps the prototype's shape -- an `interrupt` handled on the reader thread,
a probe before acting -- and departs from it in four places, each for a measured reason.

**The signal is aimed at the main thread.** `signal.raise_signal` called from the reader thread
delivers the signal to the reader thread, since `raise(3)` is thread-directed. The handler still
runs on the main thread, but only at its next bytecode boundary, so a main thread blocked in a
system call never wakes to run it: measured, `time.sleep(2)` ran its full two seconds.
`signal.pthread_kill` aimed at the main thread interrupts the call itself, and PEP 475 runs the
handler before retrying it. Measured: `time.sleep`, `Event.wait`, and `Thread.join` broke as soon
as it was sent, and `subprocess.run(["sleep", "3"])` a quarter of a second later, which is
`Popen.wait`'s own grace for a child to exit. On the main thread, then, a `time.sleep`, a
blocking read, and a synchronous `subprocess.run` are no longer calls Python cannot break into.

**The handler raises only into agent code.** It takes no lock and writes nothing, since the main
thread may be anywhere, and it raises `KeyboardInterrupt` only when all three of these hold:

- An interrupt is armed, and the execution it names still holds the slot. A SIGINT nobody armed --
  agent code running `pkill -INT python3` -- changes nothing.
- Walking out from where the signal landed, a frame compiled from a submission comes before the
  loop's dispatch frames or the fork hooks. Every callback the loop runs, the interpreter's own
  included, runs beneath them, so an idle loop, a result being written, a callback being chosen:
  each declines. This is what keeps an interrupt out of `_send`, where it would tear a protocol
  line, and out of `_run_once`, where it would lose a callback. The one exception is a walk that
  reaches the dispatch from inside a task agent code started: that is agent code however it was
  compiled, such as a coroutine an imported module defines, scheduled with `create_task` or
  `gather`. The only tasks the interpreter starts are the executions' own wrappers.
- The code is the named execution's, including a task it started; or the execution has not
  started, so whatever holds the loop is what keeps it from starting; or the host has judged the
  loop a runaway, which justifies ending whatever agent code spins on it. Without one of these, a
  user's interrupt for a suspended execution could end another's healthy task.

So the third row of the SIGINT table neither exits nor escapes the loop any more: an interrupt
aimed at an execution suspended on an await declines, and a cancel is the remedy.

**A cancel is delivered through the loop, and is never lost.** `cancel` is handled on the reader
thread too and goes to the loop with `call_soon_threadsafe`, where it cancels the execution's
task. A task cancelled before its first step would throw into a coroutine that never entered its
own `try`, and would never report; the loop instead marks the execution, which then reports
`CancelledError` -- "stopped before it started" -- without running anything. The wrapper names
`CancelledError` as a result. It also still catches everything else, because an exception that
left it would end the task with no reply while the host held a slot the interpreter had freed --
and on the port no `KeyboardInterrupt` that reaches it is one that should end the interpreter:
the handler raises only into agent code, so one arriving there came from the execution's own
body, or from awaiting a task an interrupt ended. Formatting the traceback runs agent code too
(`traceback` guards `__str__` against anything, but `__notes__` only against `Exception`), so it
is guarded the same way.

**The probe asks two questions.** The inventory, answered on the loop, says whether the loop is
turning. A second request, answered on the reader thread, reports the CPU time of the thread
running the loop. A loop that does not answer while its thread is idle is blocked in a system
call, which may be perfectly healthy; one that does not answer while its thread burns CPU is
spinning. Measured, as a share of one CPU across the probe:

| the loop's thread                     | share |
|---------------------------------------|-------|
| blocked in `sleep` or `waitpid`       | 0.000 |
| spinning, alone                       | 1.0   |
| spinning, beside one CPU-bound thread | 0.505 |
| spinning, beside two                  | 0.336 |
| spinning, beside three                | 0.252 |

A spinning thread gets only its share of the GIL, so the line is drawn at 0.1: far above idle, and
below any plausible number of busy siblings.

### When the host acts

**On its own, only against a runaway.** From 30 seconds after submission and every 30 seconds
after, the host checks. A loop that answers is healthy however long the execution has run. A loop
that does not answer with its thread idle -- a synchronous `subprocess.run`, a `time.sleep`, a
blocking read -- is left alone, since the failed probe is no evidence about the child. A loop that
does not answer while its thread stays busy is interrupted with the widest scope, and checked
again five seconds later rather than concluded about: the interrupt may have ended a task another
execution left spinning, and this one may be healthy. A probe the loop has not answered is waited
on again rather than sent again, so a loop blocked for an hour does not return to a queue of them;
one it answered before a check began is dropped, since it says the loop turned then, not now.
After three interrupts that each leave the loop spinning, the host stops waiting -- the execution
is `unknown`, keeps its slot, and the model is told why. The host never cancels on its own, so a
suspended execution keeps its slot indefinitely.

**For the user, anything goes.** Ctrl-C during a call cancels the execution -- cheap, and the
right remedy for a suspended one -- and checks. A loop that does not answer is interrupted too:
with the widest scope if its thread is busy, and otherwise aimed at the execution's own blocking
call, since a person may legitimately stop healthy work. The model then reads how the code ended,
and the round goes on; the turn's later calls are not run, since the model wrote them before it
knew. A second Ctrl-C on the same execution stops waiting for it, after a last second's look for
an outcome already on its way. A press is acted on at once, even in the middle of a check. What
the user did is recorded apart from what the host did, so a stop the code survived still stops
the turn when the host goes on to interrupt it as a runaway. Each press counts against the
execution it was made during, so a press racing a result never reaches the next one. An answered
probe is not proof the cancel was delivered -- `task.cancel()` schedules a wake-up that lands a
loop turn after the probe's reply -- and nothing relies on it.

**Behind an abandoned execution.** A submission refused behind an execution nobody waits for any
more is the host's one chance to look at that execution again. It checks, interrupts it if it is
spinning, and tells the model what it found.

### Every shape, and what happens

- **A wedge in the execution's body.** Interrupted, by the host after a check or by Ctrl-C. An
  error result with a traceback; the slot is freed, and the next submission runs.
- **An await that never resolves.** The host never acts, since the loop answers. Ctrl-C cancels
  it: `CancelledError`, and the slot is freed.
- **A spin in code an imported module defines.** Awaited from a submission, it is found through
  the submission's frames. Run as a task -- `create_task`, `gather` -- it is found as a task agent
  code started. Either way it is interrupted like any other.
- **A task an earlier execution left running wedges, with nothing in the foreground.** Nothing
  notices until the next submission, which is admitted but cannot start. Its check, or Ctrl-C,
  interrupts the task. The task ends with `KeyboardInterrupt` as its exception, reported once,
  billed to the execution that started it. The submission then runs -- or, if Ctrl-C was pressed,
  reports that it was stopped before it started.
- **The same, while another execution is suspended.** The host's interrupt ends the task, and the
  suspended execution keeps waiting, not marked a runaway. With Ctrl-C, its cancel lands once the
  loop turns again. Every id stays its own.
- **Native code holding the GIL.** The reader thread cannot run, so nothing can be armed and the
  CPU request goes unanswered. The host says so once and keeps waiting, since a large `sorted` is
  long rather than endless; Ctrl-C twice stops waiting. If it never returns, this is session
  failure, and named as such. It covers GIL-holding loops that do check for signals, such as `re`
  backtracking, since the signal is never sent. A call that releases the GIL -- `hashlib` or
  `zlib` on a large input -- reads as quiet and busy, and the interrupt lands as it returns.
- **A synchronous `subprocess.run`.** The host sees an idle thread and leaves it. Ctrl-C breaks
  the wait; `run` kills its direct child, and what that child started keeps running, which the
  result says. The killed child stays a zombie until the next spawn. A bare `Popen(...).wait()`
  does not kill even the child.
- **`os.system`.** musl's `system()` ignores SIGINT process-wide for as long as it waits, so an
  interrupt sent then is discarded. A later check sends another.
- **Agent code that blocks or replaces SIGINT.** Blocked with `pthread_sigmask`, the signal waits
  to be unblocked. Replaced, the reader thread notices before arming and says so, and only a
  cancel can reach the execution.
- **Agent code that pumps the loop by hand.** The loop's own frames come before the agent's, so the
  interrupt declines there, where it could lose a callback. Only a cancel can reach it.
- **An interrupt the code catches.** The host tries three times, then stops waiting; a second
  Ctrl-C stops waiting at once.
- **Nothing executing.** Nothing is sent. A request naming an execution that holds nothing changes
  nothing, and neither does a SIGINT nobody armed.

The interpreter's `podman exec` client runs in a process group of its own. Left in outrig's, it
took the terminal's SIGINT on every Ctrl-C -- at the prompt too -- and exited, and the interpreter
exited with it when its stdin closed: measured, the session's next submission found the
interpreter gone.

## The memory ceiling, as built

Threads contain a runaway loop, not a runaway allocation. A list one agent grows until the machine
runs out has the kernel kill the interpreter, and every agent with it. The prototype's answer was
one call at boot: `RLIMIT_AS` at 512 MiB, under which an outsized allocation raised at once and a
gradual one after 484 MiB. Built in `0003-07`, the port keeps the shape -- a limit the interpreter
sets on itself before any agent exists -- and departs from it in four places, each for a measured
reason. Measured with the payload on a 32-core, 125 GiB host.

**The knob is `RLIMIT_DATA`.** Inside the interpreter the two behave alike: at 512 MiB, gradual
growth raised at 484 MiB under `AS` and 504 MiB under `DATA`, and an outsized request raised at
once under both. They differ in what else they count, which matters because every program the
agent starts inherits the limit (below), and many reserve address space far beyond what they use.
`AS` counts every reservation. `DATA` counts private writable memory -- the heap, thread stacks --
and not file mappings, nor space reserved and never made writable. The smallest ceiling, of
256 MiB and each power of two to 8 GiB, that each program ran under:

| program                            | `RLIMIT_AS`      | `RLIMIT_DATA` |
|------------------------------------|------------------|---------------|
| `cc` on a one-line C file          | 256 MiB          | 256 MiB       |
| `go version`, `node -e`            | 1 GiB            | 256 MiB       |
| `node` creating a wasm memory      | none up to 8 GiB | 256 MiB       |
| `rustc`, or `cargo build`, a hello | 4 GiB            | 2 GiB         |
| `java -version`                    | 8 GiB            | 4 GiB         |

Java's is the JVM committing a sixty-fourth of the machine for its first heap, so it grows with
the host. What `DATA` misses is shared anonymous memory -- `mmap.mmap(-1, n)` without
`MAP_PRIVATE` -- which it does not count.

**The value is half the memory the container can see**: the smaller of its cgroup's limit
(`memory.max`, or v1's `memory.limit_in_bytes`) and `MemTotal`. OutRig sets no memory limit on the
container, whose `memory.max` reads `max`, so today that is half the machine. Half, so that the
interpreter runs out before the machine does; of what is there, so that the ceiling grows with the
machine as the programs above do. The prototype's 512 MiB passed on to them would have failed
every row but the first. A lower soft limit already in place is kept. The value is not
configurable: a config key is a surface, and nobody has needed another number.

It is growth that the ceiling is for. Without one, `[x] * 10**12` raises anyway -- the kernel's
overcommit heuristic refuses a single request larger than the machine -- but a 100 GiB list was
allocated in full.

**Programs the agent starts inherit it, each on its own.** A limit passes across fork and exec, and
a threaded process has no safe point between them to hand a child anything else: `preexec_fn` is
documented unsafe with threads, raising the process's limit around a spawn lifts it for every
other thread too, and a wrapper in front of each program breaks how `Popen` reports one that does
not exist. So the soft limit is the policy for descendants as well, and the hard limit is left
where it was. A lowered hard limit could not be raised again by an unprivileged process; one left
high lets a program that needs more lift its own, with `ulimit -d unlimited` or
`resource.setrlimit`. It is a ceiling per process rather than a share of one, so it bounds nothing
about their sum; the container's memory still does that.

**A reserve, so that running out can still be reported.** One call at boot was not enough. Growing
a list a small string at a time leaves nothing behind it, and against the port the interpreter then
could not format the traceback, and the next execution ran out in its own cleanup before it freed
the slot -- so every submission after was refused, and the session was over in all but name.
Coarser growth, an outsized request, and growth inside a comprehension, whose list is freed as the
error unwinds, all reported cleanly. Fine growth into a global did not.

So the interpreter holds 32 MiB back, mapped and never written: counted against the ceiling, costing
no memory. Agent code runs with it held. The interpreter gives it back when it works for itself --
formatting a failure, collecting output and reporting, starting an execution, scheduling a callback,
encoding a message, reading a request -- and takes back what fits before the next body: in one map
when all of it fits, and a mebibyte at a time when not. After each result it takes back what fits
while leaving a mebibyte free, since the loop goes idle then and its own machinery must not find the
ceiling where agent code left it. Taking it back in part is the point. While agent code pins memory
at the ceiling, the next execution runs with almost nothing left, and fails at once unless it frees
something first, which `x = None` and `del x` do without allocating. Measured with the reserve,
every shape above reported with its traceback, the release that followed ran, and later work
succeeded -- including running out a second time before releasing anything.

Giving and taking it costs a trivial submission about a tenth of its round trip. Thirty-two maps
remade for every execution cost 60 percent, which is why it is one map when it can be.

**Two threads of the interpreter's own run while agent code does,** and neither can use the
reserve without handing it to that code. Both once stopped for good at the ceiling, found in review.

- *The drain.* It read each chunk of an execution's output into a buffer it allocated per read.
  At the ceiling that raised, the thread ended with the pipe still open, and every writer -- the
  body among them -- blocked on a full pipe; an interrupt could not help, since reporting writes to
  that pipe too. The drain now reads into a buffer it is given before it starts, counts as dropped
  what it has no memory to keep, and closes the pipe however it ends, so a writer gets an error
  rather than waiting for ever. Its first cut then counted a whole read dropped when it ran out
  part-way through one, keeping some of it too, so a result could report more bytes than were
  written. Now every step keeps all of a range or none of it, and only what it lets go is counted:
  each byte once, and uncounted rather than twice where there is no memory even to count it.
- *The event loop.* A completing future schedules its callbacks with `call_soon`, and asyncio
  drops one it cannot allocate. When that was an awaiting execution's wakeup, the task never ran
  again -- not even to take a cancel -- and its slot stayed held. Reading the loop's wakeup pipe
  failed the same way, and was retried on every turn. The interpreter's loop now gives the reserve
  back and tries `call_soon` once more, and its exception handler gives it back on a `MemoryError`,
  so a failed wakeup read is followed by one that works.

Beneath the reserve, what must not depend on memory does not. The slot is freed before anything in
the report allocates. A result that still cannot be sent whole is sent as its error alone. A reader
thread that cannot read waits rather than ending the process, and a loop that cannot report an
exception escaping it carries on.

What the ceiling does not do:

- **Isolate one agent from another.** The ceiling is the process's. While one agent holds memory up
  to it, every agent's allocations fail: pinned by a test, a sibling's million-element list raises
  `MemoryError` until the holder lets go, and then runs.
- **Keep the reserve for the agent whose report needs it.** It is one pool. While one agent's report
  has it given back, another agent's running code can take it.
- **Guarantee a traceback, or the output.** A traceback CPython had no room to build is reported as
  `MemoryError` alone, and output written at the ceiling is counted as dropped rather than kept. A
  request line longer than the reader's buffer that runs out halfway is lost.
- **Keep the loop turning once the reserve is spent.** Agent code that goes on holding memory at
  the ceiling after the reserve has been given back to its loop can starve that loop again.
- **Stop `os._exit(0)`,** which ends the session whatever the ceiling.

## What is still unsolved

**A call Python cannot break into.** On the main thread a signal breaks a system call, so the
primary's `time.sleep` or blocking read can be interrupted. What remains is native code that holds
the GIL, `os.system`, and any call on another thread: Python takes an asynchronous exception at a
bytecode boundary, and there is none coming. Nothing short of stopping the container frees them,
and the host learns of them only by the absence of a result. This is the documented final
containment action and it stays that way.

**A wedged subagent, which is contained but not recoverable.** The interrupt above reaches the
main thread, and subagents are not on it. A subagent that wedges is gone: the host learns of it
from the absence of a result, the application marks its endpoints failed, and its parent is told.
The thread keeps spinning until the session ends. Its siblings survive -- measured at 50% of
compute throughput and a worst-case event-loop tick of 5.1 ms beside one wedge -- but that is the
cost of *one*. N of them leave the rest 1/(N+1) of a core, so a long session degrades rather than
fails. Recovering a wedged subagent needs an interpreter per agent or subinterpreters, and
`agent-placement.md` records why neither is taken here.

**Resource limits besides memory.** Nothing constrains CPU or process count, and the container's
cgroups are shared with every other tool in it, so those consequences are not confined to the
interpreter. Memory has its ceiling, which makes running out legible rather than preventing it and
is not per-agent isolation (above). `os._exit(0)` remains uncontained: one line of generated code
ends the session. Operator-controlled CPU and process limits stay a later milestone.

**Descendant processes.** Generated code may start subprocesses. Interrupting the execution that
started one does not stop it -- `subprocess.run` kills its direct child and no further -- and it
may hold the capture pipe open after its parent execution has been reported. The result of an
execution the user stopped says so.

**Output flooding as a denial of the session.** Output is bounded per execution, and
between-execution output is bounded separately so a chatty background task cannot displace the
result the model asked for. Both were prototype fixes. What is not bounded is the *rate*: code
that writes continuously keeps the drain thread busy indefinitely.

## For the first milestone

The interrupt path and the liveness probe came across with the port (`0003-06`). They are not
optional polish: without them the first `while True:` ends the session, and an agent that cannot
survive its own mistakes cannot be evaluated.

Retaining the foreground execution's task handle joined them, and so did the Ctrl-C path that
uses it. They are small -- a binding instead of a discarded future, and a request the REPL already
has a key for -- and without them an ordinary bare `await` on something that never resolves ends
the session's usefulness while every diagnostic reports health.

The memory ceiling joined them too (`0003-07`), for the same reason rather than as an early start
on resource limits: co-hosting makes memory the one resource an agent can exhaust on everyone
else's behalf. It turned out not to be the single call at boot it was planned as -- running out
has to leave room to say so -- but the rest of the limits work stays where it was.

Everything else on this page is a later milestone, and the acceptance criterion for that
milestone is worth stating now: a session survives a runaway execution without the operator
stopping the container, and every failure it cannot survive says which one it was.
