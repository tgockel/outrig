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

## Decisions

Forks 1-3 were taken as recommended. What no doc decided is recorded here.

- **Two builds; the smaller was kept.** `/simplify` compared the task's first build with one
  written from the same requirements. The first added about 1,340 lines of logic: an owner
  subsystem of its own, with request state kept in three places. The second added about 610, with
  the program speaking the kernel's own lines, but it missed six requirements. The branch keeps
  the second, with those six fixed, and with the defects two reviews of it found fixed too.
- **The program speaks for its instances as a kernel's code does.** It sends the registry the
  lines a kernel sends -- `agent.spawn`, `agent.release`, `agent.instance.collected`,
  `.released` and `.handle.collected` -- as the parent `owner`, and hears what a kernel would be
  told. Launching, releasing, the limits and the events are therefore the registry's existing
  code. A kernel has no use for the three request lines, `ask`, `withdraw` and `verdict`; they go
  on to the interpreter, whose routes for them address no kernel.
- **The program decodes; the child's kernel decides.** A request the program makes is a `_Work`
  in the interpreter, whose sender stands for the program. The lock that orders a kernel's
  answer, cancel, release and close therefore orders the program's verdicts too.
  - The child's `reply(value)` sends the value's JSON form, numbered, and waits for the
    program's verdict on that number.
  - A value with no JSON form is found wrong in the kernel and never sent.
  - A verdict names its reply, so one on a reply whose wait was cancelled is never taken for the
    next. The interpreter reads the reply number, and the future waiting for that verdict, under
    the lock they are set under, and counts an attempt only when the verdict on it is taken.
  - Each problem a verdict carries is cut, and a lone surrogate in it, which no UTF-8 line can
    carry, is escaped, so the verdict always reaches the child.
  - The handle ends only as the kernel says (`agent.ended`), except for a request never sent,
    which ends in the program. An accepted request ends with the value the program decoded from
    the reply the kernel accepted. That reply's own call passes its number to the end, so another
    reply the program accepted meanwhile, on another thread, is never given in its place. A
    `rejected` one ends with the last reply's JSON form and problems, as the kernel sends them.
- **`cancel()` waits for the child's kernel.**
  - A request never sent ends in the program, and is never sent.
  - A request already sent is withdrawn in the kernel, which may have ended it another way first.
    `cancel()` blocks its caller, often the program's loop, until the kernel's end arrives, for
    `CANCEL_WAIT` (10 seconds) at most. It then returns whether that end was the cancel; only the
    program cancels its requests. With no answer in that time, it returns no.
  - The connection's reading thread takes a request's end and records it under the request's
    lock. It leaves the future, and letting go of the instance, to the loop. None of the
    program's code runs on that thread.
  - One lock per request orders its own line and a cancel's, so a `withdraw` from another thread
    never reaches the interpreter before the `ask` it withdraws.
- **Owner protocol, version 2.** The kinds are `owner/mod.rs`'s.
  - Lines are up to 1 MiB both ways, the same bound as a kernel's own spawn line
    (`MESSAGE_MAX`).
  - The program encodes with `allow_nan=False`, so a line the engine could not read raises before
    it is sent.
  - A reply, a request's body, a rejected reply and the description a root child is made from
    cross as JSON text inside the line. The engine parses a line into JSON values, which sort an
    object's keys and hold an integer past 64 bits as a float. As text, each dict keeps the order
    it was made in, as between kernels, and each integer stays the integer it was.
- **Nothing the program awaits is left waiting.**
  - The registry's close pushes `agent.closed`, with `terminated` when the interpreter exited
    first. That ends each open request `terminated` (`SessionClosing`) and each instance's
    `ready()`.
  - The interpreter's exit is pushed whenever it comes, `terminated`: after an earlier close, the
    requests that close left to the interpreter would otherwise wait for good.
  - The session's exit does the same for anything left.
  - The program's launches and releases still running at the shutdown are awaited before the
    program stops hearing the session, so their answers come before the shutdown's.
  - An `ask` the interpreter cannot serve ends at once: `failed` if no child of the program's has
    that id or its body does not decode, and `closed` if it comes once admission has closed. One
    whose route ran out of memory part way through is ended `failed` by the route's second try,
    rather than left unsent or sent twice.
