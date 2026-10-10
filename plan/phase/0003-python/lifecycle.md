# Lifecycle

How a session ends, and what its owner can rely on when it stops one. The owner is the Rust
program holding the session through the API in `embedding.md` -- `run-new`, or an embedder. This
page is the sequence that runs when the owner stops, what it does to each thing still running at
that moment, and the report the owner reads afterwards.

**Decided in planning (2026-09-30).** The owner stops a session with two calls,
`close_admission()` and `shutdown(deadline)`, and neither needs anything from the agent. `0003-19`
builds both for what exists today -- executions, the interpreter, the container -- and every later
task that adds something that can still be running at close adds its row to the close table below
and tests it. Nothing here resumes a session: a session that stops stays stopped, and nothing it
ran is run again.

## Three things end separately

| what               | ends when                                       | does not mean             |
|--------------------|-------------------------------------------------|---------------------------|
| a round            | the model yields, or a limit stops it           | Python stopped; work done |
| an execution       | it has an outcome: `ok`, `error`, or `unknown`  | the round is over         |
| an embedder's task | the embedder says so, after the shutdown report | an execution succeeded    |

A round ending means only that the model yielded, by itself or because a limit stopped it. Python
it started may still be running -- a background task the agent kept a reference to keeps going
after the round (`execution-and-rounds.md`) -- and an embedder's task is not finished because a
round was. An embedder that took a reply as completion would release its task while its agent's
code still ran. The shutdown report is the first point at which the owner knows that nothing the
session started is still running, so that is where an embedder decides its task is done.

## Admission

**Admission** is the gate for all new work: a boundary request -- a hosted call
(`hosted-objects.md`) -- on its way to its target, an execution, and a child's launch. While it is
open, new work starts. Once it closes, a boundary request the owner has not forwarded to its
binding is refused, a submission is refused rather than run, and a child launch fails at once.

The owner's single `close_admission()` is one state change in Rust, and it returns at once. That
change closes Rust's gate and stops the relay forwarding to every binding, together, so no request
is admitted by one and not the other. The bindings learn of the close afterward:

- **Rust's gate**, one per session, decides executions, child launches, and the boundary requests
  held for a decision -- those a rule escalated or sent to the evaluator. `0003-23` builds it for
  three races: an interrupt, a close, and an approval that arrives late. Whichever reaches the gate
  first decides. "Closed" and "allowed" cannot both win, so an allow that arrives after the close
  is recorded and changes nothing.
