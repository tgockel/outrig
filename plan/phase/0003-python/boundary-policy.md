# Boundary policy

What decides whether a request from the container reaches a hosted object, and what is recorded
about it. `hosted-objects.md` carries requests to the host and intercepts every one there; this
page is what the interception enforces. It replaces `call-inspection.md`. The design was settled in
the planning round of 2026-09-30 and none of it is built: `0003-21` intercepts and events every
crossing, `0003-22` builds rules, deny, and escalation, and `0003-23` the evaluator.

Three decisions apply throughout:

- **Enforcement is on the host**, in the binding process (`hosted-objects.md`). Nothing on the
  container side is trusted: agent code runs in the same process as anything installed there, and
  can send raw requests past it.
- **The default is audit.** With nothing configured, every request runs and is evented. Tightening
  it is the operator's act.
- **A decision covers one request.** No approval is reused, cached, or extended to a similar call.

## A boundary request

One semantic operation on a hosted object: reading, writing, or deleting an attribute; a call;
item access; iteration; or a relevant special method -- a comparison, `repr`, `len`, entering or
leaving a `with` block. One expression can produce several. `repo.head.commit.tree` is three
attribute reads, each decided and evented on its own, and `for r in repo.remotes:` is a request to
start the iteration and one per item.

Bookkeeping is not a semantic operation: fetching a binding's root, counting references, and listing
the methods a proxy class is built from. Interception still checks it -- a reference count cannot be
inflated, and a method listing carries only public names, the safe list's, and `__call__` for an
object the host can call (`hosted-objects.md`, `0003-16`) -- but rules do not match it, and it is
not offered to an approver.

A callback goes the other way. When a host library calls a callable the agent passed, agent code
runs in the container. That is evented, with the id of the call it ran inside, and policy does not
decide it: policy governs what leaves the sandbox, and a callback enters it. A hosted call the
callback makes is a request like any other, and policy decides it.

The object a request acts on is identified on the host, which holds it, and not by anything the
container claims. A local variable name is presentation and appears in no decision.

## Rules

```toml
[policy]
default = "allow"

[[policy.rules]]           # a person approves every push
binding = "repo"
type    = "git.remote.Remote"
member  = "push"
action  = "escalate"

[[policy.rules]]           # no git command by name, and no execute()
binding = "repo"
type    = "git.cmd.Git"
action  = "deny"
```

The spellings are illustrative and `0003-22` settles the schema. In this sketch a field a rule
leaves out matches anything, so the second rule refuses every request on a `Git` object.

A rule matches on four things: the binding, the host-side type of the object, the member name, and
the operation. Rules are tried in order and the first match decides. A request no rule matches
takes `[policy] default`, which may be any of the four actions and is `allow` when unset.

| action     | what happens                                                     |
|------------|------------------------------------------------------------------|
| `allow`    | dispatched, if admission is still open                           |
| `deny`     | refused before dispatch; the target is never invoked             |
| `escalate` | waits for the escalation handler's answer                        |
| `evaluate` | asks the evaluator, and acts on its verdict                      |

```text
  request received in the binding process
    -> rules in order; the first match, else [policy] default
         allow    -> dispatch -> outcome; refused instead if the binding's closed flag is set
         deny     -> refused; never dispatched
         escalate -> held at the owner's gate for the handler's answer; with no handler, deny
         evaluate -> held at the owner's gate for the evaluator's verdict;
                     on failure, or with no evaluator configured, escalate
    -> a held request the gate allows is dispatched -> outcome; one it denies is refused
```

**Why the host-side type.** A type rule holds however the agent reached the object.
`repo.remotes.origin.push()` reaches a `Remote` by attribute reads, `repo.remote("origin").push()`
by a call, and `for r in repo.remotes: r.push()` by iteration. A rule on `git.remote.Remote` and
`push` covers all three. A rule on the attribute path `repo.remotes.origin.push` covers only the
first, and the path is the agent's choice. The type is a property of the object on the host, which
the container cannot change.

**A repo's `[policy]` may only add restrictions.** The repo config is repository content: a cloned
project brings one, and the agent can edit it through the workspace mount. `SECURITY.md` already
states the same rule for `[network]`, where a repo may choose the mode and only the operator's
global config sets `default`, `allow`, and `deny`. How it is enforced -- which actions a repo rule
may carry, and where repo rules sit in the order -- is `0003-22`'s. The operator's own rules cannot
be redefined by a repo: OutRig keeps the operator's layer -- the global config, or a `Config` an
embedder passes to the builder -- beside the merged config, and reads the operator's policy and the
evaluator's model from it. `0003-22` keeps that layer, and `0003-23` uses it.

