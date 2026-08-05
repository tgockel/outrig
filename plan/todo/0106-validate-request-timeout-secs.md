# 0106 -- `request-timeout-secs` has no range check

`retry-budget-secs` validates against `RETRY_BUDGET_SECS_CEILING` (3600) at both
the top level and per provider. Its sibling `request-timeout-secs`, which has
been on `LlmProvider` since 0.2.0-rc.1, validates nothing: any `u64` parses and
becomes a `reqwest` timeout.

The asymmetry was deliberate when the budget landed -- an absurd budget wedges
an interactive turn for as long as it names, while an absurd timeout only
affects one request, which the budget then bounds anyway. But "only" is doing
work there: with `request-timeout-secs = 86400` and the default budget, a hung
endpoint parks the REPL for ten minutes with no output at all, since the first
attempt never returns to print a retry line.

A `0` is the other end of the range. Checked against reqwest 0.13.4,
`timeout(Duration::ZERO)` is an *immediate* timeout, not a disabled one, so
`request-timeout-secs = 0` means every request fails before it can be answered.
It is a config that parses, validates, and then cannot work -- which is what a
range check exists to catch, at the file rather than at the endpoint.

## Goal

Give `request-timeout-secs` the range check its sibling `retry-budget-secs`
already has, before 0.2.0 final freezes the set of configs the release accepts.

## Deliverables

- `REQUEST_TIMEOUT_SECS_CEILING`, and a `validate_request_timeout_secs` beside
  `validate_retry_budget_secs` in `crates/outrig/src/config/validate.rs`.
- Reject `0` -- an immediate timeout is never what the value means, and the
  rejection reads the same way the ceiling does.
- Call it from the same per-provider loop the budget check already uses; the
  `match` there is already exhaustive over `LlmProvider`.
- A new `ConfigValidationError` variant beside `RetryBudgetSecsTooLarge`; the
  enum is `#[non_exhaustive]`, so the addition is not itself a break.

## Acceptance

- Over-ceiling values rejected at the top level and per provider, with the
  `providers.<name>.request-timeout-secs` path in the message.
- `config.md`'s "Validation rules" gains the row.
- A `config_merge.rs` case per path, matching the `retry-budget-secs` ones.
- `0` is rejected at both paths, with a message that says what `0` would do.

## Dependencies

None. The scheduling constraint is the only one that matters: the bound has to
land before 0.2.0 final, since adding it afterwards rejects configs that 0.2.0
accepted, which is a user-facing break rather than a fix.
