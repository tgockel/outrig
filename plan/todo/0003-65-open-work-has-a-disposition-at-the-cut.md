# 0003-65 -- Open work has a disposition at the cut

## Context

A request a child holds ends one of eight ways: `RequestEnd` is `Accepted`, `Rejected`, `Failed`,
`Cancelled`, `Released`, `Budget`, `Closed` or `Terminated` (`crates/outrig/src/harness/event.rs:
1238-1256`), mapped from the interpreter's name (`crates/outrig/src/agent/children.rs:1401-1411`,
`outcome`), and the host ends one itself only through `_HOST_ENDS` (`crates/outrig/src/python/
interpreter.py:6437-6452`), which `0003-37` extends with `cancelled`. At the close, the registry
notes every request open (`children.rs:1375-1397`, `close`; `:81-97`, `at_close`) and the
interpreter's `_close` ends each with `SessionClosing` (`interpreter.py:6207-6238`); the report
lists them `closed`, or `terminated` when the interpreter died first (`children.rs:269-287`,
`report`; `crates/outrig/src/harness/report.rs:119-126`). A message between co-hosted agents
never reaches the host (`messages.md`, lead), so the host holds a bounded preview of a request's
body (`event.rs`, `AgentRequestSent`) and the ids unread and taken per child
(`children.rs:1012-1037`, `summary`; `:1041-1072`, `counts`), never the body. The primary's
`user` channel is fed by the host (`agent/channel.rs`, `UserChannel`), and the interpreter
answers how many wait unread (`crates/outrig/src/python/host.rs:1147-1149`, `pending`). `0003-52`
gives an external request channel whose requests the backend holds by durable id; `0003-53` gives
stable item ids and the cut descriptors; `0003-56` gives an MCP call its dispatch states, one of
them possibly dispatched; `0003-62`'s cut refuses while any of this is open.

OR-SUS5 asks that "child histories/results and obligations need supported dispositions; logical
IDs may reattach supported handles, not pickle futures"; that "unsupported open children must be
refused or explicitly ended/degraded, never silently lost"; that a checkpoint "includes ...
accepted unread child results"; and that "unsupported live state/children produce an explicit
restoration limitation, not fake faithful continuation". OR-CH3 and OR-SUS6 ask that external
requests and their positions are captured and reconciled by durable identity rather than
reposted.

**This task reverses a recorded decision, in part.** `lifecycle.md`, "Closing a session", says
closing has "child work and unanswered requests cancelled, and every waiter on them settled with a
documented error", and the interpreter settles them `SessionClosing`; a sleep that ended in that
close would cancel every request the agent holds and tell the owner `SessionClosing`, "which is
the opposite of capturing their positions" (the validation, reversal 4, second part; the conflict
check's C6, point ii). What this task adds: a terminal request end for suspension, recorded before
the close settles anything, with the `terminated` end the close uses for an interpreter death as
its precedent. And `lifecycle.md`, "The interpreter's death ends the session", says "A fresh
interpreter inherits no pending handles and no proxies, only the notice", with "Rejected:
reattaching to proxies after the owner restarts" under its rejected alternatives. What this task
narrows, not reverses (reversal 4, third part; C6, point i): a handle to a durable external
request is reattached by its logical id through `external-channels.md`'s registry, because the
backend holds the request and its identity outlives the session; no proxy is reattached, and every
other handle is ended. The adversarial review (issue 1) asks for an explicit in-flight disposition
for a park in the middle of a wait, and says root-only histories are defensible if stated.
`continuation.md`, "Open work at the cut", designs what this task builds.

## Goal

Every piece of work open at a cut is given a recorded disposition before the close settles
anything: external requests end `suspended` and reattach by durable id on resume, local children
are ended or refuse the cut, and calls whose outcome cannot be known are recorded uncertain.

## Deliverables

- **A terminal end for suspension.** `RequestEnd::Suspended`, its name in `outcome`, in
  `_HOST_ENDS`, and in `doc/reference/events.md`'s `agent.request.settled`; a Python exception
  `outrig.SessionSuspended`, an `AgentError` (fork 4), raised by a `reply` or `fail` on a request so
  ended and by `await` on its handle in a kernel, and the same end read on the Rust control
  handle `0003-46` gives an owner for a root instance (A-02) -- and on a host program's
  `handle.future` where `0003-35` exists. The end is applied by the cut, before admission closes,
  so the close finds each such request ended and settles nothing it should have recorded;
  `ShutdownReport.requests` lists them `suspended`.
- **External requests owed a reply** (`0003-52`): at a sleep, each is ended `suspended` and the
  backend is told so under the request's durable id through `external-channels.md`'s registry
  (fork 5), the request staying the backend's; the manifest records each with its durable id and
  the channel. On resume (`0003-64`), the backend redelivers by id and the registry reattaches the
  delivery under the same durable id, so a reply after the resume reaches the backend under it.
  At a save, nothing ends: the request stays open and the manifest records its position.
