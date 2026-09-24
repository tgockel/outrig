# A thread the agent starts has no execution to bill its output to

## Problem

The interpreter bills output to an execution through a `contextvars.ContextVar`
(`crates/outrig/src/python/interpreter.py`, `_CURRENT`). asyncio copies the context into every
task and `asyncio.to_thread` copies it into its worker, so both are attributed. Two common routes
are not:

- `threading.Thread(...)`. Python 3.13 starts a thread with an empty context.
- `loop.run_in_executor(None, fn)`. It submits to a pool without copying the context. This is the
  spelling older code and many libraries use.

What either prints lands on the exec's stderr, the unattributed bucket, which the host records but
the model never sees. Nothing is misattributed, but an agent that moves blocking work onto a thread
loses its output. `agent-placement.md` states this as a known gap.

## Why the obvious fix is wrong

The obvious fix is to patch `Thread.start` to run `run` inside the starter's context, the way
3.14's `sys.flags.thread_inherit_context` does. That fixes a one-off thread and breaks pools. A
`ThreadPoolExecutor` worker is created by whichever submission first needed it and then serves
every later one, so it would bill every later execution's work to that first execution. That is
worse than billing nothing.

## Sketch

Attribute per work item, not per thread:

- wrap `ThreadPoolExecutor.submit` so that each callable runs in `contextvars.copy_context()`
  taken at submission (`run_in_executor` goes through `submit`);
- patch `Thread.start` only for threads that are not pool workers.

Worth checking first: whether 3.14's flag, if the payload moves to it, already covers
`run_in_executor`. It does not copy context per item either.

## See also

- `plan/phase/0003-python/agent-placement.md` -- "Output stays attributed", which lists this gap.
