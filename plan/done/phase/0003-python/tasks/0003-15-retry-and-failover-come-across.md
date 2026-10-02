# 0003-15 -- Retry and failover come across from the 0.2.x loop

## Context

`0003-04` copied the minimum a round needs and deliberately left retry and failover behind, so the
CLI could run sooner. This finishes the copy. Until it lands, `run-new` is less resilient than
`run`: a rate-limited provider ends a round that the legacy loop would have recovered.

`harness-components.md` lists `llm/retry.rs` and `llm/failover.rs` among the files copied rather
than moved, because the 0.2.x line actively edits them and a copy is what keeps those merges
clean. `llm/mistralrs.rs` and `llm/registry.rs` are explicitly **not** copied: the in-process
backend is deprecated, its removal is `plan/next/remove-deprecated-local-llm.md`, and omitting it
avoids `mistralrs-core`, `hf-hub`, and `candle-core` becoming library dependencies.

Two defects already filed against the originals are worth carrying rather than reproducing.
`plan/next/clamped-ceiling-is-silent.md` records that the effective `max-tokens` never escapes
`build_agent`, so nothing else in the process can see the ceiling that applies. And
`plan/next/chain-attribution-names-the-first-candidate.md` records that `ModelLabel` names
candidate one, so a failover mid-round changes which model answered without changing the label.

## Goal

`run-new` survives a transient provider failure and a failed candidate as well as `run` does, and
knows which model actually answered.

## Deliverables

- `retry.rs` and `failover.rs` copied into `outrig`'s private agent module, with the error type
  converting at the boundary rather than carrying rig's.
- **The in-process backend is not copied.** Neither is the registry it needs.
- **The exported effective ceiling kept correct across a candidate switch.** `0003-04` exports it
  at the point it is resolved and `0003-13` records it; what this task adds is that a failover to
  a different candidate updates it rather than leaving the first candidate's number standing.
- **Attribution that names the model that answered.** `failover.rs`'s private `Abandoned` carries
  the hop; today the chain's state is recovered by prefix-matching the rendered error string,
  which a structured channel replaces.
- The tests come across with the code. A copy without its ~4,900 lines of tests is a copy whose
  behavior nobody can check.

## Acceptance

- A mock provider returning 429 and then succeeding: the round completes, with the retry recorded
  rather than only printed.
- A failover chain whose first candidate is unreachable resolves to the second, and **the
  attribution names the second**, not the first.
- An exhausted chain ends the round with the reason, and the conversation is retained per the
  existing wording, not discarded.
- **The end-to-end checks `0003-12` and `0003-13` could not run**, because this is the task that
  adds failover: a view rebudgeted against a smaller candidate's allowance after a real hop, and
  retry and failover events emitted into the stream whose schema `0003-13` defined.
- The effective `max-tokens` is readable outside `build_agent`, asserted rather than assumed.
- `git diff` shows no change under `crates/outrig-cli/src/llm*`.
- `crates/outrig/public-api.txt` regenerated and clean -- nothing here should widen it.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether to fix the attribution defect here or carry it -- Recommended: fix.** It is cheap in
   a fresh copy and expensive later. If it turns out larger than it looks, leave
   `plan/next/chain-attribution-names-the-first-candidate.md` in place and say so. The ceiling
   defect is not a fork here: `0003-04` owns the export, and this task only keeps it accurate
   across a hop.
