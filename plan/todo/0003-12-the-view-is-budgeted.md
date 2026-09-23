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
