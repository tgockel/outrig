# `build_agent` is `async` with nothing left to await

## Context

`build_agent`, `build_single` and `build_candidate` (`crates/outrig-cli/src/llm.rs`) were `async`
because one arm loaded -- and on first use downloaded -- a multi-gigabyte model: the in-process
`mistralrs` backend. That backend is gone. Every remaining arm builds a `reqwest` client and hands
it to rig, which does no I/O, so the only `.await`s left in the three are the calls between them.

Nothing is wrong at runtime; an `async fn` that never suspends costs a state machine and nothing
else. The cost is to readers. The signature still claims a build can wait, and every caller --
`cli/run.rs`, `RebuildingAgent::run_turn`, `subagent::build_subagent_agent`, and the tests in
`tests/anthropic_mock.rs`, `cli/run.rs` and `subagent/mod.rs` -- awaits it. `clippy::unused_async`
would flag it, but it is pedantic and not enabled.

Found while removing the in-process backend. Left alone there because dropping `async` ripples
through every caller, which is a refactor rather than a removal.

## Sketch

- Make the three plain functions, and drop the `.await` at each call site.
- `RebuildingAgent::run_turn` keeps its own `async` -- the turn it runs still awaits.
- Worth doing together with `plan/next/http-client-rebuilt-per-agent-build.md` if that one lands
  first, since caching the client moves the same code.
- Two neighbors lose their reason at the same time. `RebuildingAgent::new` takes an agent the
  caller already built, a split that existed so the model load reported progress in the caller;
  it could build its own and drop `cli/run.rs`'s `agent_tools.clone()`. And the `building agent`
  / `agent ready` progress span in `cli/run.rs` now times an in-memory step. That one is
  user-visible -- `doc/usage/run.md`'s sample output and `tests/run_smoke.rs` both show it -- so
  dropping it is a deliberate output change, not a cleanup.

## Acceptance

- `build_agent` is not `async`, and nothing awaits it.
- `cargo clippy --all-targets -- -D warnings -W clippy::unused_async` reports nothing under
  `crates/outrig-cli/src/llm.rs`.
