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

`boundary-policy.md` makes the default with no policy configured "audit": every request runs and is
recorded. `observability.md` puts those records in the integration-audit category, which `0003-13`
defined without a producer, and `0003-22` adds its decisions to the same events. What counts as one
request is `0003-16`'s inventory: an attribute read, a call, or one step of an iteration is one
boundary request, recorded as events that share its id -- its receipt, its dispatch and its
outcome, with `0003-22`'s decisions between the first two -- while reference counting is
bookkeeping, recorded only when the interception refuses it. `lifecycle.md` has the rows this task
makes true: a call not yet dispatched at the close never is, and a call already sent to its binding
drains and is then killed with the binding's group -- `unknown`, never `failed`, because a call cut
off may already have acted on the host.

## Goal

A binding's name, in any kernel, is a proxy for the host's object; every request through it is
checked on the host, refused once admission has closed, and recorded before it reaches its target;
and a call still running at shutdown is drained, then killed and reported.

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
  threading, callbacks, interrupts and one request at a time per binding, run by every session
  with a binding rather than by tests alone; and the vendored RPyC mounted read-only into the
  container beside the payload, by the route `0003-20`'s fork 5 settles, and imported by the
  interpreter as `0003-16`'s fork 5 settles.
- **A request's events**, in the integration-audit category (fork 4): its receipt, published
  before the request is dispatched; its dispatch, when its target is invoked; and its outcome, all
  carrying the request's id. A request that is never dispatched has no dispatch event. Together
  they name the binding, the agent and its execution, the request's id, the operation, the member,
  the qualified name of the host-side type of the target, bounded previews of arguments, when the
  request was received and whether and when its target was invoked, the outcome, the duration, and
  the parent call's id for a request made inside a callback. A callback's own invocation is an
  event, its parent the call it was passed to.
  - **The outcome is one of five.** `returned`; `raised`, with the exception's type; `refused`,
    with the reason -- here the interception or admission closed, and from `0003-22` and
    `0003-23` a rule, the approver or the evaluator; `cancelled`, never dispatched, with the
    reason interrupted or the close; and `unknown`, dispatched with no reply, with the reason
    interrupted, its binding killed at shutdown, or its binding process gone (fork 3).
    "Interrupted" is a reason attached to `cancelled` or `unknown`, not an outcome of its own.
  - **An interrupted call is `cancelled` if its target had not been invoked, and `unknown` if it
    had.** Whether it had is the host's to say, from the request's state: one still waiting its turn
    in the binding (`0003-17`'s one request at a time) is dropped, never invoked, and is
    `cancelled`, as is one `0003-22` holds for a decision. A call never sent -- woken by an
    interrupt, or by the close, while it waited for its connection's lock (`0003-17`) -- is recorded
    `cancelled` from the interpreter's notice of it, since its binding never received it.
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
- **Admission in the binding.** `close_admission()` sets each binding's closed flag before it
  returns (fork 2), beside `0003-19`'s gate in Rust. The flag decides the requests a binding's own
  rules allow, which until `0003-22` adds rules that hold a request for Rust's gate is every
  request. A request the binding reads after its flag is set is refused and its target is not
  invoked -- `refused`, admission closed -- whether it sat in the relay's queue at the close or was
  sent later, which the binding cannot tell apart. Its caller gets `0003-19`'s closing error. A
  request the binding had read but not started when its flag was set, waiting its turn
  (`0003-17`), is never started: `cancelled`, with the close as the reason. An allow Rust's gate
  issued before the close is still honored when it reaches the binding after the flag
  (`0003-22`).
- **A call waiting for its connection's lock at the close** has not been sent (`0003-17`). The
  close reaches the interpreter too, whose reader thread wakes every such call; each raises the
  closing error and is `cancelled`, with the close as the reason.
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
  it by its own class. A request's receipt is in the stream before its target runs, and a request
  refused at admission has a receipt and a `refused` outcome and no dispatch.
- **A callback is recorded under its call.** A fixture method calls a callback that makes a hosted
  call of its own: the callback's invocation event and the nested call's events carry the outer
  call's id as their parent.
- **The hand-written-client suite passes in production.** `0003-16`'s tests of a client written by
  hand pass through the relay in `host.rs`.
- **A refusal by the interception is recorded.** A hand-written client's handler id outside the
  table, `del` with an inflated count, and `inspect` of an object its connection was never handed
  each leave a receipt and a `refused` outcome naming the check, and no dispatch. Agent code that
  reads a private attribute gets an error of OutRig's that is also an `AttributeError`, which
  `except` on the fixture's exception classes does not catch, and the read is recorded the same
  way.
- **An interrupt ends the wait, not the binding.** An interrupt during a slow call raises in the
  agent's code, the call's outcome is `unknown` with the reason interrupted, and the next call on
  that binding returns its own result. A call that kernel B made, still waiting its turn in the
  binding behind kernel A's slow call when B is interrupted, is `cancelled` with the reason
  interrupted and is never invoked; so is a call interrupted while it waits for its connection's
  lock, which never reaches the binding.
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
- **A closed session invokes nothing.** A request after `close_admission()` is refused with the
  documented error and recorded `refused`, admission closed, and a fixture method that writes a
  file when called leaves no file. A second `to_thread` worker waiting for its connection's lock
  behind a slow call at the close raises the closing error at once, never reaches the binding, and
  is recorded `cancelled`. A call from kernel B waiting its turn in the binding behind kernel A's
  slow call at the close is `cancelled` and never invoked, while A's call drains.
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
2. **How a binding's closed flag is set -- Recommended: by a control line the relay puts on each
   binding's stdin, ahead of anything still queued, before `close_admission()` returns.** It is
   exact. A request the binding reads after the line is refused. One it read before and had
   started is dispatched, and drains; one it read before but had not started, waiting its turn
   behind another connection's request (`0003-17`), is cancelled. It costs nothing per request.
   The flag decides only what the binding's own rules allow (`0003-22`'s fork 1): a request held
   for a decision is Rust's gate's, and an allow the gate issued before the close is honored even
   when it reaches the binding after the line. The alternative, asking the owner before each
   dispatch over `0003-18`'s decision line, adds a round trip to every attribute read, and
   `0003-22`'s fork 1, which keeps rules in the binding so that an allow costs no round trip,
   assumes it is not taken.
