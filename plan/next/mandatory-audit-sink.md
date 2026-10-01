# A required audit subscriber that fails does not stop boundary calls

## Context

Phase 0003's event stream is in memory, sequenced at publication, and delivered to every
subscriber (`0003-19`). A subscriber that does not take events as fast as
they are published loses some and is told how many; it never delays a producer
(`plan/phase/0003-python/observability.md`). That is right for a log file or a UI. It is wrong for
an embedder whose rule is that no effect happens without its record: if that embedder's audit
subscriber stops accepting, hosted calls keep running and their records are counted as lost.

The phase 0003 design brief (now in `plan/phase/0003-python/observability.md`) proposed a mandatory
sink for this. The maintainer deferred it past 0.3.

## Shape

As 04 proposed it, not settled:

- A subscriber registered as mandatory. When it cannot accept a record -- it failed, or stayed
  full past a bounded wait -- trusted Rust closes admission directly, through the same gate as
  `close_admission()` (`plan/phase/0003-python/lifecycle.md`), and not through a policy callback
  or agent Python. No call is dispatched before its pre-dispatch record is accepted.
- Admission reserves room for the call's outcome record, in capacity kept apart from ordinary
  events, so a call already admitted can always record how it ended.
- If an outcome record still cannot be delivered after the call ran, the report says "effect may
  have occurred; audit incomplete", never that the call was prevented.
- A failed sink delays neither `shutdown(deadline)` nor a synchronous call that is waiting. The
  `ShutdownReport` carries the delivery failure, since the sink cannot.

## Open questions

- What "accepted" means: queued in memory, acknowledged by the subscriber, or stored durably by
  the embedder. In-memory acceptance does not survive the process, and no durable ledger is
  planned.
- How a refusal for lack of audit capacity is itself recorded when every reserve is full.
- Whether refused and denied attempts need reserved room too, and a rate limit, so that a loop of
  denied calls cannot use it all.

## Acceptance

- With a mandatory subscriber that stops accepting, the next hosted call is refused before
  dispatch with an error distinguishable from a policy deny, and the refusal is in the report.
- A call admitted before the failure records its outcome from the reserved capacity, or the
  report marks it "effect may have occurred; audit incomplete".
- A failed mandatory subscriber never makes `shutdown(deadline)` overrun its deadline.
