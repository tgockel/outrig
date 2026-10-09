# Trim a cut-short round for a strict provider, rather than withhold it whole

## What #472 does

`role-alternation = "strict"` has the view withhold whole turns so that what a strict provider is
sent alternates. A round that ended on tool results -- the tool-call cap, a turn too large to
send, Ctrl-C -- is followed by the next round's opening, a user message after a user message, and
since every turn of such a round ends on results, clearing the pair withholds the whole round.
The model sees nothing of it from the next round on. The round stays in `runtime.history`, the
opening line counts it, and the manifest names each turn, but the issue's own reproduction --
the cap fires, the user says "continue" -- leaves the model without what it was doing.

## The alternative

Leave out only the last turn's tool calls and their results, keeping the reply's text, so the
round ends on the model's own words and the opening follows a reply. A reply with no text is left
out whole, and the rule applies to the turn before it. `left_out` already expresses this with no
schema change: `{turn, message, part}` for each call part and each result part, and
`doc/reference/events.md`'s rebuild rule -- less each part named, and any message all of whose
parts are named -- rebuilds the call as sent. `as_sent` would filter user-message parts as it
does assistant parts. Pairing holds, since a call and its result leave together, and nothing is
synthesized.

What it costs: the view shows a reply that made no calls where it did, which is a cut inside a
turn -- the unit `history.md` chose because it needs no repair -- and the rendered record has to
say that a call's results were left out, not only its reasoning.

## What would decide it

- How often a round ends on results on a strict provider in practice: the cap and Ctrl-C are
  exceptions, a turn too large to send rarer still. If they are rare, whole-round withholding is
  a simpler rule for a rare event.
- Whether the model copes with the whole round gone, given the opening line's count and
  `runtime.history`. A session transcript where the agent re-reads the round from the store would
  settle it.
- Whether the half-told round -- a reply whose calls vanished -- misleads the model more than the
  round's absence does.
