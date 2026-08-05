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
- New `ConfigValidationError` variants beside `RetryBudgetSecsTooLarge`; the
  enum is `#[non_exhaustive]`, so the additions are not themselves a break.

## Acceptance

- Over-ceiling values rejected on every remote provider, with the
  `providers.<name>.request-timeout-secs` path in the message.
- `config.md`'s "Validation rules" gains the row.
- A `config_merge.rs` case for the provider path, matching the
  `retry-budget-secs` ones, on both remote styles.
- `0` is rejected, with a message that says what `0` would do.
- The ceiling value itself is accepted, so the bound is inclusive.

## Dependencies

None. The scheduling constraint is the only one that matters: the bound has to
land before 0.2.0 final, since adding it afterwards rejects configs that 0.2.0
accepted, which is a user-facing break rather than a fix.

## Decisions

1. **There is no top-level `request-timeout-secs`, so there is one path, not
   two.** As written this task asked for rejection "at the top level and per
   provider", mirroring `retry-budget-secs`. That sibling genuinely has two
   paths -- a `Config::retry_budget_secs` default plus a per-provider override,
   merged in `merge.rs`. `request-timeout-secs` has only ever been a field on
   the `OpenAi` and `Anthropic` variants; `Config` has no counterpart and
   `merge` has no line for one. The Acceptance criteria above were corrected to
   the surface that exists. Adding the top-level default is additive, does not
   need the pre-final window, and is filed as
   `plan/next/top-level-request-timeout-secs.md` rather than folded in here.

2. **`0` is rejected, and the reason was verified rather than inherited.** The
   `plan/next/` entry this task was groomed from claimed `Duration::ZERO`
   *disables* the timeout in reqwest; the task file claimed the opposite. Read
   against the pinned reqwest 0.13.4 (`Cargo.toml` pins it, and
   `remote_http_client` is the only site that builds the LLM client):
   `builder.timeout(d)` stores `Some(d)`, which becomes
   `tokio::time::sleep(d)`, which `PendingRequest::poll` polls before anything
   else -- ready immediately at zero, returning `TimedOut`. No `is_zero`
   special-case exists anywhere in its `src/`. So `0` is an immediate timeout:
   a config that parses, validates, and then cannot work.

3. **`0` means something different here than on `retry-budget-secs`, and that
   is not an inconsistency.** `retry-budget-secs = 0` means "do not retry" and
   stays legal, pinned by `zero_retry_budget_secs_is_accepted`. That key counts
   attempts, so zero attempts-after-the-first is a coherent request. This key is
   a per-attempt deadline, so zero is not a weaker deadline but an impossible
   one. The two keys are one-sided in opposite directions for that reason: the
   budget has a ceiling only, the timeout has both bounds.

4. **Ceiling is 3600, matching `RETRY_BUDGET_SECS_CEILING`.** An endpoint that
   has not answered within a minute is not going to, so an hour is already a
   degenerate case; the value is a fat-finger guard, not a tuning knob. Sharing
   the sibling's number keeps one number to remember for the two keys that are
   documented side by side. The bound is inclusive -- `3600` is accepted --
   which the `at most` wording in both messages already implies and which now
   has a test.

5. **The unit stays in the key name.** `request-timeout` parsed with
   `humantime` would read better (`"24h"` makes this task's motivating
   fat-finger self-evident in a way `86400` does not), but the repo has no
   duration parser in the tree and spells the unit in every comparable key:
   `retry-budget-secs`, `tool-result-max`/`--max-tool-result-bytes`. More to the
   point, `request-timeout-secs` shipped in 0.2.0-rc.1, so a rename is a
   breaking change to a released key, and it would not stay contained -- the
   sibling is explained in the same paragraph, so the coherent change converts
   every duration key at once. Filed as
   `plan/next/duration-keys-humantime.md`. Note that `humantime` accepts `"0s"`,
   so it would not subsume the zero check either way.

6. **Both rejection messages state the full range, from review.** The ceiling
   message first read `must be at most 3600 seconds`, which is what the sibling
   budget check says -- but that key is genuinely one-sided and this one is not,
   so the wording described a rule the code does not enforce. A user who fixed
   an over-ceiling value by setting `0` would have hit a second rejection the
   first message gave no hint about. Both now open with
   `must be between 1 and 3600 seconds`, matching the two-sided checks already
   in the file (`ToolCallMaxOutOfRange` and the subagent limits), and
   `RequestTimeoutSecsZero` carries a `max` field to say it. The zero message
   keeps its explanation of *why* -- an immediate timeout rather than a disabled
   one -- because the sibling's `0` means the opposite and the reader is
   entitled to expect the same here.

7. **`outrig build` does not enforce these bounds, and that is deliberate.**
   The provider loop sits inside `ValidationOptions::validate_llm`, which
   `validate_for_build` sets false, so `outrig build` accepts a timeout
   `outrig run` rejects. It matches how every other LLM-side rule already
   behaves and is right on the merits -- the bound exists to stop an interactive
   turn wedging, and a build has no turn to wedge -- but it was undocumented.
   Now noted on the `validate_llm` field and in the CHANGELOG entry.

8. **The CHANGELOG entry is filed as Breaking under `### Changed`.** It first
   went under `### Added` on the grounds that the two error variants are
   additive. They are, but the *validation* is not: a config 0.2.0-rc.1 accepted
   can now fail to load, which is exactly why this task was scheduled before the
   release rather than after. Every comparable entry in this file marks that
   Breaking.
