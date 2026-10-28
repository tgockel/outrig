# 0003-56 -- An MCP call has three dispatch states

## Context

After `0003-55` an MCP call is a service call (`services.md`): admitted, checked, dispatched with
a `CallContext`, bounded by the service policy's deadline, cancelled through the token at the
deadline, on an interrupt and at the close, and ended in one of a service call's five outcomes.
What the mechanism cannot say of an MCP call is where in its flow a failure happened, which is the
agent's question: did the server see the request. Today's `call_tool`
(`crates/outrig/src/mcp.rs:403`) folds a failed send and a failed wait into one
`McpCallToolFailed` (`crates/outrig/src/error.rs:242`) whose `McpFailureKind` says what failed
and not where; a variant a later rmcp adds classifies as `Other` (`session_error_from_rmcp`,
`mcp.rs:817`) and a reading from the kind has to guess. The client's deadline is per client
(`with_call_timeout`, `:306`), and past it the client sends `notifications/cancelled` through
`CancelOnDrop` (`:513`), as it does for a dropped call; a transport failure finishes the client's
session, and nothing reconnects (`McpFailureKind::Transport`'s doc, `error.rs:363`).
`doc/concepts/mcp-servers.md` ("What if a server crashes mid-session?") records the legacy answer:
"the server is gone for the rest of the session". `McpClient` is public and not among the items
A-08 freezes; its signatures stay as they are regardless, and this task adds beside them.

The requirement is OR-MCP3 of `02-channels-and-mcp.md`: calls classified as known not dispatched,
may have dispatched, or response received; a failed or partial send, timeout or cancellation after
a possible send cannot prove non-execution; a server-reported error is a response and may include
partial effects; useful error categories, deadlines and best-effort cancellation; no generic
replay, no safe-retry inference from names or annotations, closing a transport not equated with
stopping the server; reconnect and relist for future calls. The validation's constraint 5 bears: a
running hosted call cannot be stopped, and the trait entry's `CallContext` cancellation is "what
OR-MCP3's best-effort cancel needs" -- now `services.md`'s token, which this task's `call` observes.
The design is `plan/phase/0003-python/mcp-presenter.md` ("Calls", "Admission, the close and
the report", "Events"). `execution-and-rounds.md` ("No rollback, ever": a lost result "must never
be retried automatically"; "Four different cancellations") and `lifecycle.md` ("The report": a
local stop says nothing about the remote outcome) are the rules this task applies to a new
resource, not decisions it makes. `0003-65` reads this task's record of uncertain calls when it
gives open work a disposition at the cut.

## Goal

Every MCP call ends in one of three dispatch states decided by where in the call's flow it ended,
never guessed, mapped onto the service call's outcome so one reader covers every service; the
cancel the token asks for reaches the server as a notice that promises no stop; a service whose
transport failed is re-established for future calls with nothing replayed; and the report says of
each uncertain call what is known.

## Deliverables

- **State by position.** A bounded form of the call added beside `call_tool` -- `call_tool_within
  (name, args, deadline, token)`, spelling the task's -- returns, on failure, a type carrying the
  dispatch state decided where it happened: the argument check fails before anything, known not
  dispatched; `send_request_with_option` fails, may have dispatched -- a failed or partial write
  proves nothing; the wait fails, times out or is cancelled by the token, may have dispatched; a
  response arrives -- a tool result, a JSON-RPC error, a response of another type -- response
  received. `McpFailureKind` stays beside the state as `failure`. `call_tool` itself is unchanged
  and calls the bounded form with its client-wide deadline and no token.
- **The mapping onto outcomes** (`mcp-presenter.md`, "Calls"): `McpService::call` returns `Ok`
  with the result's JSON for response received with a result, `is_error` either way; `Err(
  ServiceError)` with `InvalidArguments`, `NotFound` or `Failed` for a JSON-RPC error or a response
  of the wrong type, state response received; and, for may have dispatched, an error the table
  records as `unknown` with `failure` beside it -- a `ServiceError` category `Unknown` added for
  it, or the table's own handling of a service that reports an uncertain outcome (fork 1). The
  table's refusals are known not dispatched by construction. A kind a later rmcp adds reaches
  Python with its state intact.
- **Exceptions.** `outrig.McpError`, a subclass of `outrig.ServiceError`, with `state`, `failure`,
  `service`, `tool` and `call`; the three state subclasses `NotDispatched`, `MayHaveDispatched`,
  `ResponseReceived`; under them `UnknownTool`, `InvalidArguments`, `ServiceUnavailable`,
  `AdmissionClosed`, `Paused` (not dispatched), `ToolError` (`0003-55`) and `ProtocolError`
  (response received); the mechanism's result-too-large error read as response received. Filled by
  the `mcp` presenter from the answer line's fields.
- **The cancel notice.** `McpService::call` observes `cx`'s token: when it fires -- at the
  deadline, on an interrupt, at the close, at a `Stop` quiesce -- the client sends the server
  `notifications/cancelled` and the call returns at once with may have dispatched and the failure
  `Timeout` or `Canceled`, so the call never reaches the mechanism's grace or abort; the server's
  late answer is discarded by the client. The orientation's paragraph says a timeout stops the
  wait, not the server. A per-service deadline is the declaration's `call_timeout_secs` through
  the description's `call_deadline` (`0003-54`); `timeout=` on a call lowers it as `services.md`
  says, and raising it is the mechanism's `ValueError`.
- **Reconnect for future calls.** A failure of kind transport marks the service disconnected in
  the MCP table, with an event. The next call on it, or `reconnect()` on the presenter, re-
  establishes it through its `Placement` (fork 2): a new exec in the primary, a new exec in a
  running sidecar or a new sidecar when its container is gone, a new entrypoint container, with the
  values resolved at start and never resolved again, within the connect budget and the startup
  classification, then relisted through `service.described`; only then is the new call dispatched.
  The earlier call is not sent again. `McpPolicy::reconnect_max` bounds reconnects per service
  (fork 4); past it the service is unavailable and a call is `refused`, known not dispatched. After
  `close_admission()` no reconnect is made. A server the old transport spoke to may still run until
  teardown, and `writers()` says so.
- **Uncertain calls.** The MCP table keeps the calls that ended may have dispatched during the
  session, by call, service, tool and agent, as `uncertain()` for the cut (`0003-65`), and
  `ShutdownReport` gains `mcp_uncertain` listing them (fork 3); the mechanism's `services` already
  lists a call in flight at the close, which for an MCP call ends `unknown` with `state:
  may_have_dispatched` and the notice sent. A call in either with that state is an unknown outcome
  for the verdict (`crates/outrig/src/harness/report.rs:41`), stopped with unknown and never
  failed. The Python `ShutdownReport` mirrors it. `lifecycle.md`'s close table gains the **MCP
  calls** addendum rows the page lists: a call waiting for a reconnect at the close is `refused`,
  reason closed; a call in flight ends `unknown`, may have dispatched, the notice sent.
- **Events**, integration audit, in `doc/reference/events.md`: `state` and `failure` on
  `service.call.ended` from the Rust state for every ending; `mcp.service.disconnected` and
  `mcp.service.reconnected` with `attempts` and `listing`, followed by `service.described`. The
  reference says that non-dispatch is read from `outcome` and `state`, never from a missing
  `service.call.dispatched`.
- **A-08.** `McpClient::call_tool`, `with_call_timeout` and the rest of its surface are unchanged;
  the bounded form is added beside them; `McpFailureKind` and `McpSessionError` gain nothing or
  gain additively on their `#[non_exhaustive]` items; `Outrig::call_tool` is untouched.
