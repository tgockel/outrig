# 0003-02 -- The interpreter runs an agent's Python and answers by agent id

## Context

`agent-placement.md` settles the shape: one interpreter process per session, one agent per thread,
each agent owning a **kernel** -- its session module, event loop, endpoints, output buffer, and
execution slot. The process owns the protocol descriptor, the reader thread, and the signal
handler.

The first milestone runs one agent. The point of building it multi-agent-shaped now is that it
cannot be retrofitted cheaply: the prototype's program is a pile of module globals (`_running`,
`_foreground`, `_captured`, `_runtime`, `G`, `_ARRIVED`), and every protocol message would have to
gain a required routing field later -- a breaking change across both halves plus a rewrite of the
twenty protocol tests whose survival was the argument for porting rather than rewriting.

Three mechanisms in the prototype are load-bearing rather than incidental, and the port must keep
them: the protocol never shares a file descriptor with the executed program, session globals live
in a module registered in `sys.modules` so `@dataclass` and `typing.get_type_hints` can resolve
annotations, and output is bounded at the descriptor rather than at `print`.

## Goal

An interpreter that hosts agents by id, each with its own namespace and bounded observations, with
nothing in it assuming there is only one.

## Deliverables

- The interpreter program, ported from the prototype's `kernel.py`, with per-agent state on an
  agent object rather than in module globals. Each agent gets a session module registered under a
  distinct `sys.modules` name, its own event loop on its own thread, and its own execution slot.
- **The primary agent runs on the main thread.** `runtime-protection.md` measures why: Python
  signal handlers run only there, so it is the only agent an interrupt can reach.
- **Every protocol message carries an agent id**, in both directions, including `ready`.
- Bounded observations, ported intact: `OUTPUT_MAX` per execution, `BG_MAX` of between-execution
  background output kept as a labeled tail, `REPR_MAX` for one echoed value or inventory entry,
  `INVENTORY_MAX` names. `kernel-findings.md` records why the background bound is separate -- a
  chatty task displaced the result the model asked for.
- **Output attributed by execution, not merely by agent.** A `contextvars.ContextVar` naming the
  originating execution, so a task cell A left running is billed to A while cell B holds the
  foreground. `agent-placement.md` records that keying on the agent alone reintroduces the defect
  the background bound exists to fix.
- **A capture descriptor per execution, not per agent.** The contextvar covers Python writes; a
  subprocess writes bytes that carry no writer identity, so one pipe shared by an agent's
  executions produces a stream nothing can attribute. The `Popen.__init__` patch supplies the
  *current execution's* descriptor. An execution's result must not wait on a descendant still
  holding that descriptor open -- drain it independently and bound it.
- The naming the port implies: files named for the interpreter, since "kernel" now means the
  per-agent environment they host.

## Acceptance

- Protocol tests in the shape of the prototype's `tests/python_kernel.rs`, run against the real
  payload interpreter: a submission returns its output, a raising submission returns a traceback
  naming `<execution>`, a second submission while one runs is refused, and an unknown message type
  is ignored without killing the interpreter.
- **Two agents, exercised.** Submissions to two agent ids land in separate namespaces: a name
  bound in one is absent from the other, and results carry the id that asked. One agent runs in
  the milestone, so this is the test that keeps that from becoming an assumption.
- `@dataclass` and `typing.get_type_hints` work on a class the submitted code defines. This is the
  `sys.modules` trick, and a bare dict passes every other test while failing this one.
- Raw `os.write(1, ...)` and an inherited subprocess's stdout are both captured, and neither can
  produce a line the host parses as a protocol message.
- Output from a background task started by an earlier execution is attributed to that execution
  and does not consume the current one's budget.
- **The same for subprocesses, which is the harder half.** Execution A starts a child and
  returns; B starts another; A's child emits afterwards. Assert both attribution and quota -- that
  A's late bytes do not land in B's result and do not consume B's budget -- rather than only that
  the two agents were kept apart. If the chosen capture cannot do this, the weaker guarantee is
  stated in `agent-placement.md` rather than left implied by a passing per-agent test.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether agent teardown is in scope -- Recommended: not yet.** One agent's lifetime is the
   session's. Releasing an agent belongs with `work.md`, which is unqueued.

