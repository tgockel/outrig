# 0003-07 -- An agent's memory has a ceiling that reports rather than kills

## Context

`agent-placement.md` co-hosts agents in one interpreter, which makes memory the one resource an
agent can exhaust on everyone else's behalf. Threads contain a runaway loop; they do not contain
`[x] * 10**12`, which OOM-kills the process and takes every agent with it.

`runtime-protection.md` puts `RLIMIT_AS` in the first milestone for that reason rather than as an
early start on resource limits: it is a single call at boot, and without it one agent's
allocation ends the session with no explanation. Measured against a 512 MiB ceiling, an outsized
allocation raised `MemoryError` immediately, a gradual one raised after 484 MiB, and the
interpreter stayed usable afterwards.

The limits of the fix are as important as the fix. The ceiling is process-wide, so while one agent
sits pinned at it an ordinary `import json` on another thread raises too -- measured. It makes an
OOM legible rather than mysterious; it is not per-agent isolation, and the task should not let
anyone read it as such. `os._exit(0)` stays uncontained and is one line of generated code.

## Goal

An allocation an agent cannot afford comes back as an error it can see, instead of an interpreter
that vanished.

## Deliverables

- `resource.setrlimit(RLIMIT_AS, ...)` at interpreter start, before any agent exists.
- **A ceiling that is chosen, not hardcoded by accident.** Whether it is operator-configurable is
  fork 1; either way the value and its reasoning are written down, because a ceiling set too low
  turns ordinary work into `MemoryError` for everyone.
- **A decision about descendants, because the limit reaches them.** Resource limits are inherited
  across fork and exec -- measured: a child exec'd from a 512 MiB interpreter reports
  `(536870912, 536870912)`. So the ceiling applies to compilers, linkers, and any JVM or Node the
  agent launches, which routinely reserve large address ranges while using modest resident memory,
  and which fail by aborting rather than by raising a Python `MemoryError`. Decide whether this is
  an interpreter limit or a descendant policy, and state the soft and hard values with the
  restoration rule: a lowered hard limit cannot be raised back by an unprivileged child, while
  leaving it high lets a child raise its own soft limit. Do not solve it by raising a
  process-global limit around `Popen` in a threaded interpreter, and do not reach for
  `preexec_fn`, which Python documents as unsafe with threads.
- The resulting `MemoryError` reported as that agent's error result with its traceback -- the same
  recovery shape the interrupt path produces.
- **The limitation, stated where a reader will meet it.** Process-wide, degrades siblings while
  pinned, and `os._exit` remains uncontained.

## Acceptance

- `[x] * 10**12` returns an error result naming `MemoryError`, and the next submission runs.
- A gradual allocation reaches the ceiling and raises rather than being killed, and the
  interpreter is usable once the memory is released.
- **A sibling's ordinary work fails while the ceiling is pinned, and recovers when it is not.**
  This is the honest half and it is worth a test, so the limitation cannot quietly be forgotten.
- **The limits an exec'd child actually sees are asserted**, and a representative real build runs
  under the chosen policy. Interpreter-allocation tests alone would have missed this entirely; the
  512 MiB figure is evidence from one measurement, not a defensible universal default.
- Nothing in the docs describes this as isolation.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **`RLIMIT_AS` or `RLIMIT_DATA` -- Resolved: `RLIMIT_DATA`.** `AS` is simpler and is awkward for
   mmap-heavy code. The measurements behind the design used `AS`; a switch needs its own evidence,
   which Decisions records.
2. **Operator-configurable or fixed -- Resolved: not configurable.** A config key is a surface
   commitment, and nobody has yet needed a different number. The value is a fixed rule rather than
   a fixed number: half the memory the container can see.

## Dependencies

- **Hard: 0003-02.** The ceiling is set by the interpreter at start.
- **Soft: 0003-06.** Both are survivability, and landing them adjacently keeps the story together.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- the measurements and the stated limits.
- `plan/phase/0003-python/agent-placement.md` -- why memory is the shared resource once agents are
  co-hosted.

## Decisions

Measured with the payload (3.13.15, static musl) on a 32-core, 125 GiB host.
`runtime-protection.md`, "The memory ceiling, as built", carries the long form.

