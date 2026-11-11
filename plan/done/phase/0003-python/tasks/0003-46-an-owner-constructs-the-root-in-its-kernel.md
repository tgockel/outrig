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
for a spawn waiting in `_spawning` (`crates/outrig/src/python/outrig/_interpreter.py:6267-6275`);
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
  `_Instance._release` waits for the launch task (`_interpreter.py:5216-5218`); `ready()` then
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
  `crates/outrig/src/python/outrig/_interpreter.py` (`_open`, `_Instance`),
  `crates/outrig/src/python/outrig/_agents.py`.

## Decisions

Forks 1 through 4 and 7 through 9 were taken as recommended; fork 5 is not built and fork 6 is
recorded and not built. Both forms of fork 1 landed on one branch: the maintainer chose that over
the split `plan/todo/README.md` allowed, since the builder form is a thin layer over the
Session form.

- **"No code of the program's ever crosses" stands.** `agent-classes.md`'s sentence and
  `0003-35`'s "never code, pickle or a reference" hold for a class a program declares and does not
  provision: that path is unchanged, its instance stays in the program, and its
  `agent.instance.started.self` is `false`. For a class the application code holds, the code is
  provisioned by OutRig (`0003-43`) and named, never sent: what the owner sends is
  `module:qualname`, JSON, facilities' bound names, alias names and a model's name.
- **The construction site is the root's own kernel**, beside `0003-35`'s declaration-as-data path.
  The host opens the kernel with a construct record (`{"construct": <name>, "model": ...}` in
  `open`'s `child`), then sends `construct`; the kernel answers `constructed` with the fields a
  spawn line carries of any instance -- `prompt`, `manifest`, `facilities`, `channels`,
  `instance` -- or `{code, message, traceback}`. `make` merges the answer into the spawn line, so
  everything after it is the code every instance's launch runs.
- **The Rust owner speaks `0003-35`'s owner lines in-process.** `harness::root::Owner` attaches
  through `Children::attach_owner` on a session's first construction and sends `agent.spawn`
  (with the construct record), `agent.release`, `ask` and `withdraw` through
  `Children::owner_line`, hearing `spawned`, `released`, `oriented`, `agent.closed` and each
  request's steps, as `_host.py`'s `_Root` and `_Request` do. Reused whole: admission,
  `children-max`, the `owner` node at depth 0, the start-and-release-once bookkeeping, the
  close's release of each root, the report's requests and the request routes. Built here: the
  construct record and route, the kernel's judging of a constructed root's replies, the
  `channels` and `failure` the `spawned` answer now carries, and `requests-max` counted per root in
  Rust, as the program counts it.
- **The owner holds a handle, never the object.** `RootHandle` (`id`, `agent`, `ready`,
  `release`, `channels`, `request`) and `RequestHandle` (`id`, `outcome`, `cancel`).
  Every launch failure is `ready()`'s, as `rust-owner.md` lists them; `Session::construct`
  returns `Err` only for a session with a primary agent, `SessionError::Primary`, whose owner is
  the user at its `user` channel. A handle's outcomes are `RootError`'s variants, one per way the
  engine ends a request or a launch.
- **The reply is judged in the root's kernel** (fork 5's "the kernel's check is the one check"):
  a constructed root's owner request takes `_answered`, as a parent kernel's does, so repair,
  `agent.request.invalid` and `CompletionRejected` after three attempts are the existing ones, and
  `agent.ended` carries the accepted reply as JSON text under `json` (not `value`, which
  `_Work.end` takes itself). A rejected request's `last` is the reply's JSON form where it has one.
  The flag that decides it is named for what it governs, `_Child.owner_judges`, which `_rooted`
  sets for a program's root; a Rust owner's verdict (fork 5) would set it too.
