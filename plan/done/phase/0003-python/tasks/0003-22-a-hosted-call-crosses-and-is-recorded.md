# 0003-22 -- A hosted call crosses the boundary and is recorded

## Context

After `0003-20` a session can declare, approve, install, start and stop a binding, and the agent is
told that it exists, but agent code cannot reach it: `0003-16` and `0003-17` proved the transport
and its threading through a relay that exists only in tests. This task puts the relay in `host.rs`,
binds `0003-21`'s facade under each binding's name in every kernel, and records every request.

Today `host.rs` speaks to the interpreter alone. Its reader task parses each line into
`enum Reply` and hands it to whoever waits for it, what the host sends is built ad hoc with
`json!`, and a line for any agent other than the primary is logged and dropped. A hosted call adds
traffic the host passes on without reading: RPyC frames between a kernel and a binding process. The
checks are the binding's (`0003-16`), so the relay carries bytes. What it must not do is let one
binding delay anything else. A binding process that stops reading must not stop the user channel,
the inventory probe, an execution's result, or another binding's frames.

`boundary-policy.md` makes the default with no policy configured allow, published as events:
every request runs and is recorded. `observability.md` puts those records in the integration-audit
category, which `0003-13` defined without a producer, and `0003-23` adds its decisions to the same
events. What counts as one
request is `0003-16`'s inventory: an attribute read, a call, or one step of an iteration is one
boundary request, recorded as events that share its id -- its receipt, its dispatch and its
outcome, with `0003-23`'s decisions between the first two -- while reference counting is
bookkeeping, recorded only when the interception refuses it. `lifecycle.md` has the rows this task
makes true: a call the relay had not forwarded at the close never reaches its binding, a call it
had forwarded is the binding's to finish and drains, and one still running at the deadline is
killed with the binding's group -- `unknown`, never `failed`, because a call cut off may already
have acted on the host.

## Goal

A binding's name, in any kernel, is a proxy for the host's object; every request through it is
checked on the host, refused in Rust once admission has closed and in its binding once the close
has reached it, and recorded before its target is invoked; and a call still running at shutdown is
drained, then killed and reported.

## Deliverables

- **The relay in `host.rs`**: an `rpc` kind in `enum Reply` and in what the host sends, tagged by
  agent and binding, carried between the interpreter and `0003-18`'s supervisor for every agent the
  host has opened -- today the primary, and later `0003-26`'s children through the same path. Each
  binding has a bounded queue, so a binding that stops reading delays only the calls waiting on it
  (fork 1).
- **Stubs**: `0003-21`'s facade, bound under each binding's name in every kernel, the primary
  and every kernel opened later, before
  `_boot` is taken, so the inventory lists no stub. The first `await` opens the connection and
  fetches the root, on the worker thread. `async for` makes one request per item, `__iter__` then
  one `__next__` each, as RPyC's proxies do, rather than batches through `buffiter`: one request
  then stands for one item, and a batch size chosen by the request is
  `plan/next/hosted-reference-and-payload-bounds.md`'s concern. The runtime keeps its own
  reference to each kernel's stubs, apart from the global names the agent sees, so `repo = None`
  or `del repo` changes nothing the runtime uses; `0003-29`'s injection reads the stub from there.
- **From tests to sessions**: `0003-16`'s interception and argument rules and `0003-17`'s
  threading, callbacks, interrupts, connection pool and `serialize = true`, run by every session
  with a binding rather than by tests alone; and the vendored RPyC mounted read-only into the
  container beside the payload, by the route `0003-20`'s fork 5 settles, and imported by the
  interpreter as `0003-16`'s fork 5 settles.
