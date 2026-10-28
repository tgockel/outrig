# Model policy

A **model policy** is a rule about the model requests an agent makes: how many it may make, and
which member of an alias serves them. This page decides two. A **model-request limit** is a
per-agent count of the model requests its runner issues, with warnings the model reads, a
structured stop the owner reads, and a reset the owner performs; `0003-50` builds it. A
**selection policy** is how an alias chooses the member a call goes to: head-first, as every call
does today, or **sticky**, staying on the member that worked until it fails in a class that
warrants moving, with the selected member saved and restored by name; `0003-59` builds it. Both
sit in the agent loop under the round, where `0003-15`'s retry and failover sit, and neither is a
tool the model calls or a rule generated code can see (`work.md`, "Limits belong outside
generated code"). The first application's `max_turns` and its alias pin are the running example
and the reference behavior (`cococlaw-loop.md`, sections 3.2 and 3.10, in the companion notes);
nothing here names the application, and the numbers it uses are an owner's inputs, not defaults.

What this page builds on and does not restate: the four limits of `0003-30` and what each bounds
(`work.md`, "Limits belong outside generated code"; `agent-classes.md`, "Limits and placement");
a round's vocabulary, and that a redirection stays in the same round because "limits hang off
it" (`execution-and-rounds.md`, "Being redirected mid-wait"); the child's rounds and `0003-37`'s
warning round then cancel (`agent-classes.md`, "The child's rounds";
`plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md`); the per-candidate view and its budget
(`history.md`, "The view", "The budget"); the two ids every model event carries and the late-usage
rule (`observability.md`, "What is emitted"); the public surface's freedom to change until the
release (`embedding.md`, "Stable at 0.3.0, not before"); the report (`lifecycle.md`, "The
report"); the format that carries continuation metadata (`continuation.md`, "The format"); and
the owner's authoritative reads (`rust-owner.md`; `0003-48`, `0003-49`).

## What exists

Four limits bound a `run-new` session's agents, read once as the session starts from the top
level or `[agents.<name>]` (`crates/outrig/src/config/mod.rs:78-87`, `Limits`; `:1439-1457`,
`Config::limits`). `subagent-depth-max`, `children-max` and `requests-max` refuse at the call with
`AgentLimitReached` (`crates/outrig/src/agent/children.rs:432-451`;
`crates/outrig/src/python/interpreter.py:4419-4426`); `model-concurrency-max` holds a place per
request in flight (`crates/outrig/src/agent/ledger.rs:187-193`). `tool-call-max` stops a round at
its cap (`crates/outrig/src/agent/round.rs:622-624`). `token-budget` is a session-wide latch on
reported spend: the ledger marks it spent as the attempts recorded reach it
(`ledger.rs:222-232`); the round's hook stops the loop before the next call once it is
(`round.rs:616-620`); each retry loop refuses a request once it holds its place
(`crates/outrig/src/agent/retry.rs:911-916`, `:626-628`); a child's runner settles every open
request `budget` and marks the child idle (`crates/outrig/src/agent/runner.rs:77-98`); the
sender's `await` raises `BudgetExhausted` (`interpreter.py:6437-6452`, `_HOST_ENDS` and
`_settle_work`); an instance's `ready()` raises it (`crates/outrig/src/python/outrig/
_agents.py:544-553`). None of these counts model requests per agent, and none ends in a stop the
owner can lift: a spent budget is final for the session.

A **logical model request** has a `call_id`, minted once by `CallSlot::begin_call` as the chain's
`completion()` begins (`ledger.rs:529-540`; `crates/outrig/src/agent/failover.rs:589`), and each
request actually sent for it has an `attempt_id` (`ledger.rs:579-596`, `sending`), so a call that
was retried, or moved to another member, has one `call_id` and several attempts
(`observability.md`, "What is emitted"; `doc/reference/events.md`, "Integration audit"). The slot
is per agent (`ledger.rs:363-372`); the ledger that mints the ids is per session.

An alias resolves to every concrete row this build can reach, in order
(`crates/outrig/src/agent/resolve.rs:270-317`, `selectable`; `:354-363`), as
`ResolvedAgent.candidates`, with the alias's own name kept in `ResolvedAgent.model`
(`:198-216`). `build_agent` makes one `FailoverModel` per agent over them
(`crates/outrig/src/agent/build.rs:56-100`), with one `RetryPolicy` whose budget is the head's
provider's `retry-budget-secs` (`:70-73`, `:210-216`) and one `ChainDeadline` armed per call
(`retry.rs:177-215`; `failover.rs:588`). Every call starts at the head and moves to the next
member on any failure once that member's own retries are spent (`failover.rs:597-601`, the
comment; `:674-680`, the move), each member sent a view assembled for its own window, protocol
and calibration (`:609-616`; `crates/outrig/src/agent/budget.rs:154-185`, `Budget`; `:195-241`,
`Wire`; `:122-150`, `Calibration`). A member whose window cannot hold the call's latest turn is
passed over (`failover.rs:643`; `:521-528`, `unfit`); each move is a `model.failover` event
(`:381-402`, `announce`); exhausting the chain reports every member's reason and whether any may
clear (`:405-441`, `Exhausted`; `:471-518`, `may_clear`). Nothing is pinned, and nothing of the
selection is saved.

The classification the loops apply today: `408`, `425`, `429` and every `5xx` are retried within
the budget, honoring `Retry-After` (`retry.rs:1063-1065`, `is_retryable_status`; `:938-944`;
`:1100-1133`); a connection error or a timeout is retried unless the client could not build the
request (`:1071-1073`, `is_permanent`; `:1213-1224`, `is_transient`); a `200` rig can make nothing
of is re-rolled twice (`:546-647`); anything else -- a `401`, a `400`, a model that does not exist
-- is final at once (`:1241-1248`, `is_recoverable`; `doc/concepts/llm-providers.md`, "Transient
failures"). An empty reply whose finish reason is `content_filter` is a cut-short turn the model
layer re-rolls, not a class of its own (`retry.rs:757-759`, `is_normal_stop`), and a provider's
`400` or `422` is read only for the role-alternation hint (`failover.rs:539-542`, `refused`).

## The model-request limit (`0003-50`)

### One count per logical request, per agent

The limit counts logical model requests: one per `call_id` the agent's slot mints, committed to the
count once, when the ledger records the call's first answered attempt. A retry and a move to another
member belong to the call they serve and add nothing; a call no provider answered -- every attempt
failed before a completion, the token budget refused it, or no member had room for it -- is never
counted ("Framework-opened rounds"), and a request that never began is never counted. The count is
the agent's own: the primary's, each child's, an owner-constructed root's (`task-agents.md`, "The
owner-constructed root"), each under its own limit and each from zero. A child's count is not added
to its parent's; the token budget is the bound on the tree (`work.md`, "Spend"). Nothing divides a
round's requests among the requests a child answered in it, which `agent-classes.md` ("Limits and
placement") decided for spend. The counter lives on the agent's `CallSlot` beside the round's
attempts (`ledger.rs:374-389`, `SlotState`): `begin_call` mints the `call_id`, the one place every
model request of the agent passes, and the count and the warnings are committed once per `call_id`
where the ledger records its first answered attempt, so an id minted for a call no provider answers
is allocated and never spent.

### Where the limit stops a round

The check is the token budget's, one arm below it. `RoundHook::on_event` stops the loop on
`StepEvent::CompletionCall` when the budget is spent (`round.rs:616-620`); it stops it next when
the agent's count has reached its limit, and only then for the tool-call cap (`:622-624`). The
order says which stop a round reports when two apply: the budget, which ends every agent's calls,
before the limit, which ends one agent's until it is lifted, before the cap, which ends one
round. The stop lands at the budget's safe boundary: before a request is sent, with every tool
result of the last turn kept (`round.rs:221-266`, `stopped_short`, which flushes the chat history
rig hands back), so the conversation ends on a complete turn and no request is sent past the
limit. Nothing is rolled back and nothing is cancelled: Python the round started keeps running
(`execution-and-rounds.md`, "One at a time"), the kernel keeps every name, the facilities stay
bound, and the history is intact.

The stop is **structured**. Today `Finished.stopped`, `RoundEnd.stopped` and
`model.round.completed.stopped` are one text (`round.rs:41-52`;
`crates/outrig/src/harness/session.rs:591-607`; `crates/outrig/src/harness/event.rs:535-545`),
which an owner would have to parse. `0003-50` gives the stop a kind the owner reads without
parsing -- the tool-call cap, the token budget, a turn no member had room for, and the
model-request limit with its `spent` and `limit` -- and `0003-48`'s state enum gains
`limit reached`, carrying `spent`, `limit` and the round, set by the runner under the registry's
lock when the check fires and replaced when a reset or a raise lets the agent go on
(`rust-owner.md`, "The stop read"). Whether the kind replaces the text or sits beside it, in
`RoundEnd.stopped` and in the `RoundFacts` `0003-48` reads it into, is `0003-50`'s fork 1; the
text stays what a terminal prints.

Three things the stop is not, which A-04-1 requires. It is **not `BudgetExhausted`**: that
error means the session can make no further call, and here it can; what a sender meets is
`AgentLimitReached`, below. It is **not a release**: the agent stays resident, counted by
`children-max`, addressable on its channels, holding its kernel. It is **not a failure**:
`model.round.completed` is recorded with the stop, not `model.round.failed`; the runner's
backoff after a failed round (`runner.rs:46-50`, `:166-176`) is not entered; and `0003-37`'s
clause for a round that ended in an error does not apply, since none did.

### The primary, the root and a child at the limit

**The primary.** `Session::round()` at the limit opens no round. It returns
`RoundOutcome::Ended` whose `stopped` is the limit's kind, with an empty reply, no `round.started`
and no turn committed, and whatever waits on the `user` channel stays unread and is announced
again after a reset (`session.rs:350-383`). An owner that loops on `round()` reads the same stop
until it resets or raises; the awaitable stop read of `0003-48` is the wait to use in place of
polling. `run-new` prints the stop as it prints the cap's.

**An owner-constructed root** (`0003-46`) is a child whose runner opens rounds on arrivals; it is
a child below, and its control handle carries the reads and the reset.

**A child.** The runner checks the limit where it checks the spent budget, before it compares
summaries (`runner.rs:77-98`, then `:100-124`). A child at its limit settles every request it holds
-- unread or taken -- with the end `limit`, through the `settle` line the budget uses
(`children.rs:1196-1200`; `interpreter.py:6444-6452`), and a request that arrives while it stays
at the limit is settled `limit` as it arrives, before any round opens and with no model call. The
sender's `await` raises `AgentLimitReached`, whose message names the child, its limit and its
count; `h.status` reads `failed: limit`, and its `child` reads `limit-reached`
(`_agents.py:164-182`, `Status`; `children.rs:128-145`, `Life`, which gains the state and tells
the parent through the `child` line, `:1175-1192`). `RequestEnd` gains `Limit`
(`event.rs:1238-1258`), so `agent.request.settled.outcome` says `limit` where the host's
`outcome()` would today read an unknown name as `terminated` (`children.rs:1401-1412`). An
orienting round the limit stops part way has ended, so `ready()` settles and the child is at its
limit from then on. `ready()` raises `AgentLimitReached` only when the orienting round cannot
open at all -- the runner's check fires first, and `oriented`'s error arm gains `limit` beside
`budget` and `failed` (`children.rs:1205-1221`; `interpreter.py:6455-6461`) -- which a limit of at
least 1 and a count from zero rule out at a first start, and which a resume with a carried count
at its limit reaches (`0003-64`); `agent.instance.ready.error` names the limit then. The child
stays resident and addressable; it keeps its kernel, its namespace, its facilities and its
history; and after a reset the next request opens a round on that history, which is A-04-3's
"answered from the retained context".

`AgentLimitReached` is the existing `limit` reason (`_agents.py:125-127`; `interpreter.py:4180`,
`_REFUSED`; `Status.reason`, `:169-170`): "A limit the session holds every agent to refused at
once, rather than waiting". The model-request limit is such a limit, and its message says which.
`Status.child` tells a caller a limit-reached child from one that refused for `requests-max`. A
subclass of its own is `0003-50`'s fork 4.

### Framework-opened rounds

Some rounds no arrival earned: an instance's orienting round (`runner.rs:130`;
`crates/outrig/src/agent/orientation.rs:360`, `ORIENTING`), `0003-37`'s warning round, the reopen
of a round that failed before its code ran (`runner.rs:166-176`), `0003-60`'s consultation round
and `0003-61`'s designation round. A-04-1 leaves whether they count or are refunded open.

**Recommended: every model request a provider answered counts, whoever opened the round, and nothing
is refunded.** The count is the number of `call_id`s with an answered attempt -- a completion, a
reply the loop rejected and re-rolled, or a content-policy refusal, each of which the ledger records
per attempt -- so the ledger, the events, the owner's read and the warning the model reads agree
with no second figure, and A-04-1's "retries and failover attempts for one request count once"
holds. A `call_id` no provider answered -- every attempt a connection error, a `5xx`, a timeout or a
rate limit past the retry budget -- is not counted: the reopen after such a failed round
(`runner.rs:166-176`) mints a `call_id` with no answer and spends nothing, so a provider outage
shows as `0003-48`'s `unavailable` and never as a limit stop that settles the held requests `limit`,
and OR-A6-1's "the request is kept for a later round" holds through the outage. Bounding the reopen
loop itself, which today runs for as long as the session lasts after a backoff of two to sixty
seconds (`runner.rs:46-50`), is a separate question (#765). A refund would need the slot to know why
a round was opened, a refunded count that the warning arithmetic and the continuation metadata must
carry, and a rule for a framework-opened round in which the model did work, which the warning round
and the designation round both are. What the framework opens and the provider answers is bounded and
known to the owner that sets the limit: one orienting round per instance, at most one warning round
per unchanged summary (`0003-37`), and one designation round per sleep or save (`0003-61`).

**The alternative, a refund** of the orienting round and the warning round, is what the first
application does for a nudge (`cococlaw-loop.md`, section 3.10). Its cost is the second figure
above and a count that no longer equals the ledger's. Under the recommendation, A-04's
acceptance numbers hold as written for an agent with no orienting round, an instance's child
spends one of its limit on orienting, and an outage spends none; `0003-50`'s `## Decisions`
records the choice.

### Warnings and notices

The owner configures **warning thresholds** as remaining counts -- `[10, 1]`, say; the first
application will pass `max(10, max/10)` and `1`, computed on its side -- and each is delivered
**once**. When a request begins and the count remaining first falls to or below a threshold not
yet warned, one warning is queued naming the lowest such threshold, and every threshold at or
above the new remaining count is marked warned. A reset or a raise re-arms each threshold below
the new remaining count.

A warning is **an input the model reads before its next request**, in a documented position
relative to deliveries. Two places carry an input to the model between its requests, and the
warning uses both:

- **Within a round**, the next tool result's prefix. `SubmitPython::announcement` puts the
  delivery announcement there as one bracketed line (`crates/outrig/src/agent/tool.rs:151-153`;
  `crates/outrig/src/agent/channel.rs:40-120`, `Announcer`). The warning follows the delivery
  announcement when there is one and precedes the result's text: deliveries first, since they are
  what the round acts on, then the constraint on how to go on.
- **At a round's opening**, the opening line (`orientation.rs:433-443`, `opening`;
  `crates/outrig/src/agent/history.rs:487-492`, `open_round`). The warning follows the
  announcement sentence and the omitted-turn count. A child's host-written opening
  (`runner.rs:130-160`; `orientation.rs:405-417`, `requests`) carries it the same way.

