# An unusable response keeps the usage it reported

## Context

A `200 OK` that rig cannot make a completion of -- an Anthropic reply with no content and a
`stop_reason` other than `end_turn`, `mock_http::unusable()` -- still carries a `usage` block, and
the provider bills it. rig's conversion fails before OutRig sees the response, so the error that
reaches `Settling::settle` in `agent/ledger.rs` holds a message and no usage: the attempt is
recorded with `usage: null`, and the session's and the round's totals leave those tokens out.
`an_unusable_response_is_retried_without_running_python_again` is such a case: the unusable reply
reports usage that no total counts. This predates `0003-19`, which made the attempt visible but
could not give it the usage rig dropped.

## Shape

- Read the usage from the raw response body before rig's fallible conversion, where
  `ModelCandidate::completion` or the HTTP layer still holds it, and attach it to the attempt the
  `CallSlot` has in flight, so `settle` records it whether the conversion succeeds or not.
- The same seam serves `plan/next/usage-fields-reported-or-not.md`, which reads per-field usage
  from the provider's own object.

## Acceptance

- The unusable-then-retried fixture's session total counts both responses' usage, and the
  unusable attempt's `model.attempt` carries its usage beside its `unusable response` error.
