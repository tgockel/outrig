# Tickets for human-length waits on a hosted service

## Shipped

A hosted call is synchronous and blocks its caller's kernel thread until the host answers
(`hosted-objects.md`, "Calls are synchronous"). A binding process serves each connection on its
own thread, and a kernel keeps a pool of connections per binding, up to 4, so one blocked call
blocks no other call, even from the same kernel. A binding whose library is not thread-safe
declares `serialize = true` under `[bindings.<name>]` and gets one call at a time. A call that
should not stop its kernel's event loop runs under `asyncio.to_thread`. `0003-17` proves the
arrangement and adds a service-shaped fixture -- a method that blocks for minutes, an `ask ->
ticket` plus poll pair, and bulk record operations -- and measures RPC count, process memory and
tail latency across concurrent sessions.

## Alternative

A service interface rather than a library interface, for the CocoClaw case, where a Rust service
is reached through agent Python, RPyC, a hosted Python client and the application's transport.
A human wait becomes `ask -> ticket`: the call returns a ticket at once, and the agent either
polls with short status calls or is told by the owner, through a channel, when the answer
exists. Close cancellation has a path that does not depend on the possibly blocked client, and a
completion signal returns immediately. Operations that exchange records are bounded and carry
values, in batches where that fits, rather than object identity and remote iteration per item.

What it costs: the application keeps ticket state and designs its polling or notification; a
batch carries less per-item detail to policy; and the agent's code has two shapes of call where
a library has one.

## Evaluation

`0003-17`'s numbers for the service-shaped fixture: RPC count per operation, the binding and
interpreter processes' memory, and tail latency, for representative list, update and question
operations across concurrent sessions, with a call blocked for minutes present throughout. If
that call costs only its thread and no other call waits on it, the concurrent-connection design
covers the wait and tickets are an application's choice. If memory or tail latency rises with
the number of blocked calls, tickets are the pattern the preamble should teach. The step past
tickets -- a direct Rust service adapter, with no hosted client and no RPyC -- is
`plan/next/rust-object-as-python-object.md`, and the same numbers decide whether it is worth
building.

## When

After `0003-17` reports. A ticket pattern needs no change to OutRig and an embedder can adopt it
at any time; what this entry decides is whether OutRig recommends it and builds support for it.
