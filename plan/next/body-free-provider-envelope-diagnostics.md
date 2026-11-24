# Keep provider-error envelope bodies out of retry diagnostics

## Goal

Make retry diagnostics honor their body-free contract when an HTTP 200 response
contains a provider-error envelope. This predates the child-round retry event
added for #764 and was identified while reviewing that change.

## Deliverables

- Replace the raw `error.to_string()` diagnostic in the unusable-response path
  in `crates/outrig/src/agent/retry.rs` with a body-free classification.
- Keep enough information to explain the failure without copying the provider's
  response body into `model.retry.error` or `agent.round.retry.error`.
- Test an HTTP 200 error envelope containing a sentinel body, including the
  child runner's retry event, and assert that diagnostics do not contain it.

## Acceptance

Retry events explain the failure without carrying the sentinel response body;
the retry policy and the classification of recoverable failures are unchanged.
