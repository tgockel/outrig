# Boundary policy

What decides whether a request from the container reaches a hosted object, and what is recorded
about it. `hosted-objects.md` carries requests to the host and intercepts every one there; this
page is what the interception enforces. It replaces `call-inspection.md`.

In 0.3 the answer is the binding: **having a remote object is permission to use it.** `0003-20`
built who may grant one, and `0003-22` built the rest -- every request runs, recorded -- and
admission's two places. `0003-24` built a `[policy]` of rules over a binding's types, members
and operations, with escalation to a person and a model evaluator (`0003-25`) to follow. Both
were taken out before release (2026-10-18). Rules about method calls cannot say what an operator
means -- "read-only git" -- and a policy written in them is a mess no one can review. Finer
permissions are deferred to `plan/next/binding-permissions.md`, which starts from what people
want to say rather than from method calls. `0003-24`'s Decisions are the record of what was tried.

Three decisions apply throughout:

- **Enforcement is on the host**, in the serving process -- the bindings process, or a host
  program's serving module for a live object it injected (`hosted-objects.md`). Nothing on the
  container side is trusted: agent code runs in the same process as anything installed there, and
  can send raw requests past it.
- **The binding is the grant.** An object reaches the agent only as a binding the operator's
  global config declares, or one a repository's config declares and the operator approved, that
  exact declaration in that repository (`0003-20`). What it grants is the object and everything
  reachable from it.
- **Every request is published as events** on the session's stream: its receipt, its dispatch,
  and its outcome ("Events").

## A boundary request

One semantic operation on a hosted object: reading, writing, or deleting an attribute; a call;
item access; iteration; or a relevant special method -- a comparison, `repr`, `len`, entering or
leaving a `with` block. One expression can produce several, and each request is made as the
operation runs (`hosted-objects.md`, "Calls are synchronous"). `repo.head.commit.tree` is three
attribute reads, each evented on its own, and `for r in repo.remotes:` is a request to start the
iteration and one per item. `repr(x)`, `str(x)` and `dir(x)` are requests when they are called, as
they would call methods of a local object, and so is whatever `help(x)` reads.

Bookkeeping is not a semantic operation: fetching a binding's root, counting references, and listing
the methods a proxy class is built from. Interception still checks it -- a reference count cannot be
inflated, and a method listing carries only public names, the safe list's, and `__call__` for an
object the host can call (`hosted-objects.md`, `0003-16`) -- but it is not recorded as a request.

A callback goes the other way. When a host library calls a callable the agent passed, agent code
runs in the container. That is evented, with the id of the call it ran inside. A hosted call the
callback makes is a request like any other.

The object a request acts on is identified on the host, which holds it, and not by anything the
container claims. A local variable name is presentation and appears in no record.

**Admission is decided in two places**, and the owner's one `close_admission()` reaches them at
different times (`lifecycle.md`). The call itself is one state change in Rust, and it returns at
once: it stops the relay forwarding to every binding. A request the relay had not forwarded is
refused in Rust and reaches no binding, and one it had forwarded ends with the outcome the binding
gives it (`0003-22`). Each serving process also has a closed flag: once the flag is set, the
binding refuses a request, with admission closed as the reason. The flag is installed after the
close has returned -- the relay enqueues a control line behind the frames it had forwarded, without
waiting for a binding that has stopped reading, and the binding sets the flag when it reads the
line -- so a forwarded request ahead of the line may be invoked after the close, and one the
binding had read but not started, waiting for the serialize lock, is cancelled once the binding
observes the flag and not before. The same close admits no new executions or child launches, so
closing admission stops all new work, not only boundary requests. A call still waiting in the
container for a free connection at the close has not been sent: it is woken with the closing error
and recorded `cancelled`.

## What a binding grants

Every request to a bound object runs and is published as events. The consequence should be read
plainly: with a library that can run commands -- GitPython's `execute` -- a binding of it is
arbitrary execution on the host, as the user, which the events record. Binding a whole library is
an affirmative grant of the host user's authority: the events record what ran and constrain none
of it. What an operator grants is what the factory returns; a binding that should do less is an
object that does less ("Narrow bindings for consequential actions").

"Evented" means published, and where it goes matters. Every request's events go to the session's
in-memory stream (`observability.md`). An embedder subscribes to it; the CLI's subscriber writes
`events.jsonl` only when `[events] mode = "record"`, and it is off by default. A subscriber that
falls behind loses events and is told how many it lost. Refusing a request that cannot be recorded
is `plan/next/mandatory-audit-sink.md`.

## Events

A request is recorded as events that share its id. Three words are used exactly, here and in the
tasks: a request is **forwarded** when the relay in Rust has sent it to its binding's process; its
target is **invoked** when the binding calls it; and `dispatch` names the event the binding
publishes just before it invokes the target, and nothing else. A forwarded request can still be
refused or cancelled by its binding, so forwarded does not mean invoked. The events are: its
receipt; its dispatch, published before its target is invoked; and its outcome. A request whose
target is never invoked has no dispatch event, and its outcome event says why.

Published means placed in the session's in-memory stream (`observability.md`), in sequence, before
the invocation. It says nothing about what any reader has consumed. A subscriber runs when it
runs, not when an event is published; one that falls behind loses events in a counted gap, and can
receive a request's receipt, lose its dispatch in the gap, and then receive its `returned` -- and
the target ran. A subscriber that had not yet taken a published receipt when the owner died has
lost it with the owner, so the tail of a crashed session's record is short of events the stream
had published, in memory and on disk alike: `events.jsonl` is written by a subscriber, and a
receipt is on disk only once that subscriber has written it. The rule for a reader follows.
Non-invocation is read from an authoritative outcome, `refused` or `cancelled`, or from a history
known to be complete -- no gap, and the session's shutdown report in hand -- and never from a
dispatch event that is absent across a gap, from a file cut short, or from a receipt with nothing
after it. A receipt in an incomplete record shows the request was made; what is missing after it
shows nothing.

