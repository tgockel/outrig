# 0003-42 -- An agent is given its facilities

## Context

A child is given data: `_inputs` checks each input against the serializable subset, previews
it and deep-copies it (`crates/outrig/src/python/interpreter.py:4325-4348`), `_hosted_name`
refuses an object a binding holds with "a child reaches the session's bindings itself, by name"
(`:4314-4322`), `_spawn` sends the manifest to the host and keeps the copies in `_spawning`
(`:3907`, `:3941-3949`), and `_open` binds them into the child's globals (`:6279-6282`). A child
is given every binding: `_bindings` records the session's bindings in `_BINDINGS` and puts their
package directories on `sys.path` (`:6176-6204`), `Kernel.__init__` calls `bind_stubs`, which
installs a stub per binding under its name (`:5439-5445`; `stub` and `_pool`, `:5447-5476`),
`runtime.bindings` returns `_BINDINGS` (`:4016-4025`), `outrig.bindings[name]` resolves the
calling kernel's stub (`crates/outrig/src/python/outrig/_kernel.py:55-79`), and the orientation
lists every binding (`hosted`, `crates/outrig/src/agent/orientation.rs:267-295`; `child_preamble`
takes `bindings: &[Brief]`, `:168-175`, given `settings.bindings` by `make`,
`crates/outrig/src/agent/children.rs:584-591`). A hosted request is attributed to the kernel
whose `rpc` line carried it, accepted only for a kernel the host opened
(`crates/outrig/src/python/hosted.rs:343-353`; `crates/outrig/src/harness/event.rs:775`).

OR-AG4 asks for `dict[str, Any]` facilities beside the constructor's data, each agent with an
independent mapping whose entries may share facilities, and orientation, discovery and
convenience access reflecting that mapping and not the whole registry, with mutation, validation
and discovery refresh as open choices. OR-AG5 asks that OutRig manage per-context references and
caller attribution, that a child's release not close a parent-used service, and that the
constructor's `task` may itself be a live facility that need not be a dict entry. OR-A1-2 adds
that a facility passed through two constructor levels is one underlying service with three
attributed callers, and that a value neither data nor a facility -- an open socket, a coroutine
-- is refused at construction naming the key; `facilities.md`, "What a dict may hold", says how
far that refusal reaches in one heap.

**This task reverses recorded decisions.** `typed-agents.md`, "Placement and limits": "A child
has the same bindings, since every kernel gets a stub for each (`hosted-objects.md`), and the
same skill catalog. That is sharing, not isolation: code in a child can reach its parent's
objects, and giving co-hosted agents different grants would be presentation only, so none is
offered." `typed-agents.md`, "Inputs are values": "A hosted object is not in the subset, so it
cannot be an input. It does not need to be one: a child has the session's bindings already".
`agent-classes.md`, "Construction and readiness": the constructor "checks each input against the
serializable subset ... so a hosted reference or an open file is refused in the constructor with
nothing launched." The validation found the reversal sound because authority stays the
session's and only presentation narrows (its reversal 3, the conflict check's C2 and C3, and its
constraint 4), and found the mechanism: the `outrig.bindings` lookup made into a value.
`facilities.md` records what replaces the sentences and why; `0003-36`'s "Every kernel reaches
every binding, children included, since the grant is the session's" stands as the grant.

## Goal

An agent's constructor takes a dict of facilities beside its data inputs; each agent has its own
mapping; a child's orientation, `runtime.bindings`, convenience globals and `outrig.bindings`
follow its mapping; and a facility reference resolves to the calling kernel's own proxy, so a
child opens its own connections and is attributed its own calls.

## Deliverables

- **The facility reference.** A value with a `kind` and a `name` that resolves, at each attribute
  read, call, item access, iteration, `len`, `repr` and comparison, to what the calling
  execution's kernel holds for that facility, through `_kernel.py`'s `_calling`; outside every
  execution it raises `RuntimeError` as `outrig.bindings` does. Kinds register a resolver and a
  brief; this task registers `binding`, resolving to `Kernel.stub(name)` for a factory or live
  binding, and `object`, which is the object itself and needs none. `outrig.facility(name)` makes
  a `binding` reference (fork 1); the class is exposed for `isinstance`, with `kind` and `name`.
  On the host (`0003-35`'s program; `0003-46`'s owner) the same call makes a reference that is
  data to send and raises on use. The kinds registered later use the same registry: `service`
  (`services.md`, `0003-44`), of which the MCP presenter is one, and `channel`
  (`external-channels.md`, `0003-51`), which OR-CH1's "Channel discovery follows supplied
  facilities" makes a dict entry.
- **The dict.** `Agent.__init__(..., facilities=None)` shallow-copies the dict at once into the
  canonical mapping, exposed from then on as a read-only property on the instance (fork 2;
  `facilities` recommended), whose name joins those a request channel cannot take. Keys follow
  the input-name rules plus `outrig` and `self`, and may not repeat an input's or a bound class's
  name (`one_name_each`).
