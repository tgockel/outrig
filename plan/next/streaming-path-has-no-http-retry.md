# `RetryingHttpClient::send_streaming` delegates without retry

`crates/outrig-cli/src/llm/retry.rs` retries in `send`, and hands
`send_multipart` and `send_streaming` straight to `reqwest::Client`'s own
`HttpClientExt` impl.

Both are correct today and for different reasons:

- **multipart** -- a `MultipartForm` is not cloneable, so there is no body to
  replay. outrig has no multipart path at all.
- **streaming** -- outrig's remote turns are non-streaming. The streaming path
  (`run_turn_streaming_inner`) is `#[cfg(feature = "local-llm")]` and backed by
  mistralrs, which is in-process and never reaches an HTTP client.

The trap is the day a streaming remote provider is wired. It will get no retry
at all, silently, and the failure will look like the provider's.

Streaming is also not a straight port of the `send` loop: a failure can land
*mid-stream*, after the caller has already seen bytes. Retrying from scratch
would duplicate the prefix. The honest options are to retry only failures that
happen before the first chunk, or to not retry streaming at all and say so in
the provider docs.

## Sketch

- Retry only the pre-first-chunk window in `send_streaming`: the request is
  replayable up to the point where the response body starts yielding.
- Or leave it, and make `doc/concepts/llm-providers.md`'s transient-failures
  section name streaming as the exception, so the gap is documented rather than
  discovered.

Nothing to do until a streaming remote provider exists. Filed so that work
starts by reading this.

## Acceptance

- Whichever route: a test that a streaming remote turn behaves as documented on
  a transient failure, and `llm-providers.md` saying which it is.
