# 0003-60 -- A session assesses the cost of sleeping

## Context

The facts an owner would want before deciding to put a session to sleep exist on the host and are
reported nowhere together; `0003-49` gathers the agent-level ones into the roster snapshot
(OR-A6-3), and this task builds on it. The children's registry knows each child's life --
launching, running, idle, wedged, released (`crates/outrig/src/agent/children.rs:127-134`) -- and
the requests open on it, unread and taken (`:1012-1037`, `summary`; `:1100-1108`, `open`). A
round's ending names the tasks its code left running (`crates/outrig/src/harness/session.rs:
605-631`, `StillRunning`), and the interpreter answers `tasks` and `pending` on request
(`crates/outrig/src/python/host.rs:1147-1156`). The relay knows every hosted call it was sent and
has not seen end, which is what the shutdown's drain waits on
(`crates/outrig/src/python/hosted.rs:509-511`). The liveness probe
asks two questions, one answered on the loop and one on the reader thread, so a loop that is not
turning is told from one that is blocked (`runtime-protection.md`, "What the port does";
`crates/outrig/src/python/interpreter.py:5887-5899`, `cpu`). Host-opened rounds exist: the
runner opens one with a fixed opening through `Agent::opened` (`crates/outrig/src/agent/
runner.rs:166-170`), for the orienting round (`crates/outrig/src/agent/orientation.rs:358-362`,
`ORIENTING`) and, after `0003-37`, for the warning round. `outrig.Agent` has no hook an
application could supply (`crates/outrig/src/python/outrig/_agents.py:490-570`). The stream's
sequence is one gap-free number per event (`crates/outrig/src/events.rs:203-237`).

OR-SUS1 asks for "runtime assessment and optional LLM consultation about interruption/restoration
cost", advisory and "distinct from verified blockers, preparation and proof of capture", reporting
"freshness, uncertainty and unavailable/timed-out assessment instead of requiring universal
responsiveness from a blocked kernel"; cheap repeated assessment "must not notify the model to
prepare, mutate work or secretly save at each scheduling poll"; and "optional application context
is supported without requiring `CococlawAgent.assess_sleep` or another custom method list." The
planning's own correction (`decisions.md`, the task-bound-agent correction) says the same: generic
assessment belongs to OutRig's base agent and `Session`.

The validation found that the runtime facts exist and the probe pattern gives "unavailable or
timed out" honestly; that a host-opened assessment round would, under `0003-37`'s rule, be a round
that ends with the request summary unchanged and so a step toward cancelling every request the
agent holds (the conflict check's tension 2); and that `0003-37` already exempts a round that
ended in an error, which is the precedent for exempting this one. Constraint 12 holds: the task
itself is not a request, and an agent holding none is idle, which is a fact the owner reads.
`continuation.md`, "Assessment", designs what this task builds, and the exemption is built here
first, since this task's round is the first host-opened round that is not about requests.

## Goal

An owner reads runtime facts about a session with a freshness stamp as often as it likes, and may
pay for the model's advice, without anything being prepared, told or saved.

## Deliverables

- **`Session::assess(options)`** (the spelling is fork 1) returning an `Assessment`, a public
  `#[non_exhaustive]` value built on the roster snapshot `0003-49` serves (OR-A6-3) rather than
  assembled here: for each agent its id, kind, parent, depth, state -- launching, running, idle,
  retrying, limit-reached, releasing, released -- its last stop, the model in use and its
  outstanding requests, read under the runner's lock as that task reads them. The assessment adds
  what the roster does not carry: the executions holding a slot and the tasks on each loop; the
  hosted calls in flight, the MCP calls in flight as `mcp-presenter.md` records them, and the
  live-object calls running; each channel's positions -- delivered and unread counts on the
  primary's `user` channel and on each external channel, and the ids unread and taken on each
  local child's channels; the known writers -- the bindings process and what else is in its
  group, each MCP placement and whether it can be paused, each live object; the standing
  designation and when it was made, once `0003-61` exists; the last successful save and the time
  since, once `0003-63` exists; and the stamp: the stream sequence at which the facts were read
  and the wall time. Every fact is `Some(value)`, `Unavailable { why }` or `TimedOut`, never a
  guess, and the operation returns within the deadline the options give.
- **Model consultation**, with `consult`: one host-opened round on the primary, or on each root
  agent of a session with none (fork 5), with a fixed opening that asks how costly an interruption
  and a restoration would be now, what the agent would have to prepare, and with what confidence,
  and says that nothing is being prepared and no save follows. The reply text is returned as
  advice beside the facts; an agent that does not answer within the deadline is `Unavailable`. The
  round is recorded as any round is and costs one model call. Draft, for the task to tune:

  > [outrig] the session's owner asks how costly it would be to interrupt you now and restore you
  > later in a new interpreter: what would you have to save or prepare first, how long would
  > getting back to this point take, and how sure are you? Answer in your reply. This is a
  > question only: nothing is being saved, nothing will restart because of your answer, and your
  > work is not affected.

