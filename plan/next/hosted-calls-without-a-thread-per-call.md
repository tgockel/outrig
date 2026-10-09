# Every hosted call in flight holds a worker thread

## Context

A hosted object is awaited through a facade over RPyC (`plan/next/a-hosted-object-is-awaited.md`,
to be numbered; `plan/phase/0003-python/hosted-objects.md`, "Calls are awaited"). `await`
submits a job to the kernel's pool for the binding, a worker thread replays the path's steps as
RPyC requests through one of the pool's connections and settles the awaiting code through the
kernel's loop, and the loop keeps turning meanwhile. That meets the first and third acceptance
items of this entry's earlier form: a 10 s awaited call leaves the kernel's other tasks running
and `runtime.wait` returns on user input during it, and policy, events and admission see the
call as they see any other. What remains is the second item, no thread per call. Each in-flight
call holds a worker thread for its whole duration, with the 8 MiB stack the memory ceiling
counts (`plan/next/one-drain-thread-for-every-execution.md`); the pool's executor bounds them
at four per kernel and binding, and a fifth call queues with no thread until a worker is free.

The earlier sketch had the interpreter's reader thread, which already receives the pipe's `rpc`
frames, process each reply and settle its future. That was wrong. Unboxing a reply can issue a
nested synchronous `inspect` request for the object's method names (`0003-16`), and only a
kernel-side thread may dispatch, never the reader thread (`0003-17`), so the reader thread would
wait for a reply only it can deliver. `AsyncResult.add_callback` does not change that: the
callback runs only when some thread serves the connection, which is the thread the sketch
wanted to remove.

RPyC 6.0.2 has no asyncio support. `rpyc.async_` returns an `AsyncResult` with no `__await__`;
upstream issue tomerfiliba-org/rpyc#506 is open, with nothing released as of 2026-10-09. The
alternative that removes the thread per call and the pool together is
`plan/phase/0003-python/potential/custom-hosted-object-protocol.md`.

`0003-17`'s "See also" names this entry by its earlier filename,
`plan/next/awaitable-hosted-calls.md`. A done task's file is a record and is not edited, so the
reference stays as written.

## Shape

- A long-lived serving thread per connection, at most four per pool, in place of a worker per
  in-flight call. A job's requests go out with `async_`, and the connection's thread reads and
  unboxes the replies -- a nested `inspect` goes out and comes back on that thread, as today --
  and settles each job's future through `loop.call_soon_threadsafe`. Several jobs are in flight
  on one connection at once, matched by RPyC's sequence numbers, so a tenth call waits for no
  connection and starts no thread.
- A callback the host makes during a call still runs on a kernel-side thread: a plain function
  on the serving thread, a coroutine function on the kernel's loop, as the facade runs them.
- An interrupt, a cancel and `close_hosted` settle a job's future as the facade's wake does; a
  late reply is dropped by the serving thread when it arrives, which replaces the tidy.
- Open: the binding serves each connection on one thread and runs each request on it, so a long
  call would hold the other replies of its connection until it returned. Either the binding runs
  a request on a worker of its own, or the pool assigns a job to a connection with nothing in
  flight and the gain is the thread per call alone.

## Acceptance

- Ten awaited calls in flight from one kernel on one binding start no thread beyond the pool's
  serving threads; thread names show at most four for the pool.
- `0003-17`'s interrupt and callback items still pass: an interrupt raises `KeyboardInterrupt`
  where the call is awaited within a second, with `0003-17`'s message, and the connection is
  usable after; a callback the host makes during the call runs, and is evented with the parent
  call id.
