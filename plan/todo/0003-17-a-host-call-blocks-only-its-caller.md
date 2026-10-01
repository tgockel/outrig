# 0003-17 -- A host call blocks only its caller

## Context

A hosted call is synchronous (`hosted-objects.md`): `repo.index.commit("msg")` blocks the calling
kernel's thread until the host answers, as a local call would. While it waits, that kernel's event
loop does not turn, which `execution-and-rounds.md` states as the cost. Everything else has to keep
working: other kernels, the interpreter's reader thread, a callback the host makes into the waiting
call, other calls to the same binding, and the host's ability to stop it. `0003-16` proved the
transport with one connection on one thread. This task proves it with several.

Five facts decide the design:

- **RPyC serves a request on whichever thread is reading the connection.** A thread waiting for
  its reply reads the connection, and a callback request that arrives meanwhile runs on it.
  `serve_threaded`'s docstring warns that a reply can be read by another thread serving the same
  connection, and recommends a connection per client thread; a callback request goes to whichever
  thread reads it in the same way. So no connection is shared between kernels, one call is in
  flight on a connection at a time, and a kernel with more than one call in flight has more than
  one connection: a pool of up to four per binding (`hosted-objects.md`, "Calls are synchronous").
- **An RPyC proxy is bound to the connection that produced it.** It sends its requests there
  (`netref.syncreq`), a proxy sent on another connection is boxed as a reference back into the
  sender (`Connection._box`), and the host's object table is per connection. For a call to take
  any free connection of the pool, the binding keeps one object table per kernel, shared by that
  kernel's connections, and the container side sends a proxy's request on the connection its call
  took.
- **The reader thread delivers every frame.** If it ever waited for an RPyC reply, nothing would
  deliver that reply. The primary kernel is built before the reader thread starts, and every other
  kernel is built on the reader thread, by `_open`, so no kernel can resolve a binding while it is
  being built. A connection's root is fetched on the kernel's own thread, at first use.
- **RPyC has no cancel.** A request runs on the host until it returns. A client that stops waiting
  only stops reading for it, and the host-side call goes on. The binding runs it on the thread
  serving that connection, so the connection is not free again until the call's reply arrives,
  though nobody wants it by then.
- **Today an interrupt reaches only the primary.** `agent-placement.md` records that Python runs
  signal handlers on the main thread alone, and `Kernel.interrupt` declines for every other kernel.
  A kernel waiting on a hosted call, though, waits on `0003-16`'s condition variable, which the
  reader thread can signal for any kernel. A hosted call is the one blocking call a child's kernel
  can be woken from.

`runtime-protection.md`'s check reads a kernel waiting on a hosted call as quiet: its thread is
idle in a wait, as in `time.sleep`, so the host's runaway check leaves it alone, and a user's
Ctrl-C is what stops it. That page says nothing about hosted calls yet.

The binding process serves each connection on a thread of its own, RPyC's ordinary server model,
so calls on different connections run in the hosted library at the same time. Until the review of
2026-10-02 this task recommended the opposite -- one request at a time per binding process, so that
no hosted library had to be thread-safe -- and the reviewer showed what it costs a service: a
hosted method that waits for a person for hours would have held every other call to that binding.
The maintainer agreed. A library that is not thread-safe now declares `serialize = true` in its
`[bindings.<name>]` entry (`0003-20`), and only that binding runs one call at a time. GitPython is
such a library: its `cmd.py` says that "GitPython isn't thread-safe", and that `stream_object_data`
needs "one independent `Git` instance per thread".

The same review asked that the binding be measured as a service client would use it, not only as a
Git library: a method that blocks for minutes, a question answered through a ticket and polled for,
and record operations in bulk, across two concurrent sessions. That is this task's second half,
and the numbers it produces decide whether a Rust service needs an adapter of its own
(`plan/next/rust-object-as-python-object.md`) or is served well enough by a hosted Python client.

## Goal

