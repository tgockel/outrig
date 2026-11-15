# 0003-48 -- The owner reads an agent's stop

## Context

A child's runner decides when a round opens and what the child is doing, and keeps that in state
nothing outside reads. `runner::run` holds the baseline summary, whether the child's model is
broken, and the backoff after a failed round (`crates/outrig/src/agent/runner.rs:64-69`); when a
round ends well it keeps the live summary as the baseline and discards the round's `Finished`
(`:187-196`); when the model call fails before any code ran it warns and backs off from two
seconds, doubling to sixty, and opens the round again (`AgentError::Prompt`, `:166-182`;
`RETRY_FIRST`, `RETRY_MAX`, `back_off`, `:48-50`, `:222-225`); when it fails after code ran it
backs off and decides again (`:198-209`); on any other failure it marks the model broken, ends
every request the child holds and goes idle (`:66`, `:77-99`, `:212-216`). `Finished` is the
round's final text, why a limit cut it short, and its final reasoning
(`crates/outrig/src/agent/round.rs:41-50`); for a primary agent `Session::round` returns the same
as `RoundEnd` (`crates/outrig/src/harness/session.rs:591-597`), and a `no_primary` session's
`round()` answers `NoPrimary` (`:350-353`). The registry keeps each agent's `Life` -- launching,
running, idle, wedged, released -- the round it is in, its model and its requests under one lock
(`crates/outrig/src/agent/children.rs:78`, `:99-134`), moves them as the runner reports
(`round_opened`, `round_ended`, `idle`, `:1130-1173`), and tells the child's parent kernel the
word for its life, which is what a Python handle's `Status.child` reads (`tell_life`,
`:1175-1194`; `crates/outrig/src/python/outrig/_agents.py:164-180`). A failure's class is
decided once, beside the loops that retried it: `is_recoverable` says whether a failure may
clear without a change to the configuration (`crates/outrig/src/agent/retry.rs:1213-1246`),
`prompt_failure_text` renders it (`:1203`), and the round fails `Prompt` or `PromptTerminal`, or
their after-work forms (`crates/outrig/src/agent/mod.rs:390-408`). A send the agent makes reaches
the host as a `Sent` reply and is recorded as `message.sent`
(`crates/outrig/src/python/host.rs:2232-2240`), an event and not a record the read may use; on a
channel the host does not read it is refused (`:2243-2255`), and a root has no `user` channel.
The report lists each request
open at the close and how it ended, `terminated` when the interpreter died first
(`crates/outrig/src/harness/report.rs:113-126`; `children.rs:266-287`). Events describe each of
these -- `model.round.completed`, `model.round.failed`, `model.retry`, `model.failover`
(`crates/outrig/src/harness/event.rs:535-592`) -- on a stream that is bounded and lossy by design
(`doc/reference/events.md`, "Durability and loss").

OR-A6-1 asks for an owner-awaitable, authoritative read of why an instance is at rest, as a small
generic set each with its round and attempt, delivered reliably rather than through a
subscription, with task completion not in the set; OR-A6-2 for the per-round facts an owner's
reply policy needs -- the round's final text and the deliberate sends on each external channel
by name, count and item id -- with OutRig implementing no policy; OR-A6-4 that retrying is
distinguishable from broken, with the failure class, the first failure's time and the next
attempt. The planning's `decisions.md`, section 4, rejects generic stop-cause machinery and an
application-specific enum, and `stop-events-clarification.md`, section 3, records that the
runner's backoff is tracing-only and called more telemetry no requirement; A-06 asks for the
read, not the telemetry, because an owner that cannot tell retrying from broken either kills live
work or waits on dead work. `rust-owner.md`, "The stop read", "Retrying is distinguishable from
broken" and "Per-round facts", record the design.

`0003-51`, which follows, records each send against the holder's round and adds the round's sends
to the read this task defines; until it lands the `sends` field is empty, and this task's docs
say so. `0003-50` adds the `limit reached` state and `0003-49` reads this task's state into the
roster.

## Goal

The owner's control handle gives an awaitable, authoritative account of why its instance is at
rest -- with the round and the attempt that decided it -- and of what each round said and sent,
served from the host-side state the runner holds, under its lock, and never from the stream.