- **Instance ids are the interpreter's.** A root child's kernel takes its instance id from the
  counter every instance and request there takes, and says it on its `ready` line. The spawn's
  answer carries the id to the program, which names the instance by it in what it sends. What the
  program is told of an instance (`oriented`, `child`) names the program's own number for it,
  which it has before the launch is answered.
- **An instance's start and its release are each recorded once.** For each root child, the
  registry records under its lock whether the start and the release were recorded.
  - The engine records the program's release as it makes it, `how` coming on `agent.release`
    with a collection recorded first, rather than on a notice the program would send after the
    answer, which a shutdown finishing in between would drop.
  - The interpreter's close may say the release too; the registry records it once.
  - A release that reaches the engine once admission has closed releases nothing and is
    answered at once: the instance is the close's to release, as a kernel's is.
  - A release the close said before the start was recorded keeps the launch from starting.
  - A root child's kernel does not open once the close has begun, and that refusal is `closed`.

  So a release racing the close, or a launch the close meets, records both a start and a release,
  or neither.
- **`requests-max` is counted in the program, and requests are held there.** A call raises
  `AgentLimitReached` at the call, as in a kernel, counting the instance's open requests against
  the session's `requests-max`, which `session.start`'s answer and the spawn's answer carry. A
  request made before the launch is answered waits in the program, and is sent in order once it
  is.
- **`Handle.id` is `None` until the child's kernel has the request.** The id is the
  interpreter's, and it reaches the program with the request's first step.
- **`session=` is a keyword of every `outrig.Agent`.** A kernel's constructor refuses it with
  `TypeError`, because the instance runs in the kernel's own session. So no input can be named
  `session`, just as none can be named `model`.
- **The owner in the tree.** As the session starts for the program, the registry adds `owner` to
  its tree at depth 0, on `default-model`, with a call slot of its own. A root child's depth,
  `children-max`, places, budget and spend are therefore a child's. A launch that names no model,
  in a configuration with no `default-model`, is refused at `ready()` with `ValueError`, as an
  unknown model is.
- **One `Handle`.** The kernel's `Handle`, and the future view it gives `asyncio.wait`, moved
  from the interpreter into `outrig/_agents.py`, and the program's requests use them too. The
  interpreter's `-c` argument is 120,522 of its 131,072 bytes, 1,848 more than before the task.
- **Types cross as descriptions.**
  - Each class a root child needs crosses by name as its fields (type, `kw_only`, default), with
    its `frozen` and `__qualname__`.
  - The child's kernel makes the classes with `make_dataclass`, and types their fields once every
    class exists, so a type that names itself can cross.
  - Every default crosses as its value, tagged as an input is, and is made again for each
    instance. A mutable or dataclass default is therefore never shared, and a dataclass default is
    an instance in the child.
  - A `default_factory` is called when the class is described, at each construction. If it
    raises, or its value is not of its field's type, the constructor raises a `TypeError` naming
    the field.
  - An input crosses tagged where its lists, dicts and dataclass instances are, an instance's
    fields included, and arrives as instances of the classes made again. Each instance is made of
    the class it was, not decoded from its JSON form, which cannot say which member of a union it
    is when the members share their fields.
- **The registry takes the host's table lock only without its state lock.** The host's reader
  holds its table lock while it hands the registry a line, which takes the registry's state lock.
  `tell_life` held the state lock across `Interpreter::tell`, and `launch` across reading the
  lifecycle, each of which takes the table lock, so each could wait on the reader for good. Both
  were there before this task; fixed here, because the program's launches and requests reach
  both. `tell_life` now reads a child's life and tells its parent under a lock of its own, which
  the reader never takes, so the last line told is still the latest.
- **The real-model e2e reads the maintainer's configuration.** It runs with
  `OUTRIG_E2E_REAL_API=1`, and `OUTRIG_E2E_GLOBAL_CONFIG` naming a global configuration that
  declares the model. The program mounts this repository read-only with `outrig.Mount` and passes
  its path as an input, because the workspace mount is read-write and a real model's code could
  write to it.
