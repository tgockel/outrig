# The token estimate is never compared with what the provider counted

## Context

`0003-12` holds each model call to a budget estimated before the call: a token per three ASCII
bytes of each message's JSON, one per other character (`agent/budget.rs`). That over-counts code
and prose, which is the safe direction, and can under-count text that tokenizes densely -- hex,
base64, minified data -- where a provider may still refuse a request as too long.

Every successful call reports what it cost. rig's `StepEvent::ModelTurnFinished` carries `usage`,
and `Manifest.estimate` holds what OutRig guessed for the same request. Nothing compares them.

## Shape

- Record both. `0003-13` emits the manifest and the round's usage as events; with both in the
  stream, a human can see how far off the estimate runs for a real session, which is the evidence
  the next step needs.
- Then, if it is worth it: keep a per-candidate ratio of reported to estimated input tokens, and
  scale the estimate by it, never below the static rule. A ratio learned on one model says nothing
  about another, so a failover resets it.
- A provider's own refusal (`prompt is too long: N tokens > M maximum`) names the limit it holds
  to. Parsing it is guessing from an error string, which this project has declined before for
  failover state; a calibrated estimate makes it unnecessary.