## Dependencies

- **Hard: 0003-01.** There is no interpreter to run the program in until the payload is mounted.

## See also

- `plan/phase/0003-python/agent-placement.md` -- the per-agent split, and the measured output
  attribution.
- `plan/phase/0003-python/execution-and-rounds.md` -- what an execution is, and its outcomes.
- The prototype branch's `crates/outrig-cli/src/python/kernel.py` and `kernel-findings.md` -- the
  mechanics to port and the two defects already found in them.

## Decisions

- **Output that no agent can be billed for goes to the exec's stderr.** This was the maintainer's
  choice. fd 1 and fd 2 are dup2'd onto the stderr the host already drains as diagnostics.
  - That covers `os.write(1, ...)`, `os.system`, and a thread no execution started.
  - stdout carries nothing but protocol. Every protocol message names a real agent. A flood cannot
    delay a reply.
  - The cost falls on `0003-13`: it gets this output on the same stream as the interpreter's own
    crash output and `outrig-interpreter:` diagnostics.
  - The rejected alternative was a protocol message with `agent: null`, forwarded by a drain
    thread.

- **The protocol.**
  - `open`, `exec`, and `inv` go in. `ready`, `result`, and `inv` come out. Every one carries
    `agent`.
  - The primary's id is the program's one argument (`python3 -I -c PROGRAM <id>`). Its `ready`
    doubles as the process greeting. `open` starts any other agent on a thread of its own and is
    answered by that agent's `ready`.
  - An id is valid when `outrig_session_<id>` is an identifier, since that is the kernel's
    `sys.modules` name.
  - Execution and inventory ids must be integers.
  - A message the interpreter cannot route is never answered. That covers unknown `t`, non-JSON, a
    non-object, an unknown agent, a duplicate or invalid `open`, and missing fields. Each is
    ignored with one `outrig-interpreter:` line on stderr. An unknown `t` is ignored so that an
    older interpreter survives a newer host.

- **Results are structured**, where the prototype rendered text.
  - A result carries `status` (`ok`, `error`, or `refused`), `output`, `dropped`, `error`, and
    `background`.
  - `background` is a list of `{id, output, dropped}`, grouped by the execution responsible and
    bounded by `BG_MAX` in total, tail kept.
  - That structure is what lets a test say that A's bytes are billed to A and not to B. It also
    gives `0003-13` a count of truncation, which `observability.md` notes nothing reports today.
    The host renders it for the model.
  - The traceback keeps the prototype's inline cap: it is formatting the interpreter produced,
    not captured bytes.
  - `refused` carries `holder`, the id in the slot, and no prose; the host words it. It does not
    take the backlog.
  - `unknown` is the host's outcome to assign, in `0003-03`.

- **Attribution is per execution, and the mechanism is a pipe plus a sentinel.**
  - Each execution gets its own `os.pipe()`, a random 16-byte sentinel, and a drain thread.
  - While the body runs, `print` writes into that pipe too, not straight into a buffer, so Python
    output and child output share one FIFO and keep their order. Appending Python writes straight
    to a buffer would let a `print` overtake a child's bytes that are still in the pipe.
  - When the body finishes, the sentinel is written and the interpreter's end closed. The drain
    bills what precedes the sentinel to the result, against `OUTPUT_MAX`, and what follows it to
    the kernel's backlog under the execution's id, until EOF. **The result waits for the
    sentinel, never for EOF**, so a child holding the pipe cannot delay it.
  - Python output from an execution that has already reported goes straight to the backlog. A
    child spawned after the report, by a task the execution left running, gets a pipe of its own
    billed the same way.
  - Every spawn gets a descriptor of its own: a `dup` of the execution's pipe while the body runs,
    otherwise a new pipe. It is closed after `Popen.__init__` returns, and no lock is held while
    `Popen.__init__` runs.
  - Once the budget is spent, a Python write is counted as dropped without being piped. A loop of
    prints past 16 KiB otherwise paid a pipe write and a drain wake-up each. `/simplify` measured
    that at about 12 µs per `print`, which is roughly 12 s for a million lines.
  - An exception asyncio catches is reported through a per-kernel handler. The handler reinstates
    the context that the task or callback carries. That covers an unretrieved task exception and
    "Exception in callback". Without the handler, the traceback -- "often the whole story" -- would
    land in the stderr bucket.