- **A request's events**, in the integration-audit category (fork 4): its receipt, published
  before any decision about it; its dispatch, published before its target is invoked; and its
  outcome, all carrying the request's id. A request whose target is never invoked has no
  dispatch event, and its outcome says why. Published means in the in-memory stream, in
  sequence, before the invocation, and not that any subscriber has consumed it: a subscriber that
  falls behind loses events in a counted gap (`observability.md`), one that had not taken a
  published receipt when the owner died has lost it, and a file a subscriber writes is
  best-effort. A reader infers non-invocation from a `refused` or `cancelled` outcome, or from a
  history known to be complete, and never from a dispatch event absent across a gap or from an
  incomplete file (fork 4). Together
  they name the binding, the agent and its execution, the request's id, the operation, the member,
  the qualified name of the host-side type of the target, bounded previews of arguments, when the
  request was received and whether and when its target was invoked, the outcome, the duration, and
  the parent call's id for a request made inside a callback. A callback's own invocation is an
  event, its parent the call it was passed to.
  - **The outcome is one of five.** `returned`; `raised`, with the exception's type; `refused`,
    with the reason -- here the interception or admission closed, and from `0003-23` and
    `0003-24` a rule, the approver or the evaluator; `cancelled`, never invoked, with the
    reason interrupted or the close; and `unknown`, forwarded with no reply, because its binding
    was killed at shutdown or its process died (fork 3). "Interrupted" is a reason attached to
    `cancelled`, or a note on a later `returned` or `raised`; it is not an outcome of its own.
  - **An interrupt ends the caller's wait; a forwarded call is the binding's to finish.** The
    interrupt raises where the call is awaited, by waking the worker's wait, and the relay sends
    the binding a cancel line naming the request. A request the binding has not started --
    waiting for the serialize lock in a binding declared `serialize = true` (`0003-17`) -- is
    dropped and recorded `cancelled`, as is one `0003-23` holds for a decision. One already
    invoked runs on, since RPyC has no request that stops a call; its reply, when it comes,
    records `returned` or `raised` with the note that the caller had been interrupted, and
    `unknown` is recorded only if no reply comes before the binding is killed or dies. A call
    never sent -- woken by an interrupt, or by the close, while it waited for a free connection
    of its kernel's pool (`0003-17`) -- is recorded `cancelled` from the interpreter's notice of
    it, since its binding never received it.
  - **A request the interception refuses is recorded whatever its kind**, bookkeeping included:
    a receipt and a `refused` outcome naming the check. Bookkeeping that passes the checks is not
    recorded, and an `inspect` answer from which private names were only filtered is not a
    refusal. The caller's error names the refusal and is not a class the hosted library defines,
    so `except` on the library's own classes does not catch it; a refused attribute read raises
    one that is also an `AttributeError`, so `except AttributeError` at the `await` catches it.
    `hasattr` and `getattr` with a default cannot see it: a proxy's lookup builds a path and
    raises nothing until awaited (`0003-21`).
  - The operations form a fixed vocabulary, taken from the inventory, which `0003-23`'s rules match.
  - A preview is built from by-value data only. An object is named by its type and an opaque
    reference id, never by its `repr`, which would run code.
  - A raised call's event carries the host's traceback text, bounded, which the agent's error does
    not.
- **Admission closes at the relay.** `close_admission()` closes `0003-19`'s gate in Rust and
  stops the relay forwarding to every binding in one state change, and returns at once (fork 2).
  The relay is the point that orders requests against the close. A frame it had not forwarded --
  one in its queue at the close, or one sent later -- is refused in Rust and never reaches the
  binding: its caller gets `0003-19`'s closing error, and it is recorded `refused`, admission
  closed, from the interpreter's notice of it, as a call never sent is. A request the relay
  forwarded before the close belongs to the drain, whatever the binding does with it: the drain
  waits for it, and its outcome is the one the binding reports. The relay then enqueues a control
  line to each binding, behind the frames it had forwarded and without waiting for the pipe -- a
  stopped binding with a full pipe does not delay the close -- and the binding sets its closed
  flag when it reads the line. So the flag is installed after the close has returned, and a
  forwarded request's outcome depends on where it was when the binding observed the flag:
  `returned` or `raised` when the binding ran it, which it may do after the close returned;
  `refused`, admission closed, with its target not invoked, when the binding read it after its
  flag; `cancelled`, with the close as the reason, when the binding had read it but not started
  it when it observed the flag -- one waiting for the serialize lock in a binding declared
  `serialize = true` (`0003-17`), which until then may take the lock and run; and `unknown` when
  the binding's group is killed first. The flag decides the requests a binding's own rules allow,
  which until `0003-23` adds rules that hold a request for Rust's gate is every request. An allow
  Rust's gate issued before the close is still honored when it reaches the binding after the flag
  (`0003-23`).
- **A call waiting for a free connection at the close** has not been sent (`0003-17`). The close
  reaches the interpreter too, whose reader thread wakes every such call; each raises the closing
  error and is `cancelled`, with the close as the reason.
- **Shutdown.** Calls already sent to their bindings drain until the deadline; then each binding's
  group is killed, and a call still running is `unknown`, its binding killed at shutdown, in
  `0003-19`'s `ShutdownReport` and in its event. A callback that a draining call makes runs, as
  `lifecycle.md`'s row has it; if this task's callback tests show a reason to refuse it after the
  close, the row changes with them. A callback still running at terminating is a coroutine
  cancelled with its kernel or a plain function ended by the container stop, and the call that
  made it ends with its binding.
