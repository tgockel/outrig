# A request on an agent class has no progress channel

## Context

A submission's handle has `h.progress`, a one-way endpoint the parent reads for `Delivery`
envelopes of what the child reports while it works (`work.md`; the `progress` row of
`messages.md`'s worked example). A request on an agent class (`agent-classes.md`, `0003-29`)
returns a handle with no `progress` in 0.3: the class derives its request channels from its
`async def` methods, and nothing on it declares a channel running the other way.

What that costs: a parent waiting on a long request learns nothing until the reply settles the
handle, and a child answering several requests at once has no way to say which one it is
reporting on.

## Shape

- A one-way channel declared on the class, either per instance -- `foo.progress`, one endpoint
  the parent reads, each message naming the request id it concerns -- or per request, `h.progress`
  on each handle as a submission has. One endpoint per instance is one thing to drain; one per
  request closes with its handle.
- Declared, not derived: a progress channel needs a message type of its own, which no method
  signature supplies.

## Open questions

- Whether `runtime.wait` watches it. `0003-25`'s fork says no for `h.progress`, because a parent
  with many children reporting progress would have every wait ended by them; the same reasoning
  applies here.

## Acceptance

- A child answering a request sends a progress message, the parent reads it before the reply
  settles the handle, and the handle's result is unchanged by it.