- **Docs.** `doc/concepts/mcp-servers.md`: the dispatch states, the cancel's advisory nature and
  the reconnect (the "What if a server hangs?" and "crashes mid-session" sections gain the
  Session's answer beside the legacy one); `doc/reference/config.md`'s `mcp-call-timeout-secs`
  text says the notice is advisory where it now says the call "is cancelled at the server";
  `crates/outrig/CHANGELOG.md`.
- **`## Decisions` records** the forks, the bounded call's shape, how an uncertain outcome crosses
  the `Service` trait, and the reconnect's placement by kind.

## Acceptance

Mock-driven: the fake server and fake placement of `0003-54` and `0003-55`, with the fake able to
close its transport on receipt, stall, answer slowly, or answer with an error.

- **May have dispatched, never replayed.** A `write` tool whose fake closes the transport on
  receiving the request: Python gets `MayHaveDispatched` with `failure == "transport"`; the fake
  recorded one request; `service.call.ended` says `outcome: unknown`, `state:
  may_have_dispatched`; `uncertain()` lists the call; through `shutdown` the report's
  `mcp_uncertain` lists it and the verdict is stopped with unknown; no second request ever reaches
  the fake.
- **Known not dispatched is distinct.** `svc.call("missing", {})` raises `NotDispatched`
  (`UnknownTool`); after `close_admission()`, a call raises `AdmissionClosed`; during a pause, it
  raises `Paused`; the fake sees nothing in each case, and `service.call.ended` says `refused`,
  `state: not_dispatched`, with no `service.call.dispatched` before it.
- **A response is a response.** A tool whose fake answers a JSON-RPC error raises
  `ResponseReceived` (`ProtocolError`) with the server's message and the event says `raised`; one
  answering `isError` raises `ToolError` and the event says `returned`; both record
  `response_received`.
- **The notice on the token.** A tool that stalls, called with `timeout=1`, raises
  `MayHaveDispatched` with `failure == "timeout"` after about a second and before the mechanism's
  grace; the fake receives `notifications/cancelled` naming the request, as `mcp.rs`'s
  `next_cancel` checks; `service.call.cancel.sent` says `deadline`; the fake's late answer is
  discarded and no event records a second ending. A per-service `call_timeout_secs` bounds that
  service alone.
- **An interrupt cancels.** An interrupt of the primary during a call raises in agent code, the
  fake receives the notice, `service.call.cancel.sent` says `interrupted`, the call ends `unknown`
  with `failure == "canceled"`, and the kernel's loop is free afterward. A call in a `to_thread`
  worker is not woken and ends at its deadline.
- **Reconnect.** After the fake closes its transport, the next call reconnects (the fake placement
  counts two connects), relists -- with `mcp.service.reconnected` then `service.described` naming
  `added` when the fake's second listing differs -- and dispatches the new call once; the earlier
  call is in `uncertain()` and is never re-sent. `svc.reconnect()` does the same without a call.
  With `reconnect_max` 1, a second disconnection leaves the service unavailable and a call
  `ServiceUnavailable`. After `close_admission()` a disconnected service is not reconnected: the
  connect count stays.
- **Values are not resolved again.** A counting `Secrets` resolved each `${VAR}` once at start and
  is not called by the reconnect.
- **The close.** `shutdown(deadline)` with a call in flight whose fake answers within the deadline
  records `response_received`; with a fake that never answers, the notice is sent at terminating,
  the call ends `unknown`, `state: may_have_dispatched`, reason `closed`, `services` lists it, and
  the verdict is stopped with unknown. A call made during the drain is refused `AdmissionClosed`.
- **A `Stop` quiesce.** `quiesce(Stop)` with a call in flight sends the fake the notice, the call
  ends `unknown` with reason `stopped`, and `uncertain()` lists it.
- **State by position holds for an unknown kind.** A failure the classifier files as `Other`,
  injected after the send, reaches Python as `MayHaveDispatched` with `failure == "other"`.
- Gated e2e, as `tests/mcp_handshake.rs` is: the `mcp-fs` fixture as a sidecar, its container
  killed with `podman kill` during a call, gives may have dispatched; the next call reconnects
  through a new container and lists the same tools.
- `crates/outrig/public-api.txt` regenerated, `McpClient`'s existing lines unchanged; `cargo test
  --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass; `lifecycle.md`'s rows,
  `events.md`'s entries and the docs say what the code does, checked by reading.

## Design forks

1. **How an uncertain outcome crosses the `Service` trait -- Open; recommended: a `ServiceError`
   category `Unknown` the table records as the outcome `unknown`, with the service's detail as
   `failure`.** It keeps the trait's `Err` as the one path for a failure and lets the table apply
   its reader's rule. The alternative is a third variant of the call's return beside `Ok` and
   `Err`, which every other service would carry for a state only MCP has.
2. **When a reconnect runs -- Recommended: on the next call, and on `reconnect()`.** A background
   reconnect would spend the budget while nobody calls and race the cut's pause. The alternative
   reconnects as soon as the disconnection is seen.
3. **Uncertain calls in the report -- Recommended: listed, as `mcp_uncertain`**, beside the
   mechanism's close-time `services`. The hosted report lists only outcomes after the close and
   leaves mid-session unknowns to events; here the requirement says a write "remains uncertain
   through shutdown/resume", and `0003-65` reads the list, so the report carries it. The
   alternative keeps the report to the close and gives the cut `uncertain()` alone.
4. **The reconnect bound's default -- Recommended: 3 per service per session.** Past it a server
   that keeps dropping is reported, not chased. The alternative is unbounded within the budget each
   time.

`timeout=` above the ceiling is settled by `services.md` (`0003-44`'s fork 6) and is not a fork
here.

## Dependencies

- **Hard: `0003-44`.** The `CallContext` token, the deadline, the outcomes, `service.call.*`
  and the report's `services`.
- **Hard: `0003-54`.** `McpService`, the MCP table, the `Placement` trait, the policy, the quiesce
  control and the service rows.
- **Hard: `0003-55`.** The `mcp` presenter kind that fills `McpError` from the answer line.
- **Hard: `0003-22`.** The relay's drain and report pattern.

## See also

- `plan/phase/0003-python/mcp-presenter.md` -- "Calls", "Admission, the close and the
  report", "Events".
- `plan/phase/0003-python/services.md` -- "Dispatch, deadline and cancel".
- `plan/phase/0003-python/execution-and-rounds.md` -- "No rollback, ever", "Four different
  cancellations".
- `plan/phase/0003-python/lifecycle.md` -- "Closing a session", "The close table", "The report".
- `plan/phase/0003-python/continuation.md` -- the disposition of open work (`0003-65`).
- `crates/outrig/src/mcp.rs`, `error.rs`, `harness/session.rs`, `harness/report.rs`.
