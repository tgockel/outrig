# The startup banner has no tests

## Context

`print_banner` (`crates/outrig-cli/src/cli/run.rs:746`) is the first thing a user reads every
session -- agent, model, provider, identifier, the tool-call and tool-result caps, the model
weights, and now the failover chain -- and nothing in the tree asserts a single line of it.

0113 noticed this rather than caused it. It added
`[outrig] model failover:    <names>`, printed from `ResolvedAgent::fallback_names()`
(`llm.rs:304`), and that line is the *cheaper half* of 0113's mitigation for its own headline
hazard: a chain means one session can span two models, so the vendors it may move to are named
up front rather than first appearing in a move announcement mid-reply. The expensive half --
the move announcement itself (`llm/failover.rs:330`) -- is likewise unpinned.

`fallback_names` has exactly two references in the tree: its definition and the one call site.
A refactor that dropped the call would leave the banner silently shorter and every test green.

## Goal

Pin the banner's shape well enough that a line cannot disappear unnoticed, without turning it
into a golden-file test that fights every legitimate addition.

## Deliverables

- Tests over `print_banner`'s output. It already writes into a `buf` rather than straight to
  stderr, so it is testable as-is with no production change -- `StartupBanner` is the only
  fixture needed.
- Coverage for the conditional lines specifically, since those are the ones a refactor can drop
  without a compile error: the agent-vs-agentless fork (`resolved.agent_name`), the failover
  line's presence for a chain and **absence** for a single candidate, and the model-weights
  line.
- The move announcement in `FailoverModel::completion` beside them, if it can be reached without
  a second mock endpoint; `tests/anthropic_mock.rs`'s
  `tools_run_before_a_move_are_not_re_executed` already stands two mocks up and could assert on
  captured stderr instead of a new fixture.

## Acceptance

- A single-candidate agent's banner has no `model failover:` line.
- A chain's banner names every fallback, in preference order.
- An agentless session prints the `model:` form and an agent session the `agent:` form.
- Deleting the `fallback_names()` call from `print_banner` turns a test red.

## Dependencies

- **Landed: `plan/done/0113-model-alias-failover.md`**, which added the line and left this gap.
  Its decision 8 is the argument for why the line exists at all, and so for what a test should
  hold it to.
