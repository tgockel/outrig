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