A kernel blocked in a hosted call blocks only itself: other kernels run, the reader thread answers,
a callback runs on the blocked thread, an interrupt wakes the call on any kernel, and no other call
to the binding waits for it -- from another kernel or from the same kernel's workers -- unless the
binding is declared `serialize = true`. Then, with that proven, the binding is measured as a
service client. This is a spike: if an acceptance item cannot be met, the task stops and reports to
the maintainer rather than changing the threading model on its own; if what fails is the pool's
shared object table or its request routing, it measures fork 4's alternative first, so the report
holds numbers for both.

## Deliverables

The task has two halves. The first is the threading, which `0003-20` and `0003-21` build on. The
second is the service-shaped fixture and its measurements, which nothing in this phase builds on
and which `/groom-plan` may split into a task of its own ("Dependencies").

### First half: threading

- **A pool of connections per kernel and binding, up to four.** A call takes a connection with no
  call in flight, opens another when none is free and fewer than four exist, and otherwise waits
  for one. The first is opened at the kernel's first use of the binding, with its root resolved
  then, on that kernel's thread -- never on the reader thread, and never while a kernel is being
  built; a later one is opened by the thread whose call needs it. A proxy's request travels on
  whichever connection its call took: the binding keeps one object table per kernel, shared by
  that kernel's connections, and the container side sends the proxy as a reference into that
  table (fork 4). A connection whose call was interrupted is not free until the discarded reply
  arrives.
- **RPyC's `sync_request_timeout` off.** A call waits as long as the host takes, and stopping it is
  the interrupt's job.
- **One call in flight per connection**, so a callback is served by the thread whose call it
  belongs to. A thread that finds no free connection waits where the reader thread can wake it, as
  a caller waiting for its reply does, and a call woken there was never sent. `0003-21` wakes
  every such waiter with the closing error when admission closes, and its call is recorded
  `cancelled` (`lifecycle.md`). A hosted call made inside a callback goes on the callback's own
  connection, whose lock is re-entrant for the thread holding it, so the binding serves it on the
  thread running the outer call.
- **A thread per connection in the binding process**, and **`serialize = true`** read from the
  declaration `0003-20` delivers. Without it, calls on different connections run in the library at
  the same time. With it, the binding runs one call at a time across all its connections, in the
  order they reached it, and a call made inside a callback of the call in progress runs at once:
  it arrives on the outer call's connection and is served on the thread holding the binding's
  lock, which is re-entrant (fork 3). The lock is taken just before the target is invoked, after
  policy has decided, so a held request holds it for nobody (`0003-22`).
- **`asyncio.to_thread(...)` as the awaitable form**, proved here with what the pool means for it:
  two workers' calls on one binding run at the same time, and under `serialize = true` one after
  the other. `0003-20`'s orientation tells the agent, and tells it to put the whole expression in
  the worker, since a method looked up on the kernel's thread costs that thread the round trips.
- **Callbacks on the caller's thread.** The host's request to run a callback is served by the
  thread waiting on that connection, which is the thread that made the call. The proxy is revoked
  when that call returns. A hosted call made inside the callback is a new request on the same
  connection, and is checked on the host like any other.
- **Interrupts and cancels.** The host's `interrupt` or `cancel` aimed at a kernel whose thread
  waits on a hosted call, for its reply or for a free connection, wakes that wait from the reader
  thread, on any kernel -- under the rules `_on_sigint` applies to whose code an interrupt may
  end, and on the primary per fork 2. The call raises in its caller (fork 1) and says that the
  call's outcome on the host is unknown, or, when it was woken from the wait for a connection, that
  it was never sent. The connection stays usable, and the reply that arrives later is discarded
  rather than given to a later call. A `to_thread` worker's call is not woken: cancelling the
  coroutine that awaits it cancels a wait, and the call goes on (`hosted-objects.md`).
- **`runtime-protection.md` updated** with a kernel blocked in a hosted call: what the check reads,
  what Ctrl-C does on the primary and on a child's kernel, what the agent is told, and that the
  host-side call keeps running.
