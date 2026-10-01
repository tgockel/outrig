# Nothing limits what one hosted connection holds or asks for

## Context

`0003-16` bounds a frame on the interpreter pipe, so one message cannot take unbounded memory in
the relay, and `0003-21` gives each binding a bounded queue. Those are transport limits. Nothing
limits what a connection uses inside the binding process, which runs on the host, outside the
container's cgroup and the interpreter's memory ceiling:

- **Live references.** Every object the host returns by reference stays in the connection's
  object table (RPyC's `_local_objects`) until the container side drops its proxy and RPyC sends
  `DEL`. A library that often returns a new object on each access, as GitPython does, lets a loop
  over a long history keep one host object per iteration, for as long as the agent's code holds
  them.
- **Payload size.** Beyond the frame bound, phase 0003 has no limit on a call's by-value
  arguments and result that an operator can read and set, and no error that names one.
- **Iteration buffers.** RPyC's `BUFFITER` request returns `tuple(itertools.islice(obj, count))`
  with `count` taken from the request, so agent code that writes raw requests chooses how many
  items the binding process collects at once.

The phase 0003 design brief asked for bounds on all three (now in
`plan/phase/0003-python/hosted-objects.md` and `boundary-policy.md`), and for existing references
never to be evicted to make room.

## Shape

- Per-connection limits on live references, payload bytes per call and items per buffered
  iteration, each with a default and a config key, enforced on the host side, where agent code
  cannot change them.
- A request that would exceed a limit is refused with an exception distinguishable from a library
  error, a policy deny and a transport failure, naming the limit and its value, and is evented.
- Existing references are not evicted. The agent makes room by dropping references.

## Acceptance

- A loop that keeps every returned reference stops at the limit with that exception, and the
  binding process's memory stays bounded.
- Dropping references lets the same loop continue.
- A raw `BUFFITER` request with a count over the limit is refused.
