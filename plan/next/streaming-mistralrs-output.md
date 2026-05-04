# Streaming output for the in-process mistralrs path

> **Status:** preliminary spec. Carved into a numbered task in `plan/todo/` when ready.

## Context

`src/llm/mistralrs.rs::stream` (lines ~238-246) returns an error:
streaming is unimplemented on the in-process path. Every turn goes through
`Agent::prompt(...).await` in `src/llm.rs::run_turn_inner`, which blocks
the REPL until the model has decoded its *entire* reply.

For OpenAI-style providers this is bearable -- they decode at hundreds of
tok/s, so even a verbose answer lands in seconds. For mistralrs on CPU
the math is brutal: a 1.5B Q4_K_M on a modern AMD core decodes at roughly
15 tok/s, so a 4000-token reply is a four-minute stare at a blank prompt
with no signal that anything is happening. Bigger models are worse.

The fix is to stream the assistant's reply token-by-token to stderr (or
the REPL's transcript buffer) as it's generated, so the user sees
progress and can hit Ctrl-C if the model wanders.

## Goals and non-goals

**In scope:**

- Implement `MistralrsModel::stream` so it returns a working
  `StreamingCompletionResponse`. mistralrs-core's `Pipeline::send`
  already supports streaming via the `Response::Chunk` variant on the
  mpsc receiver -- the v0 implementation just collected all chunks
  before returning.
- Plumb the stream through `RigAgent::run_turn` so each emitted token
  is printed to stderr as it arrives. Final history bookkeeping
  (`history.extend(response.messages)`) stays at end-of-turn.
- Same `OutrigPromptHook` tool-call cap and trace-line behavior: tool
  calls still print `[outrig] tool call: ...` between streamed
  segments. Tool-call decoding flushes the buffered text before the
  call's arguments, so the rendered transcript is in execution order.

**Out of scope:**

- Streaming for the OpenAI-style path. rig's `prompt(...).stream()`
  already exists; if/when we want streaming on that side it's a
  separate caller-loop change. This task is purely about the in-process
  branch reaching parity.
- Pretty-printing (ANSI colors, prefix indicators). Plain stderr writes
  are enough for v1.
- Mid-stream cancellation via Ctrl-C handling. The REPL already
  forwards SIGINT; making it actually interrupt mid-decode is a
  follow-up.

## Approach sketch

mistralrs-core's pipeline writes `Response` variants to an `mpsc::Sender`.
Today `MistralrsModel::completion` collects them all then returns once
`Response::Done` arrives. For streaming:

1. `MistralrsModel::stream` builds the same `NormalRequest` but takes a
   non-collecting receiver. It returns a
   `StreamingCompletionResponse<()>` whose underlying stream yields
   `rig::completion::AssistantContent` chunks.
2. Translate each `Response::Chunk` (containing partial text and any
   in-progress tool-call deltas) into the right rig stream item. Pay
   attention to tool-call assembly: mistralrs emits tool-call name +
   args incrementally; rig wants a complete `ToolCall` object on the
   stream, so buffer until the tool call is fully formed.
3. `run_turn_inner` (`src/llm.rs:352`) becomes two-phase: poll the
   stream, write text chunks to stderr as they arrive, accumulate the
   final `messages` for history. Use `extended_details()` after the
   stream completes to collect the same metadata it does today (tool
   calls, etc.).
4. `Repl::run`'s `on_prompt` callback returns `Result<String>`; today
   the assistant's reply is *the* return value. With streaming, the
   reply is also written incrementally; the returned String can be the
   already-printed text or empty (the REPL re-prints it today, so we'd
   want it empty to avoid duplication). Decide which.

## Files

- `src/llm/mistralrs.rs` -- replace the `stream()` placeholder with a
  real implementation. Lift the channel-collection loop from
  `completion()` into a shared helper if it's clean; otherwise keep
  separate.
- `src/llm.rs` -- `run_turn_inner` switches from `agent.prompt(...)`
  to `agent.prompt(...).stream(...)` (for the mistralrs arm at least).
  The OpenAi arm can keep the non-streaming path until separately
  decided.
- `src/repl.rs` -- `on_prompt` contract clarified for streaming
  callers; suppress the trailing reprint.
- `tests/llm_resolve.rs` or a new `tests/mistralrs_streaming.rs` --
  exercise the stream against a tiny stub that emits scripted chunks.
  No real model load; mock the pipeline at the `Response` level.

## Acceptance

- `outrig run` against a mistralrs agent prints assistant tokens to
  stderr as they're decoded. A 100-token reply produces ~100 stderr
  writes spread across the decode wall-clock.
- A model still working but slow (5+ tok/s) produces visible output
  every few seconds rather than dead silence.
- Existing tests pass; the REPL doesn't double-print replies.

## Notes

- mistralrs-core's tool-call streaming format is documented in their
  pipeline mod -- worth re-reading before implementing.
- Tool calls happen at `[outrig] tool call: ...` boundaries today;
  stream-aware printing must flush text *before* the call line.
- This task is independent of GPU support
  (`mistralrs-gpu-device.md`) but combined they make the in-process
  path actually pleasant to use; a 7B Q4_K_M on a CUDA GPU streams at
  100+ tok/s and the user sees output instantly.