- **The exemption from `0003-37`'s count.** A host-opened round that is not about the request
  summary -- this consultation, and `0003-61`'s designation round -- neither gives nor uses the
  warning and moves no baseline, as `0003-37` has a round that ended in an error do: the runner's
  comparison at `runner.rs:100-126` and its baseline update at `:187-196` skip such a round, and
  the round counts no attempt against any request. Recorded in `## Decisions`, and
  `agent-classes.md`, "The child's rounds", gains one sentence (the integrator's edit).
- **An application hook on the base class.** `outrig.Agent` gains one optional method with a
  default of none (fork 3), which a subclass may override to return data in the serializable
  subset; with `hook` in the options, the assessment runs it on the agent's loop under a bound and
  returns what it returned as `application` beside the facts, or `Unavailable` when the loop does
  not answer or the method raises, with the exception's type. No subclass has to implement it.
  Its name is reserved on every subclass as every member of `outrig.Agent` is, and the orientation
  says so where it lists the reserved names. This is interpreter code, which `0003-39` makes room
  for.
- **The Python owner.** `await session.assess(...)` on `outrig.Session`, a request on the owner
  connection (`crates/outrig/src/owner/mod.rs:12-28`) answered with the assessment as JSON and
  handed to the program as a Python object.
- **Events**: `continuation.assessed`, execution diagnostics, with the stamp, whether the model
  was consulted and whether the hook ran, added to `doc/reference/events.md`; the consultation
  round's own events are the round's.
- **`lifecycle.md`'s close table** gains a row: an assessment in progress at the close returns
  with every fact not yet read `Unavailable`, the close as the reason; a consultation round
  running at the close runs on as any round does, its advice discarded.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **Repeated assessment changes nothing.** An agent waiting in `runtime.wait` on a task that never
  finishes is assessed ten times: each returns the task's name among the loop's tasks and the
  stamp, no `exec.submitted` is recorded, no turn commits, no file in the workspace changes, and
  the mock provider is never called.
- **Facts are what the host knows.** With one child idle holding a taken request, one retrying
  after a failed round, a hosted call in flight on a slow fixture binding, and two user messages
  unread: the assessment's roster rows are `0003-49`'s for the same session -- the idle child's
  outstanding request count 1, the retrying child's state with its next attempt -- and the
  assessment adds the request id as taken, the call by binding and member, and the user channel's
  unread count as 2; the stamp equals the stream's last published id at the read.
- **A blocked kernel is unavailable, not guessed.** With the primary's loop spinning in Python,
  the loop's tasks are `TimedOut` within the deadline, the registry's facts are present, and the
  stamp is present.
- **Consultation is one round.** With `consult`, the mock provider is called once, its reply text
  is the advice, one turn commits, and the round's opening is the draft above; a mock that does not
  answer within the deadline leaves the advice `Unavailable` and the round runs on.
- **The round is exempt.** A request child that took a request and ended a round without
  answering it -- so that `0003-37`'s warning is next -- is consulted: the consultation round
  opens, ends, and is followed by the warning round and then the cancel exactly as it would have
  been without the consultation, with no `agent.request.cancelled` caused by the consultation and
  no attempt counted.
- **The hook.** A subclass whose hook returns `{"index": "stale"}` has it in `application`; one
  whose hook sleeps past the bound is `Unavailable`, `TimedOut`; one whose hook raises is
  `Unavailable` naming the exception's type; a subclass without one yields `application: None`;
  the method's name as a request channel is refused at class creation, naming it.
- **The Python owner** reads the same assessment a Rust test does for the same session state.
- **At the close**, an assessment started as `shutdown` runs returns with the unread facts
  `Unavailable`, the close as the reason, within the test's deadline.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the operation lives -- Recommended: `Session::assess(options) -> Assessment`**, with
   the Python owner's `session.assess(...)` over the connection. The alternative, a builder
   subscription that pushes facts on every change, is the latest-state snapshot
   `potential/live-state-snapshot.md` evaluates and is not adopted here.
2. **The consultation's shape -- Recommended: a host-opened round on the agent itself**, which
   knows its own state and whose answer is in its history. The alternative, a fresh typed child
   asked about the parent's state, has none of that context and costs the same model call.
3. **The hook's name -- Open; recommended: `assess`.** Whatever the name, it is reserved on every
   subclass (`agent-classes.md`, "The declaration") and the orientation lists it. A longer name
   collides less and reads worse; the task chooses and records it.
4. **The stamp -- Recommended: the stream sequence and the wall time**, since a reader with the
   stream can line the facts up with the events after them. The alternative, time alone, cannot.
5. **Which agents a consultation reaches -- Recommended: the primary, or each root agent of a
   session with none**; a local child is not consulted, since `0003-65` ends or refuses it at a
   cut. The alternative consults every agent and spends a model call per child.

## Dependencies

- **Hard: `0003-39`.** The hook is interpreter code, and the program is near its argument cap.
- **Hard: `0003-49`.** The roster snapshot the assessment's facts are built on, read under the
  runner's lock.
- **Hard: `0003-19`, `0003-29`.** The session API and the Python owner the operation joins.
- **Hard: `0003-30`, `0003-34`.** The children's registry and the agent class the hook sits on.
- **Hard: `0003-37`.** The warning and cancel the consultation round is exempt from.
- **Soft: `0003-41`.** The metaclass construction the base class gains; the hook sits beside it.
- **Soft: `0003-53`, `0003-54`.** External channel positions and MCP calls in flight as facts;
  without them the fields are `Unavailable` with the reason that the facility does not exist.
- **Soft: `0003-46`.** The owner-constructed root a consultation reaches as a root agent.

## See also

- `plan/phase/0003-python/continuation.md` -- "Assessment", "Where the queued 0003 tasks
  interact", "Constraints carried from the validation".
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds", where the exemption attaches.
- `plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md` -- the count the round is exempt
  from, and the error-round precedent.
- `plan/phase/0003-python/runtime-protection.md` -- "What the port does", the probe.
- `plan/phase/0003-python/potential/live-state-snapshot.md` -- the same facts, stream-sequenced,
  for a monitor; not adopted here.
- `plan/next/a-round-names-the-hosted-calls-it-left-running.md` -- the relay's record of calls in
  flight, which the facts read.
- `crates/outrig/src/agent/runner.rs`, `children.rs`, `orientation.rs`; `crates/outrig/src/python/
  host.rs`, `hosted.rs`, `interpreter.py`, `outrig/_agents.py`, `outrig/_host.py`.
