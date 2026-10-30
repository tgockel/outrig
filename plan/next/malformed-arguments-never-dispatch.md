# A tool call with malformed arguments never reaches the truncation ladder or the breaker

## Context

Under rig 0.40 a tool call's arguments reached OutRig's tools as the string the model wrote, so
`crates/outrig-cli/src/builtin_tool.rs`'s truncation ladder could read a cut-off
`outrig__set_result` payload (`looks_truncated`) and say so, and the repeat breaker
(`RepeatTracker` in `crates/outrig-cli/src/llm.rs`) counted the call. rig 0.45 decodes the
arguments itself: a call whose arguments are not a JSON object never dispatches, so neither
`on_dispatch` nor `on_outcome` sees it, and the model is told only that the arguments were
malformed. An OpenAI-style reply cut off mid-`arguments` is exactly that case, so the ladder's
first rung -- name the ceiling, say the payload was cut -- is dead for it, and a model that
repeats the same malformed call is not stopped by the breaker.

## Shape

- `AgentHook::on_invalid_tool_call` (rig-agent) sees a `MalformedArguments` call before rig
  answers it. An arm there for `outrig__set_result` can run `looks_truncated` over the raw text
  (`ToolFunction::raw_arguments`) and rewrite the feedback with the ladder's remedy, and feed the
  call to `RepeatTracker` so the breaker counts it.
- The `on_dispatch` and `on_outcome` paths stay as they are for well-formed calls.

## Acceptance

- A chat reply whose `arguments` string ends mid-value on `outrig__set_result` is answered with
  the truncation remedy naming `max-tokens`, not rig's generic malformed-arguments text.
- Three such replies in a row trip the repeat breaker as three well-formed failures do.
