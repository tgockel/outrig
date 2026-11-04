# A model row's calibration is the session's, not each agent's

## Context

A chain holds a `Calibration` per candidate (`crates/outrig/src/agent/failover.rs`,
`FailoverModel::new`): the scale its provider's usage reports set, and, since #833, the window it
is held to once its provider rejected a request as over its input limit. Every agent builds its
own chain (`build.rs`), so a child learns both again from its own calls -- for the window, by
taking its own `400` and resending -- and nothing a child learns reaches the primary. The window a
provider *reports* is already session-scoped and keyed by `[models.<name>]` row (`window.rs`,
`Windows`, held beside the children).

## Shape

- Key `Calibration`s by model row in a session-scoped store beside `Windows`, each chain holding
  an `Arc<Calibration>` per candidate.
- With the limit learned once, a size rejection that outlasts its resends could count as a
  failure the next round gets past (`candidate_may_clear`), as a call too large for the window
  (`TooLarge`) does today; it stays terminal until then.

## Acceptance

- A child whose primary has been rejected for size is held to the lowered window from its first
  call, with no `400` of its own.
