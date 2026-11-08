# A service's calls in flight are counted by nothing

## Context

`0003-44` serves a Rust service to agent Python: each `service.call` line is a task on the host
running the service's `call`, holding its arguments -- up to 16 MiB of JSON, the bound on a line
the host reads -- until the call ends. Nothing caps how many are in flight. A hosted call is
capped per agent: the relay holds at most 1,024 requests per agent not yet ended and refuses the
next `queue_full` (`0003-22`, `CALLS_MAX` in `crates/outrig/src/python/hosted.rs`). Agent code
that starts threads, each making a call of a service that waits -- or one that answers slowly --
holds a task and its arguments on the host for each, bounded only by the deadline: 600 s by
default. The interpreter's own memory ceiling bounds the arguments it can make, not what the
host holds once they have crossed.

## Candidate bounds, none chosen

- **A per-agent cap, as the relay's.** A call past `CALLS_MAX` calls of its agent in flight is
  refused, with a reason beside `admission_closed` (`queue_full`, as the hosted one), before
  anything else is decided about it.
- **A per-service cap in `ServicePolicy`**, counting every agent's calls, for a service whose
  backend takes few at once; the owner sets it, and a call past it is refused.
- **A bound on the bytes held**, summing each call's arguments, which caps memory directly and
  lets many small calls through.

## Acceptance

- Calls of a service that waits, made from more threads than the chosen bound, are refused past
  it with a documented reason in `service.call.ended` and in the `outrig.ServiceError` the caller
  gets; the service sees none of the refused ones; calls under the bound, and other agents' calls
  under theirs, run on.