- **`lifecycle.md`'s rows for hosted calls and callbacks**, true of the implementation and tested.

## Acceptance

- **Every request is recorded, receipt first, through `run-new`** with a mock model and a binding
  of `0003-16`'s fixture library: `await obj.a.b` makes two requests, each with a receipt, a
  dispatch and an outcome event sharing its id; `async for` over a hosted sequence makes one
  request per item the iteration takes, as the inventory classifies them; a method call makes two,
  `getattr` and `call`, until `plan/next/one-request-per-hosted-method-call.md`; and a call that
  raises the fixture's exception has an outcome that names the type, while the agent's code catches
  it by its own class. A request's receipt and its dispatch are published before its target runs,
  shown by the dispatch event's publication time against the start time the fixture method
  records, both on the host's clock; and a request refused at admission has a receipt and a
  `refused` outcome and no dispatch event.
- **A callback is recorded under its call.** A fixture method calls a coroutine callback that
  awaits a hosted call of its own: the callback's invocation event and the nested call's events
  carry the outer call's id as their parent.
- **The hand-written-client suite passes in production.** `0003-16`'s tests of a client written by
  hand pass through the relay in `host.rs`.
- **A refusal by the interception is recorded.** A hand-written client's handler id outside the
  table, `del` with an inflated count, and `inspect` of an object its connection was never handed
  each leave a receipt and a `refused` outcome naming the check, and no dispatch event. Agent
  code that awaits a private attribute gets an error of OutRig's that is also an `AttributeError`,
  which `except` on the fixture's exception classes does not catch, and the read is recorded the
  same way.
- **An interrupt ends the wait, not the binding.** An interrupt during a slow call raises where the
  agent's code awaits it; the call's outcome is recorded `returned` when the binding's reply
  arrives, with the note that the caller was interrupted; and the next call on that binding
  returns its own result. The same call with the binding killed before it replies is `unknown`.
  With the fixture declared `serialize = true`, a call that kernel B made, still waiting for the
  serialize lock behind kernel A's slow call when B is interrupted, is `cancelled` with the reason
  interrupted and is never invoked; so is a call interrupted while it waits for a free connection,
  which never reaches the binding.
- **Shutdown kills what it must.** Shutdown during a slow call reports it `unknown`, its binding
  killed at shutdown, in the report and in its outcome event, and no process remains in the
  binding's group.
- **A draining call's callback runs.** The session closes while a fixture method sleeps before
  calling a callback: the callback runs during the drain, and the outer call ends `returned`.
- **A callback that never returns leaves shutdown bounded.** A coroutine callback that catches
  every `CancelledError` in a loop, and a plain one that loops on its worker thread, each made by a
  call that is draining at the close: `shutdown` returns within `0003-19`'s stated bound after its
  deadline, each outer call is `unknown`, and no process remains in the binding's group.
- **A stopped binding stops only its callers.** With a binding stopped by `SIGSTOP`, plain Python,
  the user channel and a second binding all work as usual.
- **A binding process that dies mid-session gives fork 3's result.** With the recommendation, a
  binding killed with `SIGKILL` during a slow call leaves that call `unknown`, its binding process
  gone; a later use raises an error naming the binding and how its process ended; the stream and
  the shutdown report record the exit; and plain Python, the user channel and a second binding
  work as usual.
- A stub is bound in a kernel opened after start, and the inventory lists no stub.
- **A request the owner still holds at the close invokes nothing.** A request after
  `close_admission()` is refused with the documented error and recorded `refused`, admission
  closed, and a fixture method that writes a file when called leaves no file. A fifth awaited call
  on one kernel, waiting for a free connection behind four in flight at the close, raises the
  closing error at once, never reaches the binding, and is recorded `cancelled`.
- **A serialize-lock waiter is cancelled when its binding observes the flag.** With the fixture
  declared `serialize = true` and the binding reading, a call from kernel B waiting for the
  serialize lock behind kernel A's 10 s call at the close is `cancelled`, with the close as the
  reason, and never invoked, once the binding reads its control line, while A's call drains to
  `returned`; the record shows the line was read before A's call released the lock. The
  cancellation is relative to the binding observing its flag and not to `close_admission()`
  returning: a waiter the lock reaches before the binding reads the line runs, and drains.
- **A forwarded request is the binding's to finish.** With the fixture binding paused by
  `SIGSTOP` and several requests from one kernel sent to it, enough that the binding's pipe is
  full, `close_admission()` returns at once, and the requests fall into two sets: those the relay
  had forwarded, and those it refuses in Rust, which raise the closing error at once and are
  recorded `refused`, admission closed. On `SIGCONT` every forwarded request ends with the
  outcome the binding gives it -- `returned`, or `refused` with admission closed where the
  binding read it after its flag -- with a receipt the binding published, and the drain waited
  for it. The binding's count of requests received equals the number the relay forwarded, and no
  request is recorded as never sent.