## Deliverables

- **The state.** A public enum of what an instance is doing, with the states `rust-owner.md`
  tables: `launching`, `running { round }`, `unavailable { class, since, next_attempt, round }`,
  `idle { round }`, `refused { round, attempt }`, `failed { class, round, attempt }`,
  `released`, `cancelled` and `terminated`; `limit reached` is `0003-50`'s to add, and the enum
  is `#[non_exhaustive]`. A state at rest carries the round it belongs to and, where one decided
  it, the provider attempt's id as `model.attempt` names it. Task completion is not a state.
  Spellings are this task's.
- **The read**, on `RootHandle` (`0003-46`), per fork 1: `status() -> (seq, state)`;
  `changed(seq)`, which resolves once the sequence has advanced past `seq`; `next_stop(seq)`,
  which resolves with the next state at rest after `seq`, so a `seq` read before a request is
  sent loses no stop to a round that ends first. A `tokio::sync::watch` beside the registry's lock
  carries the sequence, and every change is made under the lock.
- **Where the state comes from.** The runner records into the registry, under its lock: the round
  opened (as today) and, new, the round's `Finished` as it ends, kept rather than discarded; the
  failure's class, the first failure's time of the current run of failures, and the next attempt
  before it backs off; the terminal failure's class when it marks the model broken; and the rest
  when it waits. `oriented`'s error, a release, the launch given up and the interpreter's death
  each set their state. The read is served from that state and from nothing else: no event is
  consulted, and the interpreter is never asked.
- **Retrying against broken** (OR-A6-4). A `Prompt` or `PromptAfterWork` failure whose
  `is_recoverable` holds puts the instance in `unavailable`, with the class the attempt's label
  gives -- server error, rate limited, connection refused, as `prompt_failure_text` renders them
  -- and `next_attempt` the time the runner will reopen; a `PromptTerminal` or
  `PromptAfterWorkTerminal` failure, or any other error the runner gives up on, puts it in
  `failed` with the class, and no next attempt. A round that ends with an error in the agent's
  own Python is neither: its execution's error was a result the model read, and the round ended.
- **Per-round facts** (OR-A6-2), per fork 2: `RoundFacts { round, reply, stopped, reasoning,
  sends, any_send }`, where `sends` lists each external channel the round sent on by name with
  the item ids `0003-51` mints, in order, and `any_send` is whether the list is non-empty. The
  sends are the external channels' registry's, which records each send against the holder's round
  it was made in (`external-channels.md`, "Sending", "Positions, the registry and the close"),
  never `message.sent` events'. This task lands before `0003-51`, so it defines the read with the
  text and a `sends` field that is empty, and its docs say so; `0003-51` adds the round's sends
  from its registry, and `0003-53` reads them by round. The facts of the last ended round ride on
  the state at rest, and `rounds()` returns a bounded history, the bound fork 2's. Nothing
  delivers the text anywhere; the owner's policy runs outside OutRig.
- **No dependence on the stream.** The reads give the same answers with no subscription, and
  with a subscription that never reads.
- **Events and the close table.** No new event: the reads are the authority and the stream the
  diagnostic surface; `doc/reference/events.md` gains a sentence under the `model.round.*`
  entries saying the owner's authoritative account is the control handle's. No close row: a read
  closes nothing. The task's `## Decisions` says both.
- **`## Decisions` records**: the enum's spelling and that completion is not in it; fork 1's
  shape; that `sends` is the registry's and empty until `0003-51`; the bound on retained rounds;
  the mapping from provider finish reasons to `refused` (`rust-owner.md`, "Open questions"); and
  that the runner's backoff stays unevented here, with `diagnostics.md` named for the question.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a Rust test with a mock model, against a root `0003-46` constructs.

- **Refusal.** The mock refuses on content policy twice: `next_stop()` resolves `refused` with
  the round and the attempt; the request the round opened on stays open and nothing more is sent;
  a request on a declared channel afterward opens a round that the mock answers, so the instance
  was resumable.
- **Unavailable.** The mock returns 503 past the retry budget: the state is `unavailable` with
  `class` naming the server error, `since` the first failure's time and `next_attempt` between two
  and sixteen seconds out; the request that opened the round stays open; when the mock recovers,
  the reopened round answers it and the state becomes `idle`.
