# The `openai` provider arm sends no `max-tokens` at all

## Symptom

A think-heavy turn against an OpenAI-style provider comes back carrying reasoning and no text.
`crates/outrig-cli/tests/anthropic_mock.rs`'s `a_reasoning_only_turn_is_recovered_and_reported`
pins what outrig now *does* about that -- salvage the reasoning, name the finish reason, refuse to
end the turn in silence -- but not what put the turn in that state. The provider truncated it at a
ceiling outrig never chose and cannot see.

## The asymmetry

`build_single` and `build_candidate` (`crates/outrig-cli/src/llm.rs`) hand `candidate.max_tokens`
straight to `finish_agent` for the OpenAI arm, and `finish_agent` skips the builder call entirely
when it is `None`. So a model with no `max-tokens` in config produces a request with no ceiling on
the wire, and whatever default the endpoint picks governs.

The Anthropic arm does not have this hole. It routes through `anthropic_model()`, whose three-tier
precedence -- configured value, then rig's published ceiling for a recognized identifier, then
`ANTHROPIC_FALLBACK_MAX_TOKENS` -- guarantees a number, and `warn_fallback_ceiling` says so when
the last tier fires. That tier exists because Anthropic *rejects* a request carrying no ceiling;
the OpenAI schema does not, so the gap was never forced into the open.

## Why this was not just fixed

The obvious symmetry -- give the OpenAI arm the same fallback -- is not obviously safe. The
Anthropic fallback is a floor under an API that would otherwise refuse the request. Applying a
number to arbitrary OpenAI-compatible endpoints is a **cap**, and one set below what a given
endpoint would have allowed introduces exactly the truncation this entry is about. `32_768` is
chosen against Anthropic's published ceilings and means nothing to a gateway fronting some other
model.

So the design question is what a defensible tier 3 looks like here, and the candidates differ in
kind:

- **No fallback, better diagnostics.** What ships today: `report_textless_completion`
  (`crates/outrig-cli/src/llm/retry.rs`) names the finish reason and says whether a ceiling was
  sent. Cheap, no regression risk, but leaves the user to discover `max-tokens` from a warning.
- **Fallback only for identifiers outrig can recognize.** The identifiers in the wild are
  gateway-shaped (`aws/anthropic/bedrock-claude-opus-5`), so this needs a parse that would have to
  keep pace with every gateway's naming -- the thing
  `plan/next/failure-label-drops-the-source-chain.md` is already wary of.
- **Require `max-tokens` for `style = "openai"` at config load.** Honest and static, but breaks
  every existing config, and `config-init` would have to pick a number it equally cannot know.

## See also

- `crates/outrig-cli/src/llm.rs` -- `build_single`, `build_candidate`, `anthropic_model`,
  `warn_fallback_ceiling`, `ANTHROPIC_FALLBACK_MAX_TOKENS`.
- `crates/outrig-cli/src/llm/retry.rs` -- `report_textless_completion` and
  `provider_finish_reason`, the diagnostics that stand in for the fix today.
- `plan/next/validate-max-tokens-against-the-ceiling.md` and
  `plan/next/clamped-ceiling-is-silent.md` -- the other two open questions about this same
  ceiling, worth deciding together.
