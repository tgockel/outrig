# 0003-25 -- A parent awaits its child's typed result

## Context

`work.md` designs the explicit child API -- `runtime.spawn` to make a child, `child.submit` to give
it work, and a handle that settles once -- and `agent-placement.md` puts each child in a kernel of
the session's one interpreter. None of it is built. The phase has run one agent until now.

Planning settled what was open there:

- **A child is a fresh kernel** with the same bindings and skills as its parent, and no user
  channel. It shares its parent's trust domain -- separate namespaces in one interpreter are not
  isolation -- and the user talks to the main agent only.
  `plan/next/children-have-a-user-channel.md` records what that costs.
- **The host runs each child's rounds.** A Rust runner per child calls the model and submits the
  child's Python, so a child keeps working while its parent's code does something else.
- **Completion is a Python call in the child**, `await runtime.complete(value)`, whose spelling
  was left open (fork 3). A result can then be built from objects that never pass through the
  model. The call decodes with `0003-24`, problems go back to the child to fix, and only the final
  answer is repaired: the executions before it are never run again. A child whose round ends
  without completing is asked once more, in a trailing round that counts no attempt; if that round
  ends without a completion too, the child goes idle and the request stays open until its caller
  ends it (fork 13, revised 2026-10-02).
- **Every request has an id and a reply type, and there are two child kinds that never mix.** A
  *work child* -- `spawn` then `submit`, or a decorated call (`0003-26`) -- has its inputs bound as
  variables, one active submission at a time, and answers with `await runtime.complete(value)`. A
  *request child* -- `spawn(..., requests=...)`, or an instance of an agent class (`0003-29`) --
  receives each request on a request channel and answers it by id with `await d.reply(value)`
  or `await d.fail(message)`, and has no `runtime.complete`. One machinery serves both: a handle per
  request, strict decoding (`0003-24`), repair, the attempt limit and the events. The class form is
  `0003-29`'s, a thin layer over what this task builds. Rejected: `complete` answering "the one
  outstanding request" of a request child -- with two received, which one it answers depends on
  timing.
- **Two session-wide limits beside depth and the token budget**, decided on 2026-10-02 after the
  design critique: `children-max` (default 64) on the children resident in the session, and
  `model-concurrency-max` (default 8) on the model requests in flight. A launch past
  `children-max` raises `AgentLimitReached` at once and never waits; a model request past
  `model-concurrency-max` waits for a permit in a cancellation-safe queue. The new loop does not
  read `subagent-width-max`, which stays `run`'s key at its default of 8 and ceiling of 16; the
  raise to 16 and 32 planned on 2026-09-30 is dropped, and `run`'s launches past its cap are still
  refused. The earlier shape, a launch past a cap on live children waiting for a slot, was dropped
  on 2026-10-01: a request child idle between requests would have held a slot while making no
  model call, and generated code fanning out with `asyncio.gather` would have waited on a release
  only the instance's owner could perform. `children-max` counts idle children, because it bounds
  threads and memory, and it refuses rather than waits, which a program can handle and a model
  calling a tool can answer by choosing something else. A scheduler for children working at once is
  `plan/phase/0003-python/potential/resource-scheduling.md`.

`work.md` also names the limit that does not exist yet and should: cumulative model spend. With
usage read per round since `0003-13`, a per-tree token budget can be enforced, and a tree that can
spawn children is where spend without a limit costs most. The same usage, attributed to the
child's round that spent it and added to the rounds above it, is how an embedder learns what a
piece of work cost (`typed-agents.md`).

Ending a child has to be as complete as ending a session. A child being released may have
children of its own, requests received and not yet answered, a call waiting for an approval, an
execution running, and hosted calls under way on the host. `lifecycle.md`'s "Releasing a child"
closes that subtree the way a session close does, and this task builds it.

## Goal

An agent's code can make a child, give it typed work -- a submission, or a request on a channel --
and await a result validated against the declared type; and every wait on that work ends,
including when the session closes.

## Deliverables