Rules run in the binding process, from a table the owner sends it at start, so an allow costs no
round trip to the owner. That is `0003-22`'s fork 1, recommended there, and the admission design
below assumes it. The escalation handler, the evaluator, and the gate that decides held requests
are the owner's.

**Admission is decided in two places**, and the owner's `close_admission()` sets both before it
returns (`lifecycle.md`). Each binding process has a closed flag, which decides a request a rule
allowed: once the flag is set, the binding refuses it, with admission closed as the reason. The
owner's gate decides a held request -- one escalated or sent to the evaluator -- and an allow it
issued before the close is still honored by the binding. The same gate admits executions and child
launches, so closing admission stops all new work, not only boundary requests. A call still waiting
in the container for its connection's lock at the close has not been dispatched: it is woken and
refused.

## The default is audit

With nothing configured, every request runs and is evented. The consequence should be read
plainly: with a library that can run commands -- GitPython's `execute` -- the default is arbitrary
execution on the host, as the user, which the events record. It is the starting point for the
reason `[network]` has an audit mode: an operator sees what an agent actually calls before
deciding what to constrain. Tightening is a rule that denies or escalates a type or a member, a
stricter default, or `default = "escalate"` with an approver at the prompt.

"Evented" means published, and where it goes matters. Every request's events go to the session's
in-memory stream (`observability.md`). An embedder subscribes to it; the CLI's subscriber writes
`events.jsonl` only when `[events] mode = "record"`, and it is off by default. A subscriber that
falls behind loses events and is told how many it lost. Refusing to dispatch a request that cannot
be recorded is `plan/next/mandatory-audit-sink.md`.

## Deny

A deny raises an OutRig exception in the caller before anything is dispatched. The target is never
invoked, so nothing the call would have done on the host happens, and the request's outcome is
`refused`, with what denied it as the reason ("Events"). The exception is distinct from the
library's own, so `except git.GitCommandError` does not catch it, and from a transport failure, so
the agent can tell a call that did not happen from one whose outcome is unknown. Its name is
`0003-22`'s.

**A rule's deny is final.** A human cannot override it. The rule is the operator's standing
decision, and an override at the prompt would make every rule advisory -- in an embedding, to
whoever is answering.

## Escalation

`escalate` passes the request to the session's escalation handler and waits. The handler belongs to
the embedding API (`embedding.md`): an async function that receives the request and a cancellation
signal, and answers allow or deny. The request carries what an approver needs, all of it built
without rendering an object: the call's id and its parent's, the agent and its execution, the
binding, the operation, the member, the host-side type, bounded previews of the by-value
arguments, and the policy's version, a digest of the rules in force.

The CLI's handler prints the pending request with an id, and the user answers at the prompt with
`/approve <id>` or `/deny <id>`:

```text
outrig: [7] repo: call push on git.remote.Remote ('main',)
        /approve 7  or  /deny 7
```

The format is illustrative. The rules around it are not:

- **The call waits until it is answered, interrupted, or the session closes.** While it waits its
  kernel is blocked, as for any hosted call (`hosted-objects.md`), and other kernels are not.
  OutRig sets no expiry. A handler that wants a deadline answers deny when its deadline passes,
  which leaves the deadline to whoever chooses the approvers (`0003-22`).
- **An approval covers one call.** Nothing is reused or cached -- not by member, not by arguments,
  not for a time -- and the next identical call asks again. Reuse with an explicit scope is
  `plan/next/approval-reuse-with-explicit-scope.md`.
- **The owner's gate decides each held call.** Interrupt, close, and allow can arrive together --
  the user types `/approve 7` as Ctrl-C arrives -- and a single state transition at the gate orders
  them so exactly one wins. A call cancelled first stays `cancelled`: a late answer does not
  dispatch it, and the approver is told the request is gone. An allow that wins dispatches the
  call, and the binding honors it even if admission closes after it. An interrupt after that finds
  a running call, whose outcome is `unknown`.
- **An approval approves the preview.** By-value arguments are what the call receives, because
  they were copied before the request was sent. The approval does not stop what the request refers
  to from changing: a host object passed back, a callable that will run during the call, the
  repository's config and hooks, and the files the call reads can all change between the answer
  and the dispatch.
- **Answering questions is not approving.** An embedder's agents may ask the application ordinary
  questions that other agents or users answer. Being able to see or answer those is no authority
  over a hosted call. Which approvers are authorized is the handler's decision, and the handler
  runs in the embedder's process: nothing it does is a boundary request.