- **A spawn never runs under a lock, and the slot is claimed on the reader thread.** A design
  review of the plan found both hazards, and measured each on the payload before the code was
  written. A review of the commit then moved the claim again; see "After review" below.
  - `Popen.__init__` warns through `sys.stderr` (`bufsize=1` in binary mode, or `pass_fds`
    without `close_fds`), and that write takes the execution's lock.
    - The review proposed an `RLock`, and the first cut used one.
    - `/simplify` placed the fix one level down instead. The spawn takes a `dup` under the lock
      and runs without it, which also covers a callout on a second thread; an `RLock` would not.
      A plain `Lock` is then enough.
  - Claiming the slot in the coroutine instead lets two `exec` lines that arrive in one loop
    iteration both run.

- **More hardening from the same review.**
  - Every write goes through a helper that survives a child setting `O_NONBLOCK` on a shared
    pipe.
  - Drain threads never die.
  - The reader validates messages and catches everything: `json.loads(b"[" * 100000)` raises
    `RecursionError`, and the prototype's reader died on `[1]`.
  - Every string in an outgoing message is made surrogate-clean in `_send`, because serde rejects
    the lone `\ud800` that `json.dumps` otherwise emits, and the whole reply would be lost. Doing
    it once there, rather than at each free-text field, means a new field cannot forget it.
  - The inventory always replies. It snapshots the globals, skips non-`str` keys, caps names and
    type names, hides boot names by identity (so a rebound `asyncio` is listed), and reads a
    type's name without consulting its metaclass.
  - Submissions compile with `dont_inherit=True`.

- **stdin is moved aside as stdout is.** The protocol's input is dup'd to a private descriptor and
  fd 0 reads `/dev/null`. The prototype left fd 0 shared, so `input()` or a `cat` in a child would
  have eaten protocol lines. A forked child closes both protocol descriptors, through
  `os.register_at_fork`.

- **Loops run with `run_forever`, not `asyncio.run`, and are re-entered.**
  - A subagent's loop can be created on the reader thread and handed to its own thread before it
    runs.
  - `0003-06` has no `asyncio.run` handler to override; it installs one directly.
  - A `SystemExit` or `KeyboardInterrupt` from a background task escapes `run_forever`. That would
    otherwise end the agent, and for the primary the process. `serve` re-enters the loop, which
    was measured to resume pending tasks.
  - **stdin EOF exits the process from the reader thread**, so a wedged loop no longer keeps a
    dead session alive. The prototype measured "closing stdin exits? no".
  - A plain SIGINT on the port, measured: a wedge is recovered as an error result, and idle or
    suspended interpreters survive. `runtime-protection.md` records the table, and what was not
    measured.

- **Every thread gets an 8 MiB stack.** Found here, not in the prototype: musl gives a thread 128
  KiB, and deep but legal recursion off the main thread (`json.loads` of a nested document, `repr`
  of a nested list) segfaulted the whole interpreter instead of raising `RecursionError`.
  - It was hit first by the routing test's nested-JSON line on the reader thread.
  - Measured: 1 MiB still crashes and 2 MiB does not. 8 MiB matches the main thread.
  - `threading.stack_size` is set once at boot, so threads the agent starts inherit it.
  - It costs address space, which `0003-07`'s `RLIMIT_AS` counts.
  - The root cause is in the payload: `bin/python3.13`'s `PT_GNU_STACK` size is 0, which is what
    makes musl fall back to 128 KiB. A `sys.executable` child still has it.
    `plan/next/payload-threads-get-musls-128k-stack.md` covers patching the header. That change
    edits the verified artifact, so it is not made here.

- **A traceback starts at the submission's first frame.** The frames above it are the
  interpreter's own wrapper, and Python 3.13 made that worse by quoting `-c` source: every error
  opened with an interpreter line, `# noqa` and all. A compile error has no frame of the
  submission's and is reported as the exception alone.
  - A frame of the interpreter's in the middle of a traceback is left as it is. An example is the
    `Popen` patch when a spawn fails. Scrubbing those would mean relying on `linecache`'s private
    cache of `-c` source.
  Otherwise the echo rewrite, top-level await, `<execution>`, and all four bounds are ported
  intact. The echo call now takes the trailing expression's location.

