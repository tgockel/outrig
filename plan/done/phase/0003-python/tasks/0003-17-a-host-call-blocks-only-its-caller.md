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

## Decisions

Every acceptance item of the first half was met, so the spike did not stop, and fork 4's
alternative was not measured. The forks were taken as recommended, with fork 3 as the maintainer
settled it: a woken call raises `KeyboardInterrupt` for an interrupt and `asyncio.CancelledError`
for a cancel, each naming the binding and saying the call's outcome on the host is unknown, or
that the call was never sent (fork 1); the primary's hosted wait is woken as a child's is, and
SIGINT is sent only when the primary's thread is not waiting (fork 2); one request per connection,
each on its own thread, and one at a time only under `--serialize` (fork 3); a pool of up to four
connections per kernel and binding, with one object table per kernel in the binding (fork 4).
The maintainer chose, when asked: both halves in this task; the minutes-long measurements as
`#[ignore]`d tests run by hand for this record; and the 60-second item run at 60 seconds under
`cargo test`.

- **The pool, as built.** `_Pool` in `interpreter.py`, one per kernel and binding, holds the
  connections and one condition variable, which every channel of the pool waits on and notifies:
  a frame, a freed connection and an interrupt all wake the same wait, which is what lets a
  thread waiting for a free connection learn that an abandoned connection's late reply has
  arrived. A connection has an owner (the thread whose call is in flight), a depth (the nested
  calls a callback served on it makes) and `pending`, the requests sent on it whose reply has not
  been read. Free is no owner and nothing pending; abandoned is no owner and a reply to come.
  `take` gives a thread its own connection when it already holds one (a nested call goes on the
  callback's connection, where the host's thread is waiting), else tidies an abandoned
  connection whose frames have arrived, else a free one, else opens one under four, else waits.
  Nothing dispatches, boxes or writes a frame under the condition variable: a tidy is claimed
  under it and run outside it. A request is pending from before its first byte goes out until
  its reply frame is *read*, before unboxing -- unboxing can wait for a nested `inspect` and be
  woken there, which would otherwise leave the outer reply pending for ever -- and a `del` is
  never pending, since nothing waits for its reply.
- **A proxy stays bound to the connection that produced it and routes through the pool.**
  `Hosted.sync_request`, which `netref.syncreq` calls on the proxy's own connection, is the one
  entry point; it refuses a closed connection's proxy with the close reason and otherwise asks
  the pool for a connection. Releases keep travelling on the producing connection, because the
  binding counts references per connection; the proxy cache stays per connection, so the same
  object on two connections is two proxies, as `hosted-objects.md` says; and the proxy a reply
  creates is bound to the connection the call took. `_box` accepts a proxy of any connection in
  the same pool as `LABEL_LOCAL_REF`, and refuses another binding's or another kernel's as
  before.
- **RPyC's `serve` is replaced.** Its receive lock and event exist so that several threads can
  read one connection; the pool gives a connection to one thread at a time, and a callback's
  nested `serve` runs on that same thread, so the replacement waits on the pool's condition
  variable for a frame, a close, or the exception the reader thread left, and raises that
  exception there, between frames, and nowhere else. `AsyncResult.wait` re-checks its own
  predicate after every `serve`, so the `waiting` argument is accepted and ignored.
- **A tidy drops a late reply unread**, queuing a release for every reference the reply holds so
  the binding's count stays right, and answers a request found there -- a callback the library
  made after its caller was interrupted -- with a `RuntimeError` saying the callback did not
  run, so the library's call of it raises. The limit this leaves: a library that calls back after
  its caller was interrupted waits until the kernel next calls into that pool, because only a
  kernel-side thread may dispatch, never the reader thread. Recorded in `hosted-objects.md`.
- **The wake applies `_on_sigint`'s rule.** Whether an interrupt may end a hosted call is decided
  when the call starts, from the calling thread's own frames, by the walk `_on_sigint` makes from
  a signal's landing frame (`_agent_code_outward`, which both now use): a hosted call the
  interpreter makes for itself -- formatting a traceback whose exception holds a proxy -- is not
  woken. The reader thread re-checks the thread's record under the pool's condition variable
  before setting the exception, so a wake cannot land on a record the thread has just dropped.
  A thread inside a hosted call but not waiting -- running a callback's code, boxing, sending --
  is `busy`: the exception is left for its next wait, and on the primary SIGINT is sent too, for
  a runaway inside a callback. The wake path sets `_landed`, so an interrupt that ends a
  background task's call is reported to that task's owner, and never `_interrupting`, which
  would arm the handler for a signal nobody sends. `Kernel.cancel` wakes the same way and then
  still cancels through the loop, for code that catches the error and carries on.