- **Classification at attachment**, in place, per `facilities.md`, "What a dict may hold": a
  reference stays; a `HostedRoot` becomes a `binding` reference by its binding's name (fork 5); a
  netref raises `TypeError` naming the key and saying to give the binding or a narrower facility
  the owner binds; a coroutine, generator or async generator object raises `TypeError` naming the
  key, since a one-shot awaitable is no facility (OR-A1-2); anything else is an `object`
  facility. `_hosted_name`'s message for a hosted
  object given as an *input* now points at `facilities=`.
- **What crosses.** The spawn line carries `facilities: [{name, kind, target, type}]`, a JSON
  manifest the interpreter sends the host beside `manifest`; `_spawning[spawn]` holds the mapping
  as a Python object, which `_open` gives the child's `Kernel`. The host never carries a facility.
- **The view.** `Kernel.bind_stubs` installs, for a kernel given a mapping, the kernel's own stub
  under each `binding` key and the object under each `object` key, where the name is free, and
  nothing else; `self.stubs` is the view; `outrig.bindings[key]` resolves it, raising `KeyError`
  for a session binding the mapping does not name; `runtime.bindings` reads the mapping live,
  each entry a `Binding` record gaining `kind`, with the binding's description and type for a
  `binding` and the type's qualified name and docstring's first line for an `object`, read as
  `0003-36` reads a live object -- no `repr`, property or iteration. A kernel without a mapping
  inherits its maker's view: the session-wide view under the CLI's primary, and the parent's
  mapping as it stands at the spawn under a dict-constructed agent (fork 6); `help(runtime)` says
  which view a kernel has.
- **Mutation and discovery** per fork 3: the mapping is a plain dict the agent may change;
  `runtime.bindings` and `outrig.bindings` classify at the read; convenience globals install once;
  the orientation is composed once and says that a later entry is reached through the mapping.
- **The orientation's data.** `child_preamble` takes the facility manifest in place of
  `bindings` for a dict-constructed child: each entry by name and kind with the text its kind
  allows, and `hosted()`'s rule about a blocking call when any entry is a `binding`. The full
  composition is `0003-47`'s; this task changes what `hosted()` is given and tests the list.
- **Attribution** holds by construction: a reference resolves to the calling kernel's stub, so
  its requests travel on that kernel's connections and `hosted.*` events name it.
- **A missing convenience fails clearly**: `NameError` for an unbound global, `KeyError` from
  `outrig.bindings`, never the session's registry.
- **Events and the close table.** `agent.spawned` and `agent.instance.started` gain `facilities`
  (`event.rs:369-376`, `:491-499`); `doc/reference/events.md` says so, and that mutation is not
  evented. No new close row: a reference at the close resolves to a request the **Hosted calls**
  rows cover; the task's `## Decisions` says so.
- **`## Decisions` records**: the reversal of the three quoted sentences, scoped to agents
  constructed with a dict, with `security.md`'s reasoning left standing; the inversion of the
  rationale -- a child should not see every binding, so it does need the ones it should see
  given to it, as references; forks 1 through 6; and that an agent constructed without a dict
  inherits its maker's view, the session-wide one under the CLI's primary and the parent's
  mapping under a dict-constructed parent.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test.

- A `Reviewer` constructed with `{"files": files}`, `files` the parent's stub: the child's
  orientation lists `files` and no other binding; `runtime.bindings` in the child lists `files`
  only, with `kind` `binding`; `files` is a global there; `outrig.bindings["repo"]` raises
  `KeyError` while the session has `repo`; the parent's `runtime.bindings`, globals and
  `outrig.bindings` are unchanged.