- **A forwarded request may start after the close returns.** With the fixture binding paused by
  `SIGSTOP`, one request A, a method that writes a file, is forwarded, which the relay's count
  confirms; `close_admission()` returns at once; then `SIGCONT`. The binding reads A ahead of the
  control line and runs it: A has a dispatch event and ends `returned`, the file exists, and the
  drain waited for it. A is never recorded `cancelled` or `refused`.
- **A stalled subscriber loses the dispatch event and still sees `returned`.** A subscriber that
  takes nothing while a hosted call runs and more events are published than the stream holds
  resumes to a counted gap that contains the call's dispatch event, and then to its `returned`
  outcome; the fixture method's file exists. A reader of that subscription classifies the call
  as invoked, from the outcome, and does not conclude non-invocation from the missing dispatch
  event: non-invocation is read only from a `refused` or `cancelled` outcome, or from a history
  with no gap.
- **Recording runs no code.** A host-side fixture object whose `__repr__` and `__str__` each write
  a file, returned by one hosted method and passed to another, leaves no file, and the events name
  it by its type and reference id.
- e2e, with network: **the example library works end to end.** Through `run-new`, with a mock model
  and a GitPython 3.2.0 binding over a test repository, agent code commits a change and pushes it to
  a local bare repository over `file://`. The bare repository then holds the commit, and the stream
  holds every request the commit and the push made. This task's `## Decisions` lists each place
  found where a proxy behaves unlike the local object -- identity, `isinstance`, `repr`, what
  iteration costs, truthiness, `len`, plain `for`, assignment, exceptions -- with what the agent
  sees in each.
- `crates/outrig/public-api.txt` is unchanged, or regenerated if the event types `0003-19` made
  public gain fields.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What a full binding queue does -- Recommended: refuse the frame that does not fit, raise in the
   calling kernel with an error naming the binding, and close that kernel's connection to it.**
   Waiting would hold the reader that every other message arrives on. A frame cut part way through
   cannot be resumed, so the connection closes, and its stub opens a new one at its next use. A call
   whose frame never reached the binding was not invoked, and the error says so.
2. **Where the close is ordered against requests -- Recommended: at the relay in Rust, with a
   control line to each binding behind it.** `close_admission()` stops the relay forwarding in
   the same state change that closes `0003-19`'s gate, and returns. A frame still in a binding's
   queue, or arriving later, is refused in Rust -- the kernel gets the closing error, as it does
   for a frame the queue cannot hold (fork 1) -- and never reaches the binding. That is the line
   between never sent and sent, and the owner draws it without the binding's help. A request the
   relay forwarded before the close belongs to the drain, whatever the binding then does with it:
   the drain waits for it, and it ends with the outcome the binding reports. The relay then
   enqueues the control line, without waiting for the pipe -- a binding stopped with a full pipe
   takes it when it reads again, long after the close returned -- and the binding sets its closed
   flag when it reads the line: a request it reads after the line is refused, admission closed,
   and one it had read but not started -- waiting for the serialize lock behind another
   connection's request (`0003-17`) -- is cancelled. So the flag is installed after the close, a
   forwarded request ahead of the line may start after the close has returned, and the
   cancellation of a lock waiter is relative to the binding observing the flag. The line can only
   make a refusal faster than running the call would be. It cannot overtake the bytes already in
   the pipe ahead of it, and writing it proves nothing about when the binding reads it, so nothing
   in the owner's accounting depends on it. It costs nothing per request. The flag decides only
   what the binding's own rules allow (`0003-23`'s fork 1): a request held for a decision is
   Rust's gate's, and an allow the gate issued before the close is honored even when it reaches
   the binding after the line. The alternative, asking the owner before each invocation over
   `0003-18`'s decision line, adds a round trip to every attribute read, and `0003-23`'s fork 1,
   which keeps rules in the binding so that an allow costs no round trip, assumes it is not taken.
3. **A binding process that exits mid-session -- Recommended: the session goes on without it.**
   `hosted-objects.md` leaves this to `0003-20` and this task. The binding's calls in flight are
   `unknown`, with its process gone as the reason, its later uses raise an error naming the binding
   and how its process ended, and the event stream and the report record the exit. The alternative,
   ending the session as the interpreter's death does, is simpler, and stops an agent that may have
   no further use for that binding.
