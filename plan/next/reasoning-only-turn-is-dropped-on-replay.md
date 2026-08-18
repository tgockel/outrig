# A reasoning-only assistant turn is kept locally and dropped on replay

## Symptom

A turn that produced only reasoning is now retained in outrig's history and reported to the user
(`a_reasoning_only_turn_is_recovered_and_reported`). It does not survive the trip back out. On the
next turn the provider sees the user's prompt with no assistant message between it and the prompt
after it -- so a follow-up like "did you do anything?" arrives as a non-sequitur, with no trace of
the turn it is asking about.

## Where it goes

This is the OpenAI chat-completions arm specifically. Its
`TryFrom<OneOrMany<AssistantContent>> for Vec<Message>`
(`rig-core/src/providers/openai/completion/mod.rs`) returns `vec![]` when a message has no text and
no tool call, reasoning notwithstanding; its own `assistant_reasoning_alone_is_dropped` test pins
that as intended.

**Native Anthropic is not affected and must not be "fixed" alongside it.** A reasoning-only choice
is more than one content part or a non-`Text` one, so `is_empty_assistant_turn`
(`rig-core/src/agent/prompt_request/mod.rs`) -- which matches only a lone empty `Text` -- keeps it
in the run's history, and rig's Anthropic conversion maps `AssistantContent::Reasoning` back to a
`thinking` block. That path round-trips correctly today; changing it would duplicate or discard
reasoning that is currently carried.

A separate Anthropic case does exist and is *not* what this entry is about: a genuinely empty
`end_turn` body, which rig normalizes to an empty-text sentinel that `is_empty_assistant_turn` then
keeps out of history. There is no reasoning to preserve there, so it wants its own answer.

So outrig's `Vec<Message>` and the conversation the provider is shown diverge on the OpenAI arm,
and nothing downstream can tell.

## Why it matters beyond the cosmetic

Consecutive user messages are not merely untidy for a Bedrock-backed Claude reached over an
OpenAI-compatible gateway, which expects alternating roles. The gap is also silent: outrig's own
history looks complete, so `/reset` reads
as unnecessary and the user has no way to see that the model is missing a turn.

## Shape

Two directions, and they differ in who owns the invariant:

- **Synthesize a text part** when salvaging, so the message that lands in history is one the
  provider layer will carry -- e.g. the recovered reasoning as assistant text. Keeps outrig's
  history and the wire in agreement, at the cost of feeding a model its own reasoning back as
  speech, which some providers charge for and some object to.
- **Drop it locally too**, matching what the wire will do. Cheaper and honest, but throws away the
  one artifact `TurnEnd::recovered` exists to preserve, and the user has already seen it.

Worth deciding against `crates/outrig-cli/src/llm.rs`'s `extend_history_with_new_suffix`, which is
the only place that already reasons about history outrig did not author.

## See also

- `crates/outrig-cli/src/llm.rs` -- `run_turn_inner`'s `Ok` arm, `recover_non_text`, `TurnEnd`.
- `crates/outrig-cli/tests/anthropic_mock.rs` --
  `a_reasoning_only_turn_is_recovered_and_reported` asserts the history is retained; it does not
  assert what the *next* request carries, which is where a test for this would go.
