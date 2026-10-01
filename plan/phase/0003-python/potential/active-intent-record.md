# An active-intent record beside the selected history

## Shipped

The view the provider receives is a window -- the first rounds and the most recent ones, which
`0003-11` fixed at the first two and the six before the current one -- plus the turns the agent
promotes with `runtime.context.promote`, assembled against a token budget (`history.md`, "The
view" and "Promotion"). The store keeps every turn, and "nothing is lost" is a statement about
storage. The design assumes that the first N and last M turns, plus what the model promotes, are
enough for decision-making. That is a hypothesis, not a measured fact: a large context window
does not stop a fixed first/recent selector from omitting a middle-round instruction while token
space remains, and this entry's evaluation is what tests it. A
late instruction in a middle round -- "do not change the public API" -- stays in the store and
leaves the view when the window moves past it, unless the agent promoted the turn or bound the
constraint to a name.

## Alternative

A small active-intent record carried in the selected context: the current goal, the explicit
constraints, decisions awaiting resolution, and instructions that were superseded. Each item
links to the canonical turn it came from, and a quoted user instruction is distinguished from an
inference the agent made. The record has an explicit budget and a stated overflow policy --
compact it, ask the user, or report that it cannot fit -- rather than an eviction rule nobody
chose. It can begin as model-maintained notes with source references, updated when an
instruction changes. The canonical turns stay the authority: the record is not an authorization
mechanism, and a note in it never outranks the turn it cites.

## Evaluation

Long-task trials, each with a late constraint, enough work to move the window past it, a revision
of the constraint, and then a choice the constraint governs. Measure adherence to the current
constraint and stale-instruction errors, with the record against disciplined promotion -- an
agent instructed to promote and to bind what matters -- and not against an under-instructed
baseline, which would overstate the record's benefit.

Two outcomes settle it. If plain promotion keeps late constraints and their revisions with no
meaningful extra failures or recovery cost, the shipped design stands. If a model-maintained
record omits or distorts constraints at about the rate promotion loses them, the record is not
worth its context cost as the default.

## When

A 0.3.1 candidate. Its precondition is already a requirement of the phase: the history and view
structures and methods are substitutable, so adding a record later breaks no API (`embedding.md`,
`0003-19`). `history-selection-seam.md` is the mechanism that would carry it.