4. **When a request's events are published -- Recommended: the receipt before any decision, then
   the dispatch before the invocation, then the outcome, as events of their own sharing the
   request's id.** A receipt published before the target runs is in the stream before the owner
   can die mid-call, so a subscriber that had consumed it knows the call was made, which an event
   written only when the request ends cannot give it; a long call is visible while it runs; and
   it is how `boundary-policy.md` and `0003-23` already describe the record, with the decision as
   one more event between receipt and dispatch. Published means in the in-memory stream
   (`observability.md`), in sequence, before the invocation; it does not run any subscriber. A
   subscriber that falls behind loses events in a counted gap, and can hold a receipt, lose the
   dispatch in the gap, and then take the `returned`; one that had not taken a published receipt
   when the owner died has lost it with the owner; and a receipt is on disk only once the
   subscriber that writes `events.jsonl` has written it, so a session that dies mid-call can
   leave a file holding the receipt of a call whose dispatch the stream had published and the
   file had not reached. The order of the stream is the evidence for a reader whose history is
   complete. A dispatch event absent across a gap, or from an incomplete file, is not proof that
   the target was never invoked; non-invocation is read from a `refused` or `cancelled` outcome,
   or from a history with no gap. The cost is volume: three events per request, so `obj.a.b`
   produces six. The alternative -- one event when the request ends, with its receipt and
   invocation times inside it -- is a third of the volume and loses all three properties.

## Dependencies

- **Hard: 0003-16, 0003-17.** The interception, argument rules, threading and interrupts this task
  puts into sessions.
- **Hard: 0003-20.** Bindings that start, stop and are described; and through it, `0003-19`'s
  admission and report, and `0003-19`'s stream.
- **Hard: 0003-21.** The facade this task binds in every kernel.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- the transport, presentation, and what crosses.
- `plan/phase/0003-python/boundary-policy.md` -- what a boundary request is, and the default of
  allow, published as events.
- `plan/phase/0003-python/observability.md` and `plan/phase/0003-python/lifecycle.md` -- the
  integration-audit category, and the rows this task makes true.
- `plan/todo/0003-21-a-hosted-object-is-awaited.md` -- the facade task: the proxy this task
  binds, what an `await` resolves, and what raises instead of crossing.
- `crates/outrig/src/python/host.rs` -- `enum Reply` and `dispatch`, which the relay extends.
- `plan/next/event-audience-projections.md`, `plan/next/mandatory-audit-sink.md` and
  `plan/next/cancel-a-running-hosted-call.md` -- deferred: fields per audience, a subscriber whose
  failure closes admission, and stopping a call on the host.
- `plan/next/hosted-calls-without-a-thread-per-call.md` -- deferred: a serving thread per
  connection in place of a worker per in-flight call.

## Decisions

- **One process holds every binding of a session.** The maintainer's call during planning,
  replacing `0003-18` and `0003-20`'s process per binding: one process is simpler to start, relay
  to, account for and stop. The accepted losses, to be fixed in pure Python if they ever matter:
  one environment for every binding (`BindingSpec::env` gave way to
  `SessionBuilder::bindings_env`), one version of each package -- the first binding's, as in the
  container -- and one failure ending every binding. It landed first, as a commit of its own,
  without renumbering anything. `/simplify` judged an independent rewrite of it simpler, and its
  pieces were taken -- a serializable `Factory` sent as the start line, one `who` for a start's
  errors, the ready line's types kept as JSON, a per-binding `lower`, no `Programs` or `distinct`,
  one start path in the test relay -- while the first version's names (`Process`, `stop_process`)
  were kept.
- **Fork 1, as recommended, by a whole-frame refusal.** The interpreter labels each frame's first
  part with its size; the relay admits or refuses the whole frame at that part, gathers its parts,
  and queues it whole once the last arrives, so the close's control line never lands inside a
  forwarded frame. A request that does not fit is refused whole and its connection stays open;
  only a reply that does not fit -- its call waits in the process, and cannot be refused -- or a
  frame whose parts outrun the size it declared closes its connection, on both sides. The queue
  holds 64 MiB of frame data, and one frame always fits an empty one; an unlabeled frame, which a
  raw client can send, is passed on a part at a time against the same bound. The supervisor's
  stdin queue holds one line, so what waits for a slow process waits in the relay's queue, where
  it can be refused.
