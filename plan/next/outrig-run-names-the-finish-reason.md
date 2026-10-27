# `outrig run`'s unusable-response error names no finish reason

#721 made the library's `RetryingModel::attempt` put the provider's finish reason first in a
`ResponseError` it could not use (`finish reason content_filter: Response contained no message or
tool call (empty)`), so `run-new`'s retry lines, events and round errors say why a reply ended.

`outrig run` keeps its own copy of the retry layer (`crates/outrig-cli/src/llm/retry.rs`,
`RetryingModel::completion`), and it is unchanged: its retry line and the final
`the model returned a response outrig could not use (...)` still say only `empty`. That layer
holds no body to read the reason from. Its HTTP layer hands a `2xx` back with the body left lazy
(`into_lazy_response`) and keeps nothing, and the only finish-reason read
(`provider_finish_reason`) runs on a completion rig made.

## Shape

- The HTTP layer keeps the body of the latest `2xx` where the model layer can read it, as the
  library's `AnsweredBody` and `Settling::body` do, and the model layer names the reason the same
  way.
- Or the `outrig run` path moves onto the library's retry layer, which removes the copy.

## Acceptance

- In `outrig run`, a reply the provider ended with `content_filter` and no content names
  `content_filter` in its retry line and its final error.
