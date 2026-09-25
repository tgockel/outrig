# A `PythonAgent` round that produces only reasoning replies with nothing

## Context

`outrig-cli`'s loop salvages a turn whose final message carries no text. rig's `output` is the
final turn's text parts concatenated, so a think-heavy turn cut off at the provider's output
ceiling arrives as `""` even though the model produced, and the user paid for, real reasoning.
`crates/outrig-cli/src/llm.rs` handles this with `recover_non_text`, `TurnEnd::recovered`,
`is_silent`, and `silent_report`. The REPL then says the turn produced only hidden reasoning,
likely because of the ceiling, and prints it.

`0003-04` copied the minimum a round needs into `crates/outrig/src/agent/`, and this was not part
of it. `PythonAgent::round` returns `response.output` as it is. A reasoning-only round therefore
returns `""`, which a caller cannot tell from a model that chose to say nothing.
`plan/next/openai-arm-sends-no-ceiling.md` covers the most common way to get there, and the copy
inherits that gap as well.

## Shape

- Copy `recover_non_text` and `is_blank` into `agent/round.rs`, and salvage when the output is
  blank.
- The open question is how the salvage reaches the caller. `round` returns a `String` and its
  error is boxed; there is nothing structured to carry "recovered reasoning" in. Either:
  - the reply text says so, as a stopped round's `(round ended: ...)` already does; or
  - `round` returns something richer. That grows the public surface, so it belongs with whichever
    task next reshapes `PythonAgent` -- `0003-15` settles what "ended the round" means, and
    `0003-13` records rounds as events.
- A test in `agent/agent_tests.rs` against the mock: a turn whose content is only a `thinking`
  block.

## See also

- `crates/outrig-cli/tests/anthropic_mock.rs`, `a_reasoning_only_turn_is_recovered_and_reported`,
  which pins the CLI side.
