# `LlmProvider` construction now speaks two idioms

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

## Sketch

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