- One binding, two agents: a call on `files` in the child is `hosted.received` with the child's
  `agent`, and one in the parent with the parent's; each kernel's connections are its own
  (`0003-23`'s pool test extended to the child). Passed through a second level -- the child
  constructs a grandchild with `{"files": self.facilities["files"]}` -- the three agents' calls
  are three attributed callers of one binding (OR-A1-2).
- `{"gen": (x for x in ())}` and `{"co": some_coroutine()}` raise `TypeError` at construction
  naming the key; `{"sock": socket.socket()}` is an `object` facility.
- `{"index": repo.index}` raises `TypeError` at construction naming `index`; no spawn.
- A `runtime.spawn` child and a dict-less `outrig.Agent` child of the `{"files": files}` child each
  see `files` and nothing else, in `runtime.bindings`, their globals and their orientation; a child
  constructed with `{}` sees nothing; a `runtime.spawn` child of the CLI's primary keeps today's
  view. A `runtime.spawn` child of a root constructed with `{"inbox": <channel>, "files": files}`
  (`0003-51`'s fixture) launches, sees `files`, and has no `inbox` in `runtime.channels` or its
  orientation; the root stays the holder.
- `{"task": task}`, a plain object with `update_progress`: `self.facilities["task"] is task` in
  the child (through `0003-45`'s `self`, or a test helper until it lands);
  `task.update_progress("x")` in the child runs plain Python, asserted by a counter, with no
  `hosted.*` event;
  `runtime.bindings` lists `task` with `kind` `object`, the type's qualified name and the
  docstring's first line; a `__repr__` that writes a file never runs.
- Mutation: the child sets `self.facilities["extra"] = ...`: the parent's mapping is unchanged;
  `runtime.bindings` in the child lists `extra` at the next read; `extra` is not a global; no new
  `model.instructions`. The child puts a netref in its mapping: the next `runtime.bindings` read
  raises naming the key.
- A `runtime.spawn` child of the dict-constructed reviewer sees every binding, and so does a
  `Reviewer` constructed without `facilities=`; every `0003-34`, `0003-30` and `0003-23` test
  passes unchanged.
- A key `runtime`, a key `self`, a key equal to an input's name, and a key equal to a bound
  class's name are each refused at construction naming the key.
- `outrig.facility("repo")` from the primary: `.head` is a `hosted.received` attributed to the
  primary; from a thread started with `_thread.start_new_thread` it raises `RuntimeError`.
- `agent.spawned` and `agent.instance.started` carry `facilities` with `name`, `kind`, `target`
  and `type` for a `binding` and an `object`.
- `help(reviewer)` names the mapping; a request method named like it is refused at class
  creation.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The reference's spelling -- Recommended: `outrig.facility(name)` returning an
   `outrig.Facility` with `kind` and `name`.** Alternatives: `outrig.bindings.ref(name)`, which
   ties the spelling to one kind; `outrig.Facility("binding", name)`, which makes the application
   name a kind it has no other reason to know.
2. **The mapping's attribute -- Recommended: `facilities`, a read-only property to the canonical
   dict.** Alternatives: `_facilities`, hidden from `help()` but still the one mapping;
   `capabilities`, the application's word, which an application can alias with a property of its
   own.
3. **`runtime.bindings` reads the live mapping -- Recommended.** The alternative snapshots it at
   attachment, which is the "constructor snapshot claimed current" OR-AG4 forbids unless said;
   a live read costs a dict walk and runs no code of the entries'.
4. **What a `binding` entry's text in the orientation carries -- Recommended: the binding's
   description and type from the session's manifest, under the dict's key, with `Runs one call at
   a time.` for a serialized one, as `hosted()` renders today.** The alternative renders the key
   only and points at `runtime.bindings`, which costs the model a read per round for a name it
   cannot learn by looking (`discovery.md`).
5. **A `HostedRoot` stub in the dict -- Recommended: converted to a `binding` reference by its
   binding's name.** `{"files": files}` is the code a parent writes. The alternative refuses it
   and requires `outrig.facility`, which is stricter and makes the common case an error.
6. **A dict-less child of a dict-constructed agent -- Recommended: inherits the parent's mapping as
   it stands at the spawn, except its channel references**, which are the holder's
   (`external-channels.md`, "One holder per channel") and would otherwise make every such launch
   fail at readiness naming the parent. The alternatives are the session-wide view, which is the
   fallback to full authority OR-AG4 excludes, and an empty view, which makes every `runtime.spawn`
   child of a narrowed agent useless with no way to give it anything.

## Dependencies

- **Hard: `0003-39`.** The reference, the view and the manifest are interpreter and package code.
- **Hard: `0003-41`.** Attachment is where the mapping is classified, and `Agent.__init__`
  records `facilities`.
- **Hard: `0003-34`, `0003-26`.** The class, and `outrig.bindings`.
- **Soft: `0003-36`.** A live object is a `binding` the reference covers; its presentation
  rules are followed for `object` entries.
- **Soft: `0003-35`.** On the host a reference is data an owner's request carries.

## See also

- `plan/phase/0003-python/facilities.md` -- all of it.
- `plan/phase/0003-python/task-agents.md` -- "The shape", "Helpers and requests".
- `plan/phase/0003-python/typed-agents.md` -- "Inputs are values", "Placement and limits".
- `plan/phase/0003-python/hosted-objects.md` -- "Presentation", "Calls are synchronous".
- `plan/phase/0003-python/security.md` -- "Agents are not isolated from each other, and it does
  not matter"; `plan/phase/0003-python/agent-placement.md` -- "What agents share regardless".
- `plan/phase/0003-python/boundary-policy.md` -- "Narrow bindings for consequential actions".
- `crates/outrig/src/python/interpreter.py` (`_inputs`, `_hosted_name`, `_Instance`, `Kernel`,
  `_bindings`, `_open`), `crates/outrig/src/python/outrig/_kernel.py`,
  `crates/outrig/src/agent/orientation.rs` (`child_preamble`, `hosted`).
