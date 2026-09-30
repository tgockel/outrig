# 0003-12 -- The view is budgeted, and promotion has settled semantics

## Context

`0003-11` gives the agent a store and the provider a view. This one makes the view safe and the
promotion predictable.

Counting retained rounds is not a budget. A first-and-recent window plus a few promotions can
still exceed the context window, and a single turn -- fifty tool calls and their results -- can
exceed it alone. `history.md` records that an earlier draft claimed the split removed the
context-overflow dead end and that this is not true: what it does is make overflow **recoverable
and legible** instead of a request resent forever.

Promotion needs semantics decided rather than discovered, because the implementation answers them
accidentally otherwise -- lifetime, idempotence, order, timing, stability, and when an in-flight
turn becomes promotable.

And a promotion is a request, not proof of what was sent. Window movement, deduplication, budget
eviction, retries and failover all change the assembled view, so only a per-call manifest can
answer "what did this decision have available".

## Goal

A session that outgrows its context window reports which turn would not fit, instead of resending
the same doomed request; and an agent that promotes something knows what that means.

## Deliverables

- **A source for the provider's context limit, which does not exist today.** `Model` has a
  `context_length`, but `mistralrs_weight_fields` lists it among the weight fields a remote row
  may not set, so an OpenAI or Anthropic model has no window configured anywhere. `max-tokens` is
  an output ceiling and is not it. Either permit an explicit context length on remote rows with
  documented semantics and validation, or define an operator-selected request budget and name it
  as that rather than as the model's window. Say what happens when no reliable limit is available,
  and give each failover candidate its own allowance -- a smaller model is a smaller window.
- A pre-call size estimate against that limit with a completion reserve, since post-call usage
  reporting cannot admit the first oversized request safely.
- A priority order when the assembled view does not fit. **The current turn and the protocol data
  it requires are not ordinary eviction candidates** -- evicting a trailing tool call and its
  result is the one way a round boundary loses information.
- Deduplication, since a promoted turn may already be inside the recent window.
- **An explicit, actionable failure when one intact turn cannot fit**, naming the turn.
- Promotion semantics, settled: persists until removed, idempotent, original chronological order,
  affects the next model call in the same round, sees a stable prefix while new turns land, and an
  in-flight turn becomes promotable when it commits.
- Commit points: a round that dies partway leaves the turns that finished and an explicitly
  incomplete one. An executed effect is never erased because a later model call failed, and a
  placeholder standing in for a missing tool result never implies the call did not happen.
- **A per-call manifest** of the canonical ids actually carried, with selection metadata.
- The memory measurement gate: host record, transport, and mirror measured separately against the
  shared address-space ceiling, with the response defined before growth is real.

## Acceptance

- A session driven past its context window reports the offending turn and continues, rather than
  failing identically on every later round. The failure mode this task exists to remove.
- **A promoted turn that overlaps the recent window appears once.**
- Promoting twice is the same as once; promoted turns appear in chronological order.
- A round that fails partway leaves the completed turns and an incomplete marker, and re-running
  is not triggered by the repair.
- **A shortened `RequestPatch.history` is exercised against each supported adapter** with
  representative fixtures, and the supported subset is published with an intelligible error for
  what falls outside it. `history.md` marks this as an acceptance gate rather than a caveat:
  tool-call pairing is universal, role-alternation rules are not, and a Bedrock-backed Claude
  behind an OpenAI-compatible gateway is already recorded as strict.
- The manifest for a call reconstructs what the provider received.
- **Candidate budgets are exercised against fixtures**, not against real failover: this task has
  no failover to fail over to, since `0003-15` adds it. Assembly against a second candidate's
  smaller allowance is checked here; the live rebudget-after-failover check belongs to `0003-15`.
- **An explicit remote limit survives the real configuration path**, and the no-reliable-limit
  behavior is exercised there too. Fixtures can pass while validation still rejects the chosen
  field or while something silently guesses a window from a model name, which is the failure this
  task exists to prevent. Assert the limit that was selected and the reserve that was applied, not
  only the arithmetic downstream of them.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Dependencies

- **Hard: 0003-11.** There is no view to budget until there is a view.

The per-call manifest this task produces is what the observability task records; that dependency
is declared there rather than here, since a forward reference in this section is read as an
ordering invariant.

## See also

- `plan/phase/0003-python/history.md` -- the budget, promotion semantics, commit points, the
  provider-validity gate, and the memory gate.
- `plan/phase/0003-python/observability.md` -- where the manifest is recorded, and why a promotion
  event alone is insufficient.

## Decisions