- **The spike's record**, in this task's `## Decisions`: the versions involved -- RPyC, the example
  library, Python, git, podman, the kernel and OS, as far as this task used them; where each
  process, mount and credential sits; what ran for real and what was mocked; what went untested;
  and which limits were reached and what happened.

### Second half: the binding as a service client

- **A service-shaped fixture binding**, pure Python, standing in for the client a Rust service
  would ship (`embedding.md`, "Services written in Rust"): `wait_for_answer()`, which blocks until
  the test answers it, minutes later; `ask(question) -> ticket` and `poll(ticket) -> answer | None`,
  the pattern a service uses in place of a blocking wait
  (`plan/phase/0003-python/potential/ticket-based-service-waits.md`); and `list_records(n)` and
  `update_records(changes)` over a few hundred small records -- dicts of a few scalar fields --
  returned as a proxied list in one variant and by value in another. The fixture comes in two
  forms: one holds its state in the binding process, for the measurements within one session;
  the other is a client of a small service process the test starts, which holds the tickets, the
  answers and the records, for the measurements across two sessions.
- **Measurements in two topologies.** First, two kernels of one session, which share one binding
  process and, under `serialize = true`, one lock: the number of RPC requests each operation
  makes, with iteration of a proxied list of 300 records against the same list copied by value;
  the binding process's resident memory before, during and after the bulk operations; and the
  latency of kernel B's `ask`, `poll` and record calls -- median and 99th percentile -- while
  kernel A is blocked in `wait_for_answer`, once with the fixture declared `serialize = true` and
  once without. Second, two concurrent sessions as two clients of one external service: each
  session has its own binding process and its own lock, so one session's `serialize = true` lock
  is never contended by the other, and what this topology measures is the service -- session 2's
  latency for the same calls while session 1 is blocked in `wait_for_answer`, and each binding
  process's resident memory with both sessions present. The numbers, with the versions and the
  machine, go in this task's `## Decisions`. They decide whether
  `plan/next/rust-object-as-python-object.md` is worth building: the adapter earns its cost if a
  hosted Python client costs a service more per record or per wait than it can carry, and not
  otherwise. The decision is the maintainer's; this task records the numbers.

## Acceptance

Through `0003-16`'s test relay, with interpreters started on the host and kernels opened over the
protocol as `interpreter_tests.rs` opens them. The fixture binding has a recording method, which
notes when it starts and ends on the host and sleeps for a given time.

### First half: threading

- **Kernel B runs while A blocks.** With kernel A blocked 10 s in a hosted call, kernel B runs
  executions to completion and answers `inv`. A answers `cpu` and `msg`, which the reader thread
  answers, and A's `inv`, which its loop answers, is answered once the call returns.
- **A callback runs on its caller's thread.** A's callback runs on A's thread while B has a call of
  its own in flight to the same binding, asserted by thread identity inside the callback.
- **Two kernels' calls run at the same time.** Kernels A and B each call the recording method for
  2 s; the two intervals overlap on the host.
- **Two workers on one kernel run at the same time.** Two `asyncio.to_thread` workers on one
  kernel each call the recording method for 2 s; the intervals overlap, the kernel holds two
  connections to the binding, and each worker receives its own result.
- **A blocked call delays no other.** With kernel A blocked 60 s in a hosted call, a call from
  kernel B to the same binding returns within its own duration, and so does a call from a
  `to_thread` worker on kernel A.
- **`serialize = true` serializes, and a callback's request still runs.** With the fixture binding
  declared `serialize = true`, the two kernels' calls never overlap on the host, nor do two
  workers' from one kernel, and a hosted call made inside A's callback while A's call is in
  progress runs rather than waiting behind it.
- **A fifth call waits.** With four calls from one kernel in flight, a fifth waits for a free
  connection and runs when one of the four returns; the kernel never holds a fifth connection.
- **A proxy's request travels on any free connection.** A proxy obtained on one connection is
  called while that connection has a call in flight; the call travels on another connection and
  returns the right object's result.
