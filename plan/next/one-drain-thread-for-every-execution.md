# Every execution starts a thread, and each costs 8 MiB of the memory ceiling

## Problem

The interpreter drains each execution's output pipe on a thread of its own
(`crates/outrig/src/python/interpreter.py`, `_Execution._drained_pipe`). Every thread gets the
8 MiB stack the interpreter sets for all threads, because musl's 128 KiB default lets deep but
legal recursion crash the process. `RLIMIT_DATA` counts those stacks, so every execution spends
8 MiB of the memory ceiling before running any code.

That cost is why an execution cannot start once agent code has pinned memory at the ceiling. The
reserve exists partly to cover it: `_start` gives the reserve back before it starts the thread. It
is also why the reserve is 32 MiB and not a few. And the thread that
`_Execution.descriptor` starts for a child spawned after its execution reported gets no such help.
Under a pinned ceiling that spawn fails.

## Why the obvious fix is wrong

Starting drain threads with a smaller stack means changing `threading.stack_size` around the
start. That setting is process-wide, so a thread agent code starts at the same moment on another
thread would get the small stack, and deep recursion there kills every agent.

## Sketch

Use one long-lived drain thread for the whole process, started at boot, that polls every
execution's pipe with `select.poll`. Each execution registers its read end and its sentinel,
which splits the body's output from its background output. Then:

- starting an execution costs a pipe and nothing counted against the ceiling;
- the late-child pipe in `descriptor()` needs no thread;
- the reserve could shrink to what formatting and reporting need.

The drain logic (`_drain`, `_take`, `_end_body`) keeps its semantics. Only its scheduling
changes.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- "The memory ceiling, as built".
- `plan/done/phase/0003-python/tasks/0003-07-a-memory-ceiling-that-reports.md` -- Decisions.
