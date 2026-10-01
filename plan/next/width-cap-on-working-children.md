# Nothing bounds how many children work at once under `run-new`

## Context

The maintainer removed the width cap from `run-new` in planning: the new loop does not read
`subagent-width-max`, which stays `run`'s key with its default of 8 and ceiling of 16 (`work.md`,
`typed-agents.md`). The cap complicated agent classes (`agent-classes.md`): an instance idle
between requests would have held a slot for as long as it lived, so a parent holding its width in
idle instances and launching once more would have waited for a release only it could make.

What bounds a tree now is `subagent-depth-max` and the per-tree token budget (`0003-25`). They
bound the tree's shape and its spend, not how many kernels run at once. A 500-way `gather` over
decorated calls is 500 children, each a thread in the one interpreter process at about 16 MiB
resident -- `agent-placement.md`'s measurement, with an event loop and a queue -- under one GIL
and one memory ceiling. The budget stops spend; it does not stop memory use or GIL contention.

## Shape

- A cap on children *working* -- a round in flight, or a request received and not yet answered --
  not on live children. An instance idle between requests holds no slot.
- Counted per launching agent, as the dropped cap was, so no wait at one level depends on a slot
  at another.
- A launch or a request past the cap waits as an ordinary await: the handle's status says it is
  waiting for a slot, and an event names what it waits on.
- A wedged child counts until the session ends, since its thread runs that long
  (`runtime-protection.md`).
- Its own key, not `subagent-width-max`, so `run`'s semantics -- refuse past the cap, a release
  the remedy -- stay as they are.

## Open questions

- Per launching agent or per tree. Per agent cannot deadlock across levels; per tree is what
  bounds the interpreter's thread count.
- Whether idle kernels need a separate, memory-driven bound. Under this cap a thousand idle
  instances hold no slot, and their kernels' memory stays allocated.
- Whether a wait only the waiter's own release could satisfy says so, rather than waiting forever.

## Acceptance

- A 20-way `gather` of decorated calls under a cap of 4 never has more than 4 children in a round
  at once, and all 20 complete.
- An agent instance idle between requests holds no slot: under a cap of 1, a second instance's
  request is answered while the first is idle.
- A handle waiting for a slot reports that status, and an event names what it waits on.
