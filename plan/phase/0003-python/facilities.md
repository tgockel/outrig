# Facilities

A **facility** is an object an agent is given to act through, and a **facility reference** is
what a dict holds for a managed one: a value that resolves, at each use, to the calling kernel's
own proxy of the facility (the phase README, "Vocabulary"). This page decides the dict an agent's
constructor takes, the reference and what a dict entry may hold, the view a dict-constructed
agent gets of its facilities, how its calls are attributed, who owns a facility's lifetime, and
what a resume reattaches. `0003-42` builds the dict, the reference with its first two kinds, the
view and the attribution; `0003-44` registers the Rust service as a kind (`services.md`), of which
the MCP presenter is one (`mcp-presenter.md`, `0003-54`); `0003-51` registers the external
channel (`external-channels.md`); `0003-64` reattaches a mapping on resume (`continuation.md`).
The instance that holds the mapping is `task-agents.md`'s.

`capabilities` is the application's word for the dict and "capability" its word for an entry;
this page says "facilities" and "facility", and they mean the same thing.

## The dict

An agent's constructor takes its facilities as a `dict[str, Any]` under names, beside its data
inputs: `Reviewer(task, {"files": files})` in `task-agents.md`'s example, which the subclass
passes to `super().__init__(facilities=...)`. The base shallow-copies it at once, so the agent
has a mapping of its own from its `__init__` on, and a child given its parent's entries shares
the facilities and not the dict: changing one mapping changes no other. The copy is the
agent's canonical mapping, reachable on the instance under one attribute (`0003-42`'s fork;
`facilities` recommended) and, in the agent's kernel, through `runtime.bindings` and
`outrig.bindings` ("The view is the agent's"). An owner constructing a root passes the same dict
as an argument, its values facility bindings named by their bound names (`rust-owner.md`,
"Construction from Rust"; OR-A1-2).

A key is a Python identifier, not a keyword, not a dunder, not `runtime`, `asyncio`, `outrig` or
`self`, and not the name of an input or of a class bound in the child, by the rules inputs
already follow (`_inputs`, `crates/outrig/src/python/interpreter.py:4334-4342`; `one_name_each`,
`crates/outrig/src/python/outrig/_agents.py:260-279`). A key is presentation: `{"files": files}`
may name a binding the session calls `repo`, and the events name the binding.

The mapping is mutable and is a working interface, not a boundary (OR-AG4). What mutation does is
"The view is the agent's"; what it never does is widen or narrow authority, which is the
session's (`security.md`, "The binding is the grant").

## A facility reference

`outrig.bindings[name]` is already the authoritative lookup of a binding: "the calling kernel's
proxy of the binding, whatever the kernel's globals hold" (`hosted-objects.md`, "Presentation"),
resolved through the execution's context variable to that kernel's stub (`_Bindings.__getitem__`,
`crates/outrig/src/python/outrig/_kernel.py:66-67`; `Kernel.stub`, `interpreter.py:5447-5450`). A
facility reference is that lookup made into a value: an object with a `kind` and a `name` that
resolves at each use -- an attribute read, a call, an item access, iteration -- to what the calling
kernel holds for that facility, and that can sit in a dict, be handed to a constructor, be stored
on `self`, and be sent by an owner as data. Outside every execution it raises as `outrig.bindings`
does (`_kernel.py:17-23`).

Why a reference and not the parent's proxy: a proxy sends on the connection that produced it, a
connection belongs to one kernel, and the bindings process keeps one object table per kernel
(`hosted-objects.md`, "Calls are synchronous", "A kernel has a pool of connections per binding").
A child using its parent's proxy would send on the parent's connection and the event would name
the parent. The reference resolves to the child's own stub, so the child opens its own connections
and is attributed its own calls (OR-AG5).

The kinds, each registered with how it resolves in a kernel and how it is described:

| Kind      | Names                                 | In the calling kernel            | Task      |
| --------- | ------------------------------------- | -------------------------------- | --------- |
| `binding` | a session binding, factory or live    | its stub of the binding          | `0003-42` |
| `object`  | a plain Python object in the dict     | the object itself                | `0003-42` |
| `service` | a Rust service bound by name; the MCP | its generated object             | `0003-44` |
|           | presenter is one (`0003-54`)          |                                  |           |
| `channel` | an external channel the owner defined | the holder's endpoint, or raises | `0003-51` |

