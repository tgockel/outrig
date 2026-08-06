# 0112 -- A connect failure is not the same kind of transient as a read timeout

## Context

`is_transient` (`crates/outrig-cli/src/llm/retry.rs:473`) treats **every** `reqwest` transport
error as retry-worthy. That is right for a read timeout or a connection reset mid-request, and
wrong for the errors that mean the endpoint was never reachable at all: a misspelled host, a DNS
failure, a refused connect, a TLS mismatch. None of those heal in ten minutes.

The visible cost: a typo in `base-url` used to fail the first turn immediately and exit non-zero.
It now spends the full `retry-budget-secs` backing off and then ends the turn, so the user watches
ten minutes of retry lines for a one-character config error. The recovery arm's doc comment
(`exhausted_transient_label`, retry.rs:492-497) owns the *classification* as a decision rather than
an oversight -- for an interactive REPL, ending the turn and naming the connection failure beats
killing the session -- but the *wait* is not defensible.

Two in-crate test fixtures point at the discard port and both set `retry_budget_secs: Some(0)` to
stay fast (`crates/outrig-cli/src/cli/run.rs`, `crates/outrig-cli/src/subagent/mod.rs`). That is
the tell: they are compensating for the classification, not for the fixture.

## Goal

Separate "the endpoint never answered" from "the endpoint answered badly", and give the first a
much shorter budget than the second.

`reqwest::Error::is_connect()` separates the two mechanically, but not cleanly enough to make
connect failures terminal outright -- it is also true for a provider briefly down behind a load
balancer, which is exactly the case the retry exists for. The honest middle is a second, much
shorter bound for failures that happen before the endpoint has ever returned bytes: long enough to
ride out a restarting load balancer, nowhere near long enough to sit through a typo. Once the
connection has produced a response, the full budget applies for the rest of that request.

## Deliverables

- **`RetryPolicy` carries both bounds.** The existing `budget` field keeps its meaning and its
  name; a second field bounds the pre-first-byte case, defaulting to roughly 30 seconds.
  **Two distinct names, not one field that means different things depending on state** -- 0113
  reads the short one directly as its "move to the next candidate now" signal, and a field whose
  meaning depends on the caller's context is not a signal anything else can consume.
- **`send_with_retry` (retry.rs:283) tracks whether any attempt has produced a response**, and
  `next_delay` (retry.rs:437) checks `elapsed` against the applicable bound. `next_delay` already
  takes every input by parameter so the whole decision is unit-testable with no RNG and no
  sleeping; the new bound joins them rather than being read off `self`.
- **Both discard-port fixtures drop `retry_budget_secs: Some(0)`** and stay fast, which is the
  end-to-end proof that the classification and not the fixture was the problem.
- **`doc/concepts/llm-providers.md`**'s transient-failures section names the split, including the
  fact that the two bounds are separately configurable or separately fixed -- whichever the
  implementation lands on, the doc has to say which.
- **`crates/outrig-cli/src/llm/retry.rs`'s module doc** grows a sentence: it currently describes
  one wall-clock budget shared by both layers, and that is about to be two.

## Design forks

1. **Is the short bound configurable -- Recommended: no, a constant, like `MAX_RETRY_AFTER`.**
   retry.rs already holds five tuning constants (`BASE_DELAY`, `MAX_DELAY`, `MAX_RETRY_AFTER`,
   `MIN_DELAY`, `RESPONSE_RETRY_ATTEMPTS`) that no config key reaches, on the reasoning that they
   describe how to be a well-behaved HTTP client rather than a preference. "How long a restarting
   load balancer takes" is that kind of number. A config key can be added later additively; the
   reverse is not true.
2. **What `retry-budget-secs = 0` means -- Resolved: still no retries, anywhere.** It is documented
   as the one "no retries" knob and the module doc says so outright (retry.rs:33-34). A zero budget
   must short-circuit the short bound too, or the knob acquires an exception.
3. **Whether the short bound also gates `RetryingModel` -- Resolved: no.** That layer retries a
   `200 OK` with an unusable body, which by construction means the endpoint answered. It is bounded
   by `RESPONSE_RETRY_ATTEMPTS` and the full budget, and neither changes here.

## Acceptance

- A misspelled `base-url` ends the turn in seconds rather than minutes, and the message says the
  endpoint was never reached.
- A read timeout or a `503` still gets the full `retry-budget-secs`.
- A request that connects, gets a `503`, and then fails to reconnect on the retry is on the *full*
  budget, not the short one -- the connection produced a response once.
- Both discard-port fixtures drop `retry_budget_secs: Some(0)` and the suite stays fast.
- `retry-budget-secs = 0` still performs exactly one attempt, with no wait, on both paths.
- `RetryPolicy`'s two bounds are named distinctly enough that 0113 can read the short one without
  knowing which request state produced it.

## Risks

- **`exhausted_transient_label` gets a second way to be reached quickly.** Its doc comment
  (retry.rs:492-497) explicitly names the typo'd `base-url` case as the deliberate reach of the
  current classification and points here as the narrowing. That comment must be rewritten by this
  task, not left describing the old behavior -- it is the only in-tree prose that explains why an
  unreachable endpoint ends a turn rather than the session.
- **Thirty seconds is a guess.** It is bounded on one side by a load balancer restart and on the
  other by a user's patience, and neither is measured. Landing it as a named constant with the
  reasoning in a doc comment is what makes it revisable.

## Dependencies

None hard. Post-0.2.0: this touches only `outrig-cli`, whose internals 0093 made private, so no
`crates/outrig` public surface moves and no CHANGELOG entry is required.

**0113 depends on this**, and closely: without a short pre-first-byte bound, a failover chain of
three candidates multiplies the full budget by three against a total outage. See 0113's
Dependencies.

## See also

- `crates/outrig-cli/src/llm/retry.rs` -- the module doc's one-budget description (19-34),
  `RetryPolicy` (76), `send_with_retry` (283), `next_delay` (437), `is_transient` (473), and
  `exhausted_transient_label` (502) with the comment that points here.
- `plan/todo/0113-model-alias-failover.md` -- the consumer of the short bound.
- `plan/next/streaming-path-has-no-http-retry.md` -- the other gap in the same layer, deliberately
  not folded in here.
