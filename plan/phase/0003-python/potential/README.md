# Potential: alternatives not adopted out of the box

Where a simpler and a more capable design were both defensible, phase 0003 ships the simple one.
This directory holds the alternatives and improvements that were not adopted out of the box, one
file each. An entry is not planned work. It names the simple option that shipped and the phase
page that holds it, the alternative, the evaluation that would decide between them, and when the
alternative could land. Some are candidates for 0.3.1; others wait on real-world usage.

`plan/next/` holds work that is planned and waits only for a slot in the numbered queue. An entry
here asks for an evaluation first, and the evaluation may find that the shipped option is enough.
`/groom-plan` may turn an entry's evaluation into a `plan/todo/` task. The entry stays here until
the evaluation has an answer, and is removed or rewritten when the design pages change.

Each entry has the same sections: `## Shipped`, `## Alternative`, `## Evaluation` and `## When`.

## Entries

- `active-intent-record.md` -- a source-linked record of goal, constraints and pending decisions
  carried in the model's view, against first-and-recent selection plus promotion.
- `history-selection-seam.md` -- a versioned selection strategy at `RequestPatch.history`, so a
  selector can be swapped in an experiment.
- `user-input-preview.md` -- a bounded, attributed preview of primary-user input in the
  announcement, against receive-and-observe.
- `input-rewritten-into-history.md` -- tooling for the agent to record a shorter form of long
  user input, since only what the agent's code observed enters the model's history.
- `exact-skill-directives.md` -- `/name` lines parsed literally and scheduled in the agent's
  kernel, against the typed agent call that maps text to parameters.
- `live-state-snapshot.md` -- a coalescing latest-state snapshot for a monitor that lost events,
  with sequence numbers to reconcile against the stream.
- `ticket-based-service-waits.md` -- `ask -> ticket` plus status calls for human-length waits on
  a hosted service, against concurrent connections per binding.
- `resource-scheduling.md` -- an active-work scheduler and a memory-driven bound on idle kernels,
  against `children-max` and `model-concurrency-max`.
- `request-capture-for-experiments.md` -- opt-in exact capture of the final provider request,
  beyond the per-call turn manifest.
- `one-shot-nudging.md` -- prompting a work child more than once, or asking what blocks it,
  before its call settles `CompletionRejected`, against one prompted round and then failure.