3. **A binding process that exits mid-session -- Recommended: the session goes on without it.**
   `hosted-objects.md` leaves this to `0003-20` and this task. The binding's calls in flight are
   `unknown`, with its process gone as the reason, its later uses raise an error naming the binding
   and how its process ended, and the event stream and the report record the exit. The alternative,
   ending the session as the interpreter's death does, is simpler, and stops an agent that may have
   no further use for that binding.
4. **When a request's events are published -- Recommended: the receipt before dispatch, then the
   dispatch, then the outcome, as events of their own sharing the request's id.** A receipt
   published before the target runs means a session that dies mid-call still shows the call was
   made, which an event written only when the request ends cannot; a long call is visible while it
   runs; and it is how `boundary-policy.md` and `0003-22` already describe the record, with the
   decision as one more event between receipt and dispatch. The cost is volume: three events per
   request, so `obj.a.b` produces six. The alternative -- one event when the request ends, with its
   receipt and invocation times inside it -- is a third of the volume and loses all three
   properties.

## Dependencies

- **Hard: 0003-16, 0003-17.** The interception, argument rules, threading and interrupts this task
  puts into sessions.
- **Hard: 0003-20.** Bindings that start, stop and are described; and through it, `0003-19`'s
  admission and report, and `0003-19`'s stream.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- the transport, presentation, and what crosses.
- `plan/phase/0003-python/boundary-policy.md` -- what a boundary request is, and the audit default.
- `plan/phase/0003-python/observability.md` and `plan/phase/0003-python/lifecycle.md` -- the
  integration-audit category, and the rows this task makes true.
- `crates/outrig/src/python/host.rs` -- `enum Reply` and `dispatch`, which the relay extends.
- `plan/next/event-audience-projections.md`, `plan/next/mandatory-audit-sink.md` and
  `plan/next/cancel-a-running-hosted-call.md` -- deferred: fields per audience, a subscriber whose
  failure closes admission, and stopping a call on the host.
