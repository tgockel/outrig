# A typo'd `base-url` now waits out the whole retry budget

`is_transient` in `crates/outrig-cli/src/llm/retry.rs` treats **every**
`reqwest` transport error as retry-worthy. That is right for a read timeout or
a connection reset mid-request, and wrong for the errors that mean the endpoint
was never reachable: a misspelled host, a DNS failure, a refused connect, a TLS
mismatch. None of those heal in ten minutes.

The visible cost: a typo in `base-url` used to fail the first turn immediately
and exit non-zero. It now spends the full `retry-budget-secs` backing off and
then ends the turn, so the user watches ten minutes of retry lines for a
one-character config error. The recovery arm's doc comment owns this as a
decision rather than an oversight -- for an interactive REPL, ending the turn
and naming the connection failure beats killing the session -- but the *wait*
is not defensible.

The two in-crate test fixtures that point at the discard port
(`crates/outrig-cli/src/cli/run.rs`, `crates/outrig-cli/src/subagent/mod.rs`)
both had to set `retry_budget_secs: Some(0)` to stay fast. That is the tell:
they are compensating for the classification, not for the fixture. If this
lands, both should be able to drop it.

## Sketch

`reqwest::Error::is_connect()` separates them, but not cleanly enough to make
connect failures terminal outright -- it is also true for a provider briefly
down behind a load balancer, which is exactly the case the retry exists for.

The honest middle is a second, much shorter budget for failures that happen
before the endpoint has ever returned bytes -- ~30s, enough to ride out a
restarting load balancer and nowhere near enough to sit through a typo. Once
the connection has produced a response, the full budget applies.

That needs `RetryPolicy` to carry the second bound and `send_with_retry` to
track whether any attempt got a response.

## Acceptance

- A misspelled `base-url` ends the turn in seconds, not minutes, and says so.
- A read timeout or a `503` still gets the full budget.
- Both discard-port fixtures drop `retry_budget_secs: Some(0)` and stay fast.
- `doc/concepts/llm-providers.md`'s transient-failures section names the split.
