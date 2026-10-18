# Finer permissions than the binding

## Context

In 0.3 the binding is the grant: having a remote object is permission to use it
(`plan/phase/0003-python/boundary-policy.md`). An operator who binds GitPython's `Repo` grants the
agent everything that object reaches, as the host user, and the events record what ran.

People want to grant less than a whole object, in their own terms: "read-only git", say. `0003-24`
built `[policy]` for that -- `[[policy.rules]]` matching a binding, the host-side type, the member
and the operation, each naming `allow`, `deny`, `escalate` or `evaluate` -- and it was reverted
before release. Rules about method calls are the wrong basis. "Read-only git" is not a list of
GitPython members and operations. Writing it as one means knowing the library's object graph,
every way a member can be reached (a read then a call, `callattr`, a bound method, a dunder name,
a host callable passed back), and which calls write. A policy written that way is a mess no one
can review, and still misses what a call does on the host.

This entry is the deferred work. It has no design yet: the design starts from what people want to
say, and then asks what can enforce it.

## What is known

- `0003-24`'s Decisions (`plan/done/phase/0003-python/tasks/0003-24-policy-decides-what-crosses.md`)
  record the rule language, the matching it needed, the gate that settles a held request against
  an interrupt and the close, and the escalation handler. Some of that machinery may serve a later
  design; the rule language should not be its starting point.
- Rejected before `0003-24`, for reasons that still hold:
  - **Per-library classification tables**, saying which methods read and which write, per library
    and version. They are library-specific, and wrong on their own terms: whether `commit` is
    dangerous depends on the hooks in the repository, which no table knows.
  - **Attribute-path matching.** It misses objects reached by calls or iteration, and it matches
    how the agent reached the object rather than the object.
- Whatever enforces a permission sees only requests unless something confines the bindings
  process. A call's hooks, helpers and the programs it runs are not requests
  (`boundary-policy.md`, "What interception cannot see"); confining those is
  `plan/next/hosted-effect-confinement.md`.
- Repository content cannot widen what the operator grants. A repo's config is content the agent
  can write, as `SECURITY.md` says for `[network]` and for bindings.

## Inherited questions

These came up under `0003-24`'s design. Each applies again if a later design holds a request for
an answer, and none does today.

- **An evaluator.** A model judging a request is `plan/next/policy-evaluator.md`, which waits on
  this entry.
- **Approval reuse.** An approval covered one call, so a loop of escalated calls asked once per
  call. Reuse needs an explicit scope -- binding, type, member, operation, argument values, the
  policy version and a lifetime -- and a way to revoke it; never for a call passing a callable,
  whose behavior can change after approval.
- **A host program's handler.** A host program (`0003-29`) has no way to answer an escalation: it
  would need a handler on `outrig.Session`, carried over the owner connection.
- **A held callback holds a serialized binding's turn.** A request a callback makes is served on
  the outer call's thread, which already holds the turn of a `serialize = true` binding, so a
  person's wait on that request would hold every other kernel's call to the binding. Releasing the
  turn would break what `serialize` promises.