- **An interrupted call leaves its connection usable.** Interrupting kernel B -- not the primary --
  while it waits 10 s on a hosted call raises in B's caller at once. B's next call returns its own
  result, the interrupted connection is not reused until the interrupted call's late reply arrives,
  and that reply reaches nobody. The same holds on the primary, and for a cancel.
- **A call woken from the wait for a connection was never sent.** With four workers' calls on
  kernel B in flight, B's own thread makes a call and waits for a free connection. Interrupting B
  raises in its caller at once, saying the call was never sent, and the binding never receives
  that request, before or after the workers' calls return.
- **A kept callback is revoked.** A callback proxy the host kept after its call returned raises on
  the host when it is invoked, and nothing runs in the container.
- **A callback's own calls are checked.** A private attribute read from inside a callback is
  refused, as it is from an execution.
- **A proxy stays with its kernel.** A proxy kernel A obtained, passed by kernel B as an argument
  to the same binding, is refused before anything is sent, and the host method is not called.
- **Nested callbacks run with every connection occupied.** Four calls from one kernel are in
  flight, each inside the host's call to a callback, and each callback makes a hosted call of its
  own. Every nested call is sent on its callback's connection and returns; none waits for a free
  connection, and the kernel never holds a fifth.
- **A disconnect drops one connection's entries only.** Kernel A holds proxies obtained on two
  connections to one binding, and the test closes the first connection while a call through the
  second's proxy is in flight. That call returns its result, the second's proxy keeps working
  afterward, the binding drops from A's table only the references the closed connection held, and
  an object both connections held stays resolvable through the live one.
- **A release while worker calls survive.** A child kernel's pool is closed, as `0003-25`'s
  release will close it, while two of its `to_thread` workers are blocked in slow calls. Each
  worker is told `unknown`, its wait ended with no result; the binding finishes both calls, their
  replies reach nobody, and each call's outcome event records `returned` when its reply comes;
  the kernel's connections are closed and its object table in the binding is gone, and another
  kernel's calls to the binding run throughout.
- **Reference counting spans the shared table.** One host object reaches kernel A as a proxy on
  each of two connections. Releasing the proxy on the first leaves the object held: a call through
  the second still resolves it. Releasing the second frees it, asserted by a weak reference the
  fixture keeps on the host; and a `del` claiming more references than its connection holds is
  refused, as `0003-16`'s inflated count is.
- `crates/outrig/public-api.txt` is unchanged.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

### Second half: the service measurements

- **A wait of minutes holds one connection.** In one session, with kernel A blocked 3 min in
  `wait_for_answer`, kernel B's `ask`, `poll` and record calls complete throughout, and their
  median and 99th percentile latency are recorded; A, which made the call through a `to_thread`
  worker, answers `inv` meanwhile. Under `serialize = true`, B's calls wait behind A's, and the
  record says for how long.
- **Two sessions are two clients.** With session 1 blocked 3 min in `wait_for_answer` against the
  external service, session 2's `ask`, `poll` and record calls to the same service complete
  throughout, with the same latencies recorded, whether or not either binding is declared
  `serialize = true`: the sessions have separate binding processes and separate locks, so
  neither's lock holds the other's calls, and any wait session 2 sees is the service's.
- **Each operation's request count is recorded**: `ask` and `poll`, one request each; iterating the
  proxied list of 300 records against the by-value copy; `update_records` with 300 changes in one
  call.
- **Memory is recorded**: the binding process's resident size before, during and after the bulk
  operations, with both sessions present.
- **`## Decisions` holds the numbers**, with what they were measured on, and no threshold: the
  numbers are the deliverable.

## Design forks

