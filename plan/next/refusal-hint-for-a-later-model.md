# A later model's refusal earns no role-alternation hint

## Problem

`RoundHook::refusal_hint` (`crates/outrig/src/agent/round.rs`) tells a refused call where its
conversation put one role after itself, and what to set: `[providers.<p>].role-alternation =
"strict"`. Since #470 it fires for an alias of several models, but only when the *head* refused
with a `400` or `422`. A chain's exhaustion carries the head's reason first, and
`failover::head_status` reads the status off it.

A model after the head that refuses the same way gets no hint. The hook cannot give it one: the
hint names the head's provider and model, and it speaks of the head's view, whose adjacency the
hook keeps in `Journal::adjacent`. The view a moved call is sent is assembled again, for the next
candidate's window and protocol, inside the chain (`View for History`, `FailoverModel::completion`).
Its adjacency lands in that move's own `model.call` manifest and goes no further.

So `doc/reference/cli.md` says the hint holds for an alias only when its first model refused the
call, and a person whose head is down and whose fallback is the gateway that wants strict
alternation is told nothing of it.

## Sketch

- `View::view` returns the call's first `Adjacent` with its messages, and `Abandoned` keeps it with
  the candidate's `Budget` (the row, the provider, the `alternation`).
- `Exhausted` gives the round each refused candidate's status, `Budget`, and adjacency, and
  `refusal_hint` words the hint from the first candidate that was refused with a `400` or `422`
  and has an adjacency, instead of from the hook's `budget` and `journal.adjacent`. The head's own
  adjacency comes from the journal, as now.
- The hint then names the row that refused. A chain of one reads as it does today.

## Dependencies

- **Landed:** #470, which types a chain's exhaustion (`failover::Exhausted`).