- **OR-A1-1, by a construction-site flag** (fork 2). `0003-41` set no context variable ("Fork 2:
  allowed ... there is no guard and no context variable"), so this task adds one,
  `_agents._SITE`: the construct route sets it to `"owner"` around its one class call, and
  `_Attaching.__call__` steps it to `"root"` for that call's extent and to `None` for any call made
  inside it, so an `Agent()` constructed in the root's `__init__` or hook is a child and keeps the
  refusal. `Agent.__init__` skips the refusal only for `"root"`; `_attach` reads the same site to
  attach the instance as `_Adopted` rather than `_Instance`.
- **`_Adopted`, beside `_Instance`.** Both derive from `_Attached`, which holds what attaching
  checks and keeps -- the inputs, classes and facilities, the aliases and the boot names --
  while the launch is `_Instance`'s alone. `_Adopted` takes the root kernel's own record.
  `ready()`, `release()` and its request methods raise `TypeError` in the kernel, since each is the
  owner's. It is not in `_instances`: the close releases the root through `_children`, as a
  program's root, so it is said once.
- **The kernel adopts the instance.** `Kernel.__init__`'s channel and runtime setup moved into
  `_equip`, which `adopt` runs again once the class has given the record its channels; the view is
  offered (`_offer_view`) only then, since the instance's mapping is the view and exists only once
  `__init__` has run; inputs, classes, `self` and the aliases are bound by `Kernel.bind`, which
  `_open` uses for a spawned child too, and the channels are described by `_Channel.said`, which
  the spawn line uses too. During construction the kernel's view is the session's, the owner's.
- **Fork 8: construction runs on the kernel's loop in a context of its own**, an `_Execution` made
  `reported`, with no pipe and no id: hosted and service calls from `__init__` are
  the root's (they already took `exec: null`), and what it prints goes to the kernel's backlog,
  which the root's first result carries. `Background.id` -- crate-private and the public event's --
  is therefore `Option<ExecId>`, `None` for the construction; the model reads it as "output from
  your construction, before your first round", and the CLI and `render-session.py` say so too.
- **No bound on a construction.** It runs the application's code, which ends when it does; the
  session's close is the bound. The interpreter's close answers each construction still in flight
  `closing`, as it ends each launch and each instance not yet ready, and a construction's own
  answer after that is not sent: its kernel is ended, `ready()` fails `Closed`, and
  `agent.construct.failed` says `closing`. The interpreter's exit, which answers nothing, ends the
  wait the same way. `lifecycle.md`'s *Constructing* row says "at the
  close" rather than "at terminating", since a construction that ended after the close could
  never start its root -- `make` now also refuses a root's start once `state.closed` is set, which
  covers a construction whose answer and the close cross.
- **The model is the owner's.** A root resolves with no `[agents.<name>]` profile, so neither a
  profile nor `default-agent` applies, and the preamble was dropped already. A `model=` that
  `__init__` gives `outrig.Agent` other than `None` or the spec's own raises `TypeError` naming
  both, rather than being dropped silently, as `0003-41`'s fork 1 refused to drop one.
- **Fork 3: no default alias.** `ConstructSpec::alias` names each; `_Instance.alias` (`0003-45`'s
  mechanism) is its first caller, and an alias naming no argument is refused.
- **Fork 7: each argument tagged** `data` (JSON text, with an optional `type`), `facility`
  (`{kind, name}`) or `facilities` (a dict of those). The Rust `Argument` enum cannot make an
  untagged one; the kernel refuses one anyway, naming it, which an interpreter test drives.
  `Facility` is `Service(name)` or `Binding(name)`, `#[non_exhaustive]` for `0003-51`'s channels.
- **`agent.construct.failed`** is session-level, as `application.imported` is: no child started,
  so no agent's section holds it. Its `code`s are the import's three, `class-missing`,
  `not-an-agent-class`, `invalid-argument`, `raised` and `closing`.
- **The builder form** (`SessionBuilder::construct`) constructs in `assemble` after provisioning,
  where the primary would be built; a root not launched fails the start through
  `Session::abandon`, as an application module that does not import does, with
  `SessionError::Construct { class, error }`, `error` the `RootError` its `ready()` would give,
  boxed as the largest variant. `check()` and both start paths resolve the
  spec's model and keys first and refuse a session with a primary. `Session::constructed` is its
  handle; `Session::root(id)` finds any live root by instance id (fork 4).
- **Until `0003-47`** the root's instructions are an instance child's, with its `self`
  sentence, and carry no skill catalog; `0003-47` gives the root its own lead and the catalog.
- **Fork 6, Python-host parity**: `session.construct` over the owner protocol, a handle whose
  replies decode into classes rebuilt from the kernel's declaration, and the reads as protocol
  requests. Nothing here forecloses it: the construct record is a line the protocol could carry.
- **`help(outrig.Agent)`** gained one sentence, that a session's owner can construct an agent as
  a root where it is `self` and needs no marked method; it still fits `help()`'s bound.
- **A pre-existing flake** found while running the suite, in `0003-45`'s
  `a_dropped_instance_lives_until_it_is_released`, also fails on the base commit:
  `plan/next/a-dropped-handles-notice-races-the-reply.md`.
- **`/simplify` changed the first build**, from four reviews: the interpreter's close answers a
  construction in place of a registry watch and a cancellation parameter only this query had; the
  base class split, `Kernel.bind`, `_Channel.said` and `_Execution`'s `reported` path replace
  copies; the failure's typed `SessionError` replaces an `InterpreterError` variant nothing in the
  interpreter raised; one map of open requests, keyed roots, and `RequestHandle::is_done` and
  `ConstructSpec::model` dropped; a spawn's answer carries `channels` and `failure` only for a
  constructed root. Not adopted: sending a data argument and a reply as JSON values rather than
  text, which keeps the line format the program's path uses.
- **A review of the branch found five lifecycle and data faults, each fixed where it arose:**
  - *A failed launch orphaned what it launched.* Only a root's constructor runs code before its
    launch is decided, so only it can launch an agent of its own first. A launch that fails now
    marks its node released -- so a launch it asked for and that has not begun never does -- and
    releases each child it began, before the node goes; this holds for any launch.
  - *A dropped `construct()` stranded its root.* The registry's spawn runs on whatever the caller
    does, so a root could launch with nothing holding it. The owner now releases a root whose
    spawn nobody waits for, as `_release_unheld` releases a spawn a kernel's code gave up on.
  - *A dropped `release()` skipped its bookkeeping.* The release is now a task of its own;
    `release()` begins it once and waits for it, so a later call waits for the first to finish.
  - *An accepted reply could outgrow its line.* The host drops a line past its bound, and the
    kernel had already accepted the reply. A constructed root's reply notice is now measured
    against `MESSAGE_MAX`, as a program's root's reply line is, before it is accepted, and one
    past it raises `ValueError` from `d.reply()`, saying to reply with less.
  - *An integer past 64 bits was rounded*, and one past `f64`'s range read as `null`, since the
    workspace's `serde_json` has no `arbitrary_precision`. `outcome()` now gives the reply as
    the JSON text the kernel wrote, which the embedder deserializes into a type of its own, as
    `rust-owner.md` has it; `RootError::Rejected::last` is text too. Turning on
    `arbitrary_precision` would change every `serde_json::Value` in the workspace.
- **A second review found three more, each fixed:**
  - *A `construct()` dropped after its answer arrived, unread, stranded its root.* The answer
    sat in the dropped channel, and the first fix caught only an answer that arrived after the
    drop. The waiter's end of the channel is now a guard (`Awaited`): dropped unread, it closes
    the channel, takes an answer already there, and releases the root it launched, through the
    same `Owner::unheld`.
  - *A rejected reply could outgrow its line.* The bound covered only an accepted reply, and a
    rejection carries the last reply. It now covers every reply to the owner before it counts:
    one past it raises the same `ValueError` whether or not it would be accepted, and counts no
    attempt, so `Rejected::last` is always a reply that fit.
  - *A failed launch's cleanup could leave a child running.* It handed every child the node had
    ever launched to the strict release, which refuses the whole list if one is released
    already. Now one lock marks the node and every child it still holds, with everything below
    each, using the same walk a release uses (`State::mark_released`), which passes over those
    released already. This one has no test. A nested launch begins only after the construction
    answers, the kernel's end follows that answer at once and cancels the constructor's
    background work, and an instance's `release()` from an ended kernel does nothing, so no
    test can release one child before the cleanup runs.
- **A third review found the rejection could still outgrow its line** through what was wrong
  rather than the reply: a problem's path quotes each key whole, so twenty problems under one
  key of 850,000 characters made a 17 MB notice from a reply that fit. `outrig.schema` bounded
  how many problems it lists but not their text. It now shortens a path or a message past 1,024
  characters in its middle, keeping where the path ends, so every line problems ride on -- the
  rejection, `agent.answered`, a `DecodeError`'s listing -- stays bounded: at most 20 of about
  2 KiB each, beside a reply of at most 1 MiB. Measuring the whole rejection before it counts,
  the review's other suggestion, would refuse a reply for the decoder's wording, which the agent
  cannot shorten.
