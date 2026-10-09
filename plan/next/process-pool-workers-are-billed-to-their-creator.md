# A process pool's workers are billed to whoever made the pool

## Problem

A fork copies the forking thread's context, and the fork hooks in `interpreter.py` give the child
a descriptor drained for the execution that context names (`agent-placement.md`, "Output stays
attributed"). A `multiprocessing.Pool` or a `ProcessPoolExecutor` forks its workers when it is
made, or on the first submissions that need them, and those workers then serve every later
execution's items. So what execution B's item prints inside a worker that execution A forked is
billed to A: it arrives as A's background, under A's id, where the model reads it as A's doing.

#474 fixed the same shape for thread pools by running each item in a copy of the submitter's
context. A process pool's item runs in another process, which no context of this one reaches; the
worker's descriptor is the only attribution there is, and it was fixed at the fork.

## Sketch

- Nothing in `multiprocessing` lets a task carry a descriptor to the worker that runs it, and a
  context cannot be pickled, so per-item attribution needs the pool's cooperation or a patch on
  the worker side of the fork.
- The honest step is a sentence in `agent-placement.md`: a process pool's workers are billed to
  the execution that forked them, so a pool made in one execution and used from the next bills
  the next's output to the first. One pool per execution avoids it.
- Decide whether anything more is worth building, against how often an agent keeps a process
  pool across executions.

## See also

- `plan/phase/0003-python/agent-placement.md` -- "Output stays attributed".
- #474 -- thread pools, billed per item.
