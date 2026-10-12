# A usage field the provider did not report is null, not zero

## Context

`observability.md` says a provider metric that is unavailable -- cached or reasoning tokens from a
provider that does not report them -- is recorded as `null`, never as zero. `0003-19` made that
true of a whole usage: an attempt that reported nothing, a round none of whose attempts did, and
`input_tokens_max` without a call to take it from are `null`. Within a usage that was reported,
each field is still a number: rig 0.40's `Usage` fills a field the provider left out with `0`, so
`cached_input_tokens: 0` from a provider that has no cache reads the same as a cache miss.

## Shape

- Read each field from the provider's own usage object rather than rig's, and carry `Option<u64>`
  per field on `harness::event::Usage`. The body is in hand at the HTTP layer, where #523 keeps it
  on the in-flight attempt (`AnsweredBody`, read by `retry::body_usage` for a response rig could
  not use). That reader maps through rig's zero-filling `token_usage()` as well, so it takes the
  same change, and the completed path can read the same stashed body in place of rig's usage.
- Or wait for rig to say which fields a provider reported, and map them then.
- Either way the public `Usage` changes its field types, so do it before the 0.3.0 release fixes
  the surface, or add the per-field form beside the existing one.

## Acceptance

- An Anthropic reply with no cache fields records `cached_input_tokens: null`; one that reports
  `0` records `0`.