- **The knob is `RLIMIT_DATA` (the maintainer's call, on this evidence).**
  - Inside the interpreter the two are alike: at 512 MiB, gradual growth raised at 484 MiB under
    `AS` (the prototype's figure, reproduced) and 504 MiB under `DATA`; an outsized request raised
    at once under both.
  - Outside it they are not. The smallest ceiling a child ran under, `AS` / `DATA`: `cc` 256 MiB /
    256 MiB; `go version` and `node -e` 1 GiB / 256 MiB; `node` creating a wasm memory, nothing up
    to 8 GiB / 256 MiB; `rustc` and `cargo build` of a hello world 4 GiB / 2 GiB; `java -version`
    8 GiB / 4 GiB. `DATA` does not count file mappings or reserved space never made writable, which
    is how the JVM, V8, and allocator arenas start.
  - What it misses: shared anonymous memory (`mmap.mmap(-1, n)` defaults to `MAP_SHARED`).

- **The value is half the memory the container can see (the maintainer's call).** The smaller of
  the cgroup's `memory.max` (v1 `memory.limit_in_bytes`) and `MemTotal`, halved; a lower soft
  limit already in place is kept, which is also how the tests set 256 MiB. OutRig sets no container
  memory limit and `memory.max` reads `max` inside, so today it is half the machine.
  - A fixed number fails one end or the other: 512 MiB breaks every toolchain above but `cc`, and
    the JVM's first heap is a sixty-fourth of the machine, so any constant is too low somewhere.
    Half scales with the machine as those needs do, and runs out before the machine does.
  - Without any ceiling `[0] * 10**12` already raises -- the kernel's overcommit heuristic -- but
    a 100 GiB list allocated in full. The ceiling is for growth, and the outsized test passes with
    or without it.

- **Descendants inherit the soft limit; the hard limit is left as found (the maintainer's call).**
  - An interpreter-only limit is not reachable: nothing race-free runs between fork and exec
    without `preexec_fn`, raising the limit around `Popen` lifts it for every thread, and a
    wrapper program in front of each spawn breaks `Popen`'s report of a missing executable
    (`FileNotFoundError` would become an exit status).
  - A lowered hard limit would leave a program that needs more no way to run. Left high, a child
    lifts its own with `ulimit -d unlimited` or `resource.setrlimit`, needing no privilege.
  - The ceiling is per process, not shared: it bounds nothing about their sum.

- **A reserve, because one call at boot bricked sessions.** Growing a global a small string at a
  time (`x.append(str(len(x)) * 3)`) under the bare ceiling lost the traceback ("could not be
  formatted"), and the next execution ran out in its own cleanup before it freed the slot: every
  later submission was `refused`. Chunked, dict, doubling, outsized, and comprehension-local
  growth were fine.
  - 32 MiB, private and never written, so it costs no resident memory. Agent code runs with it
    held.
    - The interpreter gives it back for its own work: starting an execution (`_start`, always),
      `_format_error` (both callers), the report (`_run`'s `finally`), the reader thread on
      `MemoryError`, the loop's exception handler on `MemoryError`, and, retried once, compiling,
      routing a line, encoding every message in `_send`, and `call_soon`.
    - It takes back what fits before the body. After each result it takes back what fits while
      sparing 1 MiB, so the idle loop is not left at the ceiling.
  - Taken back in one map when it all fits, in 1 MiB pieces when not, so it can succeed in part.
    - The first prototype gave the reserve back on `MemoryError` and retook it only with room to
      spare. Running out a second time before releasing then left nothing and bricked the session
      again.
    - Retaking greedily before each body means a pinned session's next body runs with almost
      nothing, and fails at once unless it frees first -- which `x = None` and `del x` do without
      allocating.
    - Always 32 pieces was measured by `/simplify` at +60 percent on a trivial submission's round
      trip. One map measures about a tenth over the previous version (423-426 against 383-401 µs).
  - `_with_room` retries on `MemoryError` alone. Catching `RuntimeError`, which a thread that
    cannot start raises, would re-parse a `RecursionError` for nothing -- and `_start` gives the
    reserve back before it starts the drain thread anyway.
  - Beneath it:
    - The slot is freed before anything in the report allocates.
    - A result that cannot be sent whole goes as its error alone.
    - The reader thread sleeps and retries on `MemoryError` rather than reaching `os._exit`.
    - `serve` swallows a `MemoryError` hit while reporting an escaped exception, with a `try`
      rather than `suppress`, which would allocate. For the primary, returning would end `main`.
  - Rejected in `/simplify`: taking and giving the reserve by moving the soft limit instead of
    mapping memory. It is simpler, but children would inherit the ceiling less 32 MiB, and it
    would silently undo an agent's own `resource.setrlimit` at the next execution.
  - `plan/next/one-drain-thread-for-every-execution.md` records the root cost the reserve covers:
    each execution's drain thread spends 8 MiB of the ceiling on its stack.

- **Review: the interpreter's own threads must survive the ceiling too.** The first cut was
  rejected in review: output written at the ceiling blocked for good. Fixing it found a second case
  of the same kind, which the review had not reported.
  - The drain allocated a buffer per `os.read`. At the ceiling that raised `MemoryError`, which
    its `except OSError` missed, so the thread ended with the pipe open. Writers blocked on a full
    pipe, and `finish()` blocked writing its sentinel before `DRAIN_TIMEOUT` could apply, so an
    interrupt could not help. Reproduced with 256 KiB written after pinning to within 4 KiB.
    - It now reads with `os.readv` into a 64 KiB buffer allocated in `_drained_pipe`, before the
      thread starts. What it has no memory to copy out is counted as dropped (`_lose`, and
      `Kernel.lose` for background bytes), still watching for the sentinel. Its `finally` closes
      the pipe first, so any exit turns a blocked writer into `BrokenPipeError`.
    - It does not take the reserve, unlike the review's suggestion. The drain runs while agent code
      does, so giving the reserve back there hands it to the code that exhausted memory. Dropped
      output can be counted honestly; a lost report cannot.
    - A second review found that fix over-counting: out of memory after `pending += chunk`, it
      counted the whole read dropped while those bytes stayed in `pending`, so 16 KiB written
      could report 19 KiB kept and dropped (22 of 30 trials, measured). Accounting is now
      structural rather than patched in the handler.
      - `_take` and `Kernel.background` keep all of what they are given or none of it, and
        `_pass` copies out one range and keeps it or counts that range dropped.
      - `_split` finds the sentinel in place, taking the held-back tail before anything is passed
        on, so no range goes two ways.
      - Where even counting fails, bytes go uncounted and the held-back tail is dropped with
        them: under-counting is the only way the numbers can be wrong.
      - 150 of 150 trials across six write sizes now report exactly what was written.
  - The event loop, found while checking the drain fix: agent code that awaits while holding
    memory at the ceiling. A completing future schedules its callbacks through `call_soon`, and
    asyncio's `Future` clears callbacks it could not schedule. With the task's wakeup lost, the
    task never ran again. `Task.cancel` could not reach it either, since it assumes the step is
    already scheduled once its waiter is done. So the slot stayed held. The loop's
    `_read_from_self` failed the same way and spun, logging, on every turn.
    - `_Loop(asyncio.SelectorEventLoop)` overrides the public `call_soon`: give the reserve back
      and try once more. `_attributed_exception` gives it back on a `MemoryError`, so a failed
      wakeup read is followed by one that works.
    - `_start` now always gives the reserve back first. Instrumented, every wakeup failure followed
      a `_start` that succeeded without giving it and left the loop at 255 of 256 MiB.
  - Regressions: `output_written_at_the_ceiling_is_drained_and_counted_once` and
    `an_execution_awaiting_at_the_ceiling_is_still_woken`. Each hangs, quiet for 20 s, without its
    fix. The first also asserts kept plus dropped equals what was written, for five write sizes,
    which fails on the over-counting cut.

- **Residuals, stated in `runtime-protection.md`.** Co-hosted agents share the reserve, so one
  agent's running code can take what another's report gave back. A traceback CPython had no room
  to build is reported as `MemoryError` alone -- so only coarse growth and outsized requests
  assert a traceback. Output written at the ceiling is counted as dropped, not kept. A request line
  longer than the reader's buffer that runs out halfway is lost. Agent code that keeps holding
  memory at the ceiling after the reserve went to its loop can starve the loop again. `os._exit`
  is uncontained.

- **The real build is `cargo build` of a hello world, run from the interpreter under the derived
  ceiling.** It needs nothing the test suite does not already have, and it fails at the link step
  under a 512 MiB ceiling, checked by capping the rule there.
