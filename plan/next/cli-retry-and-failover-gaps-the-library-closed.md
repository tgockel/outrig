# The CLI's retry and failover have three gaps the library's copy closed

## Context

`0003-15` copied `crates/outrig-cli/src/llm/retry.rs` and `llm/failover.rs` into
`crates/outrig/src/agent/`, and left the CLI's untouched so merges from the 0.2.x line stay clean.
That task's review found three defects in the copy. Each is fixed in the library, and each is
still in the CLI's original:

- **Anthropic's `529` is not retried.** `is_retryable_status` lists `500`, `502`, `503`, and
  `504`, not Anthropic's documented, temporary `overloaded_error`. A lone Anthropic model ends
  the turn on the first one, and a chain moves on although the same model would have answered.
- **A request that cannot succeed as sent is retried for the whole budget.** `send_with_retry`
  retries every transport error that is not a connect failure, including a redirect loop that
  reached reqwest's limit and a request reqwest could not build. With the default budget, that is
  nearly ten minutes before the turn fails or a chain moves on. The library returns such an error
  at once (`is_permanent`), and `is_transient` agrees.
- **A chain that spans providers sends Anthropic reasoning it cannot take.** An OpenAI-compatible
  model's `reasoning_content` becomes unsigned reasoning in the history. rig's Anthropic adapter
  sends that as a thinking block with no signature, which Anthropic refuses. So after one move to
  or from an OpenAI-compatible model, every call to an Anthropic candidate fails. The library
  assembles each call's view for its candidate's provider (`budget::Wire`, in
  `History::assemble`). That leaves unsigned reasoning out of a call to Anthropic, records what
  was left out in the call's manifest, and leaves the stored conversation alone.

## Shape

Port the three fixes across, and the tests named in `0003-15`'s `## Decisions`:

- `is_retryable_status` with `529`.
- `is_permanent` before the retry is scheduled, and in `is_transient`.
- A fit for each candidate's provider on every attempt in `FailoverModel::completion`. The CLI
  has no view store to do it in, and its single-candidate path never mixes providers, so only
  the chain needs it.

## Acceptance

- A `529` followed by a reply retries the same model.
- A redirect loop moves to the next candidate with no retry.
- A tool round answered by an OpenAI-compatible model and then moved to an Anthropic one sends no
  unsigned thinking block, checked on the Anthropic request body.