Either way the warning is in the turn, and so in the store and in an export, as the announcement
is. It never goes on a channel, which the model's code would have to receive. The spelling is
`0003-50`'s; a draft, in the bracketed voice the announcements use:

> [outrig] 10 model requests remain of your limit of 20. When none remain, your round ends before
> the next request and nothing runs until the owner raises the limit. Finish what is open and
> leave the rest in a state you can take up again.

A **reset notice** is delivered the same way, once, after a reset or a raise: it says the limit
was reset or raised, to what, and what remains, and, when the agent was stopped at the limit,
that its last round ended there and nothing it left running was stopped. The stop itself tells
the model nothing -- no request is sent past the limit -- so the model learns of the stop from
the reset notice, which names it.

### Reset and raise

The owner **resets** an agent's count -- `spent` to zero, with the limit kept or a new one given
-- or **raises** its limit, leaving the count, on a live agent at any time: at its limit,
mid-round or idle. Which inputs warrant a reset -- a person's message, an orchestrator's, a new
claim -- is the owner's policy; OutRig applies none, and an arrival on a channel resets nothing.
The surfaces: `Session`, by agent id, for the primary and for any child the owner names; the
control handle of `0003-48`, for a root the owner constructed; and a parent's handle in Python --
the instance, or `runtime.spawn`'s `Child` -- for a child it launched, as a kernel-to-host line the
host answers. A reset of a child at its limit wakes its runner through the child's `wake`
(`children.rs:114-126`, `ChildNode`), which clears the limit-reached state and decides from the
live summary as `0003-30`'s rule has it: the requests the limit settled are gone, so an empty
summary is idle, and the next arrival opens a round on the retained history. A reset mid-round
changes the count the hook reads at the next `CompletionCall`, and the round goes on. Every reset
and raise is an event naming who asked and what changed.

