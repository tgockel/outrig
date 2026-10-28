# 0003-52 -- A request crosses an external channel

## Context

A request between agents is built. A request child's end of a channel is `RequestEndpoint`
(`crates/outrig/src/python/interpreter.py:5310-5362`), whose `receive()` gives a `RequestDelivery`
answered with `reply` or `fail` (`:3518-3538`); the parent's end is `ChildChannel.request`, which
returns a `Handle` (`:4744-4771`, `:4641-4723`); each request is a `_Work` both kernels share,
ended exactly once under its lock (`_Work.end`, `:4492-4517`), with a late answer refused
(`refused`, `:4551-4561`) and a cancel that withdraws an unread request first (`:4631-4638`). The
host records every request of a child's in the children registry, by id, as `Unread`, `Received`
or `Ended` (`crates/outrig/src/agent/children.rs:147-167`), builds the `Summary` the runner
compares from those records (`:181-184`; `summary`, `:1012-1035`), renders the nudge from
`unread_text` and `taken` (`:1076-1093`; `crates/outrig/src/agent/orientation.rs:405-418`), ends a
request the host decides on with a `settle` line (`:1196-1200`; `_HOST_ENDS`,
`interpreter.py:6437-6442`), lists the requests open at the close in the report (`:269-287`;
`crates/outrig/src/harness/report.rs:35`), and emits the `agent.request.*` family
(`crates/outrig/src/harness/event.rs:382-444`; `RequestEnd`, `:1238-1256`). `0003-37` adds the
warning round and the host's cancel to the runner's rule, with `cancelled` in `_HOST_ENDS` and the
cause on `agent.request.cancelled`. `0003-51` gives an agent external channels of the plain kind:
a definition naming a Rust backend behind a trait, the owner's push through a channel handle, a
host end, a holder, and the four send outcomes as the agent sees them.

OR-CH2 asks to distinguish plain messages and notifications from reply-owed requests; to give a
request correlation and a waitable terminal outcome -- reply, failure, cancellation, or closure
under the backend's contract -- on which local handles and durable backend state agree, with late
replies unable to change a settled outcome; to coordinate inbound reply-owed work handled by the
resident agent with `0003-37`, warning the model and then host-cancelling with sender
notification; to leave outgoing requests to backend-defined settlement, where abandoning a local
wait neither cancels the request nor declares the peer idle; and to keep task completion a
separate, application-defined outcome. OR-A7-5 makes it concrete under the Rust backend: an
outbound request's terminal outcome is whatever the backend records; a late reply to a settled
request is refused by OutRig and reported to the backend; cancelling the local wait does not
cancel the remote request, and request cancellation does not stop the peer; peer termination
reported by the backend reaches the sender once, and later sends raise; an inbound reply-owed
request to the primary is under `0003-37`'s warning and host-cancel rules, with the cancellation
delivered to the backend as the request's one terminal outcome.

