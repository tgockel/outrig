# 0003-17 -- A host call blocks only its caller

## Context

A hosted call is synchronous (`hosted-objects.md`): `repo.index.commit("msg")` blocks the calling
kernel's thread until the host answers, as a local call would. While it waits, that kernel's event
loop does not turn, which `execution-and-rounds.md` states as the cost. Everything else has to keep
working: other kernels, the interpreter's reader thread, a callback the host makes into the waiting
call, and the host's ability to stop it. `0003-16` proved the transport with one connection on one
thread. This task proves it with several.

Four facts decide the design:

- **RPyC serves a request on whichever thread is reading the connection.** A thread waiting for
  its reply reads the connection, and a callback request that arrives meanwhile runs on it.
  `serve_threaded`'s docstring warns that a reply can be read by another thread serving the same
  connection, and recommends a connection per client thread; a callback request goes to whichever
  thread reads it in the same way. So each kernel gets its own connection to each binding, and
  calls from a kernel's threads on that connection are made one at a time.
- **The reader thread delivers every frame.** If it ever waited for an RPyC reply, nothing would
  deliver that reply. The primary kernel is built before the reader thread starts, and every other
  kernel is built on the reader thread, by `_open`, so no kernel can resolve a binding while it is
  being built. A connection's root is fetched on the kernel's own thread, at first use.
- **RPyC has no cancel.** A request runs on the host until it returns. A client that stops waiting
  only stops reading for it, and the host-side call goes on. The binding runs it on the thread that
  read it from that connection, so the connection's next request waits behind it.
- **Today an interrupt reaches only the primary.** `agent-placement.md` records that Python runs
  signal handlers on the main thread alone, and `Kernel.interrupt` declines for every other kernel.
  A kernel waiting on a hosted call, though, waits on `0003-16`'s condition variable, which the
  reader thread can signal for any kernel. A hosted call is the one blocking call a child's kernel
  can be woken from.

`runtime-protection.md`'s check reads a kernel waiting on a hosted call as quiet: its thread is
idle in a wait, as in `time.sleep`, so the host's runaway check leaves it alone, and a user's
Ctrl-C is what stops it. That page says nothing about hosted calls yet.

## Goal

A kernel blocked in a hosted call blocks only itself: other kernels run, the reader thread answers,
a callback runs on the blocked thread, and an interrupt wakes the call on any kernel. This is a
spike: if an acceptance item cannot be met, the task stops and reports to the maintainer rather
than changing the threading model on its own.

## Deliverables

- **One connection per kernel and binding**, opened at the kernel's first use of the binding, with
  its root resolved then, on that kernel's thread -- never on the reader thread, and never while a
  kernel is being built.
- **RPyC's `sync_request_timeout` off.** A call waits as long as the host takes, and stopping it is
  the interrupt's job.
- **Calls on one connection serialized**, so at most one of a kernel's threads waits on it at a
  time. The lock is re-entrant for the thread holding it, so a hosted call made inside a callback
  goes through. A thread waiting for the lock waits where the reader thread can wake it, as a
  caller waiting for its reply does, and a call woken there was never sent. `0003-21` wakes every
  such waiter when admission closes and refuses its call (`lifecycle.md`).
- **One request at a time in a binding process**, across all its connections, except a request
  made inside a callback of the request in progress (fork 3).
- **`asyncio.to_thread(...)` as the awaitable form**, proved here with what serialization means for
  it: two workers' calls on one binding run one after the other. `0003-20`'s orientation tells the
  agent.
- **Callbacks on the caller's thread.** The host's request to run a callback is served by the
  thread waiting on that connection, which is the thread that made the call. The proxy is revoked
  when that call returns. A hosted call made inside the callback is a new request on the same
  connection, and is checked on the host like any other.
- **Interrupts and cancels.** The host's `interrupt` or `cancel` aimed at a kernel whose thread
  waits on a hosted call, for its reply or for the connection's lock, wakes that wait from the
  reader thread, on any kernel -- under the rules `_on_sigint` applies to whose code an interrupt
  may end, and on the primary per fork 2. The call raises in its caller (fork 1) and says that the
  call's outcome on the host is unknown, or, when it was woken from the lock, that it was never
  sent. The connection stays usable, and the reply that arrives later is discarded rather than given
  to a later call. A `to_thread` worker's call is not woken: cancelling the coroutine that awaits it
  cancels a wait, and the call goes on (`hosted-objects.md`).
- **`runtime-protection.md` updated** with a kernel blocked in a hosted call: what the check reads,
  what Ctrl-C does on the primary and on a child's kernel, what the agent is told, and that the
  host-side call keeps running.
- **The spike's record**, in this task's `## Decisions`: the versions involved -- RPyC, the example
  library, Python, git, podman, the kernel and OS, as far as this task used them; where each
  process, mount and credential sits; what ran for real and what was mocked; what went untested;
  and which limits were reached and what happened.

## Acceptance

Through `0003-16`'s test relay, with interpreters started on the host and kernels opened over the
protocol as `interpreter_tests.rs` opens them:

