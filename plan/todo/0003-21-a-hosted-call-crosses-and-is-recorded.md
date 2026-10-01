# 0003-21 -- A hosted call crosses the boundary and is recorded

## Context

After `0003-20` a session can declare, approve, install, start and stop a binding, and the agent is
told that it exists, but agent code cannot reach it: `0003-16` and `0003-17` proved the transport
and its threading through a relay that exists only in tests. This task puts the relay in `host.rs`,
binds each binding's name in every kernel, and records every request.

Today `host.rs` speaks to the interpreter alone. Its reader task parses each line into
`enum Reply` and hands it to whoever waits for it, what the host sends is built ad hoc with
`json!`, and a line for any agent other than the primary is logged and dropped. A hosted call adds
traffic the host passes on without reading: RPyC frames between a kernel and a binding process. The
checks are the binding's (`0003-16`), so the relay carries bytes. What it must not do is let one
binding delay anything else. A binding process that stops reading must not stop the user channel,
the inventory probe, an execution's result, or another binding's frames.

`boundary-policy.md` makes the default with no policy configured allow, published as events:
every request runs and is recorded. `observability.md` puts those records in the integration-audit
category, which `0003-13` defined without a producer, and `0003-22` adds its decisions to the same
events. What counts as one
request is `0003-16`'s inventory: an attribute read, a call, or one step of an iteration is one
boundary request, recorded as events that share its id -- its receipt, its dispatch and its
outcome, with `0003-22`'s decisions between the first two -- while reference counting is
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
  host has opened -- today the primary, and later `0003-25`'s children through the same path. Each
  binding has a bounded queue, so a binding that stops reading delays only the calls waiting on it
  (fork 1).
- **Stubs**: each binding's name bound in every kernel, the primary and every kernel opened later,
  before `_boot` is taken, so the inventory lists no stub. A stub opens its connection and resolves
  its root on first use, on its kernel's thread (`0003-17`). Iteration makes one request per item,
  as RPyC's proxies do, rather than batches through `buffiter`: one request then stands for one
  item, and a batch size chosen by the request is
  `plan/next/hosted-reference-and-payload-bounds.md`'s concern. The runtime keeps its own
  reference to each kernel's stubs, apart from the global names the agent sees, so `repo = None`
  or `del repo` changes nothing the runtime uses; `0003-28`'s injection reads the stub from there.
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
    with the reason -- here the interception or admission closed, and from `0003-22` and
    `0003-23` a rule, the approver or the evaluator; `cancelled`, never invoked, with the
    reason interrupted or the close; and `unknown`, forwarded with no reply, because its binding
    was killed at shutdown or its process died (fork 3). "Interrupted" is a reason attached to
    `cancelled`, or a note on a later `returned` or `raised`; it is not an outcome of its own.
  - **An interrupt ends the caller's wait; a forwarded call is the binding's to finish.** The
    interrupt raises in the caller at once and the relay sends the binding a cancel line naming
    the request. A request the binding has not started -- waiting for the serialize lock in a
    binding declared `serialize = true` (`0003-17`) -- is dropped and recorded `cancelled`, as is
    one `0003-22` holds for a decision. One already invoked runs on, since RPyC has no request
    that stops a call; its reply, when it comes, records `returned` or `raised` with the note
    that the caller had been interrupted, and `unknown` is recorded only if no reply comes
    before the binding is killed or dies. A call never sent --
    woken by an interrupt, or by the close, while it waited for a free connection of its kernel's
    pool (`0003-17`) -- is recorded `cancelled` from the interpreter's notice of it, since its
    binding never received it.
  - **A request the interception refuses is recorded whatever its kind**, bookkeeping included:
    a receipt and a `refused` outcome naming the check. Bookkeeping that passes the checks is not
    recorded, and an `inspect` answer from which private names were only filtered is not a
    refusal. The caller's error names the refusal and is not a class the hosted library defines,
    so `except` on the library's own classes does not catch it; a refused attribute read raises
    one that is also an `AttributeError`, so `hasattr` and `getattr` with a default behave as they
    do on a local object.
  - The operations form a fixed vocabulary, taken from the inventory, which `0003-22`'s rules match.
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
  which until `0003-22` adds rules that hold a request for Rust's gate is every request. An allow
  Rust's gate issued before the close is still honored when it reaches the binding after the flag
  (`0003-22`).
- **A call waiting for a free connection at the close** has not been sent (`0003-17`). The close
  reaches the interpreter too, whose reader thread wakes every such call; each raises the closing
  error and is `cancelled`, with the close as the reason.
