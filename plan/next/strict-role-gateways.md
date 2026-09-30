# A strict gateway refuses every call while a same-role pair stays in view

## Symptom

`0003-12` found where the view puts one role after itself: a promoted turn that is not its round's
first opens on a tool call right after the model's own text, and a round cut short ends on results
that the next opening follows. Anthropic's API merges such a pair and OpenAI's accepts it. A
provider or gateway that requires alternation -- a Bedrock-backed Claude behind an
OpenAI-compatible gateway is known to -- refuses the call, and the round's error now says where
the pair was (`RoundHook::refusal_hint`).

That makes the refusal legible, not recoverable. The pair stays in view while its turns do: a
promotion until it is demoted, a cut-short round for as long as the window holds it. Every round
until then fails the same way.

## Why the view does not repair it

Merging the two messages is the obvious repair and is wrong on OpenAI: rig's adapter drops the
text of a user message that also carries tool results, so merging a round's results with the next
opening would lose the opening. Inserting a message between them is the synthesized repair
`history.md` rejects.

## Shape

- A per-provider switch that makes the view alternate by construction: leave out a promoted turn
  that would follow its own role, and let a round's opening absorb nothing. Costs what the agent
  promoted, visibly, rather than the whole call.
- Or, on the refusal, retry once with the pairs left out, recording it in the manifest. That needs
  retry, which is `0003-15`'s.
