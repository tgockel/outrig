# 0003-50 -- An agent has a model-request limit

## Context

Every bound on a `run-new` session's model calls is session-wide or per round. `token-budget` is
a latch on reported spend: the ledger marks it spent as the attempts recorded reach it
(`crates/outrig/src/agent/ledger.rs:222-232`, `count`; `:204-220`, `budget_spent` and
`budget_watch`); the round's hook stops the loop before the next call once it is
(`crates/outrig/src/agent/round.rs:616-620`), one arm above the tool-call cap (`:622-624`); each
retry loop refuses a request once it holds its place (`crates/outrig/src/agent/retry.rs:911-916`,
`:626-628`, `BudgetSpent`); a child's runner, seeing `spent_budget()`, settles every open request
`budget` and marks the child idle (`crates/outrig/src/agent/runner.rs:77-98`;
`crates/outrig/src/agent/children.rs:1196-1200`, `settle`; `:1228-1230`); the sender's `await`
raises `BudgetExhausted` (`crates/outrig/src/python/interpreter.py:6437-6452`, `_HOST_ENDS` and
`_settle_work`), and an instance's `ready()` raises it through `oriented`
(`children.rs:1205-1221`; `interpreter.py:6455-6461`;
`crates/outrig/src/python/outrig/_agents.py:544-553`). A spent budget is final for the session.
`subagent-depth-max`, `children-max` and `requests-max` refuse at the call with
`AgentLimitReached`, whose `Status.reason` is `limit` (`children.rs:432-451`;
`interpreter.py:4419-4426`, `:4180`; `_agents.py:125-127`, `:164-182`). A logical model request
is a `call_id`, minted once by `CallSlot::begin_call` as the chain's `completion()` begins
(`ledger.rs:529-540`; `crates/outrig/src/agent/failover.rs:589`); each request sent for it is an
`attempt_id` (`ledger.rs:579-596`). The slot is per agent (`:363-389`). A round's stop is one text
in `Finished.stopped`, `RoundEnd.stopped` and `model.round.completed.stopped` (`round.rs:41-52`,
`:221-266`; `crates/outrig/src/harness/session.rs:591-607`;
`crates/outrig/src/harness/event.rs:535-545`). `RequestEnd` has no `limit` end
(`event.rs:1238-1258`), and the host reads an unknown end as `terminated`
(`children.rs:1401-1412`). A child's state is `Life` -- launching, running, idle, wedged,
released -- told to its parent through the `child` line (`children.rs:128-145`, `:1175-1192`).
The limits are `Limits`, read once per session from the top level or `[agents.<name>]`
(`crates/outrig/src/config/mod.rs:78-87`, `:1439-1457`; `crates/outrig/src/config/
validate.rs:2244-2273`). `0003-48` gives the owner a state read whose enum leaves
`limit reached` to this task (`rust-owner.md`, "The stop read").

OR-A4-1 asks for a per-agent limit on the model requests an agent's runner issues, retries and
failover attempts for one request counting once, reaching it ending the round at a safe boundary
with a structured, non-error stop the owner reads (OR-A6-1), history, namespace and facilities
intact, "not `BudgetExhausted`, not a release, and not a failure", with framework-opened rounds an
open choice to count or refund. OR-A4-2 asks for owner-configured warning thresholds delivered
once each as an input the model reads before its next request, in a documented position relative
to deliveries; an owner reset or raise on a live agent, which inputs warrant one being the owner's
policy; and the counter and limit as continuation metadata, a resumed agent reporting its spent
count and the owner able to set a fresh limit at resume. OR-A4-3 asks that a child at its limit
settle a request it holds with a terminal outcome naming the limit, stay resident and addressable,
show limit-reached in its status (OR-A6-3), answer a new request from its retained context after
a reset, and get no `0003-37` warning round, since it is not idle with outstanding work.

