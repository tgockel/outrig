# Agent placement

Where an agent's Python actually runs. The prototype gave each agent an interpreter process of its
own; this phase does not. **One interpreter process per session, one thread and one event loop per
agent.** `harness-components.md` describes what the interpreter is; this page is why there is one of
them rather than one per agent, and what that choice costs.

Nothing here is exercised by the first milestone, which runs a single agent. It is settled now
anyway, because an interpreter written around module globals cannot host a second agent
without being rewritten, and the interpreter is being ported regardless.

## The shape

```text
  CONTAINER (primary)
  +---------------------------------------------------------------------+
  | interpreter process                                                 |
  |                                                                     |
  |   main thread           agent-1 thread        agent-2 thread        |
  |   +----------------+    +----------------+    +----------------+    |
  |   | primary agent  |    | subagent       |    | subagent       |    |
  |   |  session module|    |  session module|    |  session module|    |
  |   |  runtime       |    |  runtime       |    |  runtime       |    |
  |   |  endpoints     |    |  endpoints     |    |  endpoints     |    |
  |   |  backlog, pipes|    |  backlog, pipes|    |  backlog, pipes|    |
  |   |  event loop    |    |  event loop    |    |  event loop    |    |
  |   +----------------+    +----------------+    +----------------+    |
  |     ^ SIGINT lands here, and only here                              |
  |                                                                     |
  |   reader thread  -- NDJSON in on _PROTO_IN, routed by agent id      |
  |   _PROTO_OUT     -- NDJSON out, behind one writer lock              |
  |   fd 1, fd 2     -- the exec's stderr: the unattributed bucket      |
  |   RLIMIT_AS      -- one ceiling, shared by every agent              |
  +---------------------------------------------------------------------+
```

Each agent owns a session module registered in `sys.modules`, a runtime, its channel endpoints, a
backlog of background output, an event loop, and an execution slot; each execution in it owns a
pipe. That bundle is the agent's **kernel**; the process hosting them all is the **interpreter**,
and it owns the protocol descriptors, the reader thread, the address-space ceiling, and the signal
handler. Which of those two lists a thing falls into is the whole of this page.

A channel between two co-hosted agents never reaches the host, but `messages.md` governs it
unchanged: the two loops hand messages over through `loop.call_soon_threadsafe`, and the
serializable subset still applies, so object identity does not cross a thread boundary any more
than it crossed a process one.

## There was no boundary here to remove

Worth saying plainly, because the question looks like it is about giving something up. Subagents
have no process isolation today: `crates/outrig-cli/src/subagent/mod.rs` spawns a Tokio task with
its own message history, over the parent's MCP connections, in the parent's process, in the same
container. This phase would have *introduced* a per-agent process, not preserved one.

What made introducing it tempting is not that agents are separate but that execution became
arbitrary. An MCP tool call is a bounded operation; a REPL is a namespace.

## What co-hosting buys

Measured on the pinned payload, x86_64:

```text
cpython-3.13.15+20260901-x86_64-unknown-linux-musl-noopt+static-full
```

| per agent                                   | min     | median  |
|---------------------------------------------|---------|---------|
| `python3 -I -c pass`                        | 8.4 ms  | 11.6 ms |
| `-I -c` with the interpreter's import set        | 46.1 ms | 48.7 ms |
| resident memory, with an event loop and a queue | --  | 16.0 MiB |

A process per agent is not ruinous, and CPython is not even the dominant term:
`Container::exec_stdio` spawns a **`podman exec` CLI process**, whose startup is the larger cost.
The saving is real but modest, and it is worth being clear that it is memory and launch latency
that co-hosting buys -- not speed in general.

## What it costs

The payload is not a `freethreaded` build, so co-hosted agents hold one GIL between them and do
not run Python in parallel. Two agents computing at once get about half a core each. Most agent
work shells out to subprocesses, which are separate processes and genuinely parallel, so this is
usually invisible -- but in the one dimension that motivated co-hosting, it is a loss, not a gain.

## Output stays attributed

The requirement is unchanged: an execution's result holds that execution's output and nothing from
a sibling. There is one fd 1 per process, so co-hosted agents cannot attribute at the descriptor
the way the prototype's `kernel.py` did. They attribute above it, and the coverage is good:

`print()` and `sys.stdout`
: A dispatcher keyed on a `contextvars.ContextVar`. It names the **execution**, not just the
  agent, which is what the prototype's global `_foreground` flag was doing before co-hosting.
  While the execution runs, its writes go into the execution's own pipe, so they keep their order
  with a child's; once it has reported, they go to its agent's backlog under its id.

A background `create_task`
: Follows for free -- asyncio copies the current context at task creation -- and because the
  context names the execution that started it, output from a task cell A left running is billed
  to A even while cell B holds the foreground. Keying on the agent alone would let A's chatty
  task eat B's result quota, which is exactly the defect `kernel-findings` #2 fixed for one
  agent and co-hosting would otherwise reintroduce.

