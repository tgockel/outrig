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

1. **`RLIMIT_AS` or `RLIMIT_DATA` -- Open.** `AS` is simpler and is awkward for mmap-heavy code.
   The measurements behind the design used `AS`; a switch needs its own evidence.
2. **Operator-configurable or fixed -- Recommended: fixed for now.** A config key is a surface
   commitment, and nobody has yet needed a different number.

## Dependencies

- **Hard: 0003-02.** The ceiling is set by the interpreter at start.
- **Soft: 0003-06.** Both are survivability, and landing them adjacently keeps the story together.

## See also

- `plan/phase/0003-python/runtime-protection.md` -- the measurements and the stated limits.
- `plan/phase/0003-python/agent-placement.md` -- why memory is the shared resource once agents are
  co-hosted.
