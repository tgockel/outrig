# The mistralrs adapter replays no reasoning to the chat template

## Symptom

Since #179 the in-process adapter carries a local model's reasoning into outrig's history, but
`translate_assistant` (`crates/outrig-cli/src/llm/mistralrs.rs`) drops every
`AssistantContent::Reasoning` on the way back out, turns with text or tool calls included. Inside
one agent turn that costs a reasoning model its chain of thought at every tool round-trip: a
Qwen3-style template renders an assistant step's `reasoning_content` inside `<think>` for the
steps after the last user message -- exactly the tool loop -- and never receives any. Nothing
regressed, since those requests carry the bytes they did before #179; the adapter just translates
reasoning in one direction only.

## Where it goes

mistralrs hands every key of a request's message map to the Jinja template unchanged (the message
loop in `mistralrs-core`'s `pipeline/chat_template.rs`). So a `reasoning_content` key beside
`content` and `tool_calls` -- the joined `Reasoning::display_text()`, when not blank -- lets each
GGUF's template decide whether to render it. It is also the key rig's own OpenAI arm replays
reasoning under for an assistant turn with text or a tool call ("some require it to be echoed back
on assistant tool-call turns", `rig-core`'s `providers/openai/completion/mod.rs`).

## Open questions

- Templates disagree on the key. `reasoning_content` is the llama.cpp/DeepSeek dialect; check what
  the templates of the models outrig is actually run with read before settling on one.
- A template that renders the key on *every* assistant message, not only those after the last
  user message, would start feeding old reasoning back as context.
- A reasoning-only turn is not this entry's question: whether that turn is replayed at all is
  `plan/next/reasoning-only-turn-is-dropped-on-replay.md`'s.

## See also

- `crates/outrig-cli/src/llm/mistralrs.rs` -- `translate_assistant`, and `translate_choice` /
  `translate_stream_chunk` for the inbound half.
