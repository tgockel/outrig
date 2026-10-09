# A call that outlives its kernel runs on in the binding, counted by nothing

## Context

A hosted call runs in the binding on the thread serving its connection, and RPyC has no cancel
(`0003-17`; `plan/next/cancel-a-running-hosted-call.md`). When its kernel is released (`0003-26`,
fork 9) or its connection closes, the caller is told `unknown`, the binding runs the call to
completion, and nothing counts it afterward: the kernel's pool is gone, `children-max` stops
counting the child once its kernel is gone, and the releasing agent is charged nothing
(`plan/phase/0003-python/hosted-objects.md`, "Lifetime"). So spawn a child, await, in a
background task, a call that never returns, release, repeat: each cycle leaves a thread and a
call in the binding, and neither `children-max` nor `requests-max` bounds it. The maintainer
documents it as unbounded for now and notes that it needs addressing later.

## Candidate bounds, none chosen

- **A per-binding cap on calls in flight**, say `calls-max` in `[bindings.<name>]`, counting every
  call the binding is running whether or not its caller still exists; a call past it is refused
  with the reason, as `requests-max` refuses a request.
- **Charge the releasing agent.** A surviving call stays counted against the agent that released
  the child until it returns, so the cycle exhausts the spawner's own allowance.
- **Restart the binding past a threshold.** Past a count of surviving calls, kill and restart the
  binding process: every call in it is reported `unknown`, every proxy to it becomes invalid, and
  its factory runs again (option 3 of `plan/next/cancel-a-running-hosted-call.md`).

## Acceptance

- The cycle -- spawn, await in a background task a call that never returns, release -- repeated
  past the chosen bound is refused or ended there, with a documented error and an event; the
  binding's thread count stays under a stated ceiling; calls from other kernels keep running; a
  call that returns is uncounted.