2. **What a round does with a failure no resend can fix -- Resolved: it ends the round (the
   maintainer's call).** Added during planning, since `0003-04` had left "the typed error" to this
   task. See `## Decisions`.

## Dependencies

- **Hard: 0003-04.** There is no loop to extend until the minimum is copied.
- **Hard: 0003-13**, and through it `0003-12`. This task's acceptance runs the end-to-end checks
  those two deferred -- rebudgeting after a real hop, and emitting retry and failover events into
  the stream whose schema `0003-13` defined -- so the declared graph should say so rather than
  relying on numeric order to make it true.

## See also

- `plan/phase/0003-python/harness-components.md` -- what is copied and what is deliberately not.
- `plan/next/clamped-ceiling-is-silent.md` and
  `plan/next/chain-attribution-names-the-first-candidate.md` -- the two defects to fix rather than
  reproduce, and `plan/next/remove-deprecated-local-llm.md` for the arm that is never copied.

## Decisions

- **Every library agent is a chain (`agent/build.rs`).** `RigAgent` is `Agent<FailoverModel>`,
  of one candidate or more, where it was an enum over the two providers.
  - The CLI builds a lone model without a chain so its no-alias sessions stay byte-for-byte what
    they were. The library has no such constraint.
  - A chain of one returns its candidate's error unwrapped, so a lone model's failures read as
    they did.
  - Each candidate is a `ModelCandidate` over a `RetryingModel` over the provider's model over a
    `RetryingHttpClient`, as in the CLI's `build_candidate`. The whole chain shares one
    `RetryPolicy`, whose budget is the head's provider's, as `config.md` documents for `run`.
- **What came across, and what changed in coming across.**
  - `retry.rs` and `failover.rs` came across with their unit tests. A library does not print:
    each `eprintln!` is a `tracing::warn!`, which `run-new` shows at its default filter, and each
    retry and move is also an event.
  - The CLI's REPL predicates did not come across: `exhausted_transient_label`,
    `unusable_response_label`, `chain_exhausted_label`, `ChainExhaustion`, and the prefix
    matching. Nothing here reads them. `is_recoverable` remains, so an exhausted chain still says
    "failed" or "failed terminally".
  - `report_textless_completion` came across as a `tracing::warn!` naming the model.
  - The in-process backend and its registry were already gone from this line (`5fc715b`), so
    "not copied" needed nothing.
  - The task's "~4,900 lines of tests" came from `crate-split-tradeoffs.md`'s count of the whole
    loop. The tests that cover retry and failover come to about 2,100 lines, across the two files,
    `anthropic_mock.rs`, `llm_resolve.rs`, and the mock. The unit tests came with the files. The
    integration tests that apply here were rewritten as rounds, below.
- **Resolution keeps the whole chain.** `ResolvedAgent.candidates` lists every model of an alias
  this build can reach, in order, with `head()` for the first. `ResolvedProvider` carries
  `retry_budget_secs`, and `ReplyFillsWindow` is checked per candidate.
- **Each candidate carries its own `Budget`, and the budget carries its ceiling.**
  - `Budget` gained `max_tokens`: the ceiling that reaches the wire for that candidate, settled
    when it is built. `WindowTooSmall` is therefore checked per candidate, and fails the start for
    any of them.
  - The chain rewrites every attempt's `max_tokens` from it, the head's included, so the agent
    no longer bakes in a ceiling of its own.
  - `FailoverModel::budgets()` is how the agent reads the head's allowance and how tests read
    each candidate's. `PythonAgent::model()` is the head's `budget.model`.
- **The exported ceiling stays correct across a move by being recorded per call.**
  - Every `model.call` manifest's `budget` carries the `max_tokens` its call carried.
  - Every call starts at the head again, so a per-call record is the only one that cannot go
    stale. `model.instructions` keeps the head's.
- **A move rebuilds the view for the next candidate's window (the deferred `0003-12` check).**
  - The hook assembles each call's view against the head's budget, as before. A move happens
    inside the chain's `completion()`, below rig. So the chain asks a `View` (implemented by
    `History`) for the view under the next candidate's budget, and splices it in after the
    leading system message. rig 0.40's `CompletionRequestBuilder::build` moves the preamble into
    `chat_history`, so the rebuilt view goes after it rather than in place of it.
  - That assembly is recorded as a `model.call` manifest of its own, right after the
    `model.failover` that caused it, naming the candidate's budget. A retry resends the same view
    and records no new manifest.
  - A candidate whose window cannot hold the call's latest turn is passed over, with the
    `TooLarge` text as its reason (`CompletionError::RequestError`, terminal for that candidate).
  - **Limitation:** a round's opening line counts the turns left out under the head's budget, so
    a move on a round's first call can leave out more than it says.
  - **Inconsistency, filed:** when the *head* cannot fit the latest turn, the round ends, as
    `0003-12` decided, even if a later candidate could take it. Making the chain the only
    assembler would settle both cases the same way:
    `plan/next/the-chain-assembles-every-candidates-view.md`.