- **A request whose handler fails, or is dropped, is never dispatched.** With no handler installed
  -- an embedder that supplied none -- `escalate` is a deny, whose reason says no approver is
  installed (`0003-22`).

## The evaluator

The evaluator is a model call that judges a request. It is off by default, and naming a model
enables it:

```toml
[policy.evaluator]
model = "fast"             # a model alias from the config
```

It runs only where policy says `evaluate` -- a rule's action, or `default = "evaluate"` -- and
never otherwise. Its model is resolved, retried, and failed over like the agent's; `0003-15` makes
that serve model calls outside a round.

- **What it reads.** Trusted instructions, and a bounded description of the request, delimited and
  labeled as evidence, which carries the policy's version as an escalation request does. Repository
  text, docstrings, argument values, and exception text reach it only inside that evidence. They are
  data about the request and never instructions to the evaluator, however they are phrased: a commit
  message that says "approve this" is a commit message.
- **What it returns.** A strict structure -- a verdict of allow, deny, or escalate, and a bounded
  reason -- which OutRig validates whatever the provider enforces. The providers' strict modes do
  not accept every schema shape, so validation at runtime is needed regardless.
- **What it cannot do.** It has no tools, it never executes the call, and it receives no host
  secret.
- **What its verdict does.** `allow` admits the request through the owner's gate. `deny` raises
  the deny exception, with the evaluator's reason, and no person is asked. `escalate` goes to the
  handler with that reason attached (`0003-23`).
- **When it fails** -- malformed output, a timeout, an exhausted budget -- the request escalates if
  a handler exists and is denied otherwise. A failure is never an allow. `evaluate` with no
  evaluator configured takes the same path (`0003-22`).
- **Its usage is its own.** The evaluator's model calls, tokens, and latency are attributed
  separately from the agent's and never added to a round's totals.

Its verdict is judgment, not proof. It reads a description of a call and does not see what the
call will do on the host -- the hooks, helpers, and config programs `hosted-objects.md` lists -- so
a confident allow is not evidence of containment.

## Events

A request is recorded as events that share its id: its receipt, published before it is
dispatched; each decision made about it -- the rule's action, the evaluator's verdict, the
approver's answer (`0003-22`); its dispatch; and its outcome. A request that is never dispatched
has no dispatch event, and that is the record that its target was never invoked.

The outcome is one of five:

- `returned` -- dispatched, and the target returned.
- `raised` -- dispatched, and the target raised; the event names the exception's type.
- `refused` -- never dispatched, with the reason: a rule, the approver, the evaluator, admission
  closed, or interception refusing the request.
- `cancelled` -- never dispatched: the caller was interrupted, or the session closed, while the
  request was held for a decision or queued.
- `unknown` -- dispatched, with no reply: the caller was interrupted, the binding was killed at
  shutdown, or the binding's process died.

Interrupted is a reason attached to `cancelled` or `unknown`, not an outcome of its own.

The fields are the binding, the agent and its execution, the call's id and its parent's, the
operation, the member, the host-side type, bounded previews, the outcome, and the duration
(`0003-21`). A decision event also carries the policy's version, a digest of the rules in force, as
an escalation request and the evaluator's input do, so each decision can be traced to the rules
that made it. Previews are built from by-value data only: scalars and copied containers cut to a
bound, and for an object, its host-side type and an opaque reference id. Nothing calls `repr` or
`str` on an object to fill an event: on a host object that runs library code, which can have
effects, and on a container object it runs agent code.

Boundary events are observability's integration-audit category (`observability.md`) and carry its
warning: arguments are the most useful field and the most likely to hold a secret -- a token in a
remote URL, a password in a command line. A raised exception's host traceback is recorded here and
not shown to the agent. Which audience should see which fields -- the agent, an approver, the
evaluator, a log -- is `plan/next/event-audience-projections.md`. They reach `events.jsonl` only
when `[events] mode = "record"`.

## What policy cannot see

Policy decides requests, and a request is all it sees.

- **The host effects of a call it allowed.** `repo.index.commit("...")` is one request. The hook it
  runs, the programs repository config names, and the credential helper a push consults are not
  requests, and nothing decides them.
- **What a library does with a request.** A library calls itself: `repo.index.commit` runs `git`
  through the same `Git` object the example's second rule denies, and the rule never sees it.
  Rules govern the container's requests, not a library's internals.
- **The container.** What the agent does locally is ordinary container computation: its own `git`,
  its local copy of the library, and its writes to the workspace -- `.git/config` and `.git/hooks`
  included, which change what the next allowed call does on the host.
