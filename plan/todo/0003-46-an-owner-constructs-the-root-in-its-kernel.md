# 0003-46 -- An owner constructs the root in its kernel

## Context

The first application is a Rust embedder of `outrig::harness` with no Python host program
(A-00's first decision): its facilities reach agent Python as services (`services.md`,
`0003-44`), its Python package is application code (`application-code.md`, `0003-43`), and it
constructs one task agent per session and holds a control handle (`rust-owner.md`). What exists
for an owner today: `SessionBuilder` with `no_primary`, `container`, `secrets`, `bindings_env`,
`bind`, `skill_source`, `subscribe` and `check` (`crates/outrig/public-api.txt:1833-1847`;
`crates/outrig/src/harness/builder.rs:160-174`, `:312-319`), and `Session` with `round`, which
answers `NoPrimary` for such a session (`crates/outrig/src/harness/session.rs:350-353`), and no
operation that reaches an agent (`public-api.txt:1817-1830`). A root node at depth 1 is made by
`Children::root` for the primary (`crates/outrig/src/agent/children.rs:251-264`); `launch` checks
admission, the depth and `children-max` and mints an id (`:391-501`); `make` resolves the model
on the session's `Config`, drops the profile's preamble, opens the kernel, builds the preamble
and the agent and starts the runner (`:506-662`, `:518-532`). `_open` opens a child's kernel only
for a spawn waiting in `_spawning` (`crates/outrig/src/python/interpreter.py:6267-6275`);
`Agent.__init__` refuses a class with no `@outrig.agent` channel
(`crates/outrig/src/python/outrig/_agents.py:533-535`). `0003-41` runs construction before
attachment, `0003-42` makes a facility a reference an owner can name, `0003-45` binds a
constructed instance as its child's `self`.

`0003-35`, queued, gives a Python host program an agent class it declares in its own process: the
declaration crosses as data, "never code, pickle or a reference", the kernel rebuilds the types with
`make_dataclass`, the instance is a root child whose parent is the owner, the program's handles are
futures of its loop, replies decode into the program's classes, and the acceptance handshake orders
a verdict against a cancel and the close (`agent-classes.md`, "From the host program"). Such a child
has no `self`. The phase README's Tasks paragraph keeps `0003-35` and `0003-36` as queued, the
phase's Python-host deliverables, which another program depends on; they are not prerequisites of
the Rust path, and their parity with this task's operations is an open choice.

OR-A2-1 through OR-A2-5 ask: a builder or Session operation that takes an importable class
reference into the application code, constructor inputs -- data, and facility bindings named by
their bound names (OR-A1-2) -- and a model, and constructs the instance inside a kernel of a
`no_primary` Session as its primary with the owner as parent endpoint; a control handle, not the
Python object, with readiness as `0003-34`'s `ready()` reports it, release and the reads of A-06;
rounds through the instance's own runner, never `Session::round()` (CC-AGT-10); a constructor or
hook that raises yielding a handle whose readiness fails with the exception rendered, no model
call and a clean Session; a release while launching waiting for the spawn's answer; `LaunchSpec`,
`Secrets`, `bindings_env`, skill sources and subscriptions applying as to any session; model
selection by configured model or alias from the embedder's in-memory `Config`, with
`[agents.<name>]`, `default-agent` and a profile's image or preamble never applying; and typed
channels optional, owner-decoded when present. OR-A1-1 asks that a primary the owner constructs
may declare no channel; OR-A1-4 for an optional `task` alias that is the same supplied reference;
OR-A1-5 for one documented order of docstring, contributions, opening and first delivery
(`0003-47`; `external-channels.md`). OR-AG1 and OR-AG6 stand behind them. The validation found the
Rust path and `0003-35` to be two construction paths that share their kernel-side plumbing, and
the conflict check's tension 1 -- if the owner's process constructed the instance, `self` could
only be a proxy -- is resolved by constructing in the kernel.

**What this task records against `0003-35`.** `agent-classes.md`, "From the host program": "the
types reach the child as descriptions and are rebuilt with `make_dataclass`, since no code of the
program's ever crosses"; and `0003-35`'s deliverable "The declaration crosses as data ... never
code, pickle or a reference". Both stand for a class not provisioned. For a provisioned class the
construction site is the kernel, and "no code of the program's ever crosses" stays true because
the code is provisioned by OutRig, not sent by the owner. `task-agents.md`, "The owner-constructed
root", and `rust-owner.md` record the design.