- **Fork 2, as recommended, with three refinements.** Bookkeeping and replies to callbacks pass the
  close, at the relay and at the binding's flag alike: a call the binding was forwarded may need a
  proxy's methods listed to finish, and a reference released. The close also reaches the
  interpreter, which refuses a call made after it before sending it, recorded `by: interpreter`. And
  a request the relay refuses is recorded from the interpreter's labels on its frame -- its
  operation, member, target and execution -- rather than from a notice of the interpreter's, since
  the relay is what refused it. A forwarded request is always ahead of the control line, so the
  binding reads it before its flag and runs it; the flag decides only a frame forwarded after the
  close, which the relay forwards only when its label claims bookkeeping or it has none, and refuses
  one whose request is a boundary request, admission closed. Each connection's channel counts the
  frames put before the control line, and a request is judged where it was received, under RPyC's
  receive lock: a library thread calling a callback can receive the connection's next frame.
- **Fork 3, as recommended.** The relay sees the process's output close, reads how it ended without
  reaping it, records each call it had sent `unknown` (`exited`) and refuses each it still held,
  publishes `binding.exited`, and tells the interpreter, which closes every kernel's connections to
  the bindings; a later call raises an error naming the bindings and how their process ended.
  `BindingStop::exited` carries the cause into the report. With one process, the criteria's "a
  second binding works as usual" no longer holds: a second binding shares the process, so it stops
  with a `SIGSTOP` and dies with a `SIGKILL`. The tests show plain Python and the user channel going
  on.
- **Fork 4, as recommended.** The bindings process holds a request after writing its dispatch
  event until the relay answers that the event is published, so the dispatch is in the stream
  before the target runs. Each request is `RequestId`, counted from 1 per session.
- **What a refused or never-sent call raises.** A call the close kept from running raises
  `outrig.Closed`, RPyC's generic exception class for that name, so the interpreter's own refusal
  and the binding's arrive as one class; a call the queue had no room for, or that the dead
  process never got, raises `EOFError` saying why.
- **Previews are of arguments only.** A result is not previewed: the outcome names how a call
  ended, and a result's contents are the agent's business, not the audit's. A preview is built
  from what crossed, by value, cut to 200 characters, 16 items of a container and 3 levels deep;
  an object of the host's is `<type #reference>`. A raised call's traceback keeps its last 16 KiB.
- **Bookkeeping a handler refuses is recorded too**, not only what the checks before dispatch
  refuse: a `del` with an inflated count and an `inspect` of an object never handed out are
  refused inside their handlers.
- **The hand-written-client suite runs through the production relay.** The binding tests' harness
  hands the interpreter's `rpc` lines and notices to `python::hosted::Relay`, as `host.rs`'s
  reader does, and writes what the relay sends; it stands in for that reader alone. The fixture's
  source moved to `outrig_fixture.py`, which the session tests install as a wheel and the CLI's
  e2e builds one from.
- **Shutdown** drains hosted calls until the same deadline as executions, then marks the relay
  stopping, so the process's exit is expected and a call it cut off is `unknown` (`killed`), and
  waits up to five seconds after the stop for the relay to handle the process's last lines.
- **A coroutine callback's call ends `raised` at terminating, not `unknown`,** departing from the
  criterion's "each outer call is `unknown`". Shutdown's interrupt wakes the thread serving the
  coroutine, and `0003-17`'s serving code answers the outer call with it at once, whatever the
  coroutine does after; its call then ends `raised`. Whether that reached the record before the
  bindings process was killed was a race, which load decided either way, so the terminate step's
  one-second grace now covers hosted calls as well as executions: an answered call is recorded
  before the stop. A plain callback looping on its worker cannot be answered, and its call ends
  `unknown`, its binding killed. Shutdown takes up to a second longer when a hosted call never
  ends.
- **`/simplify` replaced all three of this task's own pieces.** Independent rewrites of the relay,
  the bindings process's side and the interpreter's side were each judged simpler, and each
  review found defects in the first versions that the rewrite did not have: the relay queued a
  frame's parts as they came, so the control line could land between the parts of a forwarded
  request, which the process then refused; it missed a wakeup of the drain, leaked queue room when
  a connection closed mid-frame, and dropped queued calls with no record when the process died.
  The bindings process read only one of the two forms RPyC sends a tuple in, so plain arguments
  and every keyword went unpreviewed; a library thread's callback lost its parent; and an outcome
  with a lone surrogate in its text was never written. The interpreter could notice one call twice,
  cancel requests on a connection another call had taken, and raise from a by-value `async for`
  after the close. All three rewrites were taken, with the reviews' fixes for their own gaps: the
  relay counts a request's duration from its receipt, bounds unlabeled frames, checks the binding's
  name, and looks for an exit for a bounded time; the bindings process keeps a cancel that arrives
  before its request is registered, drops a request a cancel reaches before its target runs,
  records bookkeeping only when a check refuses it, and writes the outcome before the reply; the
  interpreter writes its notices once it has let go of the pool's lock, notices a call whose root
  was still to be fetched, notices a call agent code cancelled, and names a comparison's operator.
  A preview cut at 16 items ends in `...`, as `reprlib` writes it.