- **External requests the agent made, outstanding** (`0003-52`, `0003-53`): at a sleep, each is
  ended `suspended` in the kernel -- a handle still awaited raises `SessionSuspended` -- and the
  backend is told `suspended` under the durable id through `withdrawn`; a backend that answers with
  a reply or failure it had already recorded makes the cut record that value as an accepted result
  not collected (fork 3) instead, and nothing is suspended. The manifest records each outstanding
  request with its durable id, channel, backend reference and state. On resume (`0003-64`) the
  registry is seeded with them, so the owner's `settle(request_id, reply)` is accepted and delivered
  as an arrival announced with the channel and the request id, the reply under
  `runtime.continuation.restored[request_id]`; the notice lists each as an outstanding obligation.
  At a save, nothing ends: the request stays open and its state is recorded.
- **External deliveries unread**: recorded as `0003-53`'s cut descriptors in the manifest -- ids and
  positions, no bodies -- and seeded on resume as awaiting redelivery under their saved delivery ids
  (`0003-64`); the backend redelivers by id, and the first push of each is queued.
- **The primary's `user` channel, unread** (fork 2): recorded at the cut with their delivery ids
  and bodies, since the host authored them, and redelivered on resume as pending deliveries before
  the first round, so `pending()` and the announcer count them again; counted lost where the
  alternative is chosen.
- **Local children** (fork 1): a child that is not idle, or that holds any request unread or
  taken, refuses the cut naming the child and the request ids, as `0003-62` refuses today; an
  idle child holding nothing is released at a sleep -- its requests, none, settle nothing -- and
  left running at a save. Every child's store is in the package as a record (`0003-58`), and
  none is restored; the notice names each child released and that its history is a record. The
  reason is stated in the manifest and the notice: the host never holds a local request's body.
- **Accepted results not yet collected** (fork 3): a request `Accepted` whose handle the parent's
  code has not awaited at a sleep has its value recorded in the manifest, within the message
  bound, and restored under `runtime.continuation.restored` by request id; the notice names it.
- **Calls whose outcome cannot be known**: a hosted call still running at the drain deadline, an
  MCP call `0003-56` reports possibly dispatched, and a live-object call running in a host program
  are each recorded `uncertain` in the manifest with the receipt's fields -- binding or service,
  operation, member, bounded argument previews, the execution and invocation it was made in --
  and never cancelled as if that undid them, never replayed. `0003-62`'s refusal for them becomes
  this recording under the degraded policy, and stays a refusal under the clean one.
- **Executions still running at a sleep**: `unknown`, as the close records them; at a save, the
  cut's refusal stands.
- **A child at its model-request limit** (`0003-50`, OR-A4-3): its state is limit-reached, which
  is not open work -- the limit ended its last request with the `limit` outcome, and `0003-37`
  spends no warning round on it -- so it neither refuses the cut nor is ended `suspended`: at a
  sleep it is released as an idle child is, at a save it stays resident, and the manifest records
  its spent count, its limit and the request the limit ended, so the resumed owner can set a fresh
  limit (OR-A4-2).
- **Events**: `continuation.disposition` per open item, with its kind, its id and its
  disposition, execution diagnostics; `agent.request.settled` gains the outcome. Added to
  `doc/reference/events.md`.
- **`lifecycle.md`'s close table** gains rows: a request open at a sleep's close has ended
  `suspended` before the close and is listed so, not `closed`; a local child idle at a sleep is
  released before the close; a `reply` arriving for a suspended request is refused and evented as a
  late reply is (`work.md`, "A request settles once").
- **`## Decisions` records the reversal**: the sentences quoted above, what was added and what was
  narrowed; the end's name and exception; its order against the close; what is reattached and by
  what id, in both directions, and what is ended; the limit-reached child's disposition; and the
  children, user-channel and accepted-result forks taken.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **An external request at a sleep.** With `0003-52`'s fixture backend holding a reply-owed
  request the agent has taken, `sleep` ends it `suspended`: the backend is told so under the
  durable id and not told cancelled; `agent.request.settled` carries `suspended`;
  `ShutdownReport.requests` lists it `suspended`; the manifest holds its id and channel. The
  resumed session (`0003-64`) has the backend's redelivery under the same id on the same channel,
  and the agent's reply reaches the backend under it.
- **A save leaves it open.** The same request at a `save` is not ended, the backend is told
  nothing, and the manifest records its position.
- **Local children.** A child holding a taken request makes the cut refuse naming the child and
  the id; a running child, the same; an idle child holding nothing is released at a sleep with
  `agent.instance.released` or `agent.released` recorded before `closing`, and left running at a
  save; the package holds its store as a record; the notice names it released.
- **The suspension exception.** A `reply` from the child's code after the end raises
  `SessionSuspended`; the parent's `await` raises it; the owner's control handle for a root
  instance (`0003-46`) reads the request's end as `suspended`, not `closed`; each kernel-side
  error is an `AgentError`, and none is `SessionClosing`. Where `0003-35` exists, a host
  program's `await handle.future` raises the same.