`0003-47`, which follows, puts the instruction hook's sections on the kernel's answer this task
defines; until it lands the answer carries the declaration alone. `0003-51` gives a root the
external channels through which it is delivered to; until it lands a declared channel is the one
way an arrival opens a round.

## Goal

A Rust owner constructs a task agent in a kernel of a `no_primary` Session from a class the
application code holds, with data arguments and facility bindings named by their bound names,
and holds a control handle -- readiness, release, requests when the class declares channels, and
the reads later tasks add -- never the Python object; the instance's own runner opens its rounds.

## Deliverables

- **The operation, in two forms** (fork 1). `SessionBuilder::construct(ConstructSpec)`: the
  instance is constructed as the session starts, after the application code is provisioned, and
  `check()` resolves its model and every `${VAR}` before any container exists.
  `Session::construct(ConstructSpec) -> Result<RootHandle, SessionError>`: a root constructed on
  a started session. `ConstructSpec { class: "module:qualname", args, aliases, model }`; each
  argument is `Data { json, type: Option<String> }`, `Facility { kind, name }` or
  `Facilities(map)` (fork 7); `aliases` names arguments to bind as kernel globals (OR-A1-4);
  `model` is a configured model or alias. Public API additions; `crates/outrig/public-api.txt`.
- **The control handle**, `RootHandle` (`rust-owner.md`, "The control handle"): `id()`,
  `ready()`, `release()`, `channels()`, and `request(channel, json) -> RequestHandle` when the
  class declares channels, whose outcome is the reply as JSON typed by the channel's schema text
  or `work.md`'s documented error, one terminal outcome per request. Reachable from `Session` by
  instance id per fork 4. The reads are `0003-48`'s and `0003-49`'s.