The validation listed the limit among the research findings the requirements had dropped ("A
per-task model-request budget with a parked, resumable outcome ... neither counts model requests per
task attempt or ends in 'parked, conversation saved'"), and the adversarial review's issue 3 said
where it must live: not counted from events on the application's side, which are lossy. A-04 makes
it required, and the phase README's deliverables list records it. `model-policy.md`, "The
model-request limit", designs what this task builds: the unit, the check beside the budget's, the
structured stop, the primary's, the root's and a child's behavior at the limit, the recommendation
on framework-opened rounds, the warning's position, the reset, the composition with `0003-37`, and
the metadata field.

Downstream, for the record: `0003-58` carries the `model_requests` field this task defines, and
`0003-60`, `0003-61` and `0003-64` record what an agent at its limit does at an assessment, a
designation and a resume.

## Goal

Each agent counts the model requests its runner issues against a limit of its own and stops at a
safe boundary with a structured stop the owner can read and lift, the model warned beforehand and
a child's requests settled `limit`, with nothing released, failed or rolled back.

## Deliverables

- **The count and the limit, per agent.** A counter on the agent's `CallSlot`, advanced when the
  ledger records a `call_id`'s first answered attempt and not at `begin_call` (`ledger.rs:529-540`),
  which mints the id before any provider has answered; so one `call_id` is one request however many
  attempts it takes, and a `call_id` no provider answered -- every attempt failed before a
  completion -- is never counted, so a provider outage spends nothing of the limit and shows as
  `0003-48`'s `unavailable` (`model-policy.md`, "Framework-opened rounds"). Beside the counter, the
  limit, the warning thresholds and which have been delivered. Read through the slot by the hook,
  the runner, the owner's reads and the export.
- **Two config keys**, at the top level and in `[agents.<name>]`, folded into `Limits` by
  `Config::limits` (`config/mod.rs:1439-1457`): `model-request-limit`, an integer of at least 1,
  unset for none; `model-request-warnings`, an array of remaining counts, each at least 1, unset
  for none. Validation beside `token-budget`'s (`validate.rs:2244-2273`). Documented in
  `doc/reference/config.md`'s top-level table, text and `[agents.<name>]` list, with the
  validation rules. `outrig run` ignores both and says so at startup, as it does `context-window`.
- **A child's own limit.** A keyword on `outrig.Agent`'s constructor beside `model=` and on
  `runtime.spawn`, each an integer or a `(limit, warnings)` pair per fork 3, carried on the spawn
  line and applied when the child's slot is made (`children.rs:506-662`, `make`); absent, the
  session's values apply. An owner constructing a root passes them among the construction inputs
  (`0003-46`); `0003-48`'s control handle reads them.
- **The stop.** `RoundHook::on_event` stops the loop on `StepEvent::CompletionCall` when the
  agent's count has reached its limit, after the budget's check and before the cap's
  (`round.rs:616-624`), at the budget's boundary (`stopped_short`, `:221-266`). The stop has a
  kind -- the tool-call cap, the token budget, a turn no member had room for, and the
  model-request limit with `spent` and `limit` -- carried by `Finished.stopped`, `RoundEnd.stopped`
  and `model.round.completed.stopped` per fork 1, with today's text as its rendering. The round
  is recorded `model.round.completed`, never `model.round.failed`, and the runner's backoff
  (`runner.rs:46-50`, `:166-176`) is not entered.
- **The primary at the limit.** `Session::round()` (`session.rs:350-383`) opens no round and
  returns `RoundOutcome::Ended` with the limit's stop, an empty reply, no `round.started` and no
  turn; what waits on the `user` channel stays unread and is announced after a reset. `run-new`
  prints the stop as it prints the cap's, naming `spent` and `limit`.
- **A child at the limit.** The runner checks the limit where it checks the spent budget
  (`runner.rs:77-98`), before the summary comparison (`:100-124`): it settles every request the
  child holds with the end `limit` through `settle`, and a request arriving while the child stays
  at the limit is settled `limit` as it arrives, with no round and no model call. `RequestEnd`
  gains `Limit` (`event.rs:1238-1258`; `children.rs:1401-1412`); `_HOST_ENDS` gains `"limit"`
  mapped to `AgentLimitReached` (`interpreter.py:6437-6441`), whose message names the child, its
  limit and its count; `Status.reason` reads `limit` and `Status.child` reads `limit-reached`
  (`_agents.py:164-182`), from a new `Life` state the host tells the parent (`children.rs:128-145`,
  `:1175-1192`). An orienting round the limit stops part way has ended, so `ready()` settles and
  the child is at its limit; `oriented`'s error arm gains `limit` beside `budget` and `failed`
  (`children.rs:1205-1221`; `interpreter.py:6455-6461`) for an orienting round that cannot open,
  which a carried count at a resume reaches (`0003-64`), and `ready()` raises `AgentLimitReached`
  then. The child stays resident, counted by `children-max`, with its kernel, namespace,
  facilities and history.
- **No warning round on a limit-reached child.** The limit check precedes `0003-37`'s comparison,
  so a child at its limit holds nothing when the comparison would run; the warning state is left
  as it was; a round the limit stopped is an ended round, its baseline set as any ended round's
  is; a warning round that reaches the limit inside itself stops at the next call and settles
  `limit` what it would have cancelled. `agent-classes.md`, "The child's rounds", gains the
  composition in one paragraph, and `runner.rs`'s module comment gains it beside the budget's.
- **Framework-opened rounds**, per fork 2: under the recommendation every request counts, the
  orienting round's included, and the orientation says that a round is not free.
- **Warnings.** When a request begins and the count remaining first falls to or below a threshold
  not yet delivered, one warning is queued naming the lowest such threshold, and every threshold
  at or above the new remaining count is marked delivered; a reset or raise re-arms the thresholds
  below the new remaining count. The warning is delivered once, as an input in the turn: within a
  round, in the next tool result's prefix after the delivery announcement and before the result's
  text (`crates/outrig/src/agent/tool.rs:151-153`); at a round's opening, after the announcement
  sentence and the omitted-turn count (`crates/outrig/src/agent/orientation.rs:433-443`;
  `crates/outrig/src/agent/history.rs:487-492`), a child's host-written opening included
  (`runner.rs:130-160`). Never on a channel. The text is `model-policy.md`'s draft, tuned here.
- **Reset and raise.** On `Session`, by agent id, for the primary and any child the owner names;
  on `0003-48`'s control handle for a root; on a parent's handle in Python -- the instance and
  `runtime.spawn`'s `Child` -- as a kernel-to-host line the host answers. A reset sets `spent` to
  zero and keeps or replaces the limit; a raise replaces the limit and keeps `spent`. Either wakes
  a limit-reached child's runner (`children.rs:114-126`, `wake`), which clears the state and
  decides from the live summary; either mid-round changes what the hook reads at the next call.
  A **reset notice** is delivered once, as a warning is, saying what changed and, when the agent
  was stopped at the limit, that its last round ended there and nothing it left running was
  stopped. Which inputs warrant a reset is the owner's; OutRig resets on none.
- **The owner's state.** `0003-48`'s state enum gains `limit reached`, carrying `spent`, `limit`
  and the round, set by the runner under the registry's lock when its check fires and replaced
  when a reset or a raise lets the agent go on; `RoundFacts.stopped` carries the stop's kind as
  `RoundEnd.stopped` does; the roster row of `0003-49` shows the state.
- **The owner's values.** The count, the limit, the thresholds and the delivered set are readable
  per agent on `Session` and the control handle. These values are the
  `model_requests` field of the continuation-metadata section `0003-58` carries, which lands after
  this task: `spent`, `limit` or null, `warnings`, `warned`. `model-policy.md`, "Continuation
  metadata", owns the field's meaning, and `0003-64` reads it at resume.
- **Other framework rounds at the limit.** The limit stops every request, so an agent at its limit
  makes no designation and no consultation; `model-policy.md` states it, and `0003-60` and
  `0003-61` record the outcome when they land.
- **Events**, execution diagnostics under the agent's subject, added to `doc/reference/events.md`
  with their fields and category: `agent.model_requests.warned` (`threshold`, `remaining`,
  `spent`, `limit`); `agent.model_requests.reached` (`spent`, `limit`, `round` or null,
  `requests`, the ids settled `limit`); `agent.model_requests.reset` (`by`, `owner` or `parent`;
  `spent` and `limit` before and after). `model.round.completed.stopped`'s entry gains the kind;
  `agent.request.settled.outcome`'s list gains `limit`; `agent.instance.ready.error`'s names the
  limit. The renderer's fixture is updated where the stop's shape changed.
- **`lifecycle.md`'s close table** gains rows for an agent at its limit: *idle at the limit,
  holding nothing* -- as an idle agent; *stopped at the limit with Python still running* -- as an
  execution running; *a reset or raise in flight at the close* -- refused, admission closed,
  nothing changed. Tested here.
- **Docs and records.** `doc/reference/config.md` and `doc/reference/events.md` as above;
  `work.md`, "Limits belong outside generated code", and `agent-classes.md`, "Limits and
  placement", each gain a pointer to `model-policy.md`; the `[Unreleased]` paragraph of
  `crates/outrig/CHANGELOG.md` names the limit; `crates/outrig/public-api.txt` regenerated.
- **`## Decisions` records**: the choice on framework-opened rounds (fork 2) and why; the shape
  of the stop (fork 1); the constructor and spawn spelling (fork 3); whether `AgentLimitReached`
  or a subclass (fork 4); whether `run-new` lifts a limit (fork 5); the `model_requests` field's
  schema as `0003-58` is to carry it; and the texts of the warning and the reset notice.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **A primary with limit 3.** The mock answers a tool call in round one and a tool call then text
  in round two, so the agent makes three requests across two rounds; the fourth is not sent.
  `round()` returns `Ended` with the limit's stop naming 3 of 3; `model.round.completed` carries
  it and no `model.round.failed` is recorded; the kernel's names from both rounds are intact
  (`runtime.history.turns` and a bound variable read back); a later reset is followed by a round
  that reads the reset notice and runs. A further `round()` before the reset opens no round and
  records no `round.started`, and the user message sent before it is announced after the reset.
- **Retries and failover count once, and an outage counts nothing.** A mock whose first attempt
  returns `503` and second answers, and an alias whose head returns `500` and whose second member
  answers, each advance the count by one; a mock that returns `503` past the retry budget on three
  reopened rounds advances it by nothing, the child reads `unavailable` and holds its request open;
  `agent.model_requests.*` agree with the number of distinct `call_id`s a provider answered.
- **Warnings at 10 and 1 remaining, once each.** With limit 20 and thresholds `[10, 1]`, the
  warning naming 10 appears in the tool result's prefix after the delivery announcement when a
  message arrived mid-round, and alone when none did; the warning naming 1 appears once at the
  round opening that follows the request that left one; neither appears twice; both are in the
  turns the store holds. A reset at 2 remaining delivers the reset notice once and re-arms the
  threshold at 10; a raise to 30 at 1 remaining delivers the notice and re-arms both.
- **A child with limit 2.** A request child with no orienting round --
  `runtime.spawn(..., requests=...)` with the limit keyword -- answers two requests in two rounds,
  and a third request settles `limit` without a model call: the sender's `await` raises
  `AgentLimitReached` naming the child, 2 and 2; `h.status` reads `failed: limit` with child
  `limit-reached`; `agent.request.settled.outcome` is `limit`; the child is resident, counted by
  `children-max`, and the mock receives no call. After the parent's reset, a fourth request is
  answered from a variable the first round bound. The same through `Session`'s reset by agent id.
- **The orienting round and `ready()`.** With limit 1, under fork 2's recommendation, the
  orienting round's one request is made, `ready()` settles, the child is at its limit, and its
  first request settles `limit`. A mock that makes a tool call in the orienting round under limit
  1 has that round stopped at its second call, and `ready()` still settles, since the round ended.
  `oriented`'s `limit` arm is driven directly: a runner whose check finds the count at the limit
  before the orienting round opens -- seeded as a resume with a carried count will seed it
  (`0003-64`) -- makes `ready()` raise `AgentLimitReached`, and `agent.instance.ready.error` names
  the limit.
- **A limit reached mid-round settles what the child holds.** A child holding one taken and one
  unread request reaches its limit at a tool call's return: the round ends with the limit's stop
  at the next call, both requests settle `limit`, each sender's `await` raises once, the unread
  one is out of the child's queue and `pending()` is 0, `agent.model_requests.reached` lists
  both ids, and the mock receives no call afterward.
- **No warning round on a limit-reached child.** With `0003-37`'s warning round in place, a child
  that takes a request and reaches its limit in the round that took it gets no further round: the
  mock's call count is unchanged after the stop, no warning text is composed, and the request is
  settled `limit`, not `cancelled`. A child below its limit that ends a round with its summary
  unchanged still gets the warning round, which counts one request.
- **Not a release, not a failure, not `BudgetExhausted`.** At the limit, `agent.instance.released`
  is not recorded, `children-max` is unchanged, `ready()` on a ready instance still settles, and
  no `BudgetExhausted` is raised anywhere; a session with a spent token budget and an agent at its
  limit reports the budget's stop, not the limit's.
- **The owner reads it.** `Session`'s read returns `spent`, `limit`, `warnings` and `warned`
  matching the events; after `0003-48` lands, its stop read reports the limit with the round;
  the values match what `0003-58` is to carry, asserted against the schema recorded in
  `## Decisions`.
- **Close rows.** A session closed with one agent idle at its limit and another stopped at it
  with a task still running reports as for an idle agent and a running execution; a reset sent
  after `close_admission()` is refused and changes nothing.
- **Config.** `model-request-limit = 0`, a warning of `0`, and either key on a provider-shape
  model row are refused at load naming the key; `[agents.<name>]` values win over the top level's;
  `outrig run` prints its ignoring line.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The stop's shape -- Recommended: a public `#[non_exhaustive]` enum carried in
   `RoundEnd.stopped` and `Finished.stopped` in place of the text, rendering today's text through
   `Display`, and recorded in `model.round.completed.stopped` as an object with `kind` and its
   fields.** The surface may change until the release (`embedding.md`, "Stable at 0.3.0, not
   before"), `0003-48`'s stop read needs the kind, and one value serves the terminal, the event
   and the read. The alternative keeps the string and adds a field beside it, which leaves two
   accounts of one stop to keep in step.
2. **Framework-opened rounds -- Recommended: every request a provider answered counts; nothing is
   refunded, and a request no provider answered never counted** (`model-policy.md`,
   "Framework-opened rounds"), so an outage is `unavailable` and not a limit stop. The alternative
   refunds the orienting round and `0003-37`'s warning round, at the cost of a refunded count the
   warnings and the metadata must carry and a count that no longer equals the ledger's.
3. **The constructor and spawn spelling -- Open; recommended: one keyword, `model_requests`, an
   integer for the limit alone or a `(limit, warnings)` pair**, on `outrig.Agent(...)` beside
   `model=`, which `0003-42` is reshaping, and on `runtime.spawn(...)`. The alternative is two
   keywords. Either way no input may be named as the keyword is, as none may be named `model`.
4. **The sender's exception -- Recommended: `AgentLimitReached`, with the message naming the
   model-request limit**, since its docstring already covers a limit that refused, `Status.reason`
   already reads `limit`, and `Status.child` reads `limit-reached`. The alternative is a subclass
   of it, for a caller that wants the distinction in an `except`.
5. **Whether `run-new` lifts a limit -- Open; recommended: not in this task.** `run-new` reads the
   keys and prints the stop with `spent` and `limit`; the user ends the session or starts another
   with a higher limit. A REPL line would be the first command of `run-new`'s own, which
   `plan/next/run-new-flag-parity.md` says needs deciding as a set, and a skill directive's
   `/name` form cannot be borrowed for it (`0003-33`).

## Dependencies

- **Hard: `0003-30`.** The request child, the `limit` reason, `AgentLimitReached`, `settle`, the
  runner's loop and the children's registry this task extends.
- **Hard: `0003-34`.** Instances, the orienting round and `ready()`.
- **Hard: `0003-37`.** The warning round whose absence on a limit-reached child this task tests.
- **Hard: `0003-48`.** The state enum this task adds `limit reached` to, and the control handle's
  reads.
- **Soft: `0003-49`.** The roster row that shows the state.
- **Soft: `0003-46`.** The owner-constructed root, which passes the limit among its inputs and
  whose handle carries the reset.

## See also

- `plan/phase/0003-python/model-policy.md` -- "The model-request limit", every section.
- `plan/phase/0003-python/rust-owner.md` -- the control handle, the stop read and the
  roster that report the limit.
- `plan/phase/0003-python/continuation.md` -- "The format", the metadata section; "Open
  work at the cut", a child at its limit.
- `plan/phase/0003-python/work.md` -- "Limits belong outside generated code".
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds", "Limits and placement".
- `plan/phase/0003-python/execution-and-rounds.md` -- a redirection stays in the same round.
- `plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md` -- the rule this task composes with.
- `crates/outrig/src/agent/ledger.rs`, `round.rs`, `runner.rs`, `children.rs`, `tool.rs`,
  `orientation.rs`; `crates/outrig/src/harness/session.rs`, `event.rs`;
  `crates/outrig/src/config/mod.rs`, `validate.rs`; `crates/outrig/src/python/interpreter.py`,
  `outrig/_agents.py`.
