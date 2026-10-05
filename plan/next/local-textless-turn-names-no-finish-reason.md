# A textless local turn is not told its finish reason or ceiling

## Symptom

When a remote turn comes back with no text and no tool call, `report_textless_completion`
(`crates/outrig-cli/src/llm/retry.rs`) prints the provider's finish reason and the `max-tokens` the
request carried, ahead of `TurnEnd::silent_report`. A `local-llm` turn gets the report but not that
line. So a reasoning-only local turn -- nearly always a `length` finish inside a `<think>` block --
hears only that the cut-off "usually" means the output-token ceiling, and not which ceiling,
although the adapter saw the `length` finish on the last chunk.

## Where it goes

`report_textless_completion` runs only in `RetryingModel::completion`, which the streaming local
path never calls, and `run_turn_streaming_inner` never sees a `CompletionResponse` to hand it. The
adapter reads `finish_reason` only to spot the final chunk (`translate_stream_chunk`,
`crates/outrig-cli/src/llm/mistralrs.rs`) and then drops it. Carrying it on
`MistralrsStreamResponse`, the provider's own serialized final-response type, would let a
`provider_finish_reason`-style reader find it without a local special case in `llm.rs`.

## See also

- `crates/outrig-cli/src/llm.rs` -- `run_turn_streaming_inner`, `TurnEnd::silent_reason`.
- `plan/next/openai-arm-sends-no-ceiling.md` -- the remote arm's side of "which ceiling applied".
