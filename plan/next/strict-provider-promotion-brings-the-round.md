# A promotion on a strict provider should bring the rest of its round

## Symptom

With `role-alternation = "strict"` (#472), a turn promoted without the rest of its round is
withheld where it would follow the model's own reply, and its results are withheld where the
next round's opening would follow them. In practice a lone promoted turn survives only when its
round's earlier turns are in view before it and a later turn of its round follows it. The docs
say to promote whole rounds on such a provider, which puts the rule on the agent.

## Correction

Companion selection: when the budget's provider is strict, `Store::chosen` (or the alternation
pass) adds a promoted turn's round-mates as `Promoted` too, so the promotion brings the round
and the view alternates without withholding what the agent asked for. The manifest then lists
each round-mate as carried for the promotion, and `omitted` counts them the same way.

## Scope

- Only for a strict budget: a relaxed provider is sent the lone turn, as today.
- The round-mates compete for room like any promoted turn; one that does not fit is evicted, and
  the alternation pass still runs after.
- Tests: a lone promotion on a strict provider carries its round; a relaxed one does not change.