### A child at its limit, and `0003-37`

`0003-37` guarantees that no child is idle while it holds a request it has not answered: two
rounds that change nothing, then a cancel, each sender told. The two rules compose by order and
by state:

- **Order.** The runner checks the limit before it compares the live summary with the baseline
  (`runner.rs:77-98` before `:100-124`), as it checks the spent budget. A child at its limit
  settles what it holds `limit` and then holds nothing; the comparison never runs for it, so it is
  not "idle with outstanding work it could do", and **no warning round is spent on it**. The
  warning state -- whether the warning has been given since the summary last changed -- is left as
  it was, since no round was opened on the summary.
- **A round the limit stopped** is a round that ended with a stop, as one the tool-call cap stops
  (`0003-37`, "A round stopped early counts as ended"). Its baseline is set as any ended round's
  is; on the runner's next pass the limit check fires first, so no comparison decides anything
  for a limit-reached child.
- **A warning round that opens** on a child below its limit spends a model request as any round
  does. If the limit is reached inside it, the round stops at the next call, and the requests the
  warning would have cancelled are settled `limit` instead, each sender told once.
- **After a reset**, the summary rule starts from the live summary with the warning available
  again, as after any change.

`continuation.md` ("Open work at the cut"; "Where the queued 0003 tasks interact") carries the
consequence for a cut: a child at its limit is not open work; it is released at a sleep as an
idle child is, stays resident at a save, and neither refuses the cut nor ends `suspended`.

