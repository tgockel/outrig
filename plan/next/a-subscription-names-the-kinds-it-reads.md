# A subscription names the kinds it reads

## Context

`0003-19` gives every subscriber every event, each in a bounded queue that drops its oldest when
full. `run-new`'s terminal reads only `exec.submitted` and the round ending, so a busy round queues
and evicts `model.call` openings, turn bodies and history views it will never look at. The
reported `Received::Missed(n)` counts those too: the terminal's "missed events" note can fire for
events nobody wanted, and a smaller capacity would serve it if its queue held only what it reads.
The cost is per event, not per byte: the event is one `Arc` shared by every queue.

## Shape

- `SessionBuilder::subscribe_to(kinds)` (or a filter on `subscribe_with_capacity`) taking event
  kinds by name, or by a public `Kind` enum if one is added to `harness::event`.
- The filter runs under the stream's publish lock, before the push, so an event outside it costs
  that queue nothing and never counts as missed.
- `Missed(n)` then means "n events of the kinds you asked for were dropped", which is what a
  reader can act on.
- `events.jsonl` keeps subscribing to everything.

## Acceptance

- A subscription to `exec.submitted` alone, with capacity 1, reads every submission of a round
  that also published a hundred other events, and reports no miss.