A plain Python object needs no resolution: it is one object in one heap, and `self.facilities
["task"] is task` holds. A reference to a binding is not the kernel's stub -- the stub is what
the convenience global holds, and the reference is what the mapping holds -- so
`self.facilities["files"] is files` is false while every operation on either reaches the same
binding over the same kernel's connections. `isinstance` against a library's class holds for the
sub-object proxies a call returns, as today, and for neither the stub nor the reference. A
service resolves to the object `services.md` generates from its description, one per kernel, with
each call attributed to the executing agent (OR-A3-1, OR-A3-2); the MCP presenter is such a
service, and `mcp-presenter.md` decides what the object does when used. A channel has one holder:
its reference resolves to the endpoint in the kernel the channel is attached to and raises in any
other, naming the holder (`external-channels.md`, "Who holds a channel"); OR-CH1's sentence
"Channel discovery follows supplied facilities" is what makes a channel a dict entry, and an
agent whose dict names no channel holds none. The spelling that makes a reference is `0003-42`'s
fork; `outrig.facility(name)` for a binding is recommended, and the presenters of later kinds
make theirs.

A facility need not be a dict entry to be used. A reference stored on `self._task` by `__init__`
resolves in the child as one in the dict does, and a bound service passed as the task itself is
the ordinary case for a root (OR-A3-2; `rust-owner.md`). What the dict decides is what is
presented: an entry is listed, described and bound by name; a reference held elsewhere on `self`
works and is not advertised.

## What a dict may hold

At attachment (`task-agents.md`, "Construction completes before attachment") OutRig classifies
each entry of the canonical mapping, in place:

| Entry                                          | Becomes                                      |
| ---------------------------------------------- | -------------------------------------------- |
| a facility reference, of any registered kind   | itself                                       |
| the kernel's stub of a binding, a `HostedRoot` | a `binding` reference, by the stub's binding |
| a proxy of a hosted sub-object, an RPyC netref | refused: `TypeError` naming the key          |
| a coroutine, generator or async generator      | refused: a one-shot awaitable is no facility |
| any other Python object                        | an `object` facility: the object itself      |

The stub converts because `{"files": files}` is the code a parent writes, and the stub names its
binding (`interpreter.py:5447-5450`). A sub-object proxy -- `repo.index`, a `Commit` -- is refused
because it belongs to one kernel's connection and names no facility the child could reach on its
own; the message says to give the binding, or a narrower facility the owner binds. That is
OR-AG4's rule that "Selecting an entry does not narrow its methods; supply a narrower facade", and
`boundary-policy.md`, "Narrow bindings for consequential actions", is how such a facade is bound.
A coroutine or generator object is refused because it can be consumed once and by one kernel,
which is OR-A1-2's "non-data, non-facility value"; a socket or a file object is a plain object of
the shared heap and stays one, documented as such. At the owner's boundary the question does not
arise: an argument is data or a facility binding by name, and anything else is refused there
(`rust-owner.md`, "Construction from Rust"). The one check that refuses a hosted object as a data
*input* stands (`_hosted_name`, `interpreter.py:4314-4322`); `0003-42` points its message at the
facilities dict.

A mapping's entries after attachment are read live: `runtime.bindings` and `outrig.bindings`
classify an entry when they read it, so an entry the agent adds later is listed and resolves,
an entry it removes is gone, and a netref it puts there raises at the read with the same reason.
Convenience globals are installed once, at the kernel's build, and the orientation is composed
once (`task-agents.md`, "Instructions are composed from contributions"); an entry added later is
reached through the mapping or `outrig.bindings[key]`, which the orientation says. This is the
"documented discovery behavior" OR-AG4 leaves open, decided by `0003-42`'s fork 3 as recommended
here: no snapshot is claimed current, and no immutability is imposed.

## The view is the agent's, the grant is the session's

`typed-agents.md`, "Placement and limits", records: "A child has the same bindings, since every
kernel gets a stub for each (`hosted-objects.md`), and the same skill catalog. That is sharing,
not isolation: code in a child can reach its parent's objects, and giving co-hosted agents
different grants would be presentation only, so none is offered." And "Inputs are values": "A
hosted object is not in the subset, so it cannot be an input. It does not need to be one: a child
has the session's bindings already". `agent-classes.md`, "Construction and readiness", applies
the second: the constructor "checks each input against the serializable subset ... so a hosted
reference or an open file is refused in the constructor with nothing launched." `0003-42`
reverses the decision not to offer the presentation-only narrowing, and the rationale that
refused facilities as inputs, for agents constructed with a dict:

- **A kernel built from a dict presents its dict.** Its orientation's facility manifest lists the
  mapping's entries and no other binding, service or channel; `runtime.bindings` lists them, each
  with its `kind`, the description its kind allows and its type; a convenience global is
  installed per key where the name is free, as `bind_stubs` installs every binding's today
  (`interpreter.py:5439-5445`); and `outrig.bindings[key]` resolves the mapping's entry, raising
  `KeyError` for a session binding the mapping does not name, as it raises today for a name the
  session has not bound.