- **Attribution names the model that answered (fork 1: fixed here).**
  - `FailoverModel::Response` is `Answered { model }`, in place of the provider's raw response.
    rig hands it to the hook at `StepEvent::CompletionResponse`, so the channel is typed, per call,
    and holds no shared state.
  - The hook records each call there: rig fires `CompletionResponse` and `ModelTurnFinished` back
    to back under the same suppression rules, and only the first carries the response. So the
    hook no longer observes `ModelTurnFinished`.
  - `calls[]` in `model.round.completed`, `.failed`, and `.dropped` names each call's `model`.
    `PythonAgent::model()` still names the head, and its doc says so: no public surface was
    added.
  - The per-call list now comes from the hook on every path, where a successful round used to
    read rig's `completion_calls`. rig records a completion call for a turn it retries after an
    invalid tool call, and no hook event fires for that turn, so its index would not match. The
    loop does not observe `InvalidToolCall`, so that path fails today instead. The round's total
    still comes from rig's `usage` when it succeeds.
- **The event schema.**
  - `model.retry` gained `model`. `attempt` is the attempt that failed, counted from 1, and
    `error` is `failure_label`, never the body.
  - `model.failover` is `from`, `to`, and `error`, as `0003-13` defined it.
  - Both lose their `expect(dead_code)`.
  - A chain of more than one names its fallbacks at startup through `tracing::info!`. That stands
    in for `run`'s banner line, which `run-new` cannot print without a public accessor.
