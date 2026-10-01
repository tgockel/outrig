# Every event subscriber receives the same fields

## Context

Phase 0003 has one event shape. The in-memory stream (`0003-19`) delivers
the same events to every subscriber, and `events.jsonl` is the CLI's subscriber, written under
`plan/phase/0003-python/observability.md`'s three categories and their capture rules. One split
exists: a hosted call's host traceback goes into its event and not into the agent's error
(`plan/phase/0003-python/hosted-objects.md`).

The phase 0003 design brief (now in `plan/phase/0003-python/observability.md`) proposed a projection
per audience instead: what the agent sees in errors and results, what the evaluator model is shown
(`0003-23`), what an approver is shown with an escalation, what an audit record keeps, and what an
embedder's UI displays. They need different fields. An approver may need a full argument or the
resource a call affects in order to decide; the agent, a log file and a UI should not receive
environment values, URLs carrying credentials, or a host program's raw stderr. With one shape, each
consumer has to remove what its audience should not see, from fields nobody classified when the
event was made.

## Shape

- A fixed set of audiences, named by OutRig.
- Each field classified by audience where the event is produced, on the trusted side, and one
  projection per audience applied there, before delivery.
- A subscriber states its audience when it subscribes, and the embedder decides which subscribers
  get a privileged one. The CLI's `events.jsonl` would take the audit projection.
- Building a projection never runs agent code: no `repr`, property or iteration on a reference.
- The evaluator's input (`0003-23`) is one of the projections. `boundary-policy.md` states that
  the evaluator receiving no host secret is the intent, not yet a property, because an argument
  preview can itself hold one -- a token in a remote URL, a password in a command line -- so the
  intent needs an input policy: which
  fields reach the evaluator, at what length, and what is withheld or labeled opaque. Having no
  tools does not make the evaluator disclosure-free.

## Open questions

- Free text. No string filter tells a secret from a path, a URL or a commit message (04), so
  fields have to be typed by their producer, and what stays opaque is labeled as opaque.
- The evaluator's input policy. Its prompt is a projection (above), built as `0003-23` builds
  it, from trusted instructions and delimited evidence; what the evidence may carry is open. A
  length bound alone does not keep a secret out of the first part of a URL.
- Who keeps each projection, and for how long. An event held only in memory is still a copy of
  what it carries.

## Acceptance

- A host exception whose text carries a credential-bearing URL: the agent's error and the UI
  projection omit it, and the audit projection keeps it.
- A subscriber registered for the UI audience never receives a field classified for audit only.
