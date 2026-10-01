# The sink's mechanics are tested through the network wrapper

## Context

`0003-13` extracted `LineSink` from `network.rs` so the event log could share it. Its own tests,
in `line_sink_tests.rs`, cover rollback, poisoning, bounded retention, file modes, the non-waiting
enqueue, and an aborted writer. Most of what made the sink trustworthy is still tested only
through `AuditSink`, in `network.rs`'s `mod tests`:

- a drain that gives up on a stalled writer with a full queue;
- a producer cancelled before its record is queued, charged to nobody;
- a writer stopped holding nothing, which invents no loss;
- closing a stuck writer, which ends it rather than detaching it.

They reach the sink's fields through `Deref`, which is why those fields are `pub(crate)`.

`0003-13` left them where they were, because its acceptance asked for the network tests to pass
unchanged.

## Shape

Move the four into `line_sink_tests.rs` against a generic owner, keep a network-level test only
where the attachment generation matters, and narrow the sink's fields to private once nothing
outside it builds or reads them. `LineSink::queuing_to` already builds a sink with a test-held
queue.
