# `outrig-cli`'s `[Unreleased]` changelog is stale

`crates/outrig-cli/CHANGELOG.md`'s `[Unreleased]` section describes model aliases as they
behaved when `e912388` landed, and three later commits contradict or extend it. The 0.2.0-rc.2
cut was library-only, so nothing forced the issue; the CLI's next dated section must not inherit
these.

## Wrong today

- The alias entry says selection "answers 'am I configured for this' rather than 'is this
  endpoint up'" and that an alias "does **not** fail over when a vendor rate-limits
  mid-session". `233dd1e` reversed that: `FailoverModel` moves a turn to the next candidate from
  inside one `completion()` call, so a rate limit on the head no longer ends the turn. Tool calls
  already run in that turn are not replayed, which is why the failover sits there rather than
  around `agent.prompt(..)`.

## Missing entirely

- `6b119a9` -- an endpoint that never answered gets a 30-second connect budget instead of the
  full `retry-budget-secs`, so a typo in `base-url` no longer costs ten minutes of retry lines.
  Once any attempt gets bytes back, the full budget applies for the rest of that request.
  `retry-budget-secs = 0` remains the single "no retries" knob.
- `9f44ed0` -- a provider response outrig cannot use ends the turn rather than the session.

## Acceptance

`crates/outrig-cli/CHANGELOG.md`'s `[Unreleased]` describes the alias behavior that ships, and
carries entries for both commits above. Check the same way this was found: walk
`git log <last-cli-tag>..HEAD` and confirm every commit touching `crates/outrig-cli/src` either
has an entry or is deliberately invisible to users.