- **Idle.** A round that ends with nothing outstanding gives `idle` with its round number; a
  request child of the root that `0003-37` host-cancels reads `idle` after the cancel.
- **Failed.** A 401 gives `failed` with its class and no `next_attempt`; the requests the root
  held ended `failed` as `runner.rs:88-95` ends them; `next_stop()` resolved once and `changed`
  does not resolve again until a release.
- **Released, cancelled, terminated.** `release()` gives `released`; a release while launching
  gives `cancelled`; an interpreter that dies under the root gives `terminated`, and the report
  lists the root's open requests `terminated`.
- **No stream.** Every case above passes with no subscriber on the builder, and with a
  subscription of capacity 1 that is never read.
- **Per-round facts.** A round that narrates: `rounds()` shows the narration as `reply`, zero
  sends and `any_send` false; nothing delivers the narration anywhere. The `sends` field is empty
  for every round here, by construction; `0003-51`'s acceptance has the round that calls
  `send("done: see PR 12")` on the external user channel and also narrates show one send with its
  item id beside the text.
- **Attempts agree with events.** With a subscriber that reads everything, each state at rest's
  `round` and `attempt` equal the `model.round.*` and `model.attempt` events' ids for that round.
- **Served under the lock.** The read answers while a kernel is wedged in `while True: pass`
  and while the interpreter is not answering: it never awaits the interpreter.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **One awaitable, or a status with a sequence -- Recommended: both over one watch.**
   `status()` and `changed(seq)` let an owner that keeps its own loop wait for any change;
   `next_stop(seq)` is the common case. The alternative offers `next_stop()` alone, which loses a
   change from `unavailable` to `running` an owner may want.
2. **Where the per-round facts live -- Recommended: on the state at rest for the last ended
   round, plus `rounds()` over a bounded history of 16 rounds.** The alternative is a separate
   read only, which makes the common case two calls. What decides the bound: the owner reads
   each round's facts as it ends, so a long history serves only an owner that fell behind.
3. **Whether the runner's backoff is also evented -- Open; `diagnostics.md`'s.** The read does
   not need it; the renderer may.

How a send made while no round is open is attributed is the registry's rule
(`external-channels.md`, "Sending"; `0003-51`), and the read reports what the registry records.

## Dependencies

- **Hard: `0003-46`.** The control handle the read is on, and the root it reads.
- **Hard: `0003-30`.** The runner, the registry and the request machinery whose state this
  reads.
- **Soft: `0003-37`.** `idle` means nothing outstanding, which `Registry::idle` asserts there.
- **Soft: `0003-43`, `0003-44`.** The application and the service the acceptance's root uses.

## See also

- `plan/phase/0003-python/rust-owner.md` -- "The stop read", "Retrying is distinguishable
  from broken", "Per-round facts".
- `plan/phase/0003-python/external-channels.md` -- "Sending", for the item ids.
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds";
  `plan/phase/0003-python/observability.md` -- the stream's rules.
- `plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md` -- idle means nothing outstanding.
- `crates/outrig/src/agent/runner.rs`, `children.rs`, `round.rs`, `retry.rs`, `mod.rs`,
  `crates/outrig/src/harness/session.rs`, `report.rs`, `crates/outrig/src/python/host.rs`.

## Decisions

Forks 1 and 2 were taken as recommended, with `next_stop` taking a `seq` (below); fork 3 stays
open.

- **The spellings.** `harness::AgentState` -- `Launching`, `Running { round }`, `Unavailable {
  class, since, next_attempt, round, attempt }`, `Idle { round }`, `Refused { round, attempt }`,
  `Failed { class, round, attempt }`, `Released`, `Cancelled`, `Terminated` -- with
  `at_rest()`; `Status { seq, state, last }`; `RoundFacts { round, reply, stopped, reasoning,
  sends }`; `RoundSend { channel, items }`. The enum and each struct variant are
  `#[non_exhaustive]`, for `0003-50`'s `limit reached` and whatever a state gains. Task completion
  is not a state: it is the application's operation on a service it bound, and nothing here
  infers it. `any_send()` is a method, not a field, so it cannot disagree with `sends`. Times
  are `SystemTime`, as `Event::time` is.
