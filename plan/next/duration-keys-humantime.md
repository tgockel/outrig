# Duration keys spell their unit in the name

Every duration in the config encodes its unit in the key: `retry-budget-secs`,
`request-timeout-secs`. The same convention covers bytes
(`tool-result-max` / `--max-tool-result-bytes`). There is no duration parser
anywhere in the tree -- no `humantime`, no `duration-str`.

`request-timeout = "10m"` reads better than `request-timeout-secs = 600`, and
`"1h"` beats `3600`. The gap is widest exactly where it matters: 0106 exists
because `request-timeout-secs = 86400` is an easy value to write without
noticing it is a day. `request-timeout = "24h"` is self-evidently absurd on the
page.

## Why this is one change, not two

`request-timeout-secs` and `retry-budget-secs` are documented in adjacent table
rows, explained in the same paragraph, and compared directly against each other
("that matters when it is set near `request-timeout-secs`"). Converting one and
leaving the other makes the pair actively confusing. So the coherent unit of
work is every duration key at once:

- `Config::retry_budget_secs`
- `LlmProvider::OpenAi` / `::Anthropic` -- `request_timeout_secs`, `retry_budget_secs`
- `LlmProvider::with_retry_budget_secs`, and the positional `openai` / `anthropic`
  constructors
- `DEFAULT_RETRY_BUDGET_SECS`, `RETRY_BUDGET_SECS_CEILING`,
  `REQUEST_TIMEOUT_SECS_CEILING`, `DEFAULT_REQUEST_TIMEOUT_SECS`
- both provider tables and the validation rules in `doc/reference/config.md`
- `crates/outrig/public-api.txt`

Interacts with `plan/next/provider-construction-options-struct.md` (0111), which
already reshapes those constructors -- if both are wanted, do them together
rather than rewriting the same signatures twice.

## Notes

**Breaking**, and to keys that shipped in 0.2.0-rc.1, so it wants the pre-final
window or a major. Accepting both spellings (string key alongside a deprecated
integer one) is the non-breaking alternative, at the cost of a permanent second
parse path and a deprecation to carry -- hard to justify for keys this young.

`humantime` accepts `"0s"`, so it does **not** subsume 0106's zero check; that
validation stays either way. Worth confirming the crate's exact accepted grammar
before committing to it in a published schema -- what it does with `"1h30m"`,
bare `"600"`, and whether the error text is good enough to surface raw.

## Acceptance

- Every duration key takes a `humantime` string; no `-secs` key remains.
- Bounds and the zero rejection survive the conversion, with messages that quote
  the value as written rather than a normalized second count.
- `CHANGELOG.md` records the rename with a before/after for each key.

## Dependencies

None hard. Sequence with or after 0111, which touches the same signatures.
