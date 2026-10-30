# On Anthropic's wire a user pair never reaches the provider

## Context

rig 0.45 sends each run of adjacent user messages as one (`merge_users` in its history adapter),
on every wire. On Anthropic's wire, which carries tool results in user messages, a round's
opening after a turn's results is therefore one user message, and no request OutRig assembles
shows two user messages in a row. On OpenAI's wire the results are `tool` messages the opening
follows, which a gateway in front of a strict provider still reads as a user pair, so the
concern stands there.

OutRig's role alternation (#472, `role-alternation = "strict"`) predates this. Its view pass
withholds a turn whose results a round's opening would follow, and the `Adjacent` hint a refused
call carries names a user pair where the view had one -- on an Anthropic budget, a pair the wire
never showed.

## Shape

- A strict budget on the Anthropic wire need not withhold for a user pair: `Sending` /
  the alternation pass in `crates/outrig/src/agent/history.rs` keys on `budget.wire`, and
  leaves a user pair standing where `Wire::Anthropic`. An assistant pair -- a promoted turn after
  the model's reply -- is still a pair on every wire.
- The refusal hint (`RoundHook::refusal_hint`, `history::Adjacent`) names a user pair only where
  the wire shows one.
- `doc/concepts/llm-providers.md`'s role-alternation section says which pairs each wire shows.

## Acceptance

- `a_strict_provider_is_sent_a_conversation_that_alternates_on_both_adapters` on the Anthropic
  style withholds nothing for round 4's opening after the capped round 3's results, and the mock
  still refuses nothing.
- A refused Anthropic call whose view had both pairs names the assistant pair.
