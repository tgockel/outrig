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
  |   |  out buf + pipe|    |  out buf + pipe|    |  out buf + pipe|    |
  |   |  event loop    |    |  event loop    |    |  event loop    |    |
  |   +----------------+    +----------------+    +----------------+    |
  |     ^ SIGINT lands here, and only here                              |
  |                                                                     |
  |   reader thread  -- NDJSON in, routed by agent id                   |
  |   _PROTO_FD      -- NDJSON out, behind one writer lock              |
  |   fd 1           -- the unattributed-output bucket                  |
  |   RLIMIT_AS      -- one ceiling, shared by every agent              |
  +---------------------------------------------------------------------+
```

Each agent owns a session module registered in `sys.modules`, a runtime, its channel endpoints, an
output buffer with a pipe behind it, an event loop, and an execution slot. That bundle is the
agent's **kernel**; the process hosting them all is the **interpreter**, and it owns the protocol
descriptor, the reader thread, the address-space ceiling, and the signal handler. Which
of those two lists a thing falls into is the whole of this page.

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
the way the prototype's `kernel.py` does today. They attribute above it, and the coverage is good:

`print()` and `sys.stdout`
: A dispatcher keyed on a `contextvars.ContextVar`. It names the **execution**, not just the
  agent, which is what the verification below did not cover and what the prototype's global
  `_foreground` flag was doing before co-hosting.

A background `create_task`
: Follows for free -- asyncio copies the current context at task creation -- and because the
  context names the execution that started it, output from a task cell A left running is billed
  to A even while cell B holds the foreground. Keying on the agent alone would let A's chatty
  task eat B's result quota, which is exactly the defect `kernel-findings` #2 fixed for one
  agent and co-hosting would otherwise reintroduce.

`subprocess.run([...])`
: Each agent gets a pipe of its own, supplied as the default `stdout` and `stderr` by one patch on
  `Popen.__init__`. `sys.stdout.fileno()` returns the same descriptor, so code that redirects
  explicitly lands in the right place too.

`os.write(1, ...)` and `os.system()`
: Not attributable. These name the descriptor directly, so fd 1 becomes a shared bucket. What
  lands there is recorded as an event rather than dropped -- `observability.md` -- because it
  cannot be billed to an agent and a silent hole is worse than an unattributed line.

Verified with two agents printing, spawning background tasks, and running subprocesses at the same
time: every line landed in its own agent. That experiment established attribution **between**
agents and says nothing about the case above -- one agent's old background task emitting while a
new execution runs -- which is the harder half and is not yet tested. A subprocess inheriting a
per-agent descriptor is the piece most likely to resist per-execution attribution; if it does, the
weaker guarantee should be stated rather than the stronger one implied.

The prototype's background-output fix, which `runtime-protection.md` carries, survives -- and
survives better than it did. Between-execution output stays attributed to the agent whose task
wrote it, so a chatty background task can no longer displace a result, and cannot reach a
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
- The `Popen.__init__` patch was verified against `subprocess.run`. It has not been checked against
  `os.popen`, `multiprocessing`, or a child that re-execs.