- **`run-new` says "binding call"** where the stream says hosted: the stop's verdict line, and a
  line per call that ended after the close, `call 4 to binding repo: unknown (killed)`.
- **The round's closing line** does not name hosted calls in flight; whether it should is
  `plan/next/a-round-names-the-hosted-calls-it-left-running.md`.
- **Where a GitPython 3.2.0 proxy behaves unlike the local object**, seen through `run-new` in an
  Alpine container with no `git` program:
  - *Identity.* Two awaits of `repo.head.commit` gave two objects, as they do locally: GitPython
    builds a new `Commit` on each access. One host object on one connection is one hosted object
    (`0003-21`).
  - *`isinstance` and type.* `type(c)` and `c.__class__` are `outrig.HostedObject`. GitPython's
    package does not import in a container without `git` -- it looks for the program as it is
    imported -- so the host type's class is not there for `isinstance(c, git.Commit)`, which
    needs `git` in the image or `GIT_PYTHON_REFRESH=quiet` set before `import git`.
  - *`repr` and `str`* answer in the container without a request:
    `<hosted git.objects.commit.Commit from repo>`. The host's is `await c.__repr__()`.
  - *What iteration costs.* `async for` over `c.parents` or `repo.iter_commits()` is a `callattr`
    of `__iter__` and one of `__next__` per item, plus the one that raises `StopIteration`.
  - *Truthiness, `len`, `in`, `==` and plain `for`* raise `TypeError` naming the awaited
    spelling: `await x.__bool__()`, `await x.__len__()`, `await x.__contains__(item)`,
    `await x.__eq__(other)`, `async for`. `await c.parents.__len__()` gave 1.
  - *Assignment.* `c.message = 'x'` raises `TypeError`; `await c._set('message', 'x')` sets it on
    the host.
  - *A missing attribute.* `hasattr` is always true; the `AttributeError` raises where the
    attribute is awaited. A private one raises `outrig.Refused`, an `AttributeError` too.
  - *`hash` and `dir`* answer in the container: `dir(c)` lists the facade's names, not `hexsha`.
  - *Exceptions.* A failed `git` command raises RPyC's generic class for
    `git.exc.GitCommandError` where `git` does not import; `except Exception` catches it, and
    `str(e)` reads `<Unprintable exception>`, then the host's last traceback line, since
    GitPython's `__str__` reads attributes the generic class does not carry.
  - *Values* -- a commit's `hexsha`, a config value -- come back by value.
- **A review after the rebase onto `e324d69` found five defects, fixed on this branch:**
  - *The library did not compile.* The relay's line to the interpreter called `upgrade()` on
    `host::Lines`, which the rebase made an enum whose weak form upgrades inside `send`.
  - *The relay's bound counted what labels claimed.* An empty queue admitted a frame of any
    declared size, and a frame's parts were kept until its last, so a label declaring a terabyte,
    or parts with no data, grew the host's memory without limit. The queue now counts what each
    line it queues holds, the relay's own included -- its data as text, and `LINE`, 1 KiB, for the
    rest -- and a frame reserves what the parts its declared size takes hold, split as both
    programs split one; a label past 128 MiB, or parts that outrun the reservation in bytes or in
    number, close the connection. The rest of what the interpreter's lines put in the queue was
    uncounted too: a cancel now names at most 16 requests, reaches only a connection the process
    was sent a part on, and is dropped when there is no room, its requests running as if their
    callers had waited; a close reaches the process once, and only for such a connection; and an
    agent holds at most 64 connections to a binding, past which a part is answered with a close
    and leaves nothing behind, in the relay or the process. A frame larger than the bound crosses
    only when nothing else is queued, as before, and a line of the relay's own now counts as
    something. That cap settles `0003-16`'s `binding-connections-are-uncounted.md`,
    with the process remembering the last 1,024 connections that closed rather than every one,
    and the entry is gone from `plan/next/`.
  - *A request numbered outside 0 to 2^64 - 1 held a serialized binding's turn for ever.* Its
    records carried a number the relay could not read, so its dispatch was never answered. The
    process now closes the connection before recording or waiting, and so for a request numbered
    as one its connection is still serving, which would share its record. Request numbers are not
    in arrival order on a connection -- a hosted argument's resolution sends its own requests
    before the one it is an argument of -- so the check is against the requests in flight. The
    relay answers a dispatch it could not record, unreadable or of a request it never saw
    received, with `"answer": false`, and the process refuses the request rather than run it
    unrecorded. A labeled request under the number of one in flight on its connection closes the
    connection in the relay too, where its record would have replaced the first's.
  - *A cancel naming no registered request was kept for ever*, in one set for the whole process.
    It is now its connection's channel's, kept only while a frame that arrived on the channel is
    not yet claimed -- registered as a request, or found to hold none -- at most 16, past which the
    connection closes; a cancel of a request that ended, or of none, keeps nothing. A frame with no
    data, which RPyC takes without dispatching, would never be claimed, so both programs' frame
    channels now refuse one.
  - *A stop whose output stayed open left calls unresolved.* A child the library forked that
    called `setsid` holds the process's output past the group's kill, so the relay never saw it
    close: the report left the calls out, and could say `Clean`, and their outcomes could follow
    it. After the five-second wait the relay now takes the process as gone (`Relay::abandon`):
    each call it was sent and never answered is `unknown`, `killed`, and what the process writes
    later is ignored. The report's hosted calls are read last, once the container has stopped, by
    `Relay::report`, after which the relay publishes nothing.