- **`Hosted._send` joined `_MACHINERY`; `_dispatch` did not.** A signal landing while a request is
  encoded or written is declined, as one landing in `_write_line` is. `_dispatch` stays
  interruptible so that a callback defined in an imported module -- no `<execution>` frame of its
  own -- can still be interrupted by the handler's walk, which then finds the outer call's frame.
- **`Kernel.close_hosted(binding=None, reason=...)`** is the hook `0003-21`'s close and
  `0003-25`'s release call. Under the pool's condition variable it marks the pool closed, closes
  every channel (which wakes every reply wait with `EOFError` saying the outcome is unknown),
  closes idle connections itself and leaves owned ones to their owners -- the `0003-16` rule that
  `_cleanup` runs on the serving thread, since it releases locks only the holder may release --
  and gives every thread waiting for a connection an `EOFError` saying the call was never sent;
  the close notices go out after the lock is released.
- **One table per kernel in the binding, counted per connection.** `Objects`, one per agent,
  holds `{id_pack: [object, total]}` and, per connection, the references that connection handed
  out; `_box`, `_lookup`, `_release` and the `del` handler use it, and RPyC's own per-connection
  table stays empty. A release is checked against the arriving connection's own count under the
  one lock. An object's last reference dies after the lock is released, since destructors are
  library code (GitPython's `Repo.__del__` runs `git`): what a method takes out of the table it
  leaves in a local and deletes after the `with`. A connection's cleanup gives back its own
  references. The table is dropped when the agent's last serving thread has ended -- after its
  cleanup, not when the close notice arrives -- with a diagnostic line on stderr that names the
  agent and how many objects were left, which the release test reads. The `del` handler's target
  is never unboxed: a reference to the object in the handler's frame outlived the release, so
  the object died after the reply rather than before it, and a test that asked another
  connection whether it was alive raced that frame.
- **The serialize lock is a turn, `Turn`**: taken with `with`, granted in arrival order, which
  `threading.RLock` does not promise, and re-entrant for the holding thread, so a callback's
  nested request -- served on the thread holding it -- runs at once. It is entered after
  `_target` and `_unbox`, through an `ExitStack` so that one `try` covers the handler's
  invocation, the boxing of its result and the rendering of what it raised, all library code;
  `ping` and `close` skip it, and `_cleanup` takes it itself, since taking back a connection's
  references runs destructors. `del` takes it for the same reason. A waiter is a record with a
  `dropped` reason for `0003-21` to set when a cancel names a waiting request or the session
  closes; until then a waiter whose connection closed runs its call when its turn comes and ends
  when its reply cannot be sent.
- **`--serialize`** is the binding program's optional fourth argument, which `0003-20` passes from
  the declaration; `main` replaces the module's `_turn`, a `nullcontext` by default, with a
  `Turn`.
- **A live callback read back through another connection is refused.** Found in review: it would
  have crossed as a proxy of the `Callback` object itself, callable from a connection the
  container never passed it to.
- **A method call is two requests, not one.** `root.ask("...")` costs a `getattr`, which brings
  the bound method back as a proxy, a `call` on that proxy, and a `del` of it afterwards.
  RPyC's `BaseNetref.__getattribute__` sends `getattr` for every name outside its local set and
  never consults the methods `class_factory` generated on the proxy's class; those -- the
  one-request `callattr` -- are reached only through Python's special-method lookup on the type,
  so iterating a proxied list is a `callattr` per item and per field read. The acceptance's "one
  request each" was the hypothesis; the test asserts the shape observed, and
  `plan/next/one-request-per-hosted-method-call.md` holds the stub that would make it one.
- **What stood in for the outcome event.** No hosted-call event exists before `0003-18` and
  `0003-21`. The release test reads, instead, the fixture's record that both worker calls ran to
  their end on the host, the relay's count showing that no reply line crossed to the interpreter
  for the closed connections, and the binding's table-dropped line.
- **`answer()` cannot be called inside a serialized session**: the thread waiting in
  `wait_for_answer` holds the binding's lock, so the call that would answer it waits behind it.
  The fixture is released by a file instead, and the two-session measurement answers through the
  service process. This is the ticket pattern's argument in one sentence
  (`potential/ticket-based-service-waits.md`).
- **The test relay** gained per-agent-and-binding traffic counts and the set of connection ids
  seen, which is how a test proves a kernel never held a fifth connection; out-of-order matching
  with messages held back for later reads, since two kernels answer in whichever order they
  finish; `interrupt`, `cancel`, `inv`, `cpu` and `msg` for any kernel; `--serialize`; the
  binding's resident size from `/proc`; and the service process. `Flag` and `eventually` moved
  to `testing.rs`. Where a test's kernel thread blocks right after starting a worker, the worker
  is `loop.run_in_executor`, which is `asyncio.to_thread`'s own mechanism: `to_thread` is a
  coroutine that submits nothing until it is awaited, which a blocked loop never does.
- **Simplified after review.** `/simplify` generated independent versions of the binding's half
  and the interpreter's half and compared each with the original. The binding's half took the
  alternative's shape: one `Objects` class per agent, keyed by connection, in place of a table
  and a per-connection view with RPyC's collection interface; a `Turn` taken with `with` in
  place of a lock with `acquire` and `release`; the module's `_turn`, switched by `main`, in
  place of a lock passed through constructors; and one `try` around an `ExitStack` in place of
  two. Kept from the original: the `try`/`finally` that dequeues a waiter however its wait ends,
  the explicit `del` that makes an object's last reference die outside the lock, and the object
  count in the table-drop line. The interpreter's half kept the original -- it releases the
  references in a dropped late reply, installs the wait record under the condition variable so a
  racing interrupt is not lost, and lets a child inside a callback raise at its next wait, none
  of which the alternative did -- and borrowed three constructs from it: a `ready` property on
  the frame channel in place of a zero-timeout poll under the lock, one `block` method for both
  waits, and RPyC's own callback map as the mark of an outstanding request in place of a set
  kept by `_send`. The reviewer of the interpreter's half ended on a spend limit before its
  verdict, so that comparison was made by hand from both texts.
- **Three defects found by an external review of the landed commit, each fixed with a test:**
  - A dropped late reply left the callables its request had handed out in the connection's
    table: the callback this side registered for a request was also what released them, and a
    tidy popped it unrun. The callables of each request are now kept by sequence number and
    released once the reply has been delivered -- not before it is unboxed, since a method that
    returns the callable it was given sends it back as a reference into that table, and the
    review's second pass caught the first fix breaking that -- or at once when the reply is
    dropped, the host having revoked them before replying. An interrupted call that carried a
    callable no longer holds it until the connection closes.
  - A proxy produced on a connection the kernel's calls stopped taking kept its host object
    alive: its release was queued on that connection, and the pool always took the first free
    one. Among free connections with releases queued, the one taken least recently now goes
    first, so the next calls send them before their own requests, as the single connection did;
    the review's second pass showed that taking the first of them was not enough, since a
    factory's result discarded on every call requeues a release on the connection just taken
    and would have kept the other waiting for ever.
  - On the host, a library whose callback's connection closed under it, and which swallowed the
    error and still returned an object, re-added that object to the kernel's table after the
    connection's cleanup had run; the reply then failed to send with nothing taken back, and
    the closed connection stayed a key of the table with the object under it. A reply that
    cannot be sent now gives back what it would have referenced, and a connection holding
    nothing is no longer a key.
- **The spike's record.**
  - Versions: RPyC 6.0.2 from the pinned wheel; the payload's CPython 3.13.15
    (`python-build-standalone` 20260901, x86_64, `+static`); the fixture library of this task's
    tests and no real library -- no GitPython, no git; no podman, no container; Linux
    7.0.0-38-generic on the development machine, an AMD Ryzen Threadripper PRO 5965WX with 48
    threads and 125 GiB; Rust 1.99.0.
  - Where things sit: every process on the host, started by the test binary in process groups of
    their own -- the interpreter with `-I -c` and the RPyC directory, each binding with `-I -c`,
    the RPyC directory, the fixture's install directory and `--serialize` where a test asks, the
    service process with `-I -c` and a Unix socket under the system's temporary directory --
    with `HOME` under that directory. No mount, no credential.
  - Real: both programs as a session will run them, the pool and the wakes, the shared table and
    the serialize lock, every frame crossing both channels, the interrupts and cancels through
    the protocol, and the service process behind its socket. Stood in for: the Rust relay in
    `host.rs`, by the test relay; the owner's supervision of binding processes; the hosted-call
    events of `0003-21`, as above; and a service written in Rust, by a pure-Python one.
  - Untested: a binding process dying mid-call; aarch64; the container mount; a cancel named at
    a waiting ticket of the serialize lock, which `0003-21` builds; a request held waiting for
    the lock when its channel closes, which runs its call before it ends; a `to_thread` worker's
    call cancelled, which the design says cancels the wait alone -- the worker's own call is not
    woken, which the tests show, but nothing cancels a coroutine awaiting one.
  - Limits reached: four connections per kernel and binding, with a fifth call waiting and the
    relay seeing no fifth id; a late reply after an interrupt, dropped unread; a kernel's pool
    closed under two live calls, both finished by the binding to nobody; a callback made after
    its caller was interrupted, answered only at the kernel's next call into the pool; a call
    blocked 60 s on its kernel's thread while another kernel's call and its own worker's
    returned in milliseconds; and the minutes-long waits below.
- **The measurements**, on the machine above, with the measurement tests run by hand
  (`cargo test -p outrig --lib python::binding_tests::measurements -- --ignored --nocapture
  --test-threads=1`); the numbers are the deliverable, and no threshold is set on them.
  - **Requests per operation**, counted on the container side by handler. `ask`, `poll`,
    `records_by_value(300)` and `update_records(300)`: 2 requests each (`getattr`, `call`) and
    1 release after. Iterating `list_records(300)` and reading two fields of each record: 2
    requests for the call, 903 `callattr` requests (`__iter__`, 301 `__next__`, 600
    `__getitem__`) and 301 releases flushed as it went, 2 after. The whole run of six operations
    was 1237 `rpc` lines toward the binding.
  - **One session, two kernels, kernel A blocked 180 s in `wait_for_answer` through a worker,
    answering `inv` every 30 s meanwhile; kernel B's calls, with a 10 ms pause per round, in
    milliseconds (median / 99th percentile / maximum):**

    | Operation, binding not serialized      | n     | median  | p99     | max     |
    | -------------------------------------- | ----- | ------- | ------- | ------- |
    | `ask`                                  | 4541  | 0.947   | 1.351   | 6.406   |
    | `poll` (no answer yet)                 | 4541  | 0.666   | 0.965   | 5.114   |
    | `poll` (the answer)                    | 4541  | 0.623   | 0.955   | 6.090   |
    | `records_by_value(300)`                | 4541  | 3.025   | 4.004   | 6.001   |
    | `update_records(300)`                  | 4541  | 6.686   | 8.867   | 14.352  |
    | `list_records(300)`, iterated, 1 field | 455   | 176.325 | 235.821 | 396.222 |

    B's first call returned in 1.3 ms. The binding's resident size was 17,444 KiB before, rose
    through 18,416 to 19,184 KiB during, and stayed at 19,184 KiB after a collection in both
    kernels. Under `--serialize`, B's first call -- an `ask` that reached the lock before A's
    call did -- returned in 1.5 ms, and its next waited 180,038 ms, behind A's call, until the
    file released A; the one round that followed had `ask` 1.5, `poll` 1.1, `records_by_value`
    4.1, `update_records` 7.7 and the iterated list 201.6 ms, and the binding's resident size
    went from 17,448 KiB to 17,760 KiB. The measuring window counts from the first call's
    return, so the serialized run's post-release numbers are one round; the wait is the number
    that run exists for.
  - **Two sessions as two clients of one service process, session 1 blocked 180 s in
    `wait_for_answer` against the service, session 2's calls through its own binding to the same
    service, each a round trip over the service's Unix socket beyond the RPyC request, in
    milliseconds (median / 99th percentile / maximum):**

    | Operation                              | both plain, n=4103      | both serialized, n=3990 |
    | -------------------------------------- | ----------------------- | ----------------------- |
    | `ask`                                  | 1.209 / 2.808 / 8.612   | 1.238 / 3.123 / 8.560   |
    | `poll` (no answer yet)                 | 0.836 / 1.750 / 7.088   | 0.850 / 1.970 / 14.352  |
    | `poll` (the answer)                    | 0.799 / 1.602 / 7.923   | 0.815 / 1.965 / 8.722   |
    | `records_by_value(300)`                | 3.482 / 5.242 / 10.110  | 3.498 / 5.582 / 11.469  |
    | `update_records(300)`                  | 7.345 / 11.889 / 21.833 | 7.330 / 12.312 / 15.359 |
    | `list_records(300)`, iterated, 1 field | 192.0 / 430.8 / 515.1   | 195.6 / 463.0 / 488.3   |

    Session 2's first call returned in 1.3 ms in both runs: the sessions have separate binding
    processes and separate locks, so session 1's lock -- held for the whole wait under
    `--serialize` -- held none of session 2's calls, and the service answered session 1's wait
    when session 2 called `answer` at the end. Resident sizes, in KiB, (session 1, session 2):
    plain 20,508 and 20,512 before, 20,508 and 21,340 to 21,368 during, 20,508 and 21,352 after;
    serialized 20,512 and 20,508 before, 20,512 and 21,324 to 21,352 during, 20,512 and 21,340
    after. The service round trip costs about 0.3 ms per call over the in-process fixture.
  - **What the numbers say about `plan/next/rust-object-as-python-object.md`**, for the
    maintainer to decide: a hosted Python client costs a service about a millisecond per call
    and 3 to 7 ms per 300 records by value, grows its process by about 2 MiB under three
    minutes of continuous calls, and lets a wait of minutes hold one connection and one thread
    and nothing else. What costs are a proxied collection -- about 0.6 ms per item per field --
    and `--serialize`, under which a wait of minutes holds every call of that binding from every
    kernel of its session, which is the ticket pattern's argument
    (`potential/ticket-based-service-waits.md`).