- **A kernel built without a dict inherits its maker's view.** Under the CLI's primary that is
  today's session-wide view -- every binding's stub under its name, `runtime.bindings` from the
  session's manifest (`_BINDINGS`, `interpreter.py:6173`, `:4016-4025`), the orientation's
  `hosted()` list (`crates/outrig/src/agent/orientation.rs:267-295`), and every service
  (`mcp-presenter.md`, "The Python surface") -- so the primary, its `runtime.spawn` children, a
  decorated call's child and an `outrig.Agent` it constructs without `facilities=` keep exactly what
  they have. A child a dict-constructed agent makes without a dict -- by `runtime.spawn`, a
  decorated call or a constructor with no `facilities=` -- inherits that agent's mapping as it
  stands at the spawn, never the session's registry: a narrowed agent cannot widen a child's view by
  omission, which is the fallback to full Session authority OR-AG4 and CC-AGT-3 exclude, and an
  empty dict gives an empty view. Inheritance copies the parent's facility entries except its
  channel references, which are the holder's and never inherited (`external-channels.md`, "One
  holder per channel"): the application's root, whose dict names its channels, spawns children that
  see its other facilities and none of its channels, and a child that should relay gets a narrower
  object of the parent's making. The view is presentation and not authority either way.
- **Facilities are inputs now, as references.** The rationale inverts: a child should not see
  every binding, so it does need the ones it should see given to it. A data input is still data,
  checked against the subset; a facility goes in the dict.