- **The relay** orders the requests a rule allows against the close. Rules run in the binding
  process (`0003-23`'s fork 1), so such a request never passes through Rust's gate, and the relay
  is the last point the owner holds it. A request the relay had not forwarded when it stopped --
  one in its queue, or one that arrives later -- is refused in Rust and reaches no binding. One it
  had forwarded belongs to the drain: the owner cannot take it back, its binding may read it and
  invoke its target after `close_admission()` has returned, the drain waits for it, and its
  outcome is the one the binding reports (`0003-22`).
- **Each binding process's closed flag** is installed asynchronously. The relay enqueues a control
  line behind the frames it had forwarded, without waiting -- a binding that has stopped reading,
  with a full pipe, does not delay the close -- and the binding sets its flag when it reads the
  line. From then on it refuses a request a rule allows, with admission closed as the reason, and
  cancels one it had read but not started, waiting for the serialize lock. Until the binding
  observes the flag a forwarded request may start, and then it drains; so the cancellation of a
  waiting call is relative to the binding observing its flag, not to the close returning. The
  flag only makes a refusal faster than running the call would be, and nothing the owner relies
  on depends on when a binding reads the line. A binding still honors an allow the gate issued
  before the close.

A call waiting at the close for a free connection -- every connection in its kernel's pool for the
binding has a call in flight (`0003-17`) -- has not been sent. It is woken with the closing error
and recorded `cancelled`; the host never saw it, so the interpreter reports it.

The agent sees a refused call as an exception raised in its own code. The exception reports the
refusal; it does not enforce it. Agent code can catch any exception, so enforcement is the gate's,
the relay's and the flag's: a call refused there never reaches its target, whatever the code does
next.

**Closing admission is the owner's act, not a request.** `close_admission()` is not a boundary
request: no policy rule sees it, no evaluator judges it, and no approver is asked. It changes the
session's state and returns at once. So the owner can always stop without the agent's cooperation:
nothing the agent does -- a call it holds in approval, an execution that never yields, an
evaluation still running -- can delay the close.

There is no agent-side counterpart. Agent code cannot end its own session through any OutRig API;
`embedding.md` describes how an embedder whose agent should be able to say it is done builds that
from a hosted client, and why neither side then waits on the other.

## Closing a session

`shutdown(deadline)` closes admission if the owner has not, then runs four steps:

```text
closing      admission closed: Rust's gate shut and the relay's forwarding stopped, in one
               step; a control line enqueued to each binding
             pending approvals and evaluations cancelled; a late answer is recorded and
               ignored
             hosted calls the relay had not forwarded refused in Rust; a call waiting for
               a free connection woken with the closing error
             child work and unanswered requests cancelled, and every waiter on them
               settled with a documented error; a result or reply already validated
               stays settled; an agent instance still launching never becomes ready
draining     forwarded hosted calls and running executions continue, until they finish
               or the deadline passes; a binding that reads its control line refuses what
               it reads after it and cancels what it had read but not started
terminating  every kernel interrupted
             each binding's process group sent SIGTERM, then SIGKILL
             the container stopped
reported     the ShutdownReport returned to the owner
```

The session's state, as its events report it (`embedding.md`), is *closing* from
`close_admission()` through terminating, and *reported* once the report exists.

**Closing** settles everything the owner still holds. A call waiting for an approval is cancelled
through the escalation handler's cancellation (`boundary-policy.md`), so the approver's interface
can withdraw the request. A call the relay had not forwarded is refused there and never reaches
its binding, and one waiting for a free connection is woken with the closing error. A call the
relay had forwarded is its binding's to finish: the binding refuses it if it reads it after its
flag, cancels it if it was still waiting for the serialize lock when the binding observed the
flag, and otherwise runs it -- so a forwarded call can start after `close_admission()` has
returned, and the drain waits for it. A child's work is cancelled: its round makes no further
model call, and a request it had not answered is cancelled with it. Every caller waiting on
something cancelled or refused here is woken with an error that says the session is closing, so
nothing is left waiting.

A child's waiters are all settled alike: an `await` on its handle, a `runtime.wait` or
`asyncio.wait` over it, and a second await of the same handle each raise the same documented
error, whose name is `0003-26`'s. No partial or unvalidated result is returned as a success. A
child whose result was validated before the close stays settled with it, because a settled result
does not change (`work.md`).

**Draining** gives what is already running time to finish: hosted calls the relay had forwarded
-- running, or still to be read by their binding -- and executions already running. Neither is
interrupted before the deadline, because a call allowed to
return has a known outcome and a call cut off does not. Background tasks are not waited for as
such: a hosted call one of them is making drains like any other, and the task itself is
interrupted with its kernel. The drain ends when nothing it waits for is left, or at the deadline.
The deadline is the owner's argument to `shutdown`; its default is `0003-19`'s design fork.

**Terminating** stops what is left, starting with the step agent code can respond to. Kernels are
interrupted first, so code still running can run its `finally` blocks and an execution can end
with `error` rather than `unknown`. Then each binding's process group gets SIGTERM, and SIGKILL
after a grace period, which ends a call still running on the host and every program it started --
a git process, a hook, a credential helper. Last the container is stopped, which ends the
interpreter and whatever an interrupt could not reach: native code holding the GIL, which
`runtime-protection.md` names as the case no interrupt can end, and processes the agent started in
the container.

`shutdown` returns a bounded time after the deadline. SIGKILL cannot be caught or ignored, and a
container stop is an engine call with its own grace and its own time limit. A process that still
does not exit -- one in uninterruptible sleep, say -- is what makes a report say the session is not
proven stopped.

## The close table

What closing does to each resource: one row per resource, each added by the task that introduces
the resource and covered by that task's tests, and written as a list because the rows are too wide
for a table. For each state the resource can be in at close: the outcome, whether the target --
the host object a call was aimed at -- was invoked, and what is then known about effects outside
the session.

**Executions** (`0003-19`)

- *Running.* Outcome: continues until the drain ends, then its kernel is interrupted, and the
  container stop ends whatever the interrupt could not; `ok` or `error` if its result arrived,
  `unknown` if not. Invoked: not a boundary request. Effects: what it did stands; each hosted call
  it made has a row of its own.
- *Submitted after the close.* Outcome: refused. Invoked: no. Effects: none.

**The interpreter and the container** (`0003-19`)

- *Running.* Outcome: every kernel is interrupted, then the container is stopped, which ends the
  interpreter with it; a failed stop makes the report say not proven stopped. Invoked: not a
  boundary request. Effects: what the session's processes wrote to the workspace stands.

**Binding processes** (`0003-20`)

- *Idle, or serving a call.* Outcome: after the drain the process group gets SIGTERM, then SIGKILL
  after a grace period; the binding counts as stopped once its process has been reaped and no
  process is left in its group. Invoked: not itself a call; a call it was serving is a row below.
  Effects: whatever its calls did stands.

**Hosted calls and callbacks** (`0003-22`)

- *Waiting for a free connection* -- in the container, because every connection in its kernel's
  pool for the binding has a call in flight (`0003-17`). Its job is queued in the pool's executor
  with no worker thread yet, and the code that awaits it waits on it. It was never sent, so the
  host has no record of it and the interpreter reports it. Outcome: woken with the closing error
  and recorded `cancelled`, with the close as the reason. Invoked: no. Effects: none possible.
- *Queued in the relay, or made after the close* -- not forwarded when the relay stopped, so
  refused in Rust and never seen by its binding. Outcome: `refused`, the reason admission closed;
  the caller gets the closing error. A request Rust's gate holds is cancelled by the gate instead
  (**Pending approvals and evaluations**, below). Invoked: no. Effects: none possible.
- *Forwarded, not yet read by its binding* -- in the pipe ahead of the control line, which cannot
  overtake it. The owner cannot take it back, so it belongs to the drain, and the binding decides
  it: one that reads it before the line runs it, after the close has returned, and it is then a
  running call, below; one that reads it after its flag refuses it. Outcome: `returned` or
  `raised` when it ran; `refused`, admission closed, when read after the flag; `unknown` when the
  binding's group is killed before it reports. Invoked: as the outcome says, and `unknown` leaves
  it open. Effects: as for a running call when it ran; none when refused.
- *Waiting for the serialize lock* -- read before the binding observed its flag, in a binding
  declared `serialize = true`, behind the call the binding is running (`hosted-objects.md`,
  `0003-17`). A binding without that setting invokes a request's target on its connection's
  thread as soon as it reads it, so it has no such state. Outcome: `cancelled`, with the close as
  the reason, if it is still waiting when the binding observes its flag. The binding reads its
  control line on its own schedule, so a waiter the lock reaches first starts and is then a
  running call. Invoked: no, when cancelled. Effects: none, when cancelled.
- *Running* -- its target invoked. Outcome: it drains; `returned`, or `raised` with the
  exception's type, if it returns in time, and `unknown` if its binding's group is killed first.
  Invoked: yes. Effects: known only if it returned or raised. A call cut off may have finished its
  effect somewhere the session cannot see, so it is `unknown`, never reported as having failed.
- *Making a callback* -- the running call has called back into agent code in the container
  (`hosted-objects.md`); the callback runs on the worker thread that holds the call's connection,
  or on the kernel's loop when it is a coroutine. Outcome: the callback is part of a call allowed
  to finish, so it runs during the drain; at terminating a coroutine callback still running is
  cancelled with its kernel's tasks, a plain one on a worker thread is ended by the container
  stop, and the outer call ends with its binding as above. Invoked: yes, the outer call's target.
  Effects: as for a running call.

**Pending approvals and evaluations** (`0003-23`, `0003-24`)

- *Being evaluated* -- a policy rule or the evaluator is still deciding. Outcome: `cancelled`, never
  invoked; the evaluation is abandoned and the caller gets the closing error; a verdict that
  arrives afterward is recorded and changes nothing. Invoked: no. Effects: none possible.
- *Waiting for an approval.* Outcome: `cancelled`, never invoked, through the escalation
  handler's cancellation; a later answer is recorded and ignored; the caller gets the closing
  error. Invoked: no. Effects: none possible.

**Child work** (`0003-26`)

- *Running.* Outcome: cancelled -- the child's round makes no further model call, and a completion
  the child submitted but that was not yet accepted is not accepted now; every waiter is settled
  with the documented error, and nothing partial is returned as a success. Invoked: the child's
  own hosted calls have rows of their own. Effects: whatever those calls did stands.
- *Settled with a validated result before the close.* Outcome: stays settled with it. Invoked and
  effects: as for its calls.
- *A request waiting, or received but unanswered.* Outcome: cancelled -- the child's round makes
  no further model call, and a reply that arrives after the close is refused; the waiter is
  settled with the closing error, and a reply validated before the close stays settled, as a work
  item's result does. Invoked: the child's own hosted calls have rows of their own. Effects:
  whatever those calls did stands.

**Agent instances** (`0003-30`)

- *Launching* -- the instance was constructed and its child is not yet ready (`agent-classes.md`).
  Outcome: it never becomes ready; `ready()` and the first handle raise the closing error, and a
  request sent before readiness is a waiting request (**Child work**, above). Invoked: not a
  boundary request. Effects: what its orienting round did stands; its hosted calls have rows of
  their own.
- *Idle* -- ready, with no request waiting or unanswered. Outcome: released; its kernel ends with
  the interpreter at terminating. Invoked: not a boundary request. Effects: what its earlier
  rounds did stands; each hosted call they made has a row of its own.
- *Collected during the close* -- the instance's last reference was dropped after admission
  closed, so its finalizer scheduled a release. Outcome: nothing further; the close has already
  done what the release would, and the collection is still recorded as `agent.instance.collected`
  when the finalizer's notice reaches the host. Invoked: no. Effects: none beyond the rows above.

A release closes one child's subtree the same way while the rest of the session runs on
("Releasing a child", below).

## The report

`shutdown` returns a `ShutdownReport`, and the owner reads it before deciding anything about its
own task. It holds:

- that admission closed;
- whether owned execution is proven stopped: every binding's process reaped with its group empty,
  and the container stopped, which takes the interpreter and its kernels with it;
- an outcome for each execution, each hosted call, each child work item and each request on a
  child's request channel that was live at the close;
- event delivery: the last sequence the stream published, and how many events each subscriber
  missed (`observability.md`). The stream never waits on a subscriber, so a stalled one cannot
  delay shutdown; the report says what it missed instead.

An execution's outcome is `ok`, `error` or `unknown` (`execution-and-rounds.md`). A hosted call's
is one of five, the same outcomes its events record:

- `returned` -- its target returned;
- `raised` -- its target raised, and the exception's type is recorded;
- `refused` -- never invoked, with the reason: a rule, the approver, the evaluator, admission
  closed, or interception refusing the request;
- `cancelled` -- never invoked, because it was interrupted, or the session closed, while it was
  held for a decision, waiting for a free connection, or waiting for the serialize lock;
- `unknown` -- forwarded, with no reply, because its binding was killed at shutdown or its
  binding process died; whether its target was invoked is open.

An interrupted call that the binding had already invoked is none of these: it runs on, and its
reply records `returned` or `raised` with the note that the caller had been interrupted.
"Interrupted" is a reason attached to `cancelled`, or that note; it is not an outcome of its own.

Read together, those fields say one of three things:

- **Stopped, every outcome known.** The embedder can treat the stop as clean.
- **Stopped, some outcomes unknown.** Nothing the session started is running, and some call was
  cut off. The embedder can finish its task and must treat that call's outcome as unknown: an
  `unknown` push may be on the remote.
- **Not proven stopped.** A process could not be confirmed to have exited, or the container stop
  failed. The embedder must not call the stop clean or reuse what the session ran on, because
  something the session started may still be acting.

Three rules hold for every report:

- **No false clean report.** The report says stopped only when it has seen each process exit. A
  timeout, a failed stop, or a group that is not empty is reported as what it is, and never
  counted as stopped.
- **No rollback.** A completed effect stands -- a push that returned, a file an execution wrote --
  and the report lists the push as `returned`. Shutdown undoes nothing; this is the "no rollback,
  ever" of `execution-and-rounds.md`, applied to a whole session.
- **A local stop says nothing about the remote outcome.** Killing the local process that was
  pushing does not mean the remote refused the push. Such a call is `unknown`, never reported as
  having failed: the session knows that it stopped waiting, and nothing more.

## Releasing a child

A child is released explicitly, through `work.md`'s release; for a decorated call's fresh child,
once that call's result settles (`typed-agents.md`); and for an agent class's instance
(`agent-classes.md`), through `await foo.release()`, at the exit of `async with FooAgent() as
foo:`, or by the runtime when the instance is collected unreleased. That last path is a backstop:
a finalizer cannot await and runs on whatever thread dropped the last reference, so it schedules
the release on the constructing kernel's event loop, and `agent.instance.collected` is recorded.
Whichever path releases a child, releasing it closes its subtree -- the child and every agent it
launched -- the way a session close does, while the rest of the session runs on:

- its work is cancelled, and every waiter on it settled with a documented error, as at close; a
  result validated before the release stays settled with it;
- its outstanding requests -- waiting, or received and unanswered -- settle with `AgentReleased`,
  and a reply in flight at the release is refused; a reply validated before the release stays
  settled;
- its own children are released, so each closes its own subtree the same way;
- its pending escalations are cancelled through the escalation handler's cancellation, and a late
  answer is recorded and ignored;
- its running execution is interrupted;
- its forwarded hosted calls finish, or are reported `unknown`. A release kills no binding, since
  a binding's process serves every kernel in the session, and nothing stops a call on the host: a
  call whose wait an interrupt or a release ended runs on, and its reply records its outcome; one
  still running when the session's shutdown kills its binding is `unknown`.

An execution that never yields cannot be interrupted on a child's thread, and runs until the
session ends (`typed-agents.md`). What becomes of the released child's kernel, and of a background
task still running in it, is `0003-26`'s, which builds the release and tests each item above.

## Descendants, and the owner's abrupt death

`Drop` does not run when the owner is killed with SIGKILL, and a process's children outlive it by
default. So what a session starts on the host is tied to the owner by the operating system rather
than by Rust. `0003-18` proved the arrangement and built its supervisor
(`crates/outrig/src/python/supervisor.rs`); `0003-20` connects it to the session:

- **Each binding runs in its own process group**, so one signal to the group reaches the binding
  and everything it started -- a git process, a hook, a credential helper.
- **The owner sets a parent-death signal on the binding**, `SIGHUP`, between fork and exec, and
  starts every binding from one thread that lives as long as the process. Linux sends that signal
  when the *thread* that started the process exits, not when the whole owner does, so a binding
  started from a pool thread would be signaled as soon as that thread exited. The signal stays
  blocked until the binding program has installed its handler, so a death in between is pending
  rather than lost; and after setting it the child checks that its parent is still the owner,
  and exits if not, since the signal is never sent to a process whose parent was already gone.
- **The binding kills its own group when the signal arrives**: SIGTERM to the group, a second,
  SIGKILL to the group, itself included. The parent-death setting is cleared in a forked child,
  so nothing the binding starts inherits it; the binding acts for all of them by killing the
  group it leads. End of input on its stdin -- the owner's death closes the pipe -- does the
  same, as a second detector. SIGTERM keeps its default disposition in the binding, so while the
  owner lives only the owner's own stop decides when the group ends.
- **Containers keep the existing cleanup** (`crates/outrig/src/container/mod.rs`): an explicit stop
  on the normal path, a detached `podman rm -f` from `Drop` when the future that was to run the
  stop is dropped, and a sweep from the panic hook.
- **The interpreter ends with the owner.** Its `podman exec` client runs in a process group of its
  own (`runtime-protection.md`), and when the owner dies the interpreter reads end of input and
  exits -- unless native code holding the GIL keeps its reader thread from running, in which case
  it runs until the container is removed.

Abrupt death is tested with `kill -9` of the owner during a slow hosted call that started a
grandchild: afterwards, nothing remains in the binding's group (`0003-18`).

What this does not cover, stated rather than implied:

- **A descendant that leaves the group.** A program that moves itself to a new process group or
  session is reached by neither the group kill nor the parent-death signal. A cgroup would still
  include it; none is used in this phase.
- **A group proven empty needs a reaping PID 1.** Only `kill(-pgid, 0)` answering that no process
  exists proves the group empty, and a killed member stays in its group as a zombie until its
  parent reaps it. The binding is reaped by the owner, its children by init once the binding is
  gone -- milliseconds under systemd -- so on a host whose PID 1 does not reap orphans the proof
  fails with nothing running, and the report says not proven stopped rather than guess.
- **The container after the owner's death.** None of the container cleanup runs after SIGKILL, and
  the container is podman's rather than a child of the owner, so it keeps running -- with the
  workspace mounted and whatever the agent started in it -- until someone removes it.
  `outrig clean` reports a running stray rather than removing it. Since #469 the container is
  labeled `org.outrig.session=<sid>` and named `outrig-<sid>`, so `run-new`'s is found as `run`'s
  is; the id is the `LaunchSpec`'s, minted when the caller names none, and `0003-19`'s builder
  takes it with the rest of the spec.

## The interpreter's death ends the session

A session has one interpreter, and in 0.3 its death ends the session, whatever the cause: an
`os._exit` in agent code, a kill from outside, the Linux OOM killer. Every kernel ends with it --
the main agent's and each child's -- and so does every Python-side waiter, so nothing is left in
Python to settle. The session accounts for what was live instead, and treats the death as a
close: admission closes, and what was waiting is settled as it would be at close.

- An execution that was running is `unknown`: a confirmed exit means it is over, however it ended
  (`execution-and-rounds.md`).
- Child work and outstanding requests are reported terminated.
- A pending approval is `cancelled`, since its caller is gone.
- A hosted call already running keeps running on the host, because its binding does not know the
  caller died, until it returns or `shutdown` kills the group. No caller is left to receive what
  it returns. Its outcome is `returned` or `raised` if it returns, and `unknown` if the group is
  killed first.

The owner still calls `shutdown`, which stops the bindings and the container and returns the
report. `run-new` already ends when the interpreter exits, with status 1 (`0003-08`); the session
API reports the death as an event, not as a state of its own, and the session then passes through
*closing* to *reported* (`embedding.md`).

**No restart in 0.3.** If a later phase continues a session past its interpreter's death, it
follows one rule, recorded in `plan/next/interpreter-restart-with-a-reset-notice.md`: a fresh
interpreter, a notice telling the model its Python state was reset, and no replay of source that
already ran. Replaying source to rebuild a namespace would repeat its effects -- the push, the
write -- which `execution-and-rounds.md` rules out for a single lost result and which is no safer
for a whole session. A fresh interpreter inherits no pending handles and no proxies, only the
notice.

## Five different acts

Closing uses several kinds of stopping, and they are not interchangeable.
`execution-and-rounds.md` names four of them for a single execution; a session adds a fifth,
interrupting an execution.

- **Cancel a wait.** Stop watching; the operation continues. Cancelling the code that awaits a
  child's handle leaves the child working, because the handle shields its result (`work.md`).
- **Cancel work.** Ask the operation to stop. It may refuse, or may already have acted. `cancel()`
  on an asyncio task is one, `h.cancel()` on a child's work item or request another, and closing
  cancels every child's work.
- **Interrupt an execution.** Raise `CancelledError` or `KeyboardInterrupt` in agent Python,
  through the interrupter (`runtime-protection.md`). A hosted call the execution awaits raises
  where it is awaited; one not yet invoked is `cancelled`, and one already invoked runs on in
  its binding, its reply recording `returned` or `raised` with the note that the caller was
  interrupted. Terminating interrupts every kernel.
- **Kill a process.** Send a signal, which a process may handle or ignore unless it is SIGKILL --
  to a subprocess, or to a binding's whole group at terminating.
- **Cancel an accepted remote operation.** Usually impossible, and the effect may already exist. A
  push the remote has taken cannot be withdrawn; closing never claims to, and reports such a call
  `unknown`.

A cancelled wait never cancels work, and none of the five replays anything.

## A worked example

The agent's code is running a workflow in execution E12, in round R5. The session has one binding,
`repo`, a GitPython `Repo` -- the phase's example of a hosted library. Earlier in E12 the code
started a push as a background task,
`asyncio.create_task(repo.remotes.origin.push()._resolve(), name="push")` -- call C40, in the
binding's process. It also started a child to review a section, work item W3, and now awaits
both in `asyncio.gather(push, review)`. The child, in its own kernel with its own connection to
the binding, awaited `repo.git.execute([...])`; policy escalated that call, C41, and it is waiting
for an answer. Then the operator stops the session.

```text
t0      the owner drops the round it was driving, calls close_admission(), which
        returns at once, and calls shutdown(30 s)
        closing:
          C41  approval cancelled through the handler; the child's call raises the
               closing error; C41 is never invoked
          W3   cancelled: the child's round makes no further model call; W3 settles
               with the documented error, and E12's gather raises it
        draining begins
t0+2s   the approver answers allow for C41: recorded, changes nothing
t0+4s   C40 returns: outcome returned, its result recorded
        E12 has already ended with error, from the gather; nothing is left running,
        so the drain ends
        terminating: kernels interrupted, none of them running; the repo binding's
        group sent SIGTERM, its process reaped and the group empty; container stopped
        reported: admission closed; owned execution proven stopped; E12 error;
        C40 returned; C41 cancelled while waiting for approval, never invoked;
        W3 cancelled, every waiter settled; no subscriber missed an event
```

Three variants:

- **C40 not back by the deadline.** Terminating kills the binding's group, and C40 is `unknown`,
  because the remote may already hold the pushed refs. The report reads stopped, with an unknown
  outcome: the embedder can finish its task and must treat that outcome as unknown.
- **The group not confirmed empty.** A process in it outlasted SIGKILL -- in uninterruptible sleep,
  say. The report reads not proven stopped, and the embedder must not call the stop clean.
- **The interpreter died instead.** E12 is `unknown`, W3 is terminated, C41 is `cancelled` because
  its caller is gone, and C40 runs on in its binding: `returned` if it returns, `unknown` if
  `shutdown` kills the group first. No fresh interpreter starts in 0.3.

What did not happen matters as much: nothing reconnected, nothing retried C40, no source was
replayed, no waiter was left waiting, the late allow for C41 ran nothing, and the completed push
was not undone.

## Rejected alternatives

**Rejected: stopping calls by raising an exception in Python.** The obvious mechanism, since an
exception is how the agent learns that a call was refused. Rejected as the mechanism because agent
code can catch any exception, and because an exception in the container does not stop a call
already running on the host. The exception informs the agent; the gate enforces.

**Rejected: starting calls the owner still holds during the drain.** A call queued in the relay,
held at the gate, or waiting for a connection at the close could be started and given the drain
like any other. Rejected because once the owner has decided to stop, a call that has not left it
is an effect the session can still avoid, and the agent will not get to act on its result. A call
the relay had already forwarded is outside this rule: the owner cannot take it back, so it
belongs to the drain, and its binding refuses or cancels it only once it observes its flag
("Admission").

**Rejected: leaving a cancelled child's waiters waiting.** Cheapest, since nothing has to be
settled. Rejected because the code awaiting the child would wait out the whole drain and then be
interrupted, ending with an interrupt instead of an error saying the work was cancelled. A stop
would look like a hang.

**Rejected: a durable call ledger, for exactly-once across crashes.** It would let a restarted
owner learn which calls completed. Rejected as outside this design: the embedder's own records --
a receipt, a version number -- are authoritative for its effects, and OutRig reports `unknown`
rather than claim more than it saw.

**Rejected: reattaching to proxies after the owner restarts.** A reference is usable exactly while
its session runs. The only continuation is a fresh interpreter with a reset notice
(`plan/next/interpreter-restart-with-a-reset-notice.md`).

## Open questions

- ~~The default drain deadline.~~ Settled by `0003-19`: five seconds, `harness::DEFAULT_DRAIN`,
  which `run-new` passes; `shutdown` takes the deadline as its argument, and nothing else changes
  it.
- Whether anything but the owner's argument may change the grace between SIGTERM and SIGKILL for
  a binding's group. `0003-18` made it 5 s, counted after the drain deadline rather than against
  it, which `0003-19` and `0003-22` do the counting for.
- ~~Whether a round still being driven at the close ends at once.~~ Settled by `0003-19`: it runs
  on, each submission refused with the closing reason as its result, until the model yields or
  the owner drops it.
- Whether a callback a draining call makes into the container should be refused after the close
  rather than run. The row above lets it run, as part of a call allowed to finish. Refusing it
  would stop agent code running in a session that is ending, at the cost of failing a call that
  could have returned. `0003-22`'s callback tests are where a reason either way would show.

## Unverified

- The parent-death signal's semantics were run by `0003-18`'s tests: a process started from a
  tokio pool thread was signaled when that thread exited and a binding started from the
  supervisor's thread was not; an owner killed between the fork and the setting of the signal
  left a child that found its parent changed and exited; and a signal blocked before exec and
  sent before the handler existed was delivered when the handler's installer unblocked it. Still
  read rather than run: that the setting is cleared in a forked child, which the group kill makes
  moot.
- That a group kill reaches everything a binding starts holds only for descendants that stay in
  the group. Which programs a hosted library starts that leave it was not surveyed.
- The time `shutdown` takes after the deadline was not measured. The container stop is the largest
  known part: about ten seconds today, because the primary's `sleep` is PID 1 and discards
  SIGTERM, so podman waits out its grace (#255).
- The rows for executions, the interpreter and the container run, in `0003-19`'s tests; the rest
  of the close table has not, and neither has a release. Each row's behavior is acceptance for the
  task that adds it; a release's is `0003-26`'s, and an instance's release, the finalizer path
  included, is `0003-30`'s.