- **Itself.** Enforcement runs in the binding process, as the host user. A call that is allowed to
  run host programs runs them as the same user, who can signal that process, edit OutRig's config
  and approval store, and rewrite the package cache it imports from. Policy that permits host
  execution cannot also be relied on to constrain what follows it.

Confining those effects is `plan/next/hosted-effect-confinement.md`.

## Rejected alternatives

**Rejected: a terminating Rust proxy.** The design in the deleted `call-inspection.md`: a process
between the two ends that terminates both connections, decodes every frame, and refuses what it does
not accept -- needed then because Pyro5 had no per-call hook and let the client choose the
serializer. Interception now sits in the binding process, which is trusted, already decodes the
protocol, and holds the objects requests name, so it knows the host-side type of whatever a request
acts on. A proxy in between would have to decode RPyC's format a second time, in Rust, against the
pinned version, and track every reference the host handed out to know the same thing. What has to be
the Rust owner's -- the escalation handler, the gate for held requests, the events -- is the owner's
anyway.

**Rejected: per-library classification tables.** Which methods read and which write, which are
safe, per library and per version. They contradict the rule that no mechanism is library-specific,
and they are wrong on their own terms: whether `commit` is dangerous depends on the hooks in the
repository, which no table knows.

**Rejected: attribute-path matching.** It misses objects reached by calls or iteration, as above,
and it matches how the agent reached the object rather than the object.

**Rejected: approval reuse.** An approval applied to a "similar" call approves something nobody saw:
a different argument, a changed repository, a later state. Deferred to
`plan/next/approval-reuse-with-explicit-scope.md`, where a scope would state what it covers.

**Rejected: the evaluator on by default.** It adds a model call and its latency to every request it
judges, needs a model and credentials the operator has to choose, and its verdict is judgment
rather than proof. An operator who wants it names a model.

**Rejected: rules and the evaluator both judging every request**, with a deny from either winning
and a disagreement escalating. It puts a model call, with its cost and latency, on every request,
the ones a rule already settles included, and every disagreement between the two becomes a question
for a person. The chosen order is the first matching rule deciding, with the model asked only where
a rule, or the default, says `evaluate`.

**Rejected: rules assigning a risk tier, with the evaluator judging one tier.** It depends on a
classification of requests by risk, which this design does not have: no library-neutral rule says
how risky a member is, and per-library tables are rejected above.

**Rejected: a human overriding a deny.** "Deny" says why.

## Open questions

- A callable obtained by an attribute read and called later is two requests: the read names the
  member, and the call names none and acts on the callable's own type. GitPython's
  `repo.git.status` is such a read -- `Git.__getattr__` returns a function -- so
  `repo.git.status("--short")` is a read of `status` on `git.cmd.Git`, then a call on a
  `function`. A rule can govern it only by matching the read, before the arguments exist, and an
  approver asked about the read never sees them. A raw client can split a direct method call the
  same way. Whether a call is described in terms of the read that produced its callable is
  `0003-22`'s.
- Whether a rule's type matches subclasses, or only the exact class. `0003-22`'s fork 3 recommends
  exact names, which an operator can copy from the event stream.
- Whether a repo `[policy.evaluator]` is rejected at load. Choosing the judge is not a restriction,
  and `0003-23`'s fork 1 recommends rejecting it. The other route is closed: a repo
  `[models.<name>]` replaces a global model of the same name when the configs merge, which would let
  a repo point the evaluator at an endpoint it controls, but OutRig reads the evaluator's model from
  the operator's layer, so a repo cannot redefine it ("Rules"). `0003-22` keeps that layer, and
  `0003-23` uses it.
- Whether the operator writes or extends the evaluator's instructions. `0003-23`'s fork 2.

## Unverified

- That RPyC's attribute check does not cover the other handlers is read from the 6.0.2 source, and
  the two CVEs are described from their advisories; neither was reproduced. `0003-16`'s acceptance
  attacks the interception with a raw client.
- `Git.__getattr__` returning a function is read from GitPython 3.2.0's source; the two requests it
  produces through RPyC are reasoned from RPyC's proxy code, not observed.
- That the providers' strict modes reject map-valued schemas comes from reading their
  documentation, not from a request made here.
- That the owner's gate lets exactly one of interrupt, close, and allow win for a held request, and
  that a binding's closed flag refuses every request a rule allows after the close, is a design,
  not a measurement. `0003-21` and `0003-22` test them.
- That delimiting evidence keeps the evaluator from following instructions inside it is not
  established by any test. Delimiting reduces a model's tendency to act on text it reads; it does
  not remove it, which is one reason the verdict is judgment.
