# Prompting a work child more than once before its call fails

## Shipped

The one-shot end (`0003-25`; `work.md`, "A request settles once"; `typed-agents.md`, "Validation and
repair"). A work child -- a decorated call's, `child.submit`'s, the skill parser's -- whose round
ends without a completion that passed gets one host-initiated round asking for the result by the
submission's id. If that round ends without one too, the submission settles with
`CompletionRejected` whose message says the child ended without completing; a decorated call's
child, the parser's included, is released, and an explicit work child stays idle with any queued
submission opening its own round next. Invalid completions keep their separate attempt limit, 3. The
maintainer's position for now: a model that will not complete throws on the caller's side, and the
caller decides what to do with a failed call.

## Alternative

Spend more before failing. Either further rounds of the same prompt, up to a count, or a
different prompt: a progress round that asks what blocks completion and what the child would
need, whose reply the host returns to the caller inside `CompletionRejected` or as progress, so
the parent learns why and not only that. A third shape offers the child `fail(message)` with the
blocker named, which turns a silent end into a typed one.

## Evaluation

The completion rate against the model calls spent, on tasks where the first prompted round was
not enough: how often a second or third round produced an accepted completion, how often a
progress prompt named a blocker the caller could act on, and the tokens each cost against the
token budget. The comparison is with the shipped rule's cost, one failed call and whatever the
caller does next. A rule that completes a meaningful share of stalled calls for a bounded spend
earns its place; one that mostly spends rounds on children that then fail anyway does not.

## When

After `0003-26` and `0003-28` have run against real models, which is where the stalled calls
will come from.