- **Fork 2: every failure ends the round, and the session goes on (the maintainer's call).**
  - That includes an exhausted chain and a `401` from every model. The error keeps the
    `Prompt` or `PromptAfterWork` wording, with the chain's per-candidate report as its reason.
  - Telling the two kinds apart needs public surface for the CLI to read. That, and the advice to
    resend being wrong for a failure that cannot clear, is filed as
    `plan/next/run-new-failures-that-cannot-clear.md`, with a typed exhaustion as the route.
- **Test changes.**
  - `config_in` sets `retry-budget-secs = 0`. Otherwise a scripted `failure(500)` would be retried
    into the reply scripted after it, and the tests would stop testing what they describe. The
    retry tests turn retries on, by editing the parsed `Config`.
  - The mock gained a header on canned responses, for `Retry-After`, and `unusable()`.
  - The `Retry-After` test waits a real second, and the unusable-response test waits up to one
    jittered base delay. A paused clock races the host interpreter's and the log writer's I/O.
- **Tests, by acceptance criterion.**
  - 429, then a reply: `a_rate_limited_call_is_retried_and_the_retry_recorded`.
  - An unreachable first candidate resolves to the second, and the attribution names the second:
    `an_unreachable_head_moves_to_the_next_model_which_is_named_as_answering`.
  - An exhausted chain ends the round with its reasons and keeps what ran:
    `an_exhausted_chain_ends_the_round_and_keeps_what_it_ran`.
  - The deferred end-to-end checks are `a_move_to_a_smaller_window_is_sent_a_view_assembled_for_it`
    and the two event-recording tests. The first covers the rebuilt view, its manifest, its
    rebuild from the log, and each model's ceiling on the wire.
  - The ceiling readable outside `build_agent`: that test, and
    `the_reported_ceiling_is_the_one_on_the_wire`.
  - Also: `an_unusable_response_is_retried_without_running_python_again`,
    `python_run_before_a_move_is_not_run_again`, and the chain's unit tests for reassembly,
    passing over a candidate that is too small, and recording.
  - **Mutation-checked:** dropping the reassembly on a move; attributing every call to the head;
    not recording an HTTP retry; dropping the per-candidate ceiling; not recording a move. Each
    failed at least one test.
  - `crates/outrig/public-api.txt` and `crates/outrig-cli/public-api.txt` are unchanged, and
    nothing under `crates/outrig-cli/src/llm*` changed.
- **`/simplify` left two findings on purpose.**
  - A move rebuilds the view even when the candidate's budget equals the head's. One manifest per
    candidate a call reached is the design.
  - The chain's budget comes from the head alone. That is inherited from `run` and documented, and
    changing it here would make the two commands disagree.
- **After review, three fixes.** The review rejected the first cut on three findings. Each was
  reproduced as a failing test, then fixed and mutation-checked. Each is still in the CLI's
  original, which this task does not touch, so they are filed together as
  `plan/next/cli-retry-and-failover-gaps-the-library-closed.md`.
  - **A chain that spans providers sent Anthropic reasoning it cannot take.** An
    OpenAI-compatible model's `reasoning_content` becomes unsigned reasoning in the conversation,
    and rig's Anthropic adapter sends that as a thinking block with no signature, which Anthropic
    refuses.
    - So once an OpenAI-compatible model had answered, every later call to an Anthropic
      candidate failed, the head's too, since every call starts there.
    - Each candidate's `Budget` now carries its provider's protocol (`budget::Wire`), and every
      call's view is fitted to it, the head's included. The first fix fitted each attempt in the
      chain, after its manifest was written; the second review moved it, as below.
    - For Anthropic, a reasoning block is kept only if every part is signed text or redacted,
      which is what Anthropic itself produces. A reply left empty is left out with it; it made
      no tool call, so no result is left unanswered.
    - For an OpenAI-compatible candidate nothing changes: rig sends any reasoning as
      `reasoning_content`, and drops a reply of nothing else.
    - Only the request changes. The stored conversation keeps what each model wrote.
    - Tests: `reasoning_another_provider_wrote_is_not_sent_to_anthropic` checks the Anthropic
      request body after a move from an OpenAI-compatible model, and
      `the_anthropic_head_is_sent_none_of_the_reasoning_its_fallback_wrote` checks the head's next
      call after a move to one. `signed_thinking_goes_back_to_anthropic_whole` checks that signed
      thinking survives a replay to the same provider, and
      `a_call_is_sent_only_the_reasoning_its_provider_takes` covers both protocols at the store.
  - **A request that cannot succeed as sent was retried for the whole budget.**
    - Every transport error that was not a connect failure was retried. That included a
      redirect loop that reached reqwest's limit of ten, which spent the full default budget of
      600 seconds, and with it the chain's.
    - `is_permanent` (a builder error or a redirect error) now returns at once, and
      `is_transient` agrees, so a chain reports it as terminal.
    - Tests: `a_redirect_loop_is_final_at_once`, and `a_redirect_loop_moves_on_without_retrying`
      through a chain.
  - **Anthropic's `529` was not retried.** It is that provider's documented, temporary
    `overloaded_error`, and `is_retryable_status` now includes it. `doc/concepts/llm-providers.md`
    already promised that any `5xx` is retried.
    - Test: `an_overloaded_model_is_retried_rather_than_left`, through a chain, where the same
      model answers after the wait and the call does not move.
  - The test chain's config now takes each model's provider style, so a chain can span both
    protocols. A `retrying` helper turns a provider's retries on.
- **A second review: the record must rebuild a fitted call.** The fit ran in the chain, after
  `History::assemble` had written the call's `model.call`. So the manifest's carried turns still
  held the reasoning, and rebuilding the call from the log produced a thinking block the request
  never had. The log recorded no protocol, and a model's name cannot pick the rule.
  - The fit moved into `History::assemble`, against the budget the call is held to. Each part it
    leaves out is recorded in the manifest's `left_out` as `{turn, message, part}`, so the record
    rebuilds the call mechanically, by the rule `doc/reference/events.md` states, without knowing
    any provider's rules. The chain no longer changes a request's history.
  - That makes assembly the one place a view is decided, for the head and for a moved call
    alike, which is the direction `plan/next/the-chain-assembles-every-candidates-view.md`
    proposes for the rest.
  - The size estimate still counts turns whole, so leaving parts out only makes it more
    conservative.
  - Tests: both mixed-provider tests rebuild every call from the log and compare it with what
    each mock received, through each provider's adapter (`each_call_rebuilds`). The store-level
    test checks `left_out` and the rebuild directly. `render-session.py` lists a call's
    `left_out`. Mutation-checked: an unrecorded part, and no fit for Anthropic.