- **`runtime.spawn(name, prompt=..., model=..., inputs=..., requests=...)`** makes a child: a fresh
  kernel holding none of its parent's names, the same bindings, and no `runtime.channels["user"]`.
  `model` names a configured model or alias, resolved as the session's own model is; omitted, it
  is the parent's; an unknown name fails the spawn with the configured names listed, and no child
  is made. `inputs`, a dict of names to values checked against the serializable subset, is bound
  as variables in the child's namespace before its first round, with a manifest -- each name, its
  type and a bounded preview -- in its instructions. `requests` makes a request child; without it
  the child is a work child. The child's orientation leaves out the user channel and says how to
  answer: `runtime.complete` in a work child, `receive` and `reply` in a request child. Skills
  reach a child once `0003-27` adds its import finder, which serves the whole interpreter.
- **A work child takes submissions.** `child.submit(inputs, result=T)` binds `inputs`, a dict of
  names to values, as variables in the child's namespace; `work.md`'s example, one dataclass
  instance, is the case of a single input, and the final spelling is this task's. Values are
  checked against the serializable subset, so a hosted reference is refused; the child reaches the
  same bindings by name. `T` is declared through `0003-24` at the call. The submission's round
  opens with each input's name, type and a bounded preview, and with `T`'s schema text. It returns
  a handle. A work child runs one submission at a time (fork 4), answers with `runtime.complete`,
  and has no request channels; a decorated call (`0003-26`) is one submission to a fresh work
  child.
- **A request child answers requests.** `requests` maps channel names to
  `outrig.Request(request_type, reply_type, doc=...)`, or to a bare `(request_type, reply_type)`
  tuple, the same without a description; both types are declared through `0003-24`, and `user` is
  not a channel name. The parent's end is `child.channels[name]`: `request(body) -> handle` checks
  `body` against the request type and sends it, and `pending()` counts the requests the child has
  not yet taken, as every endpoint's does. The child's end is `runtime.channels[name]`: `receive()`
  gives a request delivery with `id`, `body`, `sender` and `received_at`, and `reply(value)` and
  `fail(message)` settle that request -- `reply` decodes `value` against the reply type on the
  parent's side as a completion is decoded, and `fail` settles the handle with `AgentRequestFailed`
  carrying the message. `send` on the child's end raises: that direction carries replies, not
  messages. A request child has no `runtime.complete` and takes no `submit`; its instructions carry
  each channel's description with its request and reply schema text, and say how to receive and how
  to answer. The channel's queue is `messages.md`'s, bounded at 256 unread; a request past the
  bound, per fork 10.
- **The handle**, one per submission or request, with `work.md`'s rules: a stable id; it is
  awaitable and settles exactly once, and awaiting it again returns the same value or raises the
  same exception; `done()` and `result()` read it without awaiting; `future` is a shielded view
  that `asyncio.wait` accepts, so cancelling a waiter does not cancel the work; `runtime.wait`
  accepts a handle directly, and `asyncio.gather` takes one; and `h.cancel()` cancels the work
  without ending the child -- a request the child has not received is removed, and one it has
  received settles cancelled at once, after which its `reply` raises `RequestCancelled`.
  - **Progress**: `h.progress` on a submission's handle, a one-way endpoint yielding `Delivery`
    envelopes of what the child sends on its progress channel (`messages.md`). Nothing sent there
    settles the result; only the completion call does, and `runtime.wait` over the handle ends
    when the handle settles, not when progress arrives. A request's handle has no progress
    (`plan/next/progress-channels-on-agent-classes.md`).
  - **Status**, read without awaiting and separate from the result. A submission is queued behind
    another submission (fork 4), running, accepted, or failed -- rejected, cancelled, released,
    out of budget, or closed; a request is waiting for the child to receive it, received, accepted,
    or failed the same ways; and a child is running, idle, wedged (fork 7) or released. A settled
    handle's status does not change again. The spellings are this task's.
- **Release**, explicit and all-or-nothing across a list: a batch naming a child twice, or one
  this agent did not spawn or has already released, is refused whole, as
  `outrig__subagent_release` refuses one today.
