# No owner can read a usage total except from the lossy stream

## Context

`0003-19` keeps a ledger of every provider attempt a session makes
(`crates/outrig/src/agent/ledger.rs`), derives round totals from unique attempts, and keeps unknown
usage unknown. What an owner can read of it is the events: `model.attempt`, the round endings, and
from `0003-30` the children's. Those are a bounded stream that drops counted gaps under a slow
consumer, so a total an owner adds up from them is incomplete whenever a gap occurred, and the
session's own total -- `Ledger::session_total` -- is `#[cfg(test)]`. The assessment that accompanied
the host-program requirements recommended a final usage summary from the ledger, independent of the
stream; the requirements did not adopt it (R2: "no new ledger export API"), so it is a
recommendation, not a decision.

## Shape

- A summary on the `ShutdownReport`, or a method on `Session`, taken from the ledger once every
  producer of model calls has stopped: exclusive per-agent and per-concrete-model aggregates from
  unique attempts, attempts counted, attempts with no reported usage counted apart, never a zero
  standing for an unknown; mirrored in a host program's `shutdown_report` (`0003-29`).
- Per-field `Option` as `plan/next/usage-fields-reported-or-not.md` asks, since a summary that
  sums zero-filled fields would state what the provider did not.
- No database, no durable export, no prices.

## Acceptance

- A session with a stalled subscriber reports a summary equal to the sum of the attempts the mock
  provider answered, while the subscriber's own total is short by its counted gap.
