# No top-level `request-timeout-secs`

`retry-budget-secs` exists in two places: a top-level default on `Config`, and a
per-provider override on the `OpenAi` / `Anthropic` variants, merged
repo-over-global in `merge.rs`. Its sibling `request-timeout-secs` exists only on
the provider variants -- `Config` has no field and `merge` has no line for one.

Noticed while implementing 0106, whose Acceptance criteria as queued asked for
the range check "at the top level and per provider" on the assumption that the
two keys were shaped alike. They are not; 0106's criteria were corrected to the
one path that exists, and this is the other half.

The asymmetry is a papercut rather than a bug. A user with three remote
providers who wants a uniform timeout writes the key three times, and cannot
express "this machine is on a slow link" in one line the way `retry-budget-secs`
lets them. The docs invite the comparison -- the two keys sit in adjacent rows of
the same table and are explained in the same paragraph -- which is what makes the
missing half read as an oversight.

## Sketch

- `Config::request_timeout_secs: Option<u64>`, `#[serde(default)]`, kebab-case,
  beside `retry_budget_secs`.
- One line in `merge.rs`: `repo.request_timeout_secs.or(global.request_timeout_secs)`.
- Resolution order at the call site (`remote_http_client` in
  `crates/outrig-cli/src/llm.rs`): provider, then top level, then
  `DEFAULT_REQUEST_TIMEOUT_SECS`. Exactly the budget's cascade.
- Validate the new path with the existing `validate_request_timeout_secs`, under
  a `"top-level request-timeout-secs"` path string.
- `doc/reference/config.md`: a top-level table row, and extend the validation
  rule 0106 added to name both paths.

## Notes

Purely additive: a new `Option` field on a `#[non_exhaustive]` struct, defaulting
to today's behavior when unset. It does **not** need the pre-0.2.0-final window,
which is why 0106 left it out rather than folding it in -- adding it later breaks
nothing, whereas 0106's *bound* had to land before the release froze the set of
accepted configs.

Regenerate `crates/outrig/public-api.txt`.

## Acceptance

- A top-level `request-timeout-secs` applies to a provider that sets none.
- A provider's own value still wins over the top-level one.
- Repo overrides global, matching `retry_budget_secs_repo_overrides_global`.
- Out-of-range and `0` are rejected at the top level too, naming that path.

## Dependencies

0106 (the range check and `REQUEST_TIMEOUT_SECS_CEILING` it introduces).