1. **What a woken call raises -- Recommended: what the same request raises everywhere else,
   `KeyboardInterrupt` for an interrupt and `asyncio.CancelledError` for a cancel.** Each carries a
   message naming the binding and saying the call's outcome on the host is unknown, or that the call
   was never sent when it was woken from the wait for a connection. Agent code that catches
   `Exception` lets either through, so an interrupt still ends the execution, and the model reads it
   as it reads any interrupt. The alternative is an OutRig exception type, which can carry the
   binding and a call id as attributes. Derived from `Exception`, it is caught by a bare
   `except Exception`, and the execution carries on -- the outcome the host's interrupt was sent to
   prevent. A subclass of `KeyboardInterrupt` would have both properties, and can be added later
   without breaking code that catches `KeyboardInterrupt`.
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
3. **How many requests a binding process runs at once -- Settled by the maintainer on 2026-10-02:
   one per connection, each on its own thread, and one at a time only under `serialize = true`.**
   The earlier recommendation was one at a time across all connections, so that no hosted library
   had to be thread-safe; a request made inside a callback of the request in progress had to run,
   since that request waits for the callback and the callback waits for it. Its cost was that one
   kernel's slow call held every other kernel's calls to that binding, and for a service whose
   methods wait on people that is hours, not seconds. RPyC serves connections concurrently on its
   own, so the serialization was this design's choice to make and to drop. Under
   `serialize = true` the same callback exception holds, by a re-entrant lock on the thread serving
   the outer call. Neither mode coordinates the agent's own writes to files the hosted library also
   writes, such as a working-tree file or the index; that is documented, not prevented.
4. **How a kernel has more than one call in flight -- Recommended: the pool of up to four
   connections per kernel and binding, with one object table per kernel in the binding.** An RPyC
   proxy sends its requests on the connection that produced it and names ids from that
   connection's object table, so the pool needs two things RPyC does not do on its own: the
   binding keeps one table per kernel, shared by that kernel's connections, and the container
   side sends a proxy's request on whichever connection its call took. Both are reasoned from
   `_box`, `_unbox` and the `del` handler and not tested (`hosted-objects.md`, "Unverified"). The
   alternative is one connection per kernel and binding: the binding serves each incoming
   request on a worker thread and replies out of order, which RPyC's protocol allows because a
   reply carries its request's sequence number, and the container side keeps its waiters keyed
   by that number. No object table is shared and no request is routed. Its risk is in the
   container, where several threads then wait on one connection: RPyC's issues #354 and #530
   record races in that arrangement -- a reply read by one thread while another waits for it
   costs the waiter a timeout in one report and an indefinite wait in the other -- and
   `bind_threads`, the upstream answer, is marked experimental. A callback request arriving on
   the one connection is also served by whichever thread reads it, not by the thread whose call
   it belongs to. If the shared object table or the request routing fails in the spike, the task
   measures both arrangements against the first half's acceptance items and reports the numbers;
   choosing is the maintainer's.

## Dependencies

- **Hard: 0003-16.** The stream, the relay, the callback proxies and the host's checks that this
  task proves things about.
- **The second half may be deferred.** `/groom-plan` may split the service fixture and its
  measurements into a task of their own, to run later, without blocking `0003-20` or `0003-21`:
  both depend on the first half only. `0003-21` keeps this task as a hard dependency for the
  threading results.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- "Calls are synchronous": the pool, the thread per
  connection, and `serialize = true`.
- `plan/phase/0003-python/embedding.md` -- "Services written in Rust", the client this task's
  fixture stands in for.
- `plan/phase/0003-python/potential/ticket-based-service-waits.md` -- `ask -> ticket` and polling
  in place of a blocking wait.
- `plan/next/rust-object-as-python-object.md` -- the adapter the measurements decide about.
- `plan/phase/0003-python/runtime-protection.md` and `plan/phase/0003-python/agent-placement.md`
  -- the check, the two remedies, and why an interrupt reaches one thread.
- `crates/outrig/src/python/interpreter.py` -- `Kernel.interrupt`, `Kernel.cancel` and
  `_on_sigint`.
- `plan/next/awaitable-hosted-calls.md` and `plan/next/cancel-a-running-hosted-call.md` -- an
  awaitable call without a worker thread, and stopping a call on the host, both deferred.
