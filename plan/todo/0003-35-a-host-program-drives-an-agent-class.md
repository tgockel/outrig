# 0003-35 -- A host program drives an agent class

## Context

`0003-34` declares `outrig.Agent` for a caller in a kernel: a subclass's methods marked
`@outrig.agent` are request channels, an instance is one long-lived child, and `0003-30`'s request
machinery carries each request and decodes each reply on the parent's side. `0003-26` declares
types and decodes strictly, and `0003-29` gives a host program a session with no primary agent.
The host-program requirements (R4, R5) add the host as a caller: the program declares the class,
constructs it in its session, and the instance is a root child -- a child whose parent is the
owner -- in a session that has no primary. The program's handles are asyncio futures of its own
loop, and R4's decoding runs where the types were declared, in host Python.

Two things cross the boundary that never had to before. The declaration crosses from the program
to the engine and on to the child's kernel: the class docstring, each channel's name and
docstring, and each request and reply type. A type crosses as a description -- fields, their types,
their `doc` and `alias` markers, their defaults -- and never as code, pickle or a reference; the
kernel rebuilds the classes with `make_dataclass`, as `0003-30`'s fork 6 binds a result type, and
the program keeps its own. And the reply crosses back as JSON to be decoded into the program's
class, with the decoder's problems returning to the child's `reply` so it can repair the value
without rerunning anything. The attempt limit, `fail`, cancellation, release and the closing error
are `0003-30`'s; this task moves their parent end onto the owner connection.

R7 settles the configuration: `model=` names a configured model or alias, resolved as a child's
is; omitted, `default-model` applies; `[agents.<name>]`, `default-agent`, a profile's image and
its preamble never apply to a host-declared class, whose docstring is its instructions.

## Goal

A program declares `class ProjectReader(outrig.Agent)` with `@outrig.agent` methods, constructs it
in a session, awaits typed replies decoded into its own classes, repairs invalid ones without
rerunning work, and releases it.

## Deliverables

- **`outrig.Agent` on the host**, with the declaration checks of `0003-34` through the shared
  source (`0003-26`). Construction: `session=` when given, else `outrig.current_session()`, else
  it raises; inputs are checked synchronously against the serializable subset; the launch is
  scheduled on the program's running loop and sent over the owner connection; a constructor with
  no running loop raises.
- **The declaration crosses as data.** The class docstring, the channels with their docstrings,
  the input manifest, and each request and reply type as a declarative description -- fields,
  types, `doc`, `alias`, defaults per fork 1 -- never code, pickle or a reference. The child's
  kernel rebuilds the classes with `make_dataclass` under their names; the program keeps its own.
- **A root child.** The instance's child is spawned by `0003-30`'s machinery with the owner as its
  parent endpoint, through the seam `0003-30` leaves, at depth 1; it counts against
  `children-max`, is bounded by `requests-max`, and its model calls take `model-concurrency-max`
  permits and spend the token budget as any child's do.
- **Readiness, requests and handles.** `await agent.ready()` and `async with` as `0003-34` has
  them. A method call checks its arguments, encodes the body as JSON and returns a host `Handle`:
  awaitable and settling once; `.future`, a shielded `asyncio.Future` on the program's loop that
  `asyncio.wait` takes; `.result()`, `.done()`, `.status` and `.cancel()` with `work.md`'s
  meanings. The connection thread settles futures through `call_soon_threadsafe` (fork 2).
- **Decoding and repair on the host.** The child's `reply(value)` crosses as JSON; the program
  decodes it with `outrig.schema` into its class; problems return through the engine to the
  child's `reply`, which raises them with their JSON paths; the attempt limit, `fail` settling
  `AgentRequestFailed`, cancellation before and after receipt, release settling `AgentReleased`
  and the closing error follow `0003-30`. `outrig.AgentError` and its subclasses exist on the
  host; `ShutdownException` stays outside that hierarchy.
- **The acceptance handshake.** The engine records the program's verdict on a reply before the
  request settles, in the same request state `0003-30` orders a cancel and the close in, so a
  verdict racing a cancel or the close yields one outcome and a request never settles twice.
- **Model and configuration** (R7). `model=` names a configured model or alias and is resolved as
  a child's; omitted, `default-model` applies; `[agents.<name>]`, `default-agent`, a profile's
  image and its preamble never apply; a failure surfaces at `ready()` and the first handle, as
  `0003-34`'s rule has it.
