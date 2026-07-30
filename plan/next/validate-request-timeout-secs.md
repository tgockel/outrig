# `request-timeout-secs` has no range check

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

A `0` is the more interesting case: `Duration::from_secs(0)` is a *disabled*
timeout in `reqwest`, not an immediate one, so `request-timeout-secs = 0` means
"wait forever" -- the opposite of what it reads as, and the opposite of what `0`
means for `retry-budget-secs` two lines above it in the same table.

## Sketch

- `REQUEST_TIMEOUT_SECS_CEILING`, and a `validate_request_timeout_secs` beside
  `validate_retry_budget_secs` in `crates/outrig/src/config/validate.rs`.
- Decide `0` explicitly: either reject it, or document it as "no timeout" in
  `config.md` next to the `retry-budget-secs` row where the contrast is visible.
  Rejecting is probably right -- nobody sets `0` meaning "forever".
- Call it from the same per-provider loop the budget check already uses; the
  `match` there is already exhaustive over `LlmProvider`.

## Acceptance

- Over-ceiling values rejected at the top level and per provider, with the
  `providers.<name>.request-timeout-secs` path in the message.
- `config.md`'s "Validation rules" gains the row.
- A `config_merge.rs` case per path, matching the `retry-budget-secs` ones.