- **What the host queues for the interpreter is not bounded yet**: the bindings process's replies
  and the relay's answers wait in the interpreter's writer queue, which an interpreter that stops
  reading can grow. Backpressure there can deadlock the two pipes, so it is
  `plan/next/bound-what-the-host-queues-for-the-interpreter.md`.
- **`/simplify` on the review's fixes** had an independent version written against the same tests,
  and the review of the two found each simpler in places. Its bindings-process side and shutdown
  were taken: the cancels kept on each connection's channel, closing a connection from where the
  request was read rather than through a new exception class, and `abandon` called after every
  wait for the output, since a second `gone` does nothing. This relay was kept, since the other let
  each connection past the 64 leave a frame behind, without limit; from the other it took counting
  the relay's own lines, a `gone` that is safe to repeat, and checking for the report at the
  relay's two entries rather than at each publication. The review found two defects common to
  both, fixed here: a frame with no data threw off the count of unclaimed frames, and a labeled
  request under a number in flight took the first's record.
- **A second review found requests the relay held for ever under a label**, fixed on this branch.
  The relay held a request under the number the interpreter's label gave it, and the process
  recorded what it read under the frame's own number, so a frame the process read as something
  else -- a request under another number or one past 64 bits, a reply, a bookkeeping request -- or
  dropped unread when its connection closed left the relay's request held: memory past the queue's
  bound and the connection cap, a drain held until its deadline, and an `unknown` at the end. The
  relay now sends the process the number with the frame's first part. The process closes the
  connection on a frame that is not that request and ends the request `refused` by the
  interception; it ends one whose frame it drops unread `cancelled`, `disconnected`, a new
  `HostedReason` -- held or in progress when the connection closes, arriving after, or a part
  that is not base64, which now closes the connection rather than being skipped; and the relay
  records the receipt of either, described from the label. A labeled request is a boundary
  request in the process whatever its frame asks, so it always ends. The relay holds at most
  1,024 requests of an agent that have not ended, refusing one more `queue_full`, and keeps 256
  bytes of a member or target name a label or notice gives.
- **A boundary request runs only under the relay's label.** An unlabeled frame, which a
  hand-written client sends, used to be served and recorded under its own number, and the relay
  attached its receipt to any request it held by that number: agent code could send one under
  the number of a labeled frame still to arrive, and the labeled frame's refusal then ended the
  record of the unlabeled call while it ran, as `refused` -- a call that ran, recorded as one that
  never did. The process now refuses such a request after its receipt, before anything waits,
  so it never runs and its record ends at once; the closed flag still refuses one read after the
  close first. The hand-written clients in the binding and hosted tests label what they send, as
  the interpreter does. With nothing unlabeled running, the 1,024 bound needs no check at the
  receipt.
- **`/simplify` on the second review's fix** had an independent version written to the same
  tests, and the review judged it simpler. Taken from it: one queue of labels on each channel,
  appended at a frame's first part and taken by `recv`; the module-level `_record` and `_dropped`,
  with no callback handed to the channel; the part that is not base64; and the relay's bound
  returning its refusal. Kept from the first: a failed write on the close paths does not stop the
  cleanup; the label is handed to the request once it is read, so no second `ended` follows the
  request's own; a labeled request is recorded as a boundary one rather than refused; and the
  documentation of the new reason. The review found the gap common to both that the previous
  bullet closes.
