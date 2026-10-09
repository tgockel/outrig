# Tickets for human-length waits on a hosted service

## Shipped

A hosted call is awaited (`hosted-objects.md`, "Calls are awaited"): the awaiting code suspends,
a worker thread sends the request through the kernel's pool of connections, and the kernel's
event loop keeps running. A binding process serves each connection on its own thread, and a
kernel keeps a pool of connections per binding, up to 4, so a wait of hours holds one worker
thread and one connection of the caller's pool, not the kernel, and no other call waits on it,
even from the same kernel. A binding whose library is not thread-safe declares
`serialize = true` under `[bindings.<name>]` and gets one call at a time. `0003-17` proves the
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

`0003-17` has reported. Its numbers for the service-shaped fixture -- RPC count per operation,
the binding and interpreter processes' memory, and tail latency, for representative list, update
and question operations across concurrent sessions, with a call blocked for minutes present
throughout -- are in its `## Decisions`: a wait of minutes holds one connection and one thread
and nothing else, except under `--serialize`, where it holds every call of that binding from
every kernel of its session. So the concurrent-connection design covers the wait and tickets
stay an application's choice, and `serialize = true` is the case where they still matter. If a
session's use shows memory or tail latency rising with the number of waiting calls, beyond the
one the fixture held, tickets are the pattern the preamble should teach. The step past tickets
-- a direct Rust service adapter, with no hosted client and no RPyC -- is
`plan/next/rust-object-as-python-object.md`, and the same numbers decide whether it is worth
building.

## When

`0003-17` has reported, and the facade over RPyC (`hosted-objects.md`, "Calls are awaited") keeps
the arrangement it measured, so the question is open only to real-world usage: a session whose
memory or tail latency rises with the number of waiting calls. A ticket pattern needs no change
to OutRig and an embedder can adopt it at any time; what this entry decides is whether OutRig
recommends it and builds support for it.
