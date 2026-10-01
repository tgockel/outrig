# A latest-state snapshot for a monitor that lost events

## Shipped

One event stream, in memory, numbered at publication and delivered to every subscriber in order.
A subscriber that falls behind loses a counted gap and is told its size; no subscriber delays a
round, an execution or a shutdown (`observability.md`, "One stream, not one file per subject").
Control does not go through the stream: a round's outcome is returned to the caller that drove
it, `close_admission()` takes effect at once, and `shutdown(deadline)` returns a report
(`lifecycle.md`, `embedding.md`). An owner stops a session and learns how it ended without
reconstructing anything from events. The session directory is a record that a script renders
after the fact (`observability.md`, "The renderer"), and nothing listens on a socket.

## Alternative

A coalescing latest-state watch, or a snapshot on request, beside the lossy timeline: the current
round state, the executions in flight, the children and their statuses, the hosted calls
dispatched and not returned, and the pending approvals, each with the stream sequence number as
of which it is current, so a monitor that lost events reconciles the snapshot with the events
that follow it. Round activity and surviving work are represented separately -- "no round
running" is not "nothing running" -- and the snapshot says what it does not cover: the effects
of background Python and in-flight remote operations are not inventoried. It is an aid for a
lagging or late-joining observer and for a secondary cleanup component. It is not a prerequisite
for shutdown, which the direct controls cover, and not a durable bus, a blocking subscriber or a
second scheduler. The maintainer expects the render script to become a live monitoring page,
which would be its first consumer.

## Evaluation

Lose telemetry on purpose -- a subscriber held past the gap -- while a session runs rounds,
children and hosted calls, then recover a current view from the snapshot and the events after
it, without private APIs. The snapshot is right if what it says is running agrees with the
shutdown report that follows, and if a monitor that reconciles it against the stream never shows
a child or a call as live after its ending event. The other number is what keeping the snapshot
current costs under many publishing threads.

## When

With the live monitoring page, when it exists. Until then the record and the direct controls are
enough, and the event shape does not change if a snapshot is added later.