`subprocess.run([...])`, and `asyncio.create_subprocess_exec`, which goes through it
: A pipe per **execution**, supplied as the default `stdout` and `stderr` by one patch on
  `Popen.__init__`, which reads the same contextvar. `stdout=sys.stdout` is treated as the
  default, and `sys.stdout.fileno()` returns that descriptor while the execution runs.

  Per-execution rather than per-agent, because a child's bytes carry no writer identity: two
  executions' children sharing one agent pipe produce a stream the drain cannot attribute, and no
  contextvar recovers information that was never in it. A per-agent pipe would let a child cell A
  started consume cell B's result budget -- the same defect the background bound exists to fix,
  reintroduced one layer down.

  A child can outlive its execution and keep the pipe open, so the result does not wait for the
  pipe to close. When the body finishes, the interpreter writes a random sentinel into the pipe
  and closes its own end; the pipe's drain thread bills what precedes the sentinel to the result
  and what follows it to the backlog, under the execution's id, for as long as any child holds
  the pipe. A child started after its execution reported -- by a task it left running -- gets a
  pipe of its own, billed the same way.

`os.write(1, ...)` and `os.system()`
: Not attributable. These name the descriptor directly. fd 1 and fd 2 are the exec's stderr, the
  stream the host already reads as diagnostics, so what lands there is recorded as an event
  rather than dropped -- `observability.md` -- because it cannot be billed to an agent and a
  silent hole is worse than an unattributed line. It never reaches the protocol, which has
  descriptors of its own, and it never reaches another agent's result.

Verified with two agents printing, spawning background tasks, and running subprocesses at the same
time: every line landed in its own agent. That experiment established attribution **between**
agents. The harder half -- one agent's old execution emitting while a new one runs -- is now
tested too, in `crates/outrig/src/python/interpreter_tests.rs`, for a task and for a child
process. For both, the new execution's output is exact, it drops nothing, and every byte of the
old one's output is billed to the old one. The child case uses a child that writes only after
the new execution has started, 20,000 bytes against a 16 KiB budget.

The same holds for a child a background task starts after its execution reported, and for an
exception nobody retrieved from such a task, which asyncio reports from wherever the task happens
to be collected.

What stays unattributed, stated rather than implied:

- **A thread started with `threading`, and `loop.run_in_executor`.** Python 3.13 starts a thread
  with an empty context, so there is no execution to bill; the output goes to stderr, never to
  another execution. `asyncio.to_thread` copies the context and is attributed. Copying the
  context into every thread would misattribute a pool's workers to whichever execution created
  them.
- **A `fileno()` kept past its execution.** The number is closed when the execution reports and
  may be reused by the next one's pipe.
- **`contextlib.redirect_stdout`.** It rebinds `sys.stdout` for the whole process, so one agent's
  redirect also captures a sibling's prints while it is in force.

The prototype's background-output fix, which `runtime-protection.md` carries, survives -- and
survives better than it did. Background output stays attributed to the execution whose task
wrote it, and each result carries it apart from its own output, grouped by that execution's id and
bounded on its own. A chatty background task can no longer displace a result, and cannot reach a
sibling's at all.

What is genuinely lost is that capture stops being inescapable. At the descriptor nothing can evade
it; in Python, code can rebind `sys.stdout`. That distinction only matters against deliberate
evasion, which `security.md` excludes from the threat model.

## Interruption reaches exactly one thread

CPython runs Python-level signal handlers only on the main thread of the main interpreter.
Measured on the payload: `signal.raise_signal(SIGINT)` called *from* a worker thread still runs the
handler on `MainThread`, and the `KeyboardInterrupt` surfaces there.

So the recovery path in `runtime-protection.md` reaches one agent, and the decision follows from
that: **the primary agent runs on the main thread.** The agent with a person in front of it keeps
the interrupt path exactly as the prototype proved it. Subagents, which run unattended inside their
parent's work, do not.

A wedged subagent is therefore contained but unrecoverable. Its siblings keep running, because
CPython drops the GIL every switch interval:

| beside one `while True: pass` | alone      | wedged     |
|-------------------------------|------------|------------|
| compute-bound sibling         | 19.8M ops/s| 9.8M ops/s |
| event-loop tick, median       | 0.0016 ms  | 0.0024 ms  |
| event-loop tick, worst        | 0.011 ms   | 5.086 ms   |

Half throughput and a tail of one switch interval is survivable. The qualifier belongs with the
number, though: that is the cost of *one* wedge. N of them leave the rest 1/(N+1) of a core, and
the threads never go away, so a long session degrades rather than fails.

## Memory is a session-wide resource