- **`Launching` lasts until the first round opens**, not until the orienting round ends:
  `rust-owner.md`'s table says the latter, but then a root retrying its orienting round would
  read `launching` between `unavailable`s, and `cancelled` -- "given up before its first round"
  -- could not be told from a release during it. `ready()` is the read of the orienting round's
  end. A release while `Launching` reads `Cancelled`; a handle whose root was never launched
  (its construction failed, or the close came first) reads `Cancelled` too, `ready()` saying why.
- **Fork 1: `next_stop(seq)`, the maintainer's choice.** A request reaches the root
  asynchronously, so right after `request()` the status can still be the previous `idle`, and a
  fast round can end before an argument-less `next_stop()` is called. Taking the `seq` read
  before the request is race-free either way, and matches `changed(seq)`. Each read shows the
  state at that moment; one the root passed through between reads is not kept.
- **A watch per child, and one counter.** The registry keeps `State::seq`, the count of every
  child's state changes (which `0003-49`'s roster reads), and each `ChildNode` a
  `watch::Sender<Snapshot>` -- `seq`, state, a root's kept rounds -- sent under the state's lock
  by one helper, `ChildNode::say`, whose only rules are that a final state is never left and the
  state the child is in already is no change. The handle holds a
  receiver: a wait wakes only for its root, a read takes no registry lock and awaits nothing,
  and the last status outlives the session. Once the registry is gone, `changed` and
  `next_stop` return the last status at once rather than waiting forever.
- **What moves the state.** `launch` -> `Launching`; `open_round` -> `Running`; the runner ->
  `Unavailable`, `Refused`, `Failed` (`Children::stopped`); `idle()` -> `Idle` only from
  `Running` or `Unavailable`, so the runner's scheduling idle never hides a refusal or a broken
  model; `mark_released` -> `Released` or `Cancelled`; the close -> `Terminated` when the
  interpreter exited, else `Released`. The runner says `Failed` once, as it marks the model
  broken, and a spent budget only when nothing broke it first; no round opens after either, so
  after a 401 the requests ending, a later request ending `failed` and the runner idling change
  nothing, and `changed` resolves next on the release. `Life` stays the scheduler's and
  `tell_life`'s words are unchanged.
- **`class` is the failed request's own label**, as its `model.attempt` event's `error` says it
  -- `HTTP 503 Service Unavailable`, `HTTP 429 Too Many Requests`, `connection error: ...`,
  `unusable response (...)` -- which `CallSlot` now keeps with the attempt's id for the whole
  call (`CallSlot::last_recorded`), whichever of an alias's models the request went to: a
  fallback passed over without a request, too small for the call, leaves the head's failed
  request as the call's, where the candidate's own record (`last_attempt`) is cleared as the
  call moves. A new call starts with none. The round's `AgentError` text carries rig's
  rendering with the provider's body, which `failure_label` keeps out on purpose. No taxonomy
  is invented: the variant says retrying or broken, and the label is the detail. A failure no
  request was sent for, and one that is no model call's -- the interpreter did not answer, a
  kernel ended -- is `Failed` with the error's text and no attempt. A spent token budget is `Failed` too, with no
  attempt; `round` is `None` only when no round opened.
- **`unavailable`.** `since` is the first failure of the current run, cleared by a round that
  ends or is refused; `next_attempt` is now plus the wait the runner is about to sleep. Both a
  failure before the round's code ran (reopened) and one after (decided again) read so; the
  fifth in a row before code ran is `Failed`, as the runner's `broken` was.
- **Refusals.** The mapping for `rust-owner.md`'s open question: a reply the provider ended
  with `content_filter` (chat completions, and a Responses reply `incomplete` for it) or
  `refusal` (messages) -- `is_content_refusal`, which `CallSlot::filtered` records of the call's
  last request -- is `refused`; `length`, `max_tokens` and `max_output_tokens` stay unusable
  replies, retried and then `unavailable`. The round classifies it, in `stopped_short`, where
  it knows the slot's record is about the call that failed (only a failed call's error is):
  `AgentError::Refused` holds the `Prompt` or `PromptAfterWork` it would have been, worded as
  that one, so the primary's message and `Failure::ran_python` are unchanged. A child's runner
  retries only `Prompt`, so a refused round is not reopened, does not count toward
  `ROUND_FAILURES_MAX`, and does not break the child.