- **The limit is `[models.<name>].context-window`, in tokens (the maintainer's call).**
  - This task's first deliverable was written against a `Model::context_length` and a
    `mistralrs_weight_fields` that went with the local-llm backend. `context-length` is now a
    parse error, and `doc/concepts/llm-providers.md` says the removed keys have no counterpart on
    purpose. So the key is new rather than revived: it configures nothing on the server, it tells
    `run-new` the limit the server has, and the migration note now says so.
  - It is the model's whole window, request and reply together, as the provider publishes it.
    Only a provider row may set it. An alias refuses it through `provider_shape_fields`, since each
    of an alias's models has its own window. That is the per-candidate allowance: `Budget` is
    built from one `ResolvedCandidate`, and `0003-15` will build one per candidate.
  - It adds one public line, `Model::context_window`. That line is a deliberate addition to the
    phase's one-entry-point budget. `outrig run` parses the key and ignores it
    (`plan/next/legacy-loop-ignores-context-window.md`).

- **With no window configured, 128,000 tokens is assumed and startup warns (the maintainer's
  call).** It is never read from the identifier.
  `no_context_window_assumes_one_whatever_the_identifier` pins that across a recognized, an
  unrecognized, and an OpenAI identifier.
  - The reply's reserve is the ceiling that reaches the wire. The budget is therefore built in
    `PythonAgent::build` after `build_agent`, which may fill that ceiling in or lower it.
  - Against an assumed window the reserve is at most a quarter of it. The ceiling was chosen
    against the model's real window. rig's 64,000 for a current Sonnet would leave an assumed
    window about 62,000 tokens of room, where the quarter leaves about 94,000.
  - With no ceiling sent at all (OpenAI, no `max-tokens`), the reserve is 8,192, or a quarter of
    the window if that is smaller.

- **Checks run at resolve and build, not at config load.**
  - A new `ConfigValidationError` variant is public surface, one line per variant and per field,
    and the legacy loop would never read it. So the checks became `LlmResolveError` variants, which
    are crate-private.
  - `ReplyFillsWindow` (a configured `max-tokens` at or above the window) is raised in
    `resolve_agent`. `PythonAgent::check` reaches it before an image is pulled.
  - `WindowTooSmall` (less than 1,024 tokens of room after the reply and the system prompt) needs
    the preamble, so it is raised in `build`, once the container is up. That is a real cost, and
    it is accepted: the case takes a window below about 4,000 tokens.

- **The estimate: a token per three ASCII bytes of a message's JSON, and one per other
  character.** No tokenizer is in the tree, and each provider counts differently.
  - Counting non-ASCII characters apart keeps CJK from being under-counted by two thirds.
  - Messages stream through `serde_json::to_writer` into a counter, so a large result is never
    copied. A turn is counted once, as it commits.
  - Every call's overhead is the preamble, the tool's name, description, and schema, and 512 tokens
    for what a provider adds to a request with tools.
  - Dense text can still cost more than its estimate. Calibration against reported usage is filed
    (`plan/next/calibrate-the-token-estimate.md`).

- **The drop order is the maintainer's, and the view is filled rather than cut.**
  - Turns are admitted in the order they are kept longest:
    1. the round in progress, newest first;
    2. promotions, newest first;
    3. the first rounds, oldest first;
    4. the recent rounds, newest first.
  - A turn that does not fit is passed over, and smaller ones after it are still tried. Without
    that, the oversized turn that just ended a round sits in the next round's recent window, and a
    strict cut would drop the whole window before reaching it. The maintainer confirmed this with
    the plan.
  - Each turn is chosen for one reason, the strongest (latest, round, promoted, first, recent).
    That one reason is the deduplication, and it is why a promoted turn inside the window is kept
    as long as a promotion.

- **The latest turn is never dropped. When it cannot fit, the call is not made, and the round
  ends as the tool-call cap ends it.**
  - `RoundHook.stopped` became the reason itself (`Mutex<Option<String>>`), so the cap and the
    budget share `stop` and `stopped_short`.
  - The round keeps its turns, and its reply is `(round ended: turn N of round R ...)`, with the
    estimate, the room, the window, the reserve, the overhead, and a remedy.
  - The next round succeeds because that turn is now ordinary history, which
    `a_turn_too_large_for_the_window_ends_its_round_and_later_rounds_go_on` drives end to end.
    Its window is 7,000 tokens, because one execution's output stops at 16 KiB, about 5,500
    tokens.
  - A stop is not an error. What ran stands, and the next message continues.

- **Each call's view is assembled from the store alone.**
  - At a `CompletionCall` the journal has committed every turn of the round. rig's prompt is then
    the last message of the latest turn. On a round's first call the prompt is the opening, which
    no turn holds yet, so it is estimated at `begin_round`.
  - `History::assemble` returns the whole request, prompt last, which is what the manifest
    rebuilds. The hook takes the prompt off, since rig sends it itself, and a `debug_assert_eq!`
    checks that it is rig's in both cases. What rig does with a patch stays in `round.rs`.
  - The opening line's count now includes what the budget leaves out. The count changes the
    line's size, and the size what fits, so `History::open_round` costs the opening at the longest
    line it can be -- the one counting every turn -- both when it counts and when the round's first
    call is assembled. The count it states is then exactly what that call leaves out.
    - The first cut settled the count by iterating, on the reasoning that a longer line never
      leaves out fewer turns. Filling breaks that: a longer line can leave out one large turn and
      fit two small ones. A review found it, and
      `the_opening_counts_exactly_what_its_first_call_leaves_out` reproduces the old loop stating
      two where its call left out one.
    - Opening the round there means the store, not `RigAgent::round`, begins it, and rig is
      handed the message the store holds.

- **Promotion semantics.**
  - Removal is `runtime.context.demote(...)`, which takes what `promote` takes and is validated the
    same way.
  - The host takes both through one `on_context` handler, as `ContextChange::{Promote, Demote}`,
    so they apply in the order the agent made them.
  - A demotion sends every id it names, recorded as promoted on the Python side or not. A promote
    interrupted between writing its line and recording it is still the host's.
  - Telling the host and recording in `promoted` are one step, under the context's lock, with the
    line encoded first. A review found that a promotion and a demotion of one turn made from two
    threads could otherwise reach the host in one order and `promoted` in the other, leaving them
    disagreeing for good;
    `a_promotion_racing_a_demotion_leaves_the_host_and_promoted_agreeing` reproduces it on the old
    code.
  - The rest -- idempotence, chronological order, effect from the next call in the same round, a
    stable prefix, and a turn promotable only once it commits -- is tested at each layer. The last
    is `the_turn_in_flight_is_promotable_once_it_commits`.

- **The incomplete marker lives in the mirror.**
  - `keep_what_ran` commits through `commit_incomplete` when any call got a placeholder, and the
    agent's code reads `Turn.incomplete`.
  - The host store does not keep the flag, since nothing on the host reads it. The placeholders
    in the messages already say it to the model.
  - A failed model call leaves no marker. The turn before it is whole and was sent, and the failed
    call produced nothing to keep. A marker message would be the synthesized repair `history.md`
    rejects.
  - `a_round_ended_mid_batch_keeps_an_incomplete_turn_and_runs_nothing_again` checks through
    `on_submit` that nothing of the ended round runs again.

- **The manifest.**
  - It carries each call's sequence number, round, the `Budget` it was held to (the model, the
    window, whether it was assumed, the reserve, the overhead), and the whole request's estimate.
    It lists each turn carried and each turn evicted, with why, and holds the opening while no
    turn holds it.
  - It also records where one role follows itself.
  - `History::on_manifest` is `0003-13`'s emission point. Reconstruction is test-only, and each
    call's rebuilt messages, run through rig's own adapter conversions, equal what the mock
    received on both adapters.