- **The slot holds the execution, including its asyncio task.** `0003-03` lists retaining the task
  handle as its deliverable; it is already done here, because an execution slot has to hold
  something.

- **Tests are in the crate** (`src/python/interpreter_tests.rs`), because the payload locator is
  crate-private.
  - They run the payload this build embedded, unpacked into the user's cache as a first launch
    would. A build without it fails them, as it fails `python_payload.rs`; there is no skip path.
  - Every overlap is gated on a flag file or an `asyncio.Event`, never on a sleep. The
    subprocess-attribution test is exact: B fills its budget to the byte with nothing dropped, and
    every one of A's 20,000 bytes is accounted to A.
  - **Mutation-checked**, each by one failing test at least:
    - billing a reported execution's Python output, or a child's late bytes, to whatever runs now;
    - removing the exception handler, or its reach into callbacks;
    - claiming the slot late;
    - removing the Popen patch;
    - holding the lock across a spawn;
    - removing the stack size;
    - skipping the surrogate cleaning in `_send`.
  - Run 20 times in sequence, and 4 × 10 concurrently, with no failures.

- **Stated rather than fixed.** `agent-placement.md` records each of these:
  - A `threading.Thread` or `run_in_executor` starts with no execution, so its output goes to the
    stderr bucket. `plan/next/thread-output-is-unattributed.md` records why patching `Thread` is
    the wrong fix.
  - A `fileno()` kept past its execution may be reused.
  - `redirect_stdout` is process-wide.

- **For later tasks.**
  - `0003-03`:
    - The host's stderr drain has to be bounded now that stray output shares it.
    - Ids are integers, and a `refused` result is a possible reply to `exec`.
    - The reader thread writes protocol lines: `ready` for an opened agent, and a refusal. The
      host must read stdout independently of writing stdin, or the two can block each other.
  - `0003-06`:
    - The SIGINT handler should gate on a body running, not on the slot.
    - A `KeyboardInterrupt` landing in the middle of `_send` on the main thread could tear a
      protocol line.
  - `0003-07`: each lingering drain thread's stack counts against `RLIMIT_AS`.

- **`doc/` is unchanged and there is no CHANGELOG entry.** Nothing runs the program yet; `0003-16`
  documents `run-new`. `agent-placement.md`, `harness-components.md`, and `runtime-protection.md`
  carry the change instead.

- **After review, four fixes.** A review of the landed commit found four defects. Each was
  reproduced on the payload as a failing test before it was fixed, and each fix was
  mutation-checked.
  - **A submission during synchronous code ran instead of being refused.** The busy check was a
    callback on the kernel's loop, queued behind a body in `time.sleep` or `subprocess.run`, so it
    ran only once the body had ended and found the slot free. The slot is now claimed on the
    reader thread, under a lock that the execution's completion releases, and a refusal is sent
    from there. The test sends the second submission only once the first reports that it is
    blocking.
  - **A fork's child closed its own children's files.** The at-fork hook closed the protocol
    descriptors by number and kept the numbers, so a grandchild closed whatever the child had
    opened into them since. In the reproduction, a grandchild writing to a file its parent opened
    got `EBADF` and exited 1. The child now forgets the numbers once it has closed them.
  - **A fork after its execution reported lost its output.** The child's copy of the spent
    execution billed its writes, and a failed target's traceback, to a child-private backlog
    that nobody read.
    - A `before` fork hook now gives the forking execution a descriptor this process drains: a
      `dup` of the pipe while the body runs, or a pipe of its own afterwards, the same as
      `Popen` gets.
    - The child adopts that descriptor with fresh locks, since a lock may have been held by a
      thread the fork did not copy.
    - Both cases are tested.
  - **Empty writes grew the backlog without bound.** A zero-byte entry counted nothing against
    `BG_MAX`, so a loop of `print('', end='')` in a background task kept one tuple per call until
    the next result; the review measured about 72 MB per million. `background` now ignores empty
    data.