- **What a refusal leaves open: the requests stay open (the maintainer's choice).** The runner
  rests on the requests the child holds as the refused round ends, by id, waiting or taken, and
  opens nothing -- the warning included -- until that set changes: a new arrival, a cancel, an
  end. What the refused round itself did with them is no change: taking a request, or a work
  child being given its submission, moves it from waiting to taken, which the summary the usual
  rule compares counts as a change, and would open a round at once with nothing new (the first
  build compared that summary, and did). A request that arrived while the refused round ran is
  no request the round was refused for, so it opens the next round at once. The alternative
  ended them `failed`, which a waiting sender would hear at once but which takes the choice from
  the owner.
  This holds for every child, so a parent kernel awaiting a refused child now waits until it
  cancels or something changes, where before it got `failed` after five retries.
  `doc/concepts/python-runtime.md`'s "worked on or cancelled" says so. An orienting round so
  refused has ended, as one that failed after its work has, so `ready()` resolves and the state
  reads `refused`. A refusal sets no `cut`: a work child rests on its submission until that ends,
  so no host cancel of it follows that `cut` could be the cause of.
  `a_cancel_chains_the_round_an_unusable_reply_failed` keeps the chained cause, through a round
  an unusable reply failed after its work.
- **Fork 2: the last ended round's facts ride on `Status::last`**, the latest round that ended
  with a reply; its `round` says whether it is the state's (a refused or failed round has no
  reply). `rounds()` keeps 16, oldest first, in the root's snapshot; a child an agent's code
  launched keeps none, since nothing reads them.
- **`sends` is the external channels' registry's, and empty until `0003-51`.** `RoundSend`'s
  `items` are `String`s, which `0003-51` may retype to the id it mints; 0.3.0 is unreleased.
- **No event, no close row, and the backoff stays unevented.** The reads are the authority and
  the stream the diagnostic surface; `doc/reference/events.md` says so under `model.round.*`.
  Whether the runner's backoff is also evented, for the renderer, is `diagnostics.md`'s
  question (fork 3); the read does not need it. A read closes nothing.
- **Tests.** The refusal case's round takes its request before the provider refuses, and
  `a_submission_given_to_a_refused_round_waits_for_its_sender` refuses a work child's opening as
  it is given its submission; each fails against the first build's rest, which compared the
  summary. `a_calls_last_request_outlives_a_candidate_that_sent_none` is the alias case.
  `agent/stop_tests.rs` runs each case under three watches -- every event gathered,
  no subscription, one of capacity 1 never read (`children_tests::Watch`) -- and checks rounds and
  attempts against `model.round.*` and `model.attempt` where the events are gathered. The
  `unavailable` case sets `runner::FIRST_WAIT`, a test-only first wait in place of the 5 ms
  `RETRY_FIRST` has under test, so each `unavailable` can be read before the runner tries again;
  it checks the run's waits in units of that wait, the live 2 s doubling to 16 s being the same
  arithmetic. "While the interpreter is not answering" is covered by construction: `status()` is
  synchronous and reads the watch, so it cannot await the interpreter; the test reads it while
  the root's kernel spins in `while True: pass`.
- **`/simplify` changed the first build**, from four reviews: the refusal moved from the runner
  reading the slot after the round into `stopped_short` (`AgentError::Refused`), which also
  drops a stale reading when a round failed without a call; `Failed`'s stickiness left `say`
  for the runner saying it once; round facts are kept for roots only; the failed arms share
  `broken_by`. Tried and reverted: setting the test-only first wait once at the start, since the
  orienting round's success resets the wait, which then forgot it. Not adopted: building
  `RoundFacts` and `RoundEnd` from `Finished` in one place, which would reshape `RoundEnd`, a
  public type this task does not otherwise touch; folding `CallSlot`'s `error` into `last`,
  which every reader of `last` would then unpack.
