# A callback's call held for an answer holds a serialized binding's turn

## Context

`0003-24` holds a hosted request that `[policy]` escalates on its connection's serving thread in
the bindings process, before the request takes its binding's turn, so a held request blocks
nothing but its caller: other kernels' requests to the binding run while a person decides. A
request a callback makes is served on the thread of the outer call it runs inside, which already
holds the turn when the binding is declared `serialize = true` (`0003-17`). So a callback's
request held for an answer keeps the outer call, and with it the binding's turn, waiting: every
other kernel's request to that binding waits for the person too.

Nothing is lost -- the answer, the interrupt and the close all settle the held request, and the
outer call then goes on -- but the claim that a held request blocks only its caller does not hold
for this case.

## Options

- **Leave it, documented.** A library that calls back into agent code while it holds a serialized
  binding, and whose callback makes a request a rule escalates, is a narrow case.
- **Release the turn while a nested request is held.** The outer call's thread would give up the
  turn for the wait and take it back after, letting other requests interleave inside a call the
  binding was declared to serialize -- the hazard `serialize` exists to prevent.

## Evaluation

A test with a serialized fixture binding, a callback whose call a rule escalates, and a second
kernel's call to the same binding shows the wait. Whether releasing the turn is safe depends on the
library: it is not, in general, which argues for leaving it and saying so where `serialize` is
documented.
