# A hosted call blocks its kernel until it returns

## Context

In phase 0003 a hosted call is synchronous. It blocks the calling kernel's thread, and that
kernel's event loop with it, until the reply arrives; other kernels keep running (`0003-17`,
`plan/phase/0003-python/hosted-objects.md`). An agent that needs its loop to keep running during
a long call -- so `runtime.wait` returns on user input, or so its other tasks progress -- writes
`await asyncio.to_thread(...)`. Calls on one connection are serialized, so two such calls on the
same binding from one kernel still run one after the other.

That costs a pool thread for each call waiting at once, each with the 8 MiB stack the memory
ceiling counts (`plan/next/one-drain-thread-for-every-execution.md`), and the model has to know
to write it.

RPyC 6.0.2 has no asyncio support. `rpyc.async_` returns an `AsyncResult`, which is not
awaitable. Upstream issue tomerfiliba-org/rpyc#506 is open, and a maintainer's comment there
proposes adding `__await__` to `AsyncResult`; nothing has been released as of 2026-09-30.

## Shape

- An awaitable form of a hosted call, built on `async_`: the request is sent without waiting, and
  `AsyncResult.add_callback`, which runs when the reply is processed, settles an asyncio future on
  the kernel's loop through `loop.call_soon_threadsafe`.
- A reply is processed only by a thread serving the connection. Under `0003-17` the blocked
  caller does that. An awaitable call needs some other thread to do it, and the interpreter's
  reader thread, which already receives the pipe's `rpc` frames, is the candidate.
- A callback the host invokes during the call runs on the blocked kernel thread today. With the
  caller not blocked it would be scheduled on the kernel's loop, where a callback that blocks
  stops every task of that kernel.
- Synchronous calls stay, since attribute access on a hosted object cannot be awaited.
- Cancelling the awaiting task ends the wait only. The host call keeps running
  (`plan/next/cancel-a-running-hosted-call.md`).

## Acceptance

- An awaited hosted call that takes 10 s leaves its kernel's other tasks running, and
  `runtime.wait` returns on user input during it.
- No thread is started per call.
- Policy, events and admission treat it as they treat a synchronous call, and a callback passed
  to it is evented with the parent call id.