Threads contain a runaway loop. They do not contain a runaway allocation: `[x] * 10**12` in any
agent would have the process OOM-killed and every agent with it.

`RLIMIT_AS`, set once at interpreter start, converts that into a `MemoryError` raised in the
allocating thread, which returns as that agent's error result with a traceback -- the same recovery
shape the interrupt path already has. Measured against a 512 MiB ceiling: the outsized allocation
raised immediately, a gradual one raised after 484 MiB, and the interpreter was alive and usable
afterwards in both cases.

The ceiling is process-wide and the consequence is worse than "the greedy agent gets an error."
Measured: while one agent sits pinned at the ceiling, an ordinary `import json` on another thread
raises `MemoryError` too. Recovery is automatic once the memory is released, but for as long as it
is held, every agent in the session is degraded. This is an improvement on an unexplained OOM
kill, not an isolation mechanism, and it should not be described as one.

`os._exit(0)` remains uncontained. It is one line of generated code and it ends the session.

## What agents share regardless

Isolating an agent's names does not isolate the interpreter. Co-hosted agents share `sys.modules`
and therefore every imported module's state, plus `os.environ`, `sys.path`, `warnings` filters,
signal handlers, and the recursion limit. Most consequentially they share the working directory: a
subagent calling `os.chdir("/workspace/sub")` moves its parent.

It is not disclosure. The operator's grant is per-session, and the credential boundary is a
socket any process in the container can open -- `security.md` works this through, and
`pyro-remote-objects.md` notes the consequence for proxies, which is that one agent's prebound
name is presentation over a session-wide grant rather than a grant of its own.

What is left is interference, and it is the same kind of thing
`doc/concepts/subagents.md` already says about the workspace -- two subagents told to edit the
same file will fight over it, and OutRig does not stop them. Keeping concurrent work disjoint is
the parent's job, and after this phase that extends from files to process state.

## Rejected alternatives

**Rejected: one interpreter process per agent.** What the prototype assumed, and the stronger
containment story -- a real crash domain, a place to hang per-agent limits, no shared `chdir`. It
loses on cost paid against a boundary nothing was relying on: subagents share a process today,
the operator's grant is per-session rather than per-agent, and `security.md` already declines to
call a same-UID process in the same container a boundary. It stays the obvious escalation if a
per-agent resource ceiling ever becomes a requirement.

**Rejected: co-hosting on a single shared event loop.** Cheaper than a thread apiece and it keeps
the interrupt path working for every agent. Rejected because a wedge stops *every* agent rather
than slowing them, and because the interrupt cannot be aimed: `KeyboardInterrupt` lands wherever
the main thread is at the next bytecode boundary, which is not necessarily the agent it was meant
for.

**Rejected: subinterpreters.** They would fix the shared `sys.modules` and give each agent its own
GIL. Confirmed present on the payload as the private `_interpreters` and `_interpqueues`;
`concurrent.interpreters`, the public API, is 3.14 and is absent. Rejected for this phase as a
large lift on a private interface that would still not fix descriptors, the working directory, the
environment, or signals -- the four things that actually bite.

## Open questions

- Whether a wedged subagent should ever be recoverable. Subinterpreters are the only real route
  and the answer above is why they are not taken yet.
- What the `RLIMIT_AS` ceiling should be, and whether it is operator-configurable. A ceiling that
  is too low turns ordinary work into `MemoryError` for everyone.
- Whether `RLIMIT_AS` or `RLIMIT_DATA` is the right knob. `AS` is simpler and is awkward for
  mmap-heavy code.
- Whether an agent can ever be placed in a process of its own -- a per-agent escalation rather
  than a global choice. Nothing here forecloses it, and the protocol's agent id is what would
  make it invisible to both halves.

## Unverified

- Every measurement on this page is from the pinned payload on x86_64, on one machine, outside a
  container. Orders of magnitude, not budgets.
- The `podman exec` startup cost is asserted from the shape of `Container::exec_stdio` and was not
  measured. It is the term that decides whether co-hosting is worth anything at all, so measure it
  before citing this page as the reason for the decision.
- The `Popen.__init__` patch is tested against `subprocess.run` and `Popen` -- positional
  `stdout`, `stderr=STDOUT`, `capture_output`, a spawn that warns -- and against
  `asyncio.create_subprocess_exec`. It has not been checked against `os.popen` or a child that
  re-execs.
- A fork -- `os.fork`, `multiprocessing` -- goes through `os.register_at_fork` instead, and is
  tested with the `fork` start method. The child closes the protocol descriptors and forgets their
  numbers, so it cannot write a protocol line and its own children cannot close its files. It
  writes through a descriptor this process drains, billed to the execution it forked under: a
  copy of the execution's pipe while the body runs, a pipe of its own afterwards.