### Other framework rounds at the limit

`0003-61`'s designation round and `0003-60`'s consultation round each need a model request, and
the limit stops every request, theirs included. An agent at its limit makes no designation:
`0003-61` records that none was made and why, and the cut proceeds as it does when the agent
designates nothing; an assessment that would consult the model reports the limit as the reason it
did not (`0003-60`). An owner that wants the agent's designation raises the limit first. The
alternative, exempting framework rounds from the stop, would send requests past a limit the owner
set, which A-04-1 forbids.

### Continuation metadata

The counter and the limit are continuation metadata in the format of OutRig's own
(`continuation.md`, "The format"), which `0003-58` carries, since `0003-50` lands before it; this
page owns what the field means. Per agent, the `model_requests` field holds `spent`, the count;
`limit`, or null for none; `warnings`, the thresholds; and `warned`, those already delivered. At a
resume (`0003-64`) the count continues from `spent` under `limit`, the reconciliation notice
reports both, and an owner that gives a fresh limit at resume performs a reset as above, with the
notice saying so. The resumed agent's thresholds are re-armed from `warned`, so none is repeated
and none skipped. The field is additive: a reader that does not know it keeps it.

### Configuration and surfaces

Two keys, at the top level and in `[agents.<name>]`, read once as the session starts like the
four limits (`doc/reference/config.md`, "Top level"; `config/mod.rs:1439-1457`):
`model-request-limit`, an integer of at least 1, unset for no limit, which is today's behavior;
and `model-request-warnings`, an array of remaining counts, each at least 1, unset for none.
Validation follows `token-budget`'s (`crates/outrig/src/config/validate.rs:2244-2273`). A child's
limit and thresholds are the session's unless its launch names its own: a keyword on
`outrig.Agent`'s constructor beside `model=` and on `runtime.spawn`, which a parent may set below
or above its own, since the counts are independent (`0003-50`, fork 3). An owner constructing a
root passes them among the construction inputs (`0003-46`). `run-new` reads the keys and prints
the stop; whether it lifts one is `0003-50`'s fork 5. The legacy `outrig run` ignores both keys and
says so at startup, as it does `context-window`.

