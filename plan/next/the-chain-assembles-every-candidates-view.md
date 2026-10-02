# Two owners assemble a call's view, and they treat a call that does not fit differently

## Context

`0003-15` made every `PythonAgent` model a failover chain and gave each candidate its own window.
The view a call is sent is now assembled in two places:

- The round's hook (`agent/round.rs`, at `CompletionCall`) assembles it against the **head's**
  budget and hands it to rig as a `RequestPatch`.
- The chain (`agent/failover.rs`) assembles it again, through its `View`, for **any other**
  candidate it moves to, and splices it in after the system message rig put first.

The two answer "the latest turn does not fit" differently. When the head cannot fit it, the hook
ends the round, even if a later candidate's larger window would hold it. When a later candidate
cannot fit it, the chain passes that candidate over and tries the next.

`journal.adjacent`, which the refusal hint reads, likewise describes only the head's view. That
is harmless today: a lone model is the head, and a chain's exhaustion is a `ProviderError` that
the hint never reads. It stops being harmless if a typed exhaustion arrives
(`plan/next/run-new-failures-that-cannot-clear.md`).

## Shape

- Make the chain the only assembler. The hook keeps committing turns at `CompletionCall`, but
  stops patching history. The chain assembles every candidate's view, the head's included, and
  the `index == 0` special case goes away.
- When no candidate can fit the call, the chain returns a typed reason that the round turns into
  the same stopped ending (`(round ended: turn N ...)`) it uses now.
- The refusal hint reads the adjacency of the view that was actually refused, carried with the
  error or in the store's last manifest.

## Acceptance

- A turn too large for the head but within a later candidate's window is sent to that candidate,
  not ended on.
- A turn no candidate can take ends the round as today, keeping it, and later rounds go on.
- Every `model.call` manifest still rebuilds what its candidate received.
