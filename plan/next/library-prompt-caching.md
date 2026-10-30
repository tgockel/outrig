# The library loop asks for no prompt caching

## Context

`outrig run` marks three cache breakpoints on an Anthropic request (#669): the last tool, the
last system block, and the last block of the last user message, through rig's
`Messages::with_prompt_caching()`. The library loop (`crates/outrig/src/agent/build.rs`) and so
`run-new` ask for none, as the 0.2.x entry said: a view that is cut and reshaped between calls
would miss the moving breakpoint more often than it hit it. rig 0.45 also offers the request-level
`CompletionRequest::cache_points` (`CachePoint::{Tools, System, Messages(n), Within { .. }}`),
which the chain (`failover.rs`) could set per attempt without the wire's automatic placement, and
`GenerationOptions::cache(CacheRetention::Short | Long)` for the TTL.

## Shape

- Mark `Tools` and `System` on every attempt the chain sends: the part every call of a session
  shares, which no cut of the view changes. The moving marker stays off until the view's cuts are
  understood against the cache's prefix rule.
- `cache_points` is per request, so a candidate on an OpenAI-compatible endpoint is sent none,
  and the Anthropic wire alone encodes them. `with_prompt_caching()` is not used: it adds the
  moving marker too.
- `tests/anthropic_mock.rs`'s marker assertions are the model; the library's
  `agent_tests.rs` has a test that today asserts `cache_control` absent.

## Acceptance

- Every Anthropic request of a `run-new` session carries `cache_control` on its last tool and
  last system block, and nowhere else; an OpenAI-style request carries none.
- A read shows as `cached_input_tokens` on `model.attempt`.