- **The engine's half.** Admission, `children-max` and the model resolved as `launch` and `make`
  do, on the in-memory `Config`, with `[agents.<name>]`, `default-agent` and a profile's image or
  preamble never applying; a root node at depth 1 with parent `owner` (`0003-35`'s fork 3); a
  root kernel opened with a construct record in place of a waiting spawn; the kernel told to
  construct; its answer composed into the orientation (`0003-47`), `agent.instance.started` with
  `parent` `owner`, `self` true and `channels` possibly empty, the runner started and the
  orienting round opened; the handle returned. A failure ends the kernel, frees its place, records
  `agent.construct.failed`, and makes `ready()` fail with the exception rendered and a bounded
  traceback; no model call is made, and the session's shutdown reports `Clean`.
- **The kernel's half.** On its own thread, in the context fork 8 decides: import the class
  (provisioned by `0003-43`; a module not yet imported imports now), decode the arguments --
  data strictly into the named class when one is named, each `Facility` into a reference by kind
  and name (`0003-42`), refusing a value that is neither -- make the alias bindings, and call the
  class, so `__init__`, the attach hook, attachment and the instruction hook run there (`0003-41`,
  `0003-47`) and `self` and the aliases are bound (`0003-45`). `_Instance` is made in adopted
  form: no spawn, the kernel's own record, the owner as the parent endpoint, `ready()` settled by
  the orienting round's end as `oriented` settles it today. The answer carries the declaration
  when the class has channels and, from `0003-47` on, the sections; a failure carries `{code,
  message, traceback}`.
- **A channel-less root is constructed** (OR-A1-1): the refusal at `_agents.py:533-535` does not
  apply to an instance the owner constructs as a root, by fork 2's mechanism, and keeps applying
  to an instance a kernel constructs as a child. A channel-less root's record has no channels and
  holds no requests, so `0003-37`'s rule has nothing to count on it, and its liveness obligations
  come from reply-owed requests on external channels (`external-channels.md`, "The liveness rule
  covers an inbound request channel on a runner-driven holder").
- **The owner never drives rounds** (CC-AGT-10): `Session::round()` stays `NoPrimary`; rounds
  open through the instance's runner on the orienting round and on arrivals, and the orienting
  round requires no request or delivery to exist (OR-A1-5).
- **Release while launching** waits for the construction's answer, then releases, as
  `_Instance._release` waits for the launch task (`interpreter.py:5216-5218`); `ready()` then
  raises `AgentReleased`.
- **Owner requests** (OR-A2-5). The kernel-side half -- a host-originated request into a root's
  channel, the reply crossing to the host as JSON after the kernel checked it against the declared
  type as a parent kernel's reply is checked today, one terminal outcome per request, cancel and
  release -- is `0003-35`'s plumbing, reused when it has landed and built here when it has not.
  The owner receives JSON typed by the schema text and deserializes with serde; an application
  verdict through the acceptance handshake is fork 5.
- **Per-session inputs apply** (OR-A2-3): `LaunchSpec`, `Secrets`, `bindings_env`, skill sources and
  subscriptions as for any session; nothing here changes `no_primary`'s rule. The root is the
  application's primary, so its instructions carry the skill catalog as the primary's preamble does,
  under `0003-32`'s bound, composed by `0003-47`; `skills.md`'s "a child's instructions do not list
  the catalog" was decided for children under a primary and stands for them. A `/name` directive
  reaches the root as an item on an external channel (`external-channels.md`, "A directive crosses
  as an item"; `0003-51`).
- **Python-host parity is an open choice**, fork 6, recorded and not built: `0003-35`'s shape is
  unchanged, and a class the program declares and does not provision constructs as `0003-35`
  builds it, with `agent.instance.started.self` false.
- **Events and the close table.** `agent.construct.failed` -- `class`, `module`, `qualname`,
  `code`, `reason` -- an execution diagnostic; `agent.instance.started` as above, its `channels`
  possibly empty; `doc/reference/events.md` says both. In `lifecycle.md`'s close table the
  **Root agents** rows gain *Constructing*: the owner asked and the kernel has not answered.
  Outcome: the construction's task is interrupted with its kernel at terminating; `ready()` fails
  with the closing error; `agent.construct.failed` with reason `closing`. Invoked: not a boundary
  request. Effects: what `__init__` did in the container stands.
- **`## Decisions` records**: that "no code of the program's ever crosses" stands and why; the
  construction site in the kernel beside `0003-35`'s declaration-as-data path, and that a class
  not provisioned keeps that path with no `self`; that the owner holds a handle and never the
  object; OR-A1-1's rule for each construction site; the alias default; forks 1 through 9; and
  what of `0003-35`'s kernel-side plumbing was reused or built.

## Acceptance

Each is a Rust test with a mock model unless it says otherwise; no Python host program is used.

- **OR-A2-1.** A `no_primary` session built with `ApplicationSpec` (`0003-43`), a `task` service
  bound by name (`0003-44`) and a binding `files`; `construct` names `review.agents:Reviewer`
  with `args: task -> Facility { service, "task" }, facilities -> Facilities { files: binding }`,
  `aliases: ["task"]` and `model: "fast"`: `ready().await` resolves after the orienting round;
  `agent.instance.started` names the class, `owner` as `parent` and `self` true; the round's
  code calls `self._task.update_progress("x")` as a service call attributed to the root, and
  `task is self._task` holds.
- **OR-A1-1.** A class with a docstring, an `__init__`, ordinary methods and no marked method is
  constructed as a root and reaches `ready()`; the same class constructed inside a kernel as a
  child is refused with `_agents.py`'s message.
- **OR-A2-2.** A constructor that raises: `ready()` fails with the exception rendered;
  `agent.construct.failed` is recorded and `agent.instance.started` is not; no model call; the
  shutdown report is `Clean`; with `children-max = 1` a second `construct` succeeds, so the place
  was freed. `release()` called while the instance is launching waits for the construction's
  answer, releases, and `ready()` raises `AgentReleased`.
- **OR-A2-3.** Two sessions in one process with different images, different `Secrets` and
  different models run two roots, neither seeing the other's key or image; `check()` fails before
  any container when the spec's model is unknown or a `${VAR}` cannot resolve; an
  `[agents.<name>]` profile sharing the class's name, with its own model and preamble, changes
  nothing the root receives.
- **OR-A2-5.** With one declared channel, `request("review", json!("src/main.py"))` resolves to
  JSON the test deserializes with serde into its own struct; a reply the mock gives in a wrong
  shape is refused in the kernel (`agent.request.invalid`) and repaired without rerunning the
  child's earlier executions; a third wrong reply settles `CompletionRejected`. With no channel,
  `channels()` is empty and construction is the same.
- **Rounds.** `Session::round()` returns `NoPrimary` throughout; a request on a declared channel
  opens a round through the runner; nothing the owner does opens one directly.
- **Refusals.** `"review.agents:Missing"` and `"nowhere:Thing"` fail `ready()` naming the class
  or module; a `Facility` naming a service or binding the session lacks is refused naming it; an
  argument whose JSON fails its named class's schema is refused with the problems' paths; an
  argument that is neither data nor a facility is refused naming it.
- **Release and exit.** `release()` ends the child and settles its open requests `AgentReleased`;
  an instance alive at session exit is released by the close and `ShutdownReport::requests` lists
  its open requests; `construct` after `close_admission` is refused `closed`.
- `0003-35`'s tests pass unchanged, and its instance's `agent.instance.started.self` is `false`.
- e2e, with a real model, gated as `0003-35`'s: a Rust test provisions a small application whose
  class answers "What does this project do?" about this repository through a declared channel,
  and the shutdown report is `Clean`.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Builder form, Session form, or both -- Recommended: both.** The builder's spec is the
   primary's: constructed as the session starts, so the session is never without it and `check()`
   covers its model; `Session::construct` serves a root constructed later. The alternative offers
   one form, and the builder-only form cannot construct a second root.
2. **How OR-A1-1's exception is made -- Recommended: a construction-site flag.** The construct
   record marks the construction a root's, and `Agent.__init__`'s check reads the mark through
   the context variable `0003-41` sets around the call; a kernel-constructed child keeps the
   refusal. The alternative is a subclass marker that also lets a kernel child declare no channel
   under a documented rule, which gives a child no way to be given work.
3. **Alias default -- Recommended: none.** An argument is bound as a global only when `aliases`
   names it. The alternative binds every `Facility` argument by its name, which puts names in the
   kernel the application did not ask for.
4. **The handle reachable from `Session` by instance id -- Recommended: yes**, since the roster
   (`0003-49`) names ids and an owner holding several roots needs the handle by id.
5. **An application verdict through `0003-35`'s acceptance handshake -- Open; recommended: not
   here.** The kernel's check against the declared type is the one check; a verdict the owner
   gives before the request settles is the handshake generalized, a later task's.
6. **Python-host parity -- Open; not built.** `session.construct` over the owner protocol for
   `0003-35`'s program, with replies decoded into classes rebuilt with `make_dataclass` from the
   kernel's declaration, and the reads as protocol requests. Nothing here forecloses it.
7. **Argument encoding -- Recommended: each top-level argument tagged `Data`, `Facility` or
   `Facilities`; references nowhere deeper.** The alternative allows a reference anywhere in a
   data tree under a reserved key, which collides with legal `str` keys of data.
8. **Where construction runs in the kernel -- Recommended: a task on the kernel's loop in a
   context of its own, outside the execution slot, with output billed to the kernel's backlog as
   a background task's is.** The alternative runs it as a synthetic execution with source OutRig
   wrote, which would stand in the model's history as something it did not write.
9. **`ready()` -- Recommended: settles when the orienting round ends**, as a kernel caller's
   does; `construct` returns the handle as soon as the kernel answered. The alternative has
   `construct` itself await orientation, a model round before the owner has a handle to release.

## Dependencies

- **Hard: `0003-43`.** The class is imported from provisioned code.
- **Hard: `0003-45`.** `self` and the aliases in the root's kernel.
- **Hard: `0003-44`.** A `task` facility is a bound service, named by its bound name.
- **Hard: `0003-42`.** Facility references as arguments, and the root's view.
- **Hard: `0003-41`.** The construction sequence the kernel runs, and the context variable
  fork 2 reads.
- **Hard: `0003-30`.** The request machinery a declared channel uses.
- **Soft: `0003-35`.** The kernel-side request plumbing, reused when landed; the degenerate case;
  the parity fork.
- **Soft: `0003-36`.** A live object the owner injects as a facility of the root.

## See also

- `plan/phase/0003-python/rust-owner.md` -- "Construction from Rust", "The control handle".
- `plan/phase/0003-python/task-agents.md` -- "The owner-constructed root".
- `plan/phase/0003-python/application-code.md`, `facilities.md`, `services.md`.
- `plan/phase/0003-python/agent-classes.md` -- "From the host program";
  `plan/phase/0003-python/typed-agents.md` -- "Declared on the host".
- `plan/phase/0003-python/embedding.md` -- "What the embedder supplies", "A Python program as
  owner"; `plan/phase/0003-python/lifecycle.md` -- the **Root agents** rows.
- `plan/todo/0003-35-a-host-program-drives-an-agent-class.md` -- the plumbing reused or built.
- `crates/outrig/src/harness/builder.rs`, `session.rs`, `crates/outrig/src/agent/children.rs`,
  `crates/outrig/src/python/interpreter.py` (`_open`, `_Instance`),
  `crates/outrig/src/python/outrig/_agents.py`.