- **Events.** `agent.instance.*` and `agent.request.*` as `0003-34`, with the owner as the parent
  (fork 3); the child's own events under its subject.
- **Session exit** releases a live instance through the close rows; agent contexts belong inside
  the session context, and the orientation of the program's documentation says so.
- The explicit child API -- `runtime.spawn`, `submit`, request channels by hand -- has no host
  counterpart in this phase, stated in `agent-classes.md`.

## Acceptance

- **R5's example against a mock model.** `ProjectReader(model=...)` returns before any model
  call; `handle = agent.explain(...)` returns a handle at once;
  `asyncio.wait({handle.future}, timeout=60)` returns it in `done`; `handle.result()` is the
  reply; `await agent.explain(...)` works in one line; the record shows one spawn; the second
  request's round reads what the first bound.
- **R4's example.** `Description`, with its `Literal` outcome: the mock replies
  `{"outcome": "done", "summary": ...}` first and a valid reply second; one
  `agent.request.invalid` names `$.outcome`; the result is an instance of the program's
  `Description`; the child's earlier executions ran once; a third invalid reply settles
  `CompletionRejected` carrying the last value and its problems.
- **The planned rules hold on the host.** `alias("file-path")` makes `file-path` the key and
  `file_path` an unknown key; a nested dataclass, `dict[str, T]`, a required-but-nullable field
  and a defaulted field behave as `0003-26` says, declared in the program.
- Construction with neither `session=` nor a current session raises; `ProjectReader(session=other)`
  in a nested-session program uses `other`; without a running loop it raises.
- `asyncio.wait` timing out leaves the request pending; cancelling the awaiting task leaves it
  pending and a new await returns the result; `handle.cancel()` cancels the request and the
  instance answers the next one; `async with` exit releases the instance and settles the rest
  `AgentReleased`; a call after release raises `AgentReleased` and sends nothing.
- `explain(1)` on a `str` channel raises `TypeError` with no `agent.request.sent`; an unknown
  `model=` raises at `ready()` with the configured names; with `model=` omitted the mock sees
  `default-model`'s identifier; an `[agents.<name>]` profile sharing the class's name, with its own
  model and preamble, changes nothing the child receives.
- An instance alive at session exit is released by the close, `shutdown_report` lists its open
  requests, and `ShutdownException` is not an `AgentError`.
- `agent.instance.started` names the owner as the parent; the request events are `0003-34`'s.
- e2e, with a real model, gated: R5's program answers "What does this project do?" about this
  repository and exits 0 with a clean report.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Defaults in a type that crosses -- Recommended: a default value in the subset crosses as a
   value; a `default_factory` crosses as the value it produces, evaluated once in the program at
   transfer, and the child's class gets a factory that copies it; a factory whose value is outside
   the subset is a declaration error.** No code crosses either way. The alternative refuses every
   `default_factory`, which refuses `ReviewResult.questions` as `typed-agents.md` declares it.
2. **Where host futures are settled -- Recommended: on the program's loop, through
   `call_soon_threadsafe` from the connection thread**, as a kernel's futures are settled from
   its workers. The alternative drives the connection from the loop itself, which stalls whenever
   the program's loop does.
3. **The parent a root child's events name -- Recommended: `owner`**, a fixed subject beside the
   agent subjects. The alternative mints a synthetic agent id for the program, which would read
   as an agent that ran no rounds.

## Dependencies

- **Hard: 0003-26.** `outrig.schema` on the host, and the package's shared source.
- **Hard: 0003-29.** The session, the owner connection and `current_session()`.
- **Hard: 0003-34.** The class, its declaration checks and its type derivation; through it,
  `0003-30`'s request machinery and `0003-31`'s decorator.

## See also

- `plan/phase/0003-python/agent-classes.md` -- "From the host program".
- `plan/phase/0003-python/typed-agents.md` -- "Declared on the host".
- `plan/phase/0003-python/work.md` -- the handle's rules, which hold on the host.
- `plan/phase/0003-python/lifecycle.md` -- the agent-instance rows.
- `plan/phase/0003-python/embedding.md` -- what a program supplies and drives.