- **Releasing a child closes its subtree** as a session close does (`lifecycle.md`, "Releasing a
  child"). Its work is cancelled -- the running submission, and every request waiting or received
  and unanswered -- and every waiter on it settled with `AgentReleased`; its own children are
  released the same way; its hosted calls waiting for an approval or an evaluation are cancelled
  through `0003-22`'s gate and the handler's signal; its running execution is interrupted, and a
  child that never yields is wedged (fork 7); and the hosted calls it has dispatched run on in
  their bindings and are reported with their outcome, or `unknown`. What becomes of its kernel and
  of background tasks still running there, per fork 9. A decorated call's child is released this
  way once its call settles (`0003-26`), and an agent class's child when its instance is released
  or collected (`0003-29`).
- **A Rust runner per child**, running its rounds through the session's round loop, with the
  child's own history store and its own events.
- **The attempt limit, per request, counting invalid answers only.** `await
  runtime.complete(value)` in a work child and `await d.reply(value)` in a request child decode
  with `0003-24`; problems are raised in the child's execution with their JSON paths, so the child
  can fix the value and answer again. Each invalid answer counts one attempt against its request,
  and past the limit, 3 by default, that request's handle settles with `CompletionRejected`,
  carrying the last value and its problems, and the child is not asked about it again. Earlier
  executions are never re-run. An answer to a request that has settled -- a second `complete`, a
  `reply` after `h.cancel()` or after the limit -- raises in the child and is evented, and never
  changes the settled result (fork 3). How a child builds its result, per fork 6.
- **One trailing round, counting nothing.** A round that ends with its submission unanswered, or
  with requests announced and unread or received and unanswered, is followed by one round asking
  for them by id, with how long each has waited. If that round ends the same way, the child goes
  idle: no further round is started for those requests, and each stays open -- its handle
  unsettled, its status saying the child is idle -- until `h.cancel()`, a `runtime.wait` timeout
  in the caller, release, or the close. There is no wait budget (fork 13). A later round, opened
  by a new request or submission, names the open ones again.
- **A request child's rounds.** A request arriving while the child is idle starts a round, as a
  user message starts the main agent's (`execution-and-rounds.md`). One arriving during a round
  is announced on the next result by the announcer `crates/outrig/src/agent/channel.rs` already
  runs, whose noun "messages" becomes "requests" for a request channel. `runtime.wait` in the child
  is ended by an arriving request, since input wins. A round that ends with requests unanswered
  gets the one trailing round above; `Announcer::keep` counts what a round announced as told once
  the round ends well, so without it neither an unread nor a received request would open one.
- **Spend, attributed and added up** (`typed-agents.md`, `observability.md`). A child's model usage
  is attributed to the child's round -- its round events carry its `subject` -- and added to the
  total of each round above it in the tree, up to the main agent's round whose execution made the
  outermost call, reported per fork 8. A round that answers several requests is attributed to the
  round and not divided among them (fork 12); `agent.request.settled` names the rounds the request
  spanned, so a reader can see what was spent while it was open. `0003-28` adds the skill
  invocation the work ran under to the same chain. The evaluator's usage is never added
  (`0003-23`).
- **Depth.** `subagent-depth-max` is enforced at spawn, the main agent being depth 1: a spawn
  from an agent already at the limit is refused rather than made to wait, since waiting cannot
  change a depth.
- **`children-max`**, default 64: a session-wide cap on resident children, checked at spawn
  before any kernel is made. Resident means in the session's record of children -- launching,
  working, idle, and wedged (fork 7) until the session ends, since a wedged kernel is still a
  thread and its memory; a released child leaves the count when its kernel is gone (fork 9), not
  when the release is requested. A launch past the cap raises `AgentLimitReached` at once and
  never waits: `spawn` raises it, a decorated call's handle settles with it (`0003-26`), and an
  instance's `ready()` and first handle raise it (`0003-29`); the record shows no spawn for it.
- **`model-concurrency-max`**, default 8: a session-wide bound on model requests in flight, with
  a cancellation-safe queue. A permit is held per provider request -- one attempt, from send to
  the end of its response or its failure, a retry taking a new one -- and for nothing else: never
  during an execution, so a parent whose code awaits a child holds none, and a tree deeper than
  the permit count completes. Every model request the session makes takes one, the main agent's
  rounds and the evaluator's calls (`0003-23`) included. A hosted call held for the evaluator's
  verdict is inside an execution and holds no permit, so no deadlock follows. A request made with
  every permit held waits in a queue in arrival order; a caller cancelled while queued -- its
  child released, the session closing -- is removed from the queue without a permit being leaked
  or the queue left inconsistent. The queue holds at most one entry per agent's round, and the
  evaluator's own cap (`0003-23`) bounds its entries, so `children-max` and that cap bound it.
  Neither this nor `children-max` bounds the CPU or memory a child's own code uses once its
  kernel is admitted; that is `runtime-protection.md`'s and
  `plan/phase/0003-python/potential/resource-scheduling.md`'s.
- **Two config keys for them**, top-level with `[agents.<name>]` overrides as
  `subagent-depth-max` has, read when the session starts; their validation ranges per fork 14.
- **A per-tree token budget**, a new config key whose name is this task's, covering what fork 5
  settles. A child that has spent it starts no further model call, and its work settles with a
  typed budget error -- never an empty result.
- **`outrig.AgentError`**, the base class of every error this machinery raises in agent code:
  `CompletionRejected`, `AgentReleased`, `AgentRequestFailed`, `RequestCancelled`,
  `AgentLimitReached`, the budget error and the closing error, whose spellings other than
  `AgentLimitReached` are this task's. `except outrig.AgentError` catches a child's failure
  without catching the caller's own bugs.
- **Events**, one family for submissions and channel requests, each an execution diagnostic with
  bodies as bounded previews: spawn; `agent.request.sent`, with the request's id, its channel or
  submission, and the sender; `agent.request.received`, when the child takes it;
  `agent.request.replied`, when the child answers; `agent.request.invalid`, with the attempt
  number and the problems; `agent.request.failed`, with `fail`'s message;
  `agent.request.cancelled`; `agent.request.settled`, with the outcome, the attempts and the
  rounds the request spanned; an answer refused because its request had settled (fork 3); and
  release, naming every child it ended and every request it settled. `agent.call.started` and
  `agent.call.settled` are `0003-26`'s wrapper around a decorated call, and nothing else emits
  them; there is no `agent.completion.*` family. A child's own executions and model rounds carry
  its own `subject`, so `0003-14` renders it as an agent, and its progress messages are evented as
  `0003-13` defines for messages between agents.
- **`Delivery.id`** on every delivery, the user channel's included -- the host already posts each
  message under an id -- so a request delivery's `id` is the field every other delivery has.
- **The child-work rows in `lifecycle.md`'s close table.** A close cancels each child's work: its
  round makes no further model call, a spawn after the close fails at once, refused by the
  session's admission gate, a request waiting or received and unanswered is cancelled, and an
  answer submitted but not yet accepted is not accepted. Every waiter is settled with the
  documented closing error -- `await h`, `runtime.wait` over `h` or `asyncio.wait` over
  `h.future`, and a re-await alike -- whose name is this task's. A result validated before the
  close stays settled with it, and nothing unvalidated is ever returned as a success. If the
  interpreter dies, its waiters die with it, and the shutdown report lists the work as terminated
  and its outstanding requests by id.

## Acceptance

- **A repair does not re-run earlier executions.** A child that runs two executions, completes
  with a wrong shape, and completes again after the problems shows each execution id once in the
  host's record.
- **The attempt limit yields `CompletionRejected`**, carrying the last value and its problems, for
  a work child that completes with a wrong shape three times and for a request child that replies
  with a wrong shape three times; no model call follows it, and a request child's other requests
  are still answered.
- **A child that stops answering leaves its request open, and spends nothing more.** A work
  child whose mock ends two rounds without completing -- the submission's round and the trailing
  one -- receives no third call; its handle is unsettled, its status says the child is idle, and
  `h.cancel()` then settles it cancelled. The same for a request child with one request received
  and unanswered; a new request to that child starts a round whose opening names both ids, and a
  reply to the old one then settles its handle with the value.
- **A 20-way `gather` runs 20 children at once under the token budget and
  `model-concurrency-max = 8`, and all complete.** Twenty coroutines that each spawn, submit,
  await and release, against a mock provider that records how many requests it holds open at
  once and holds each open until eight are: the host's record shows 20 live children at one
  moment, none waits to launch, the provider never holds more than 8 requests at once, and all 20
  settle with results within the budget.
- **The 65th launch fails at once.** With `children-max` at its default and 64 children resident,
  some idle and some mid-round, a 65th `spawn` raises `AgentLimitReached` with no spawn in the
  record and no wait; releasing one child lets the next spawn succeed. With `children-max = 2`,
  one child wedged (fork 7) and one released, one spawn succeeds and the next raises: the wedged
  child counts until the session ends, the released one does not.
- **Cancelling a queued model request frees its place.** With `model-concurrency-max = 1` and
  the one permit held by a mock request kept open, a second child's round is queued; releasing
  that child removes its request from the queue, and when the held request completes a third
  child's request is sent at once. The host's permit count reads 1 held, then 0, never less.
- **A parent awaiting a child holds no permit.** With `model-concurrency-max = 1`, a parent whose
  code awaits a child whose code awaits a grandchild completes, each level's execution waiting
  while holding no permit, and the record shows the three rounds' requests one after another.
- **An evaluation and a round take turns.** With `model-concurrency-max = 1` and a rule that
  sends the fixture binding's call to the evaluator (`0003-23`), the round's model requests and
  the evaluator's request are never in flight at the same time, and both complete: the
  evaluator's `allow` admits the call, the call returns, and the round ends.
- **An exhausted token budget raises a typed error** in the waiter, and the child makes no model
  call after the budget is spent.
- **Cancelling a waiter leaves the work running**: the work settles later, and a new await
  returns its result.
- `h.cancel()` ends the work item and not the child, which then takes another submission.
- **Progress never settles the result.** What the child sends as progress arrives on `h.progress`
  in order, as `Delivery` envelopes, while `h` stays unsettled and its status reads running, until
  the child completes.
- **Statuses follow the work**: a submission reads running while its child works, accepted once it
  completes, and failed after `CompletionRejected` or a spent budget, and does not change once
  settled; a request reads waiting until the child receives it and received until it answers; and
  a child reads idle between submissions and released after its release.
- **Submissions to a busy child queue** (with fork 4's recommendation): a second submission reads
  queued until the first settles, then runs; `h.cancel()` on a queued submission removes it, and it
  settles cancelled without the child running it.
- **An answer after settlement is refused** (with fork 3's recommendation): a second
  `runtime.complete` after the first was accepted, one after `h.cancel()`, and a `reply` to a
  request that has settled each raise in the child and are evented, and `await h` still gives the
  settled outcome.
- **`spawn(..., inputs=...)` binds each input by name** before the child's first round, whose
  instructions list them, and a hosted reference among the inputs is refused with no child made.
- **A request to an idle child starts a round.** A request child idle after its launch is sent one
  request while its parent awaits nothing else: the child's runner makes a model call, the child's
  code receives the request, and its reply settles the handle with a value of the reply type.
- **A request arriving during a round is announced on the next tool result.** A request sent on
  `fizz` while the child's round is running yields, in that round's next tool result, text
  containing `1 request is waiting on runtime.channels["fizz"]`.
- **Two requests on one channel are answered in order, with matching ids.** Both are sent before
  the child receives either: `receive()` gives them in send order, each `reply` settles the handle
  whose id it carries and no other, and the second handle is unsettled while the first is
  answered.
- `await d.fail("why")` settles the handle with `AgentRequestFailed` carrying `"why"`, and the
  child's round goes on.
- **`h.cancel()` before and after receipt.** Before the child receives the request, cancelling
  removes it: `receive()` never gives it, `pending()` no longer lists it, and the handle settles
  cancelled. After receipt, the handle settles cancelled at once and the child's `reply` raises
  `RequestCancelled`.
- **A request child has no `runtime.complete`, and a work child has no request channels.** In a
  request child, `runtime.complete` raises an error naming `reply`, and `child.submit` on it
  raises; in a work child, `runtime.channels` holds no request channel, and no endpoint of
  `child.channels` on the parent's side has `request()`.
- `send` on the child's end of a request channel raises.
- **Every delivery carries an `id`**: a user-channel delivery received by the main agent has one,
  and a request delivery's `id` is the id its handle carries.
- **The trailing round lists unanswered ids, once.** A request child whose round ends with one
  request received and unanswered and another announced and unread is prompted again with both
  ids and how long each has waited, and a `reply` to each then settles its handle. When the
  trailing round ends with both still open, no further call follows, and neither request counts
  an attempt: a valid reply to either later settles it with the value, and the record shows no
  `agent.request.invalid` for it.
- **Requests past the queue bound wait, and all settle** (with fork 10's recommendation): 300
  requests sent at once to a child that answers one per round all settle with matching ids, the
  handles past the bound read waiting until the child takes a request, and none is refused.
- **Handles work in `runtime.wait` and, through `.future`, in `asyncio.wait`.**
  `runtime.wait({h1, h2})` returns when one settles, with that handle in its done set;
  `asyncio.wait({h1.future, h2.future})` the same; and cancelling either wait leaves both requests
  running, and they settle later.
- **A release batch is all or nothing.** A batch naming one child twice, and one naming a child
  this agent did not spawn, each raise and release none of the batch: every other child in it
  still takes a submission.
- **A released child's subtree goes with it.** Releasing a child whose own child is working and
  whose execution waits on an escalated hosted call: the grandchild is released and its waiters
  settle with `AgentReleased`, the handler sees the cancellation signal and the call's target is
  never invoked (a host-side counter stays at zero), and the child's own waiters settle with
  `AgentReleased` -- a request it had received and not answered among them. With fork 9's
  recommendation, a background task the child had started is cancelled, and its kernel is gone.
- **`model=` resolves through config**: a spawn naming an alias sends the child's model calls to
  the model the alias resolves to, asserted at the mock provider, and an unknown name fails the
  spawn with the configured names and makes no child.
- **Spend adds up, once each.** A parent whose child spawns a grandchild, against a mock provider
  that reports fixed usage per call: the grandchild's usage is in its own round events and in the
  total of the child's round that spawned it, the main round's total is the main agent's own usage
  plus each descendant's, each counted once, and each request's settled event names the rounds it
  spanned.
- **At shutdown, `await h`, `asyncio.wait` over `h.future`, and a re-await all raise the
  documented error, and none hangs**, each under a deadline in the test, for a running submission,
  a request waiting and a request received; a result validated before the close is still
  returned, and a spawn after the close fails at once.
- A spawn from an agent at `subagent-depth-max` is refused with a typed error.
- A child has no `runtime.channels["user"]`.
- **Every failure of a wait is an `outrig.AgentError`**: `except outrig.AgentError` around
  `await h` catches `CompletionRejected`, `AgentReleased`, `AgentRequestFailed`,
  `AgentLimitReached`, the budget error and the closing error, and a `TypeError` from the caller's
  own code passes through it.
- A `children-max` or `model-concurrency-max` outside fork 14's range is refused when the config
  loads, naming the key and the range, at the top level and under `[agents.<name>]` alike.
- `crates/outrig/public-api.txt` regenerated, its additions limited to the three config fields --
  the token budget, `children-max` and `model-concurrency-max` -- and the new events.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether a child inherits the operator's `preamble`, `tool-call-max` and `tool-result-max` --
   Recommended: the limits yes, the preamble no**, as `run`'s subagents do (the table in
   `doc/concepts/subagents.md`). A tool limit bounds every agent in the session; the parent's
   `prompt` is how a child learns what it needs, and the parent can pass on any project rule from
   the preamble. Inheriting the preamble would add every project rule to every child's
   instructions, at the cost of context in each one.
2. **Whether the request-child plumbing moves to `0003-29` -- Recommended: no.** With
   `spawn(..., requests=...)`, the request channel, the handle, the attempt limit and the request
   events built here, the class is a thin layer: it derives the channels from the methods and owns
   the instance's lifetime. Building the plumbing in `0003-29` would put one machinery's tests in
   two tasks, and the explicit form is usable without a class, as `work.md`'s explicit API is
   usable without the decorator. If this task is too large, the depth limit, the token budget,
   `children-max` and `model-concurrency-max` are the parts that can move to a task of their own
   between this one and `0003-26`, numbered by `/groom-plan`, with the acceptance items that test
   them.
3. **The completion call -- Recommended: `await runtime.complete(value)`, and `await d.reply(value)`
   the same way, each returning once the value is accepted and raising with the problems
   otherwise.** Waiting for acceptance lets `0003-26`'s validator, which runs in the parent, answer
   through the same call the decoder does. An answer after the handle has settled raises and is
   evented, rather than revising the result: `work.md` allows recording it as an update or
   refusing it, and refusing is the simpler of the two that cannot change a settled result.
4. **A second submission to a busy child -- Recommended: queued in submission order**, with a
   status that says so, since a child has one execution slot. Refusing would make every parent
   write the queue itself. `h.cancel()` on a queued item removes it.
5. **What the token budget covers -- Recommended: one budget per session, counting every model
   call of every child and their descendants, and not the main agent's rounds**, which the user
   starts and can stop. A budget per `spawn` would not bound a loop that spawns. The evaluator's
   usage is `0003-23`'s and is not counted.
6. **How a child builds its result -- Recommended: the result type and the dataclasses it uses
   are bound into the child's namespace by their class names, beside the inputs, and the
   completion call takes an instance or its JSON form.** `typed-agents.md` leaves this to this
   task. Either form is encoded as JSON and decoded strictly into new instances on the parent's
   side, so the child's object is never handed over, as `messages.md` requires of every value
   between agents. Binding the classes lets a child that found its problems with code complete
   with the structure that code built. Because the child reaches a class by its `__name__`, two
   distinct classes with one `__name__` -- among one submission's types, or across a request
   child's channels and inputs -- are an error at `submit`, `spawn` or `request`, naming both,
   rather than one replacing the other without notice; `0003-29`'s generated message types are
   bound and checked the same way.
7. **A wedged child -- Recommended: its status says it is wedged, and it stays in the session's
   record of children until the session ends.** Its thread cannot be interrupted
   (`runtime-protection.md`), so nothing can reclaim it; the status and the shutdown report are how
   an operator learns the thread is there, and it counts against `children-max` for as long as it
   does, since the limit counts kernels that exist and not a marker.
8. **How a round's total counts spend that arrives after the round yields -- Recommended: the
   round's total is what its tree had spent when it yielded, and each later addition is published
   as a usage event naming the round.** A child running in a background task can outlive the round
   that started it (`execution-and-rounds.md`), so a round's total cannot be final when the round
   yields. Holding the round's end until its tree settles would make a round wait on work the model
   chose not to await, and adding later spend to whichever round is running when it is spent would
   charge a round for work it did not start.
9. **What a release does to the child's kernel -- Recommended: every task on its event loop is
   cancelled, background tasks included, and the kernel is removed once its loop stops.**
   `lifecycle.md` leaves this to this task. A session's close ends every task with the
   interpreter; a release leaves the interpreter running, so the child's tasks have to be cancelled
   to end the same way. A background task left running would go on making hosted calls and
   launching children for an agent that no longer exists, with no one to collect what it does. A
   task that never yields cannot be cancelled on a child's thread, so the thread runs on until the
   session ends, and the child's status says wedged (fork 7).
10. **`request()` when the child's queue holds 256 unread -- Recommended: the request waits.** It
    is held on the sending side with a status that says so, `request()` still returns its handle
    at once, and it is delivered when the child takes one, so a parent that sends faster than the
    child answers is slowed and nothing it sent is lost. The alternative is to refuse, as a host
    post into a full endpoint is refused today (`_Refused` in `interpreter.py`), which would make
    every parent that fans requests out write its own queue -- the reason fork 4 queues
    submissions.
11. **Where `reply` and `fail` live -- Recommended: on `RequestDelivery`, a subclass of `Delivery`
    for request deliveries, so a user-channel or progress delivery has neither method.** `reply` on
    a delivery that answers nothing could only raise, and `help(d)` on a request delivery then
    documents the two calls that apply. The alternative, both methods on every `Delivery` and
    raising where there is no request, keeps one class at the cost of two methods that fail on most
    deliveries. The subclass's name is this task's; `outrig.Request` is the declaration, so the
    subclass is not spelled the same.
12. **Spend of a round that answers several requests -- Recommended: to the round, with each
    request's settled event naming the rounds it spanned.** Any division of one round's usage
    among the requests it answered would be invented. The rounds spanned let a reader see what was
    spent while a request was open; the round's total stays whole.
13. **Whether an invalid answer has an attempt limit -- Recommended: yes, 3 by default, shared by
    `complete` and `reply`, counting invalid answers and nothing else.** The trailing round counts
    nothing, and there is no wait budget: a child whose round ends with a request unanswered gets
    one trailing round naming the ids, and if that ends the same way the child goes idle and the
    request stays open until its caller cancels it or a `runtime.wait` timeout ends the wait.
    Revised on 2026-10-02: the earlier rule counted each trailing round against every request it
    listed, which made a child serving a batch in order fail its later requests for the rounds it
    spent answering the earlier ones correctly. The maintainer's reasons for dropping it rather
    than adding a separate wait budget: spending rounds carries no real penalty, since the token
    budget bounds spend; a long wait may be legitimate, because a request can wait on a build or a
    test run; and the child's own long waits happen inside an execution, where no round ends, so
    a round ending with a request open says nothing about whether the child is working on it. The
    alternative is no limit at all, leaving an invalid answer to be repaired until the caller
    cancels. A limit makes a child that cannot produce the shape fail finitely and by name
    (`CompletionRejected`); a caller that wants patience can raise it per request, which is a
    parameter of `request()` and `submit()` under this choice.
14. **Validation ranges for the two limits -- Recommended: `children-max` from 1 through 1024 and
    `model-concurrency-max` from 1 through 128**, checked where `subagent-depth-max`'s range is
    (`config/validate.rs`), at the top level and under `[agents.<name>]`. A thousand resident
    kernels at about 16 MiB each is 16 GiB, past which the interpreter's memory ceiling is the
    real limit; 128 requests in flight is past what any one provider key is likely to allow. The
    alternative is no ceiling, leaving an operator's memory and the provider's rate limits as the
    only check on a typo.

## Dependencies

- **Hard: `0003-13`.** A child's events go to the stream, under the child's own `subject`.
- **Hard: `0003-19`.** The runner is the session's round loop run for a child, and close and
  shutdown are the session's.
- **Hard: `0003-22`.** Releasing a child cancels its held requests through that task's gate and
  the handler's signal. Through it, `0003-21`, whose binding stubs in every kernel give a child its
  parent's bindings, and whose dispatched calls a release lets run on or reports `unknown`.
- **Hard: `0003-23`.** The evaluator whose call takes a permit, which the acceptance item on
  `model-concurrency-max = 1` exercises.
- **Hard: `0003-24`.** Completions and replies decode with `outrig.schema`, and a submission's or
  a request channel's instructions carry its schema text.
- **Soft: `0003-15`.** A child resolves its own model, alias failover included, and its rounds
  retry as the main agent's do.

## See also

- `plan/phase/0003-python/work.md` -- the two child kinds, the handle, a request settling once,
  and limits outside generated code.
- `plan/phase/0003-python/typed-agents.md` -- completion as a Python call, repair, the decoder,
  and how spend is attributed.
- `plan/phase/0003-python/messages.md` -- request channels, `Delivery.id`, and the bounded queue.
- `plan/phase/0003-python/agent-classes.md` -- the class form, a thin layer over this task's
  request child.
- `plan/phase/0003-python/lifecycle.md` -- releasing a child, and the close-state table this task
  adds rows to.
- `plan/phase/0003-python/agent-placement.md` -- children are kernels of one interpreter.
- `doc/concepts/subagents.md` -- `run`'s subagents, whose width cap `run-new`'s children are not
  under.
- `crates/outrig/src/config/validate.rs` -- where `subagent-depth-max`'s range is checked, and
  where the two new keys' ranges go.
- `crates/outrig/src/agent/channel.rs` -- `Announcer::keep`, which is why an unanswered request
  needs the trailing round to get another round at all.
- `plan/phase/0003-python/potential/resource-scheduling.md` -- the scheduler for children working
  at once that this task does not build.
- `plan/next/children-have-a-user-channel.md` -- the user channel children do not get.
