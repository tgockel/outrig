# 0003-04 -- The model's only tool submits Python, and `outrig` drives a round

## Context

This is the task where the phase's thesis becomes code: the model is handed one tool, and it
submits Python. It is also the task that starts the crate split.

`crate-split-tradeoffs.md` chose duplication over a move, because every file a move would touch is
one the 0.2.x line is still editing. What it did not settle is *how much* to copy at once. This
task copies the minimum a single round needs -- model resolution, agent construction, the round
call -- and leaves retry, failover, and the local-llm arm to `0003-15`. The exit criterion that
the loop lives in `outrig` is met from the first commit, and nothing has to be unwound later.

Two constraints from the same page bound the surface. rig stays a private dependency, so the
library's error type converts at the boundary rather than carrying `rig::completion::PromptError`.
And the new modules stay private except for one entry point, which exists because Rust requires it
rather than because anyone should build on it.

## Goal

`outrig` can resolve a model, build an agent whose only tool submits Python to the session
interpreter, and drive one round to completion -- reachable from `outrig-cli` through a single
public function.

## Deliverables

- A private `agent` module in `outrig`: model and provider resolution from the existing config
  types, agent construction, and the round call.
- The tool: one input, the source to run; one output, the execution's result. Bounded by the same
  `tool-result-max-bytes` the existing surface applies, since a traceback or a captured build log
  is exactly the oversized result that ceiling exists for.
- **An error type that names no rig type.** Conversion at the boundary, written once per variant.
- **The effective output ceiling readable outside agent construction.** It is resolved here, and
  `plan/next/clamped-ceiling-is-silent.md` records that it currently never escapes `build_agent`.
  `0003-13` records it as an event and cannot while it is trapped; exporting it at the point it is
  computed is cheaper than retrofitting it into a copy later.
- **One `pub` entry point**, and nothing else: whatever starting a session and driving a round
  requires, in terms that name no rig type. `crates/outrig/public-api.txt` regenerated, growing by
  that entry and nothing more.
- **Explicitly not copied here**: `llm/retry.rs`, `llm/failover.rs`, `llm/mistralrs.rs`,
  `llm/registry.rs`, `builtin_tool.rs`, `self_tool.rs`, `subagent/`. The first two arrive in
  `0003-15`; the in-process backend is deprecated and never arrives; the rest are unqueued.
- `outrig-cli`'s existing harness is untouched. Not "mostly untouched" -- a merge from the 0.2.x
  line must apply without conflict, which is the entire argument for copying.

## Acceptance

- A round driven against a mock provider: the model emits a tool call carrying source, the source
  runs in the interpreter, and the result reaches the model.
  `crates/outrig-cli/tests/anthropic_mock.rs` is the shape -- the mock harness that exists on this
  line. The prototype's
  `openai_mock.rs` is on a branch that never merges, so a second provider fixture is new work and
  should be named as such rather than cited as precedent.
- **The tool's result is bounded**, and a result past the ceiling arrives truncated with the
  marker rather than whole.
- `crates/outrig/public-api.txt` regenerated and clean, and the diff against the previous snapshot
  shows exactly one added entry. The `public-api` CI job is the gate.
- `git diff` shows no change under `crates/outrig-cli/src/llm*`, `rig_tool.rs`, or `subagent/`.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **How much of the loop the minimum actually is -- resolve while doing it.** Model resolution
   drags in alias handling and per-model ceilings. If a piece turns out to be load-bearing for one
   round, copy it and say so here; if it is only load-bearing for retry, it belongs in `0003-15`.
2. **Whether the entry point takes a built session or builds one -- Open.** The narrower it is,
   the less it commits to. It will move, and the task should record what it chose and why.

## Dependencies

- **Hard: 0003-03.** The tool has nothing to submit to until the host can drive the interpreter.

## See also

- `plan/phase/0003-python/harness-components.md` -- what moves, what is copied, what stays.
- `plan/phase/0003-python/crate-split-tradeoffs.md` -- why duplication, why rig stays private, why
  the surface is one entry.
- `crates/outrig-cli/src/llm.rs` -- the loop being copied from, and `rig_tool.rs` for how a
  discovered tool becomes something an agent can call.