### Events

Execution diagnostics, under the agent's subject: `agent.model_requests.warned` (`threshold`,
`remaining`, `spent`, `limit`); `agent.model_requests.reached` (`spent`, `limit`, `round` or null,
and `requests`, the ids settled `limit`); `agent.model_requests.reset` (`by`, `owner` or `parent`;
`spent` and `limit` before and after). Beside them, `model.round.completed.stopped` carries the
structured stop, `agent.request.settled.outcome` gains `limit`, and `agent.instance.ready.error`
names the limit. The roster of `0003-49` shows `limit-reached` as an agent state, and the stop
read of `0003-48` reports the limit with `spent` and `limit`; both are reads from the runner's
state, never from the stream. The spellings are `0003-50`'s.

## Sticky selection (`0003-59`)

### Two policies, spelled on the alias

An alias row gains `selection`: `"head"`, today's rule and the default, or `"sticky"`. Under
`head` every call starts at the first member, for the reason `failover.rs:597-601` gives: "The
list is a preference order, and sticking to candidate two for the rest of a session because of
one rate-limit window would silently downgrade the user's choice." That reasoning stands for the
default and is not reversed. Under `sticky` the downgrade is neither silent -- each move is an
event, the roster names the member in use, and the resume notice names the member restored --
nor against the user's choice, since the alias row asked for it. `doc/reference/config.md:706-710`
describes the head rule as the alias's only behavior; `0003-59` rewrites it as the `head` policy
and adds the other.

