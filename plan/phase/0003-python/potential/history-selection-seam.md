# A versioned selection strategy at the view seam

## Shipped

One selector. The view is chosen as `history.md` describes -- the first rounds, the recent
rounds and the promotions, filled against the token budget in a fixed priority order -- and is
sent through `RequestPatch.history` on every model call, from `OutrigPromptHook` ("The mechanism
exists"). The selector has no name and no version, and it is not separable from the code that
owns the store, the round and the per-call manifest. An experiment that wants a different
selection edits that code.

## Alternative

The selection step becomes a strategy with a name and a version, applied at the seam that already
exists: the store hands the strategy the canonical turns, the budget and the promotions, and the
strategy returns the ordered list of turn ids the call carries. Everything around it is unchanged.
The store stays append-only and host-authoritative, a call and its result stay paired in one
turn, effect ownership stays with the round, usage accounting stays as it is, and the per-call
manifest records the strategy's name and version beside the ids it chose. The shipped window is
the first strategy; `active-intent-record.md` would be the second.

The seam is narrow on purpose. General scheduler plugins and a second public action front end are
not part of it.

## Evaluation

Swap selectors in experiments and compare their outcomes on the same tasks; the long-task trials
in `active-intent-record.md` are the first use. The seam is adequate if an alternative selector
is written without touching the store, the round, effect ownership or accounting, and if the
manifest of every call says which selector chose its contents. If the first alternative needs a
change outside the seam, the seam is in the wrong place.

## When

With the intent record, which is the first selector that would use it. A seam with one
implementation adds a name and no capability, so nothing before then needs it.