- **Kernel B runs while A blocks.** With kernel A blocked 10 s in a hosted call, kernel B runs
  executions to completion and answers `inv`. A answers `cpu` and `msg`, which the reader thread
  answers, and A's `inv`, which its loop answers, is answered once the call returns.
- **A callback runs on its caller's thread.** A's callback runs on A's thread while B has a call of
  its own in flight to the same binding, asserted by thread identity inside the callback.
- **Two workers never swap replies.** Two `asyncio.to_thread` workers on one kernel call one
  binding: the second call reaches the host only after the first has returned, and each worker
  receives its own result.
- **An interrupted call leaves its connection usable.** Interrupting kernel B -- not the primary --
  while it waits 10 s on a hosted call raises in B's caller at once. B's next call on that
  connection returns its own result once the interrupted call has finished on the host, and the
  interrupted call's late reply reaches nobody. The same holds on the primary, and for a cancel.
- **A call woken from the lock was never sent.** While a `to_thread` worker's call on kernel B's
  connection is in flight, B's own thread makes a call on that connection and waits for the lock.
  Interrupting B raises in its caller at once, saying the call was never sent, and the binding
  never receives that request, before or after the worker's call returns.
- **A binding runs one request at a time**, per fork 3. Kernels A and B each call, on one binding,
  a fixture method that records when it starts and ends, and the two never overlap on the host. A
  hosted call made inside A's callback while A's request is in progress runs rather than waiting
  behind it.
- **A kept callback is revoked.** A callback proxy the host kept after its call returned raises on
  the host when it is invoked, and nothing runs in the container.
- **A callback's own calls are checked.** A private attribute read from inside a callback is
  refused, as it is from an execution.
- **A proxy stays with its kernel.** A proxy kernel A obtained, passed by kernel B as an argument
  to the same binding, is refused before anything is sent, and the host method is not called.
- `crates/outrig/public-api.txt` is unchanged.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What a woken call raises -- Recommended: what the same request raises everywhere else,
   `KeyboardInterrupt` for an interrupt and `asyncio.CancelledError` for a cancel.** Each carries a
   message naming the binding and saying the call's outcome on the host is unknown, or that the call
   was never sent when it was woken from the connection's lock. Agent code that catches `Exception`
   lets either through, so an interrupt still ends the execution, and the model reads it as it reads
   any interrupt. The alternative is an OutRig exception type, which can carry the binding and a
   call id as attributes. Derived from `Exception`, it is caught by a bare `except Exception`, and
   the execution carries on -- the outcome the host's interrupt was sent to prevent. A subclass of
   `KeyboardInterrupt` would have both properties, and can be added later without breaking code that
   catches `KeyboardInterrupt`.
2. **How an interrupt reaches the primary's hosted call -- Recommended: by waking the wait, as on
   every other kernel, and not by SIGINT.** `_on_sigint` walks outward from the frame the signal
   interrupted and raises once it finds agent code, and RPyC's frames are not among those it
   declines to raise in. So the signal can raise between the two `read` calls of `Channel.recv`,
   after a frame's header and before its body, and leave the next read starting part way through a
   frame. Waking the wait raises at one place, between frames, on every kernel alike. The cost is a
   second branch in `Kernel.interrupt`, which has to know whether its kernel's thread is inside a
   hosted call and send the signal only when it is not. `hosted-objects.md` and
   `agent-placement.md` describe this recommendation; if the signal is kept for the primary, the
   task updates them.
3. **How many requests a binding process runs at once -- Recommended: one at a time across all its
   connections, except a request made inside a callback of the request in progress.** With a
   connection per kernel, two kernels' requests can reach one binding together, and served on two
   threads they would run in the hosted library at the same time. A hosted library need not be
   thread-safe, and the example is not: GitPython's `cmd.py` says that "GitPython isn't
   thread-safe", and that `stream_object_data` needs "one independent `Git` instance per thread".
   One request at a time means no hosted library has to be. A request made from inside a callback
   of the request in progress has to run, because that request waits for the callback and the
   callback waits for it. The cost is that one kernel's slow call -- a push -- holds every other
   kernel's calls to that binding until it returns, though those kernels still run everything
   else. The alternative, a thread per connection, lets kernels' calls run together and leaves a
   library that is not thread-safe to fail in ways that depend on timing. Neither answer
   coordinates the agent's own writes to files the hosted library also writes, such as a
   working-tree file or the index; that is documented, not prevented.

## Dependencies

- **Hard: 0003-16.** The stream, the relay, the callback proxies and the host's checks that this
  task proves things about.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- "Calls are synchronous", and one connection per
  kernel and binding.
- `plan/phase/0003-python/runtime-protection.md` and `plan/phase/0003-python/agent-placement.md`
  -- the check, the two remedies, and why an interrupt reaches one thread.
- `crates/outrig/src/python/interpreter.py` -- `Kernel.interrupt`, `Kernel.cancel` and
  `_on_sigint`.
- `plan/next/awaitable-hosted-calls.md` and `plan/next/cancel-a-running-hosted-call.md` -- an
  awaitable call without a worker thread, and stopping a call on the host, both deferred.
