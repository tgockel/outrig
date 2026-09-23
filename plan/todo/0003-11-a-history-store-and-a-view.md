# 0003-11 -- The full conversation lives in Python; a view goes to the provider

## Context

Today a conversation is one thing: a `Vec<Message>` that is both the record of what happened and
the payload sent to the provider. `history.md` separates them. The store is everything, mirrored
into the interpreter as ordinary Python data; the view is the subset the provider receives.

The economics are the point. Scanning the store costs no context, because it happens in Python and
only the result of the expression is observed -- the same trick the rest of the phase rests on,
turned on the conversation itself.

The mechanism exists and is documented for this. rig's `RequestPatch.history` calls itself "the
enabling primitive for context-window compaction", and `subagent/injection.rs` already uses it to
fold a steer into a running round. It appends; a view returns a shorter list through the same
call.

Ownership is also the fix for a known defect. History is currently moved out of its cell for a
round and written back after, so nothing can touch it mid-round and an interrupt loses all of it
-- `plan/next/repl-interrupt-history-loss.md`, "SIGINT silently empties conversation history".

## Goal

An agent can read its whole conversation with ordinary Python, and the provider receives a subset
the agent had a hand in choosing.

## Deliverables

- The store: host-authoritative, append-only, a stable id per turn, mirrored into the interpreter
  as ordinary Python data. Not a proxy -- the agent writes expressions, not queries.
- The view, applied through `RequestPatch.history`, defaulting to a window of the first few rounds
  and the most recent few.
- **The unit of both the window and a promotion is a turn**, because an assistant message carrying
  a tool call must be followed by its result. A turn is the span that keeps that intact, so no
  repair logic is needed.
- **History stops being owned by whoever runs the round.** The store owns it; a round borrows a
  view. That is the same change that retires the interrupt loss.
- The three `RequestPatch` constraints honored: the patch is per-turn and non-sticky so the view
  is re-applied every turn and folded back at round end; `extend_history_with_new_suffix` decides
  by prefix-equality, so mutating the store while rig holds a stale snapshot silently concatenates
  the whole pre-prune history back on; the hook is already cloned before being handed to rig,
  which is the seam.

## Acceptance

- An agent scans its own history in Python and the scan costs no model context -- asserted on what
  was sent, not on the code.
- A promoted turn appears in the provider's view, in its original chronological position.
- **Interrupting a round no longer empties the conversation -- in `run-new`.** The defect
  `plan/next/repl-interrupt-history-loss.md` describes, tested in the shape that entry asks for:
  interrupt mid-callback, confirm the follow-up round sees prior history. Name the new driver in
  the assertion, and **leave the `plan/next/` entry open**: `run` and `run-legacy` still take the
  old path, and a fix that reaches only the new loop has not closed the reported bug.
- **A pruned view does not resurrect the pre-prune history.** The prefix-equality hazard, tested
  directly, because it fails silently and by concatenation.
- The view is re-applied on every turn of a round, not only the first -- the non-sticky property,
  which `injection.rs` was bitten by.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whole mirror versus metadata-with-bodies-on-demand -- start whole.** `history.md` records the
   fallback and why the simple thing goes first; `0003-12` carries the measurement gate.

## Dependencies

- **Hard: 0003-04.** There is no loop holding a conversation until the loop exists.
- **Soft: 0003-09.** Both touch what an agent does between rounds.

## See also

- `plan/phase/0003-python/history.md` -- the store/view split, the unit, and the rig hazards.
- `plan/next/repl-interrupt-history-loss.md` and
  `plan/next/partial-turn-history-on-failed-model-call.md` -- the defects this ownership change
  retires or enables; the second warns its fix is only right "if the hook's copy becomes *the*
  copy rather than a second one", which a store makes true by construction.