The validation found the design point this task states rather than discovers: `0003-37`'s rule
covers an inbound external request channel on the application's primary because that primary is a
runner-driven agent -- under A-02 an owner-constructed root in a `no_primary` Session, driven by
its own runner, for which the owner never calls `Session::round` (OR-A2-2; `rust-owner.md`;
`0003-46`) -- whose summary includes the request (`external-channels.md`, "The liveness rule
covers an inbound request channel on a runner-driven holder"); the `user` channel stays exempt;
and the task itself is not a request -- an agent holding none is idle, and idle is the owner's
signal (tension 5). It carried tension 4 -- one definition for two kinds, the kind declared -- and
reversal 4's second part as far as request ends go: `lifecycle.md`, "Closing a session", has the
close settle every open request `SessionClosing`, so an external request's end at the close must
be one the backend hears, for its durable record to agree with the handle; what the cut records
before the close is `continuation.md`'s (`0003-65`), which depends on this task.

## Goal

A request crossing an external channel in either direction has one terminal outcome, recorded by
the backend, on which the agent's delivery or handle and the backend's record agree, and a request
the application's primary leaves unanswered is warned about, cancelled, and its cancellation
delivered to the backend as that one outcome.

## Deliverables

- **The request kind in the definition.** `kind = request` with a direction, `inbound` or
  `outbound`, a request contract and a reply contract, each `str`, untyped JSON or a type named
  by import path; the bound counts unsettled requests -- unread, or received and unanswered -- as
  `requests-max` counts a child's (`work.md`, "Limits belong outside generated code"); the
  manifest entry says a request owes a reply, answered by `reply` or `fail` on its delivery.
- **Inbound: the owner pushes a request.** The channel handle's `deliver` with kind `request`,
  answered once the request is queued with its delivery id, as a repeat by item id, or refused
  `full`, `contract` or `closed`. The host records it as a request of the holder (fork 1), so
  `summary`, `taken`, `unread_text`, `open`, the report and the `agent.request.*` events apply to it
  unchanged, with `sender` the identity the owner gave. The agent's end is a request child's:
  `receive()` gives a `RequestDelivery`, `reply(value)` checks the value against the reply contract
  strictly and raises the problems in the agent's code, `fail(message)` ends it. Each terminal
  outcome -- `replied` with the value as JSON, `failed` with the message, `cancelled` with who
  cancelled, `closed`, and `suspended` once `0003-65` adds it -- is told to the backend once,
  through a new trait method `settled(channel, request, outcome)` carrying the owner's item id,
  OutRig's id and the reference; that is the one path by which the requester learns the outcome, and
  no owner-side awaitable is built beside it (fork 5). A reply after the request ended raises
  `RequestSettled` in the agent's code, is evented `agent.request.refused`, and is reported to the
  backend through `settled` as a refusal that changes no outcome (OR-A7-5).
- **The owner cancels an inbound request.** `cancel(request)` on the channel handle: a request the
  holder has not received is withdrawn from its queue, as `_Work.cancel` withdraws, and the holder
  never sees it; one it has received stays with it, and `reply` or `fail` then raises
  `RequestCancelled` (fork 2). The holder's execution is never interrupted for it. Each such end
  reaches `settled` as `cancelled` by the owner.
- **The liveness rule covers the holder's inbound channels.** With the records in the children
  registry, the runner's rounds open on an external request as on any request; the nudge and the
  warning name it by id and channel; the host's cancel after the warning ends it `cancelled`, with
  `0003-37`'s cause on `agent.request.cancelled`, and `settled` receives `cancelled` by the host as
  the request's one terminal outcome (OR-A7-5), so the requester is told. The rule's text is
  `0003-37`'s and this task changes none of it. A plain external channel is never warned about or
  cancelled, as the `user` channel is not (OR-A7-4). The task itself is not a request: a holder
  with no open request reads idle, and nothing here opens a round for it.
- **Outbound: the agent requests.** An outbound request endpoint with `request(body) -> handle` and
  `pending()`, the count of requests the backend has not accepted. `request` checks the body against
  the request contract, hands it to the backend through `send` as `0003-51` built it, and returns
  `work.md`'s handle at once; its status reads `sending` until the backend accepts, then `waiting`.
  The channel handle gains `settle(request, reply | fail(message) | cancel)`: a reply is decoded
  strictly into the reply contract on the agent's side, as a parent decodes a child's, and settles
  the handle; one that does not fit ends the request `failed` naming the problems, told to the
  backend (fork 3); `fail` settles `AgentRequestFailed`; `cancel` settles `RequestCancelled`.
  `h.cancel()` tells the backend through a new trait method `withdrawn(channel, request, end)` and
  settles the handle on its verdict -- cancelled, or replied or failed with what the backend had
  already recorded (OR-A7-5); a verdict that does not arrive within the bound leaves the handle and
  the request `uncertain` in the registry until `0003-53`'s `resolve(channel, request, withdrawal)`
  answers withdrawn, replied or failed, and never moves it on the send query's answer; a `settle`
  after any end is refused, the refusal is the owner's answer, and `channel.request.refused` is
  recorded. A `runtime.wait` that times out with the handle pending settles nothing, and the request
  stays outstanding until the backend settles it, the agent cancels it, the holder is released or
  the session closes.
- **Peer termination reaches the sender once.** `close(reason)` on the channel handle, as
  `0003-51` built it, ends every request outstanding on the channel `cancelled` with the closure as
  its cause: an outbound handle raises `RequestCancelled` naming it, an inbound request is
  withdrawn or its `reply` raises; each end reaches `settled` once; `runtime.wait` reports the
  failure once, and a later `send` or `request` raises (OR-A7-5).
- **The primary.** The application's primary is runner-driven, so the rule above covers it and
  nothing more is built for it. A primary the harness builds and the CLI drives through
  `Session::round` -- `run-new`'s -- is not runner-driven and declares no external channel today;
  an inbound request channel on such a primary, declared by a Rust embedder that drives
  `Session::round`, is announced with "requests" as the noun and listed, and that owner is its
  policy; the warning-then-cancel does not run there, which `external-channels.md` and the
  channel's rustdoc say (fork 4).
- **Events**, in `doc/reference/events.md`: inbound external requests under `agent.request.*`
  with `sender`; `channel.request.sent`, `channel.request.settled` and `channel.request.refused`
  for outbound requests, under the requesting agent's subject; `channel.settled`, when the
  backend acknowledged a settlement notice. Bodies as bounded previews, as the request family's.
- **The close**, in `lifecycle.md`'s table, tested here: an inbound request waiting unread or
  received and unanswered ends `closed`, `settled` receives `closed`, and the report lists it; an
  outbound request outstanding ends with `SessionClosing` at the handle, `withdrawn` receives
  `released`, and the report lists it; a settlement notice the backend has not acknowledged is
  retried through the drain and listed as unacknowledged if it still is. These ends are what
  `0003-65` records before the close settles them, and this task gives them their notices.
- `crates/outrig/CHANGELOG.md`'s `[Unreleased]` paragraph; rustdoc on every new public item;
  `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **An inbound request is replied to.** The owner's `deliver` of a request `"2+2?"` with item id
  `"q-1"` and sender `"bob"` on `questions` answers with a delivery id; the application primary's
  mock receives `d.body == "2+2?"` and `d.sender == "bob"` and replies `"4"`; the backend's
  `settled` is called once, with `questions`, `"q-1"`, the delivery id and `replied("4")`; the
  record shows `agent.request.sent` with `sender` `bob`, `received`, `replied` and `settled` with
  `accepted`.
- **`fail` and the late reply.** `await d.fail("no")` makes `settled` receive `failed("no")`; a
  second `reply` raises `RequestSettled`, `agent.request.refused` is recorded, and `settled`
  receives a refusal that names the request's end and changes nothing. A typed reply contract: a
  reply that does not fit raises the problems in the agent's code with their paths and settles
  nothing; a third invalid reply ends the request `CompletionRejected`, told to the backend as
  `failed` with the problems.
- **The owner cancels.** `cancel` before receipt: `pending()` reads 0, the mock never sees the
  body, `agent.request.cancelled` says the owner cancelled, `settled` receives `cancelled` by the
  owner. `cancel` after receipt: the mock's `reply` raises `RequestCancelled`.
- **The liveness rule reaches it, and the backend hears one outcome.** A mock primary that takes
  the request and ends every round with text is called for the round its arrival opened, the
  round naming it by id and channel, and the warning round, and for nothing after; the request
  ends `cancelled`; `agent.request.cancelled` names the host and its cause; `settled` is called
  exactly once for the request, with `cancelled` by the host; the primary reads idle. A request
  never taken is cancelled after two rounds. A reply in the warning round is answered, and
  `settled` receives `replied`. `0003-37`'s own tests pass unchanged.
- **A plain item is exempt.** The same primary with an `inbox` item left unread through three
  rounds: no warning names it, nothing cancels it, and it is still unread.
- **The task is not a request.** A primary with no request open reads idle; no round opens; the
  owner's roster read (`rust-owner.md`) shows idle and nothing else happens until a push.
- **An outbound request.** `h = runtime.channels["asks"].request("may I?")` calls the backend's
  `send` and returns at once with `h.status.state == "sending"`, then `"waiting"` once accepted;
  `channel.request.sent` is recorded; the owner's `settle(h.id, reply="yes")` makes
  `await h == "yes"`, and `runtime.wait({h})` returns it in `done`; `fail` makes `await h` raise
  `AgentRequestFailed`; `cancel` makes it raise `RequestCancelled`; a typed reply contract with a
  reply that does not fit makes it raise `AgentRequestFailed` naming `$.field`, and `settled`
  tells the backend `failed`.
- **One wait over a handle and a channel.** `runtime.wait({h, child_handle})` returns with
  whichever settles first in `done`; an arrival on `inbox` meanwhile raises `MessageAvailable` with
  both pending.
- **`h.cancel()`.** The backend's `withdrawn` is called with the id and `cancelled`; when it answers
  withdrawn, the handle reads cancelled, a later `settle` is answered refused and
  `channel.request.refused` is recorded; when the test backend answers with a reply it had already
  recorded, the handle settles with that reply and `channel.request.settled` says `replied`; when it
  never answers, the handle reads `uncertain` past the bound and the registry lists the request so;
  a test backend whose `resolve(.., withdrawal)` then answers `replied` with a value settles the
  handle with that reply, and one answering `accepted` to a send query for the same id changes
  nothing. A `runtime.wait` with `timeout=0.1` returns an uncancelled handle in `pending`; the
  request is still outstanding, the backend has not been told anything, and a later `settle` settles
  it.
- **Peer termination.** With one outbound request outstanding and one inbound request received,
  `close("peer gone")`: `await h` raises `RequestCancelled` naming the closure; the inbound
  request's `reply` raises `RequestCancelled`; `settled` receives `cancelled` once for each;
  `runtime.wait` reports the failure once and not again; a later `request` raises.
- **The bound.** `bound = 2` on an inbound channel: with one request unread and one received and
  unanswered, a third `deliver` is refused `full`; after a reply it is accepted.
- **The `run-new` primary.** A session with a harness-built primary and an inbound request
  channel declared by the embedder: `Session::round` opens with `1 request is waiting on
  runtime.channels["questions"]`; rounds that leave it open cancel nothing; it is listed open until
  answered or the close.
- **The close rows.** Shutdown with one inbound request unread, one received and unanswered and
  one outbound request outstanding: the two inbound end `closed` and `settled` receives `closed`
  for each; the outbound's handle raises `SessionClosing` and `withdrawn` receives `released`;
  the report lists all three with their ends; a `settled` the test backend never acknowledges is
  listed as unacknowledged.
- `Registry::idle`'s assertion that an idle child holds nothing open holds with external
  requests, in every test above.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`, `cargo clippy
  --all-targets`, `cargo fmt --check` pass; `doc/reference/events.md` and `lifecycle.md`'s close
  table carry what this task adds, checked by reading.

## Design forks

1. **Where an inbound external request is recorded -- Recommended: in the children registry, as
   a request of the holder, with an id the interpreter mints as it queues the request, reported
   to the owner in the push's answer.** The rule, its texts, the request events, the report and
   `0003-37`'s cancel then apply with no change to any of them. The alternative keeps external
   requests in the channel registry under the host's `MessageId` and unions them into the
   `Summary` as sets of their own, which keeps one id per item at the cost of a second record the
   rule must read.
2. **An owner's cancel of a request the holder has received -- Recommended: as `h.cancel()` does
   it**: the delivery stays, and `reply` or `fail` raises `RequestCancelled`. The alternative
   interrupts the holder's execution, which `agent-classes.md` rejects for a request: "Cancelling a
   request never interrupts the child's execution".
3. **A backend's reply that does not fit the reply contract -- Recommended: the request ends
   `failed`, naming the problems, and the backend is told.** A repair loop is for a model, and a
   backend is code. The alternative refuses the settlement and keeps the request open for a
   corrected reply, which is `0003-30`'s repair applied to a party that has no round to correct
   in.
4. **An inbound request channel on a harness-built primary driven through `Session::round` --
   Recommended: announced and listed, with that owner as the policy**, since its rounds are the
   owner's and the warning-then-cancel is the runner's; the application's primary is runner-driven
   and covered. The alternative builds the rule into `Session::round`, which would open rounds the
   owner did not ask for, for a holder no application has today.
5. **An owner-side awaitable for an inbound request -- Recommended: none.** The backend's
   `settled` is the one path by which the requester's side learns the outcome, and a durable
   record that agrees with the handle is what OR-CH2 asks for. The alternative returns a future
   from the push beside `settled`, two readers of one outcome; `0003-35`'s host handle is the shape
   it would take if Python-host parity is ever built.

## Dependencies

- **Hard: `0003-51`.** The definition, the backend trait, the channel handle, the host end, the
  holder and the send outcomes.
- **Hard: `0003-37`.** The warning round and the host's cancel this task's inbound requests fall
  under, and `cancelled` in `_HOST_ENDS`.
- **Soft: `0003-47`.** The manifest's request sentence as one contribution.
- **Soft: `0003-35`.** Only if Python-host parity is built: the host handle its inbound requests
  would take, and the reply decoded in the program.

## See also

- `plan/phase/0003-python/external-channels.md` -- "Requests", "Positions, the registry and
  the close".
- `plan/phase/0003-python/rust-owner.md` -- the application's primary and its runner.
- `plan/phase/0003-python/work.md` -- "The handle", "A request settles once", "Limits belong
  outside generated code".
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds", "From the host program",
  "Explicit plumbing".
- `plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md` -- the rule this task's requests fall
  under.
- `plan/phase/0003-python/lifecycle.md` -- "Closing a session", "The close table".
- `plan/phase/0003-python/continuation.md` -- what `0003-65` records before the close
  settles, which these ends feed.
- `requirements/outrig/A-07-owner-side-channel-backend.md` -- OR-A7-5;
  `A-02-rust-owner-construction.md` -- OR-A2-2.
- `crates/outrig/src/agent/children.rs` (`Record`, `Summary`, `summary`, `settle`, `report`),
  `runner.rs`, `orientation.rs`; `crates/outrig/src/python/interpreter.py` (`RequestEndpoint`,
  `_Work`, `Handle`, `ChildChannel`, `_HOST_ENDS`); `crates/outrig/src/harness/event.rs`
  (`AgentRequest*`, `RequestEnd`).