- **Shutdown.** Calls already sent to their bindings drain until the deadline; then each binding's
  group is killed, and a call still running is `unknown`, its binding killed at shutdown, in
  `0003-19`'s `ShutdownReport` and in its event. A callback that a draining call makes runs, as
  `lifecycle.md`'s row has it; if this task's callback tests show a reason to refuse it after the
  close, the row changes with them. A callback still running at terminating is interrupted with its
  kernel, and the call that made it ends with its binding.
- **`lifecycle.md`'s rows for hosted calls and callbacks**, true of the implementation and tested.

## Acceptance

- **Every request is recorded, receipt first, through `run-new`** with a mock model and a binding
  of `0003-16`'s fixture library: `obj.a.b` makes two requests, each with a receipt, a dispatch and
  an outcome event sharing its id; iterating a hosted sequence makes one request per item the
  iteration takes, as the inventory classifies them; a method call makes one; and a call that
  raises the fixture's exception has an outcome that names the type, while the agent's code catches
  it by its own class. A request's receipt and its dispatch are published before its target runs,
  shown by the dispatch event's publication time against the start time the fixture method
  records, both on the host's clock; and a request refused at admission has a receipt and a
  `refused` outcome and no dispatch event.
- **A callback is recorded under its call.** A fixture method calls a callback that makes a hosted
  call of its own: the callback's invocation event and the nested call's events carry the outer
  call's id as their parent.
- **The hand-written-client suite passes in production.** `0003-16`'s tests of a client written by
  hand pass through the relay in `host.rs`.
- **A refusal by the interception is recorded.** A hand-written client's handler id outside the
  table, `del` with an inflated count, and `inspect` of an object its connection was never handed
  each leave a receipt and a `refused` outcome naming the check, and no dispatch event. Agent
  code that reads a private attribute gets an error of OutRig's that is also an `AttributeError`,
  which `except` on the fixture's exception classes does not catch, and the read is recorded the
  same way.
- **An interrupt ends the wait, not the binding.** An interrupt during a slow call raises in the
  agent's code; the call's outcome is recorded `returned` when the binding's reply arrives, with the
  note that the caller was interrupted; and the next call on that binding returns its own result.
  The same call with the binding killed before it replies is `unknown`. With the fixture declared
  `serialize = true`, a call that kernel B made, still waiting for the serialize lock behind kernel
  A's slow call when B is interrupted, is `cancelled` with the reason interrupted and is never
  invoked; so is a call interrupted while it waits for a free connection, which never reaches the
  binding.
- **Shutdown kills what it must.** Shutdown during a slow call reports it `unknown`, its binding
  killed at shutdown, in the report and in its outcome event, and no process remains in the
  binding's group.
- **A draining call's callback runs.** The session closes while a fixture method sleeps before
  calling a callback: the callback runs during the drain, and the outer call ends `returned`.
- **A callback that never returns leaves shutdown bounded.** A callback that catches every
  `KeyboardInterrupt` in a loop, made by a call that is draining at the close: `shutdown` returns
  within `0003-19`'s stated bound after its deadline, the outer call is `unknown`, and no process
  remains in the binding's group.
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
  closed, and a fixture method that writes a file when called leaves no file. A fifth `to_thread`
  worker on one kernel, waiting for a free connection behind four slow calls at the close, raises
  the closing error at once, never reaches the binding, and is recorded `cancelled`.
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
  iteration costs, exceptions -- with what the agent sees in each.
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
   what the binding's own rules allow (`0003-22`'s fork 1): a request held for a decision is
   Rust's gate's, and an allow the gate issued before the close is honored even when it reaches
   the binding after the line. The alternative, asking the owner before each invocation over
   `0003-18`'s decision line, adds a round trip to every attribute read, and `0003-22`'s fork 1,
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
   it is how `boundary-policy.md` and `0003-22` already describe the record, with the decision as
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

## See also

- `plan/phase/0003-python/hosted-objects.md` -- the transport, presentation, and what crosses.
- `plan/phase/0003-python/boundary-policy.md` -- what a boundary request is, and the default of
  allow, published as events.
- `plan/phase/0003-python/observability.md` and `plan/phase/0003-python/lifecycle.md` -- the
  integration-audit category, and the rows this task makes true.
- `crates/outrig/src/python/host.rs` -- `enum Reply` and `dispatch`, which the relay extends.
- `plan/next/event-audience-projections.md`, `plan/next/mandatory-audit-sink.md` and
  `plan/next/cancel-a-running-hosted-call.md` -- deferred: fields per audience, a subscriber whose
  failure closes admission, and stopping a call on the host.