Under `sticky` the chain keeps a **pin**: the member a call starts at. The pin begins at the head.
A call tries the pinned member; when that member fails in a class that warrants moving, the call
moves to the next member in the alias's order, and on, until a member answers or every member has
been tried, and the member that answers becomes the pin, so the next call starts there. A member
that fails in a class that does not warrant moving ends the call on that member with its failure,
the pin unchanged, and the failure surfaces to the owner through the stop read (`rust-owner.md`;
`0003-48`'s "provider refusal" and "unavailable, with the failure class"). With `strikes` above
one, a non-final strike is such a failure: the call ends on the pinned member, the round fails, and
the runner's reopen (`runner.rs:166-176`) tries the same pin; only the final strike moves the pin,
and the call then moves on to the next member. The policy is the
alias's, not the agent's: a chain of one member selects nothing, and an agent on a concrete row
is unaffected.

### Classification

The policy names failure classes, and OutRig classifies, from the status, the transport error and
the finish reason it already reads (`retry.rs:1063-1073`, `:1213-1252`, `:757-759`;
`failover.rs:539-542`). The classes, and what each does under `sticky`:

- **rate-limited** -- a `429`. Retried in place by the HTTP loop, honoring `Retry-After`, within the
  retry budget; when the budget is spent, that is one **strike** against the member, and after
  `strikes` in a row the pin moves; a strike before the last fails the round on the pinned member
  with the pin unchanged. An answer clears the member's strikes.
- **server or transport** -- a `5xx`, `408` or `425`, a connection error or a timeout, or a `200`
  rig could make nothing of. Retried in place as today; when the loop gives up, the pin moves.
- **authentication** -- a `401` or `403`. Never moves: the failure is the member's configuration,
  and a pin that moved past it would hide a broken key for as long as the session lasts, where
  the head policy's move at least prints it. The call fails on the member and surfaces to the
  owner (A-05-1's acceptance: "a 401 on `b` stays on `b`").
- **content policy** -- a refusal the provider names in its finish reason or stop reason, such
  as `content_filter`. Never moves: the content is refused, not the member, and another member
  sent the same conversation answers the same. Surfaces as the owner's "provider refusal on
  content policy" (A-06-1). The model layer's re-roll of an empty cut-short reply
  (`retry.rs:546-647`) runs first, as today.
- **context window** -- no member of the chain, or not the pinned one, has room for the call's
  latest turn (`failover.rs:643`; `history.md`, "The budget"); a provider's `400` whose body says
  the request is too long is this class where the body can be read, else `other`. The call is
  passed over to the next member that fits; the pin is unchanged (A-05-4).
- **other** -- any other `4xx`, an unknown model, a request the client could not build. Never
  moves; surfaces.

`0003-48` lands first and reports a failure's class as the label the attempt renders
(`retry.rs:1194-1200`, `candidate_failure_label`; `:1203-1211`, `prompt_failure_text`);
`0003-59` makes the classification a type the policy decides on, and the stop read carries it
beside the label from then on, so the owner and the policy read one class.

### Strikes and backoff

`strikes`, on the alias row, is the number of consecutive calls on which the pinned member's rate
limit outlasted its retry budget before the pin moves; its default is 1. The retry budget is
already the patience -- `retry-budget-secs`, ten minutes by default, wall clock per call
(`doc/reference/config.md`, `[providers.<name>]`; `retry.rs:219-247`) -- and a second and third
strike would each spend another budget on a member that did not answer. The first application's
three strikes count attempts on a ladder of one to thirty seconds (`cococlaw-loop.md`, section
3.2), which the budget replaces; A-05-1 names its numbers as reference values. No backoff input
is added: the loop's curve and `Retry-After` (`retry.rs:100-108`, `:1116-1133`) and the runner's
reopen backoff (`runner.rs:46-50`) are the backoff, each with its default, and `retry-budget-secs`
is the patience per call. An operator who wants more patience on a member raises `strikes` or
the member's budget.

### A move, the pin, and exhaustion

Within one call under `sticky`, the chain starts at the pin and, on a moving-class failure, tries
the members after it in the alias's order. What it does when the end of the order is reached is
`0003-59`'s fork 2. **Recommended: a rotation** -- the members before the pin, from the head -- so
that no member is skipped and a pin on the last member does not fail every call while the head
has recovered, which is the silent downgrade `failover.rs` warns of. The member that answers is
the pin. When every member fails, the call ends `Exhausted` with every member's reason, as today
(`failover.rs:405-441`); the pin stays where it was, since nothing answered; the round fails,
recoverably when any reason may clear, and the runner reopens after its backoff with a new call
from the same pin.

`model.failover` is emitted per move within the call, as today, and gains the failure class;
`model.selection.moved` is emitted once when the pin changes, naming the alias, the member left,
the member selected, the class and the `call_id`. Under `head` the pin never changes, and the
second event never fires.

### What does not move the pin

- A **pass-over for size**: the call goes to the next member that fits, `model.failover` is
  emitted with `attempt_id` null as today, the manifest records what each member was sent, and
  the pin is unchanged. Each member keeps its own ceiling and window: `held_to(index)`
  (`failover.rs:349-354`) scales each member's `Budget` by its own `Calibration`, and the view
  is assembled per member (`:609-616`); a request that does not fit the pinned member follows
  `history.md`'s pass-over and is reported, never truncated (A-05-4).
- A failure in a **non-moving class**, above.
- A request the **token budget** refused, which ends the call for every member (`failover.rs:661`).

### Pin scope: per agent, with inheritance

`0003-59`'s fork 3. **Recommended: per agent.** The chain, its calibrations and its slot are per
agent (`crates/outrig/src/agent/mod.rs:276-345`, `assemble`; `build.rs:56-100`); a child may name
a different model; the continuation metadata is per agent (`continuation.md`, "The format"). A
child launched on the same alias as its parent starts at the parent's pin at launch, given with
the model name the launch inherits (`children.rs:455-456`), so a child does not pay the head's
failure its parent already paid; after that each moves on its own. The alternative, one pin per
alias per Session shared by every agent's chain, makes a move in one agent move every other
agent's next call, which is more coordination for the same coverage and crosses the per-agent
slot.

### Moving back

`0003-59`'s fork 4. **Recommended: the pin never returns to a higher-priority member on its own.**
The rotation above is the one way it lands on an earlier member -- when every later one failed
on one call -- and the pin then names the member that answered. The alternative, a probe of the
head after a time or a count of calls, adds a timer and a second kind of move the owner cannot
predict; the first application never falls back today (A-05, open choices). An owner that wants
the head back starts a new session, or resumes under an alias whose order puts it first.

### Persistence, and restore by name

Per agent, the `selection` field of `0003-58`'s continuation-metadata section holds `alias`, the
alias row's name; `member`, the pinned member's name; `order`, the member names in the alias's
order at the time; and `policy`. `0003-59` fills it at export. At import (`0003-58`'s
`SessionBuilder::history`; `0003-64`'s resume) the alias is resolved as the catalog stands at
resume, and the pin is restored by **name**: the member among the new candidates with that name,
wherever it stands in the order, so reordering `[a, b, c]` to `[c, b, a]` leaves the pin on `b`
(A-05-2). A member no longer present -- removed, renamed, or not selectable in this build -- falls
back to the head, with `model.selection.restored` carrying `fallback: true` and a line in the
reconciliation notice (`0003-64`). A saved selection for an alias whose policy is now `head` is
kept and not applied; a saved `head` selection restores nothing; an agent resumed on a concrete
row keeps the field unapplied and the event says so.

### A cross-provider move

A move from a member on one provider or API style to one on another follows `0003-58`'s
compatibility rule per provider pair: the next call's view omits what the new member cannot take,
`left_out` records each part with its reason, visible and tool history go whole, nothing is
invented, and a cut taken afterwards records the omissions in its manifest (`continuation.md`,
"The format"; A-05-3). `Wire::takes` today leaves unsigned reasoning out of a call to Anthropic
and sends everything to an OpenAI-compatible member (`budget.rs:202-217`); `0003-58` makes the
rule provenance-aware as a deliverable, so that one table serves the import and every later call,
which a sticky alias spanning providers needs. `0003-59` tests the sticky case of `0003-58`'s
"a mid-session move" acceptance.

### The retry budget under `sticky`

`build_agent` takes the chain's retry budget from the head's provider (`build.rs:70-73`), and
`doc/reference/config.md` says reordering an alias changes which budget governs. Under `sticky`
the pinned member may be on a provider with another budget. The chain's budget stays the head's
as documented: one knob per alias, which the alias's author set knowing the order; `0003-59`
records it, and making the budget follow the pin is an open question below. The deadline is armed
once per call and shared by every member the call reaches (`retry.rs:177-205`, `ChainDeadline`),
as it is under `head`: a member the call moves to after a spent budget gets no in-place retry in
that call, and the move takes full effect at the next call, which starts at the new pin with a
fresh budget.

### Reads and surfaces

`Session::model()` returns `head_name()`, "the row every call is tried against first"
(`agent/mod.rs:116-118`; `failover.rs:356-358`), and `agent.started.model` carries the head's
concrete row (`agent/mod.rs:327-333`). Under `sticky` both name the pinned member -- at start,
the head or the member restored -- which keeps their meaning. The roster of `0003-49` carries each
agent's model as named (`children.rs:99-112`, `Node::model`), and `0003-59` adds the member in use
beside it for an agent on an alias. Configuration:

```toml
[models.smart]
alias     = ["opus-5-bedrock", "opus-5-anthropic", "opus-5-azure"]
selection = "sticky"       # optional: head (default) or sticky
strikes   = 1              # optional; sticky only; defaults to 1
```

`selection` and `strikes` on a provider-shape row are a validation error, as `max-tokens` on an
alias is (`config/mod.rs:1218-1245`; `doc/reference/config.md`, "Validation rules"), and
`strikes` on a `head` alias is one too. `run-new` and the harness read them; `outrig run` ignores
them and says so at startup, as it does `context-window`. An embedder with an in-memory `Config`
sets them on `Model`, whose fields are public (`config/mod.rs:1218`). No facade item of A-08
changes.

## What the two share

One `call_id` is one request for both: a call that moved within itself is one request of the
limit. The owner's reads (`0003-48`, `0003-49`) report the limit's stop and the member in use from
the runner's state. Both are continuation metadata in one section per agent (`0003-58`). Neither
reaches generated code: the model learns of the limit from warnings in its inputs and learns
nothing of selection, as `work.md` has it: "An agent cannot raise its own ceiling".

## Rejected alternatives

- **Counting requests on the application's side, from events.** The stream is lossy and a count
  made from it is not authoritative (`observability.md`, "One stream"; the adversarial review's
  issue 3: "Events are lossy, and that path recreates an app-side loop"). The limit lives where
  the request begins.
- **A per-task token budget in place of the count.** A token budget bounds spend, not turns; it
  cannot be reset per input without resetting the session's; and the two coexist, one bounding
  the tree and the other each agent.
- **The limit as `BudgetExhausted`.** The error means the session can make no further call; a
  resettable stop under it would make `ready()` raise for a condition the owner can lift and
  conflate two reports in the stop read.
- **A refund of framework-opened rounds.** Recorded above as the alternative; the second figure
  it needs is the cost.
- **The warning on the `user` channel, or as a message.** A message must be received by the
  agent's code; an input the model reads before its next request is in the turn, where
  announcements already go.
- **A reset the agent can ask for.** `work.md`, "Limits belong outside generated code": the
  model's way out is to finish, and the owner's policy decides.
- **`sticky` as the default.** A changed default changes every existing alias's behavior, and the
  reason `failover.rs` gives holds where nobody asked for a pin.
- **A pin per Session**, and **an automatic return to the head**: the alternatives of forks 3
  and 4 above.
- **Strikes on the application's ladder.** OutRig's retry budget is wall clock per call; three
  strikes of ten minutes is not the same thing, and the budget is the patience.
- **One `selection` policy on the agent rather than the alias.** Which members are equivalent and
  how to treat a failure is a property of the alias; an agent chooses an alias.

## Open questions

- Whether `Session::model()` should return the alias name with the member in use beside it,
  before the 0.3.0 release fixes the surface.
- Whether the chain's retry budget should follow the pin rather than the head.
- Whether the Python owner (`outrig.Session`, `0003-29`) gets the reset and the reads;
  Python-host parity is `rust-owner.md`'s open choice.
- Whether the reopen loop of a round that keeps failing before its code ran should have a bound of
  its own; under the recommendation the limit never counts a request no provider answered, so the
  loop is bounded by nothing but the session, which is #765.
- Which provider bodies name a request as too long, so that the context-window class can read
  a `400`; a survey of the providers OutRig has clients for decides it.
- Whether a REPL line lifts the limit in `run-new`, which `0003-50`'s fork 5 leaves to
  `plan/next/run-new-flag-parity.md`'s question of a command set.

## Unverified

- That `StepEvent::CompletionCall` in `RoundHook::on_event` is the only point a model request
  begins, and that `begin_call` runs once per `completion()`: read at `round.rs:600-624` and
  `failover.rs:581-589`, not run.
- That `stopped_short`'s flush keeps every tool result of the last turn when the hook stops
  before the next call: rig's `PromptError::PromptCancelled` carries `chat_history`
  (`round.rs:221-266`), read for rig 0.40 and not run.
- That a `child` line reaches a root's owner: `tell_life` sends to the parent kernel
  (`children.rs:1175-1192`), and a root's parent is the owner endpoint `0003-46` gives it.
- The finish or stop reason each provider names for a content-policy refusal: `content_filter`
  is in `retry.rs`'s tests; Anthropic's is not checked against its documentation here.
- That `Calibration` is per member across a move: `calibrations[index]` (`failover.rs:349-354`,
  `:649`), read.
