# A bare `await handle` is not recorded as a wait

`runtime.wait` records what it waits on once it has been suspended for a second (#697:
`exec.wait.started` and `exec.wait.ended`, `Session::waits`, the `awaiting ...` status line). A
bare `await handle` goes through `Handle.__await__` (`crates/outrig/src/python/outrig/_agents.py`)
and a bare `await task` through asyncio alone, so code awaiting one child's handle directly still
reads `Python running` on `run-new`'s status line and leaves no wait in the record.

`Handle.__await__` could report the same way, as a wait on that one handle: the same `wait` and
`waited` lines, with `on` holding the handle alone, armed by the same one-second timer. A bare
`await task` has no hook of OutRig's to report from, and is left to the preamble's advice to use
`runtime.wait`.
