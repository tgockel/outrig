# Attribution names the first candidate, not the one that answered

## Context

`plan/done/phase/0002-sidecars/tasks/0002-36-model-alias-failover.md` made a session able to span
two models: a chain moves candidates *inside* a `completion()` call, so a single turn -- even a
single reply -- can be part one model's work and part another's. Every attribution surface still
reports candidate one, because that is all it has.

`ResolvedAgent`'s accessors (`llm.rs`, `model_name()` / `provider_name()` /
`model_identifier()`) all delegate to `primary()`, which is the right call for the ~70
pre-failover readers that mean "the model this session is configured for". A handful of readers
mean something else -- "the model that produced this output" -- and for those the delegation
quietly answers a different question:

- `ModelLabel::of` (`crates/outrig-cli/src/subagent/mod.rs`) builds the subagent transcript
  header and the tool-result name from `provider_name()` / `model_identifier()`.
- `plan/next/subagent-model-allowlist.md` wants to audit "the model this subagent ran on",
  which 0002-36's Dependencies section already notes is no longer a single value.

0002-36 mitigated this for the *interactive* user and said so: the banner lists the fallbacks up
front and a move prints when it happens (its decision 8). Neither reaches a subagent transcript
read afterwards, which is the surface where "which model wrote this" is actually asked.

## Goal

Let the surfaces that mean "what answered" say so, without disturbing the many that correctly
mean "what this session is configured for".

## Deliverables

- A way to ask a `RigAgent` which candidate served -- `FailoverModel` already knows, it just
  does not surface it. A per-call "last candidate" is the smallest thing that could work; a
  per-turn set is the more honest one, since one turn can span several.
- `ModelLabel` reading that rather than `primary()`, or -- if the label must stay a launch-time
  value -- keeping its current shape and letting the transcript note the chain rather than
  implying a single model.
- The distinction stated once, wherever the accessors are documented, so the next reader picks
  the right one deliberately.

## Acceptance

- A subagent whose turn moved candidates does not report only candidate one.
- A single-candidate session's transcript header and tool-result naming are byte-for-byte
  unchanged.

## Dependencies

- **Landed: `plan/done/phase/0002-sidecars/tasks/0002-36-model-alias-failover.md`.** Its decision 2
  introduced the delegating accessors and its decision 8 the mitigations that cover the interactive
  case only.
- **Soft: `plan/next/subagent-model-allowlist.md`**, which wants the same value for audit.

## The library's loop names the model that answered, and the CLI's does not

`0003-15` copied failover into `crates/outrig/src/agent/` for `PythonAgent`, and fixed this there
rather than copying it. The chain's completion carries the candidate that answered as its
response type (`failover::Answered`), which rig hands the round's hook, so each call in the event
log's `calls[]` names its own model. A move is also recorded as `model.failover` as it happens.
`PythonAgent::model()` still names the head, and says so: it is what every call is tried against
first.

The CLI's `ModelLabel` and its transcript header are unchanged, and still name candidate one.
