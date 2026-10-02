# Every way a round ends carries the same usage fields

## Context

`model.round.completed` carries `usage`, the round's summed tokens, and `input_tokens_max`.
`model.round.failed` and `model.round.dropped` carry only `calls`, the per-call list. A reader
that wants a token table therefore has two paths: read the sum, or rebuild it from `calls`.
`scripts/render-session.py` (`0003-14`) does both, and so will any other reader.

Separately, a round's `round` number is not unique: a round that commits no turn leaves its number
to the next, as `events.md` says. The renderer counts attempts to tell `Round 2` from `Round 2,
attempt 2`. A per-agent round sequence beside the number would make the identity explicit.

## Shape

- `model.round.failed` and `model.round.dropped` gain `usage` and `input_tokens_max`, computed
  the way the completed arm already sums its per-turn usage when OutRig stops a round.
- Optionally, every round event carries `attempt` or a monotonic `seq`, so readers key rounds
  without counting.
- The renderer keeps its fallback for logs written before the change.

## Acceptance

- The three round endings share their usage fields, asserted in `events_tests.rs`'s field-name
  test.