- **Per-adapter validation.**
  - The mock gained OpenAI chat completions, and `check_wire` checks each adapter's pairing rule.
    `every_cut` drives both adapters through a promotion mid-round, a capped round, a budget drop of
    the round's own turn, and a round ended mid-batch.
  - Pairing always holds. Assistant-to-assistant pairs occur on both adapters. User-to-user pairs
    occur only on Anthropic, since OpenAI carries results as `tool` messages.
  - rig's OpenAI adapter drops the text of a user message that also carries tool results. So the
    view never merges messages to force alternation, since merging a round's results with the next
    opening would lose the opening.
  - The supported subset is published in `doc/reference/cli.md`. A 400 or 422 refusal of a call
    whose manifest recorded a pair says so and points there. A strict gateway still fails while the
    pair is in view (`plan/next/strict-role-gateways.md`).
  - The pair is named for the conversation rig holds, not for the wire: "two of the model's
    replies", or "a prompt right after a prompt or tool results the model never answered". The
    first cut said "two user messages in a row", which is false on OpenAI, where results are
    `tool` messages; a gateway that turns them back into user messages is still why the hint
    fires there.

- **The memory gate, measured.**
  - The workload was a thousand turns, each a 16 KiB result, 16.4 MB of text. The host store holds
    16.7 MB of JSON, the transport carries 16.5 MB with no line longer than one turn, and the mirror
    holds 16.9 MB by `tracemalloc`, with a transient peak of about 74 KB.
  - Only the mirror is under the ceiling, which is half the container's memory.
  - The response is decided and lives in Python, which knows the ceiling. It reads the soft
    `RLIMIT_DATA` at each turn, so an agent that raised its own limit is measured against that.
    Past an eighth of it, `History` says so once on stderr and keeps everything. The fallback past
    that is filed (`plan/next/history-bodies-on-demand.md`).

- **Left out:** calibration from reported usage; bodies on demand; load-time validation; a
  configurable round window; any merging or repair of same-role messages; an `outrig init` prompt
  for the key; honoring the key in the legacy loop; everything about failover (`0003-15`).