- **A limit-reached child.** A child whose limit (`0003-50`) ended its last request `limit` is
  present at a sleep: the cut is not refused for it, no `agent.request.settled` with `suspended`
  is recorded for it, no warning round opens, it is released as an idle child is, and the
  manifest holds its spent count, its limit and the ended request's id; at a save it stays
  resident with the same record.
- **The user channel.** Two messages unread at a sleep are in the manifest with their ids and
  bodies; after the resume the channel's `pending()` is 2, the announcer counts them, and
  `receive()` yields them in order with the same bodies.
- **An accepted result not collected.** A child's reply accepted and never awaited is in the
  manifest and under `runtime.continuation.restored` after the resume, equal to the value; the
  notice names the request id.
- **Uncertain calls.** A hosted call on the slow fixture binding running at the deadline, an MCP
  call the `0003-56` fixture leaves possibly dispatched, and a live-object call on `0003-36`'s
  fixture are each `uncertain` in the manifest with the receipt's fields under the degraded
  policy, and each refuses the cut under the clean one; no cancel is sent for any of them, and
  the notice lists each.
- **Order against the close.** The sequence of events for a sleep reads `continuation.disposition`
  and the `suspended` settlements before `session.state` `closing`, and no `agent.request.settled`
  with `closed` follows for a request that had a disposition.
- **`0003-37` after a resume.** The resumed root agent holds no local request, so no warning and no
  cancel fires from the old session's state; a new request arriving after the resume opens a
  round and is counted from nothing.
- **An outbound request outstanding at a sleep.** With one request the agent made on `asks`
  unanswered: the sleep ends it `suspended`, `withdrawn` receives `suspended` with the id, the
  manifest lists it with its backend reference, and `ShutdownReport.requests` lists it `suspended`.
  The resumed session (`0003-64`'s fixture) accepts `settle(id, reply)`, the holder's next round
  opens with an announcement naming `asks` and the id, and `runtime.continuation.restored[id]` holds
  the reply. A test backend that answers `withdrawn` with a reply it had already recorded makes the
  cut record the value as an accepted result, and nothing is suspended.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Local children -- Recommended: refuse the cut unless the child is idle with nothing open, and
   release an idle one at a sleep.** Root-only histories follow: a child's history travels as a
   record and is not restored, which the adversarial review calls defensible when stated. The
   alternative ends a busy child's requests `suspended` under a policy flag and restores the
   interacting tree, which needs local request bodies the host never holds.
2. **The user channel's unread messages -- Recommended: recorded with their bodies and
   redelivered**, since the host authored them and `pending()` is exact at the cut. The
   alternative counts them lost in the notice, which was the other option of the
   restart entry `continuation.md` folded in.
3. **Accepted results not collected -- Recommended: the value is recorded at the cut and restored
   by request id.** It is in the serializable subset by construction and is the state the agent
   would otherwise lose. The alternative records only that the request was answered.
4. **The exception -- Recommended: `outrig.SessionSuspended`, an `AgentError`**, so a sender that
   catches "anything went wrong" catches it and one that tells a suspension from a close can.
   The alternative reuses `SessionClosing` with a flag, which no `except` clause can read.
5. **Telling the backend -- Recommended: the backend is told `suspended` under the durable id**,
   so its own record can say the request waits for a resume rather than for a session that is
   gone. The alternative tells it nothing and relies on redelivery alone.

## Dependencies

- **Hard: `0003-46`.** The Rust control handle an owner holds for a root instance (A-02), on
  which a suspended request's end is read.
- **Hard: `0003-52`, `0003-56`.** External requests by durable id; an MCP call's dispatch
  states.
- **Hard: `0003-62`, `0003-63`, `0003-64`.** The cut whose refusal this task turns into
  dispositions, the sleep that applies them, and the resume that reattaches.
- **Hard: `0003-50`.** The model-request limit whose limit-reached state has a disposition here.
- **Hard: `0003-30`, `0003-37`.** The request machinery and its ends, and the cancel this end is
  distinct from.
- **Soft: `0003-35`.** A host program's handles, which raise the same exception where that task
  exists; the first application holds the Rust control handle instead.
- **Soft: `0003-51`, `0003-53`, `0003-49`.** The backend, the cut descriptors, and the roster's
  limit-reached state.
- **Soft: `0003-36`.** The live-object call recorded uncertain.

## See also

- `plan/phase/0003-python/continuation.md` -- "Open work at the cut", "Where the queued
  0003 tasks interact".
- `plan/phase/0003-python/external-channels.md` -- the registry a durable request is
  reattached through.
- `plan/phase/0003-python/lifecycle.md` -- "Closing a session", "The close table", "The
  interpreter's death ends the session".
- `plan/phase/0003-python/work.md` -- "A request settles once".
- `plan/phase/0003-python/messages.md` -- why a local request's body never reaches the host.
- `crates/outrig/src/agent/children.rs`; `crates/outrig/src/harness/event.rs`, `report.rs`;
  `crates/outrig/src/python/interpreter.py`, `host.rs`; `crates/outrig/src/agent/channel.rs`.
