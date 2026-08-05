# 0111 -- `LlmProvider` construction now speaks two idioms

`LlmProvider::openai(base_url, api_key, request_timeout_secs)` and
`::anthropic(..)` take their three fields positionally. `retry-budget-secs`
could not join them without breaking every caller, so it arrived as
`with_retry_budget_secs(Option<u64>)` -- a consuming builder step bolted onto a
positional constructor.

That works, and it was the right call for an additive release, but it leaves
two problems:

- Construction is split: three fields one way, the fourth another, with nothing
  in the signature saying why.
- `with_retry_budget_secs` is a no-op on `Mistralrs`, which has no HTTP layer.
  Silently discarding a value is convenient for callers mapping over a
  heterogeneous list, and a trap for anyone who expects it to have taken.

The next optional connection field makes this worse, not better.

## Goal

Give `LlmProvider` one construction idiom instead of two, while the breaking window is open.
Rust cannot overload `openai`, so the reshape is a break whenever it happens; taking it before
0.2.0 final is the difference between one more line in an existing `### Changed` section and
waiting for the next major.

## Deliverables

An options struct, matching the shape 0095 used for the session options:

```rust
let provider = LlmProvider::openai(base_url, api_key, OpenAiOptions {
    request_timeout_secs: Some(600),
    retry_budget_secs: Some(300),
    ..Default::default()
});
```

`#[non_exhaustive]` with a `Default`, so later fields are additive again. The
`Mistralrs` no-op disappears because the options type belongs to the remote
variants only.

This is a breaking change to `openai` / `anthropic` and removes
`with_retry_budget_secs`, so it wants the next breaking window rather than a
point release.

## Acceptance

- `LlmProvider::openai` / `::anthropic` take an options struct; the builder
  method is gone.
- `crates/outrig/public-api.txt` regenerated.
- `crates/outrig/CHANGELOG.md` records it under `### Changed` as breaking, with
  the one-line migration.

## Dependencies

None hard. Queued last on purpose: rc.1 already shipped `with_retry_budget_secs`
as the non-breaking workaround and the variants are `#[non_exhaustive]`, so
downstream cannot construct them anyway. That makes this the one pre-final entry
that is API shape rather than API correctness, and the first to cut if the
window tightens. 0106 also touches `request-timeout-secs`; if both land, this one
moves the field into the options struct rather than reasoning about it twice.