The outcome is one of five:

- `returned` -- invoked, and the target returned.
- `raised` -- invoked, and the target raised; the event names the exception's type.
- `refused` -- never invoked, with the reason: admission closed, or interception refusing the
  request.
- `cancelled` -- never invoked: the caller was interrupted, or the session closed, while the
  request was waiting for a free connection or for the serialize lock.
- `unknown` -- forwarded, with no reply: the binding was killed at shutdown, the binding's
  process died, or a live object's call was still running in the host program when the report
  was sealed (`0003-36`). Whether the target was invoked is open; a dispatch event published
  before the reply was lost says that it was, and leaves whether it finished open.

An interrupted call whose target had been invoked runs on, and its reply records `returned` or
`raised` with the note that the caller was interrupted. Interrupted is a reason attached to
`cancelled`, or that note on `returned` or `raised`; it is not an outcome of its own.

The fields are the binding, the agent and its execution, the call's id and its parent's, the
operation, the member, the host-side type, bounded previews, the outcome, and the duration
(`0003-22`). Previews are built from by-value data only: scalars and copied containers cut to a
bound, and for an object, its host-side type and an opaque reference id. Nothing calls `repr` or
`str` on an object to fill an event: on a host object that runs library code, which can have
effects, and on a container object it runs agent code.

Boundary events are observability's integration-audit category (`observability.md`) and carry its
warning: arguments are the most useful field and the most likely to hold a secret -- a token in a
remote URL, a password in a command line. A raised exception's host traceback is recorded here and
not shown to the agent. Which audience should see which fields -- the agent, a log -- is
`plan/next/event-audience-projections.md`. They reach `events.jsonl` only when
`[events] mode = "record"`.

## What interception cannot see

Interception sees requests, and a request is all it sees.

- **The host effects of a call.** `repo.index.commit("...")` is one request. The hook it runs, the
  programs repository config names, and the credential helper a push consults are not requests,
  and nothing records them.
- **What a library does with a request.** A library calls itself: `repo.index.commit` runs `git`
  through a `Git` object, and no request is made for it. Interception covers the container's
  requests, not a library's internals.
- **The container.** What the agent does locally is ordinary container computation: its own `git`,
  its local copy of the library, and its writes to the workspace -- `.git/config` and `.git/hooks`
  included, which change what the next call does on the host.
- **Itself.** Interception runs in the binding process, as the host user. A call that runs host
  programs runs them as the same user, who can signal that process, edit OutRig's config and
  approval store, and rewrite the package cache it imports from.

Confining those effects is `plan/next/hosted-effect-confinement.md`.

## Narrow bindings for consequential actions

Since the binding is the grant, an operator who wants an action done and not the library's whole
reach binds an object whose methods name the intended effect and resolve identifiers through
trusted configuration. The reviewer's example:

```python
def publish_commit(repo_id, commit_oid, destination_id, expected_remote_oid): ...
```

`repo_id` and `destination_id` are looked up in configuration the operator wrote, not in anything
the agent passed; the method rechecks, when it executes, that the destination's current state is
`expected_remote_oid`, and refuses otherwise; and its implementation controls the hooks, helpers
and configuration the operation runs with, rather than inheriting the repository's. Renaming a
generic method proves nothing, since the library under it still runs whatever the repository
names.

Two things decide the effect besides the arguments, and the binding answers both:

- **What an identifier names.** `destination_id` is a name, and what it names can change between
  the request and the execution. The binding resolves it when it executes, from configuration the
  agent cannot write.
- **The state the effect depends on.** A check made at the start of the method can be stale by the
  time the operation runs. The condition has to be enforced by the operation itself, as a
  compare-and-set: a push whose ref update the remote applies only if the ref still holds
  `expected_remote_oid` (`git push --force-with-lease=<ref>:<oid>` is that form for a push), and
  not an inspection of the remote followed by an unconditional push. The binding states what the
  operation checks and refuses on in the method's description.

Binding the whole library instead -- a `Repo` with its object graph -- is an affirmative choice to
grant the host user's authority to the agent, with events as the record. It fits a trusted personal
session and not a product that promises constrained actions.

## Rejected alternatives

**Rejected: a terminating Rust proxy.** The design in the deleted `call-inspection.md`: a process
between the two ends that terminates both connections, decodes every frame, and refuses what it does
not accept -- needed then because Pyro5 had no per-call hook and let the client choose the
serializer. Interception now sits in the binding process, which is trusted, already decodes the
protocol, and holds the objects requests name, so it knows the host-side type of whatever a request
acts on. A proxy in between would have to decode RPyC's format a second time, in Rust, against the
pinned version, and track every reference the host handed out to know the same thing.

**Rejected for 0.3: rules over method calls.** `0003-24` built them and they were taken out:
see the introduction, and `plan/next/binding-permissions.md` for what was learned.

## Unverified

- That RPyC's attribute check does not cover the other handlers is read from the 6.0.2 source, and
  the two CVEs are described from their advisories; neither was reproduced. `0003-16`'s acceptance
  attacks the interception with a raw client.
- `Git.__getattr__` returning a function is read from GitPython 3.2.0's source; the two requests it
  produces through RPyC are reasoned from RPyC's proxy code, not observed.