What stands, and why the reversal is scoped: the grant is the session's. Every kernel can make a
connection to every binding (`Kernel.stub` takes any binding's name), co-hosted kernels share
`sys.modules`, the working directory and the environment (`agent-placement.md`, "What agents
share regardless"), and a child can reach its parent's objects; `security.md`, "Agents are not
isolated from each other, and it does not matter", keeps its reasoning in full. The requirement
concedes it: "The mutable dictionary is a working interface, not adversarial same-VM isolation."
What the view changes is what a model is told and what its convenience names hold. The one line
between a model and the whole registry is that OutRig never dumps `__dict__` or runs `repr` to
describe an instance (`task-agents.md`, "Instructions are composed from contributions"), which is
why `0003-47` composes the manifest from the mapping's kinds and names and nothing else.

A missing convenience fails clearly: a child built with `{files}` that writes `task` gets a
`NameError`, and `outrig.bindings["task"]` a `KeyError`; nothing falls back to the session's
registry (OR-AG4), and a child built without `task` that calls an application helper which
needs it is told "not provided" by that helper, never answered from the session (OR-A3-2).
Ordinary fields such as `self._task` are accessible Python state in both kernels, and an underscore
is a convention, not isolation (OR-AG5). Module code reaches a facility only through a dict key --
`outrig.bindings["task"]`, resolved for the calling kernel -- and reaches the instance only as
`self` in the instance's own methods: OutRig exposes no `outrig.current()` for the calling kernel's
instance in this phase, so an application facade that needs the task takes it from `self` or from
the dict (CC-AGT-4, decided on review).

## Attribution

A hosted request is attributed to the kernel whose connection carried it: the interpreter's `rpc`
line names the agent, and the relay accepts the name only for a kernel the host opened
(`crates/outrig/src/python/hosted.rs:343-353`); the `hosted.*` events carry that `agent`
(`crates/outrig/src/harness/event.rs:775`). A reference resolves to the calling kernel's stub, so
a child's calls through its mapping are the child's: two children given one binding each produce
`hosted.received` events naming themselves, and a facility passed through two constructor levels
is one underlying facility with three attributed callers (OR-A1-2). A call made inside a
`to_thread` worker is still the submitting execution's (`agent-placement.md`, "Output stays
attributed"). A service call is attributed the same way, from the calling context, and evented
like a hosted call (OR-A3-1, OR-A3-5; `services.md`).

An `object` facility is plain Python: a call on it runs in the calling execution, is billed to it
like any local code, and crosses no boundary, so nothing is evented, as nothing is for a library
imported locally (`hosted-objects.md`, "Packages"). An application that wants its task object's
operations recorded binds it -- a service, a live object through `session.inject` (`0003-36`), or
a factory binding -- and passes the reference.

A parent's proxy stored on `self` and used from the child -- `self._repo = repo` in `__init__`,
then `self._repo.head` in the child -- sends on the parent's connection and is attributed to the
parent. The classification refuses the netref case in the dict and the design states this one
rather than policing it; the orientation says to reach a facility through the mapping.

## Lifetime

The session owns its facilities. A factory binding lives in the bindings process for the
session's life and a live object in its program (`hosted-objects.md`, "Lifetime"; `security.md`);
a service is the embedder's and is bound for the session's life (`services.md`); an MCP placement
is the Session's (`mcp-presenter.md`); an external channel is defined when the Session is built
(`external-channels.md`). Releasing a child closes that kernel's connections to every binding
(`Kernel.close_hosted`, `interpreter.py:5478-5484`) and drops its mapping with its namespace;
"A release kills no binding, since the bindings process serves every kernel in the session"
(`lifecycle.md`, "Releasing a child"), so a parent using the same facility is untouched, and a
child's release leaves a service serving its parent (OR-A3-2). An `object` facility has an
ordinary Python lifetime: the child's kernel drops its reference with its globals, and the object
lives while anything else holds it.

A facility that goes away under an agent -- `binding.exited`, an MCP service torn down -- makes
the next operation through the reference raise as the next hosted call raises after
`binding.exited` (`doc/reference/events.md`), naming the facility; the reference itself is a
value and never becomes invalid.

## Reattachment on resume

A save holds no proxy, socket, client or credential (OR-AG5; `lifecycle.md`, "The interpreter's
death ends the session": "A fresh interpreter inherits no pending handles and no proxies, only
the notice"). What it holds of a mapping is its description: each key with its kind and name for
a managed kind, and the key and type name for an `object` entry, whose object is Python state and
is gone with the interpreter. A resume is a new session (`continuation.md`): the owner provisions
the application code again (`application-code.md`), constructs the root again with its arguments
and references, and each reference reattaches by kind and name to the new session's facility of
that name, with the new session's authority; a name the new session lacks is in the
reconciliation notice, and so is every `object` entry, which the application re-supplies through
its constructor or does without. `0003-64` builds it; this page fixes only what a mapping saves
as and that nothing serialized is reattached.

## Events

`0003-42` adds `facilities` to `agent.spawned` and `agent.instance.started`
(`crates/outrig/src/harness/event.rs:369-376`, `:491-499`): each entry as `{name, kind, target,
type}`, where `target` is the facility's bound name for a managed kind and `null` for an
`object`, and `type` the qualified type name. No new event: mutation of a mapping is not evented,
since nothing crosses when a dict changes. No new close row: a reference at the close resolves to
a request the **Hosted calls** rows, or a service's rows (`services.md`), already cover.

## Rejected alternatives

**Rejected: per-agent grants.** Authority narrower than the session's would need a boundary
between kernels, which one process does not have (`agent-placement.md`, "Rejected
alternatives"; `security.md`). The view is presentation, said so.

**Rejected: handing the parent's proxy to the child.** Above, under "A facility reference": wrong
connection, wrong attribution.

**Rejected: deep-copying facilities, or cloning modules per agent.** A client is not data, and a
module object is the interpreter's (`skills.md`, "A skill is a module"). Subsets preserve
references.

**Rejected: an immutable mapping, or a snapshot claimed current.** OR-AG4 asks for neither; the
mapping is a dict, read live where OutRig reads it, and the orientation says what is composed
once.

**Rejected: an application-side registry, serializer or transport for facilities.** The reference
is OutRig's, so the application invents none (`capability-passing.md`).

**Rejected: `runtime.bindings` listing the session's bindings in a dict-constructed kernel.** It
would advertise what the mapping omits, which is what OR-AG4 forbids.

**Rejected: a kind for MCP beside the service.** The MCP presenter is a service OutRig ships
(`services.md`; `mcp-presenter.md`), and one kind with one resolver and one manifest shape is what
OR-A3-5 asks for.

## Open questions

- Whether `runtime.bindings` is renamed `runtime.facilities` once kinds beyond bindings exist:
  a service or a plain object listed under "bindings" reads oddly. The phase README fixed
  `runtime.bindings` as what follows the dict; a rename beside it is a documentation question
  for `0003-44` or the documentation task.
- Whether a dict-less child of a dict-constructed parent should inherit the parent's view
  (`task-agents.md`, "Open questions").
- Whether an `outrig.Agent` instance in a dict is advertised with its request methods.
- Whether an `object` facility may carry a description beyond its type's name and docstring's
  first line -- a `description=` on the dict entry, say -- without reading anything from the
  object.
- The order `outrig.bindings` iterates a dict-constructed kernel's entries in: the dict's, or
  sorted.

## Unverified

- That a `HostedRoot` stub identifies its binding by name through its pool is read from
  `Kernel.stub` and `_Pool`'s construction (`interpreter.py:5447-5450`, `:5463-5476`); the
  attribute the classification reads is `0003-42`'s to add.
- The attribution claim rests on the relay accepting `agent` only for an opened kernel
  (`hosted.rs:343-353`) and on each kernel's pool being its own; both are `0003-22`'s and
  `0003-23`'s tested behavior, not re-measured here. A service's attribution is `0003-44`'s to
  build and test.
- Whether a live read of the mapping in `runtime.bindings` is cheap enough to run on every
  read was not measured; it reads kinds, names and type names and runs no `repr`, so the cost is
  a dict walk.
