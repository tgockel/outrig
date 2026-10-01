# An active-work scheduler beyond the two session limits

## Shipped

Two session-wide limits (`0003-25`; `agent-classes.md`, "Limits and placement"; `work.md`).
`children-max`, default 64, counts resident children, wedged ones included until they are
actually reclaimed; a launch past it raises `AgentLimitReached` at once and never waits for a
release only the caller could make. `model-concurrency-max`, default 8, bounds model requests in
flight with a cancellation-safe queue; a permit is held per provider request, never while a
parent waits on a child, so no permit can deadlock a tree. `subagent-width-max` stays `run`'s
key. Neither limit bounds the CPU or memory that code inside an admitted kernel uses;
`subagent-depth-max` and the per-tree token budget still bound a tree's shape and spend.

The numbers that motivated a bound. Every child is a thread in the session's one interpreter
process, at about 16 MiB resident with its event loop and queue (`agent-placement.md`'s
measurement), under one GIL and one memory ceiling. A 500-way `gather` over decorated calls is
500 such children; the token budget stops their spend and does nothing about their memory or GIL
contention. `children-max` refuses the 65th.

## Alternative

Two things the limits do not do.

- **An active-work scheduler.** A cap on children *working* -- a round in flight, or a request
  received and not yet answered -- rather than on children resident. An instance idle between
  requests holds no slot. A launch or a request past the cap waits as an ordinary await: the
  handle's status says it is waiting for a slot, and an event names what it waits on. Counted per
  launching agent, no wait at one level depends on a slot at another; counted per tree, it bounds
  the interpreter's thread count, which is the resource. A wedged child counts until the session
  ends, since its thread runs that long (`runtime-protection.md`). Whether a wait that only the
  waiter's own release could satisfy says so, rather than waiting forever, is open.
- **A memory-driven bound on idle kernels.** Under a working cap a thousand idle instances hold
  no slot, and their kernels' memory stays allocated. A bound on resident memory, or on idle
  kernels past a count, would reclaim or refuse under a stated policy.

## Evaluation

Real-world usage under the two limits. What decides it: how often `AgentLimitReached` is raised
in practice and by what code -- generated fan-out that a working cap would have queued, or
instances nobody released; how much memory idle instances hold in long sessions; and whether
`model-concurrency-max` alone keeps a 500-way fan-out from degrading unrelated siblings. If
sessions reach `children-max` with most children idle, and a 20-way `gather` under a working cap
of 4 would complete with at most 4 children in a round at once, the scheduler earns its place.
If `children-max` is rarely reached, it does not.

## When

0.3.1 or later, after usage. The maintainer agreed that a simple enforcer ships first and that an
advanced one needs real-world usage to design.
