# 0003-36 -- A host program injects a live object

## Context

A factory binding is built in the bindings process from a declaration: a package to install, a
callable to run, arguments. A host program already holds the objects it wants its agents to use
-- a client of its own service, a repository it opened -- and the host-program requirements (R3)
let it expose one after the session has started: `session.inject("notes", notes)` returns once
every existing agent, and every agent made later, can reach `notes`. The original object stays
in the program's process; agents hold proxies to it and use it with its library's own synchronous
convention (`0003-23`).

That makes a second serving endpoint. One bindings process holds every factory binding
(`0003-22`), and a live object is served from the program's own process, over the owner
connection, by the same serving machinery: `binding.py` holds the interception, the per-kernel
object tables, the serialize turn, the request records and the `published` handshake inside a
process entry point that owns its stdio and signals, and this task extracts them into a module the
program imports. The relay routes each binding to its endpoint; admission, events and the drain
are the same for both.

Two rules come from R3. `outrig.bindings[name]` (`0003-26`) is the authoritative lookup, and a
convenience global of the binding's name is installed only where the name is free; today
`bind_stubs` overwrites a kernel global unconditionally, which this task changes. And a live
object's lifetime is not a process the engine can kill: a call still running in the program's
thread when the session shuts down cannot be ended alone, since a Python thread cannot be killed
and exception injection is unreliable in native code and I/O. The engine reports what it knows --
the call is still running -- rather than call the session stopped, and the program decides (R6).

## Goal

`session.inject("notes", notes)` returns once every agent can reach `notes` --
`outrig.bindings["notes"]` always, the global `notes` where the name is free -- and every
operation on it runs in the program's process under the same admission, events and shutdown
accounting as a factory binding's.

## Deliverables

- **`session.inject(name, obj, *, serialize=False) -> None`**, synchronous, callable before entry
  -- installed at start -- or after. It returns after the engine registered the binding, the
  interpreter received the manifest line, and every existing kernel bound the name where free.
  A duplicate name raises; a name that is not an identifier, is a keyword, is `runtime`,
  `asyncio` or `outrig`, or is a dunder raises, as `0003-20` refuses them for a declaration; a
  name that is a builtin is registered and reachable through `outrig.bindings`, but installed as
  no global, so `print` stays `print`. The exception classes are this task's.
- **The serving module.** The interception, the per-kernel `Objects` tables, `Turn`, and the
  request records with the `published` handshake, extracted from `binding.py` into an importable
  module of the `outrig` package (fork 3). The program's copy
  serves connections carried as `rpc` frames on the owner connection, a thread per connection as
  RPyC's model has it, never on the program's loop; `binding.py`'s entry point becomes a wrapper
  over the same module. A live method that needs the program's loop uses
  `asyncio.run_coroutine_threadsafe` itself; nothing here promises an `async def` method is
  awaited for it.
- **The relay** routes each binding to its endpoint. Admission, the refusal of a request the owner
  still holds, the control line, the drain, the five outcomes -- `unknown` gaining the
  still-running reason below -- and the `hosted.*` events are the same for both endpoints; the
  report lists live calls among its hosted calls; a new `binding.injected` event, in the
  integration-audit category, records each registration with
  the binding's name and type.
- **Presentation.** `outrig.bindings[name]` resolves the proxy for the calling kernel;
  `runtime.bindings` lists the binding with its type; the convenience global is installed only
  where the name is free, in every existing kernel at injection and in every later kernel at its
  build; the orientation lists each live binding with its type's qualified name and its
  docstring's first line, and names the `outrig.bindings[...]` path where the global is occupied;
  a binding injected while a round runs is announced per fork 2. No `repr`, property or iteration
  is run to describe an object: its type and `__doc__` are read, and nothing else.
- **The grant.** The trusted program needs no declaration approval: injecting the object is the
  grant (`boundary-policy.md`), and every operation on it is still evented.
- **Lifetime and stop.** Live calls drain with the rest. At terminating, nothing can end a call
  running in the program's thread, so the relay seals its record as `unknown` with a new reason,
  still running: its `hosted.ended` event is published with that outcome before the report, the
  report lists it so, `stopped` is `NotProven` with a reason naming the binding and the call, and
  `__aexit__` raises `ShutdownException` in the program (fork 1). An outcome the program's serving
  module writes afterward reaches a sealed relay and is discarded. A program the engine had to
  kill (R6) takes its calls with it, and they are `unknown`, killed, as a binding's are.
- **Paths and mounts.** Injection adds no mount; the same-paths rule covers the directories a
  session mounts, and a path a live object returns names a container file only where a mount
  makes it so.
- **Who reaches it.** Every kernel reaches every binding, children included, since the grant is
  the session's; a kernel built after the injection gets the name at build.
- Replacing or unbinding a live object, and `inject` from a Rust embedder, are out of scope and
  stated; a Rust embedder's object is a service (`services.md`, `0003-44`).

## Acceptance

- **R3's program against a mock model.** `inject("notes", notes)` after the first `Reader` exists
  returns `None` before the first request is sent; both `Reader`s read the same `Notes` instance
  through `outrig.bindings["notes"].read()`, asserted by a counter on the object; the second
  kernel also has the global `notes`.
- **An occupied global stays.** A kernel whose code bound `notes = 1` before the injection keeps
  `1`; `outrig.bindings["notes"]` works there; the orientation for that agent names the path;
  `del notes` and `notes = None` elsewhere leave the binding, `runtime.bindings` and
  `outrig.bindings` unchanged.
- A function parameter named `notes` shadows the global normally; a module imported in the kernel
  has no `notes` global and reaches `outrig.bindings["notes"]` from a function agent code calls.
- A duplicate name, an invalid name and a builtin name behave as the deliverables say; `inject`
  before entry installs the binding at start.
- The orientation carries `Notes`'s qualified type name and its docstring's first line, and a
  `Notes.__repr__` that writes a file is never run.
- A live object's operations are evented in `0003-22`'s shape, with the binding's name.
- `serialize=True` makes two kernels' calls never overlap in the program; `False` lets them.
- **The program's loop is its own.** A live method that uses
  `asyncio.run_coroutine_threadsafe(...).result()` into the program's loop returns to its caller
  while a lease-renewal task on that loop keeps ticking.
- **A live call at shutdown.** Shutdown during a 60 s live call: the call's `hosted.ended` event
  and its entry in the report read `unknown`, still running; `__aexit__` raises
  `ShutdownException`; the report names the call and reads not proven stopped; the program
  survives and reads it; the container is stopped; the call finishing later records nothing
  further. `kill -9` of the program during a live call: the engine cleans up, the call is
  `unknown`, no container remains.
- A live object and a factory binding in one session each receive their own calls; the factory
  binding's process is stopped and reported as before; `binding.injected` is in the stream.
- e2e, with a real model, gated: **the host-program exit criterion.** A `task` object with
  `report_progress(summary)` and `fetch_input(name) -> str` injected into a session where a
  host-declared class with a typed result answers a request; the agent calls both methods as
  plain code; the program's exit after SIGTERM leaves no container and a report the program read
  or the engine printed.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **A live call still running at terminating -- Recommended: report not proven stopped and raise
   `ShutdownException` in the surviving program**, which decides what to do with its own thread;
   the engine kills a program only through R6's escalation of one that does not respond. The
   alternative kills the program for the call, which R3 allows as a last resort and which ends
   the program's other sessions and its chance to read the report.
2. **Announcing an injection made while a round runs -- Recommended: a line in the next tool
   result's prefix**, as messages are announced, moving no baseline. The alternative opens a
   round, which spends a model call for a binding the agent may not need.
3. **One serving module for both processes -- Recommended: yes**, imported by the bindings process
   from the payload and by the program from the host package. A copy would drift.

## Dependencies

- **Hard: 0003-23.** The synchronous proxies a live object is reached through.
- **Hard: 0003-26.** `outrig.bindings`, and the package the serving module belongs to.
- **Hard: 0003-29.** The session, the owner connection, `ShutdownException` and the report.
- **Hard: 0003-35.** Host-driven agents, through which this task's acceptance runs agent code.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- "Live objects", "Presentation" and "Lifetime".
- `plan/phase/0003-python/boundary-policy.md` -- the binding is the grant, and what is recorded.
- `plan/phase/0003-python/security.md` -- what a live object grants.
- `plan/phase/0003-python/lifecycle.md` -- the live-call rows.
- `crates/outrig/src/python/binding.py`, `hosted.rs` and `interpreter.py` -- the serving code,
  the relay and `bind_stubs`.
- `plan/next/cancel-a-running-hosted-call.md` and `plan/next/bound-surviving-binding-calls.md`
  -- what a running call in either process still lacks.

## Decisions

Forks 1-3 were taken as recommended. What no doc decided is recorded here.

- **A program's session names its paths as mounts at their host paths, and `cwd`.** The
  maintainer's call, made during this task: `outrig.Session(mounts=(), cwd=None, grace=None)`,
  each mount an `outrig.Mount(host_path, access="read-only")` or a path. It replaces `workspace=`
  and a mount's container path.
  - Each mount appears in the container at its host path, so the same-paths rule holds in every
    program session, with live objects or factory bindings, and nothing is translated.
  - `cwd`, a directory inside a mount, is the interpreter's working directory: what agent code's
    relative paths and imports resolve against, the orientation's working-directory line, and the
    bindings process's directory. Without it, the image's.
  - The configuration's `[workspace]` and `[[workspace.mounts]]` are the interactive session's
    and are not mounted. `outrig.config()` shows `workspace` and gains `mounts`, for a program
    that mounts them itself. `workspace` stays the CLI's notion.
  - A directory given twice, or a `cwd` outside every mount, raises `ValueError` at construction.
    At entry, before anything starts, the engine refuses a mount that is not an existing
    directory or is `/`, and a `cwd` that is not a directory. The owner test of a binding's paths
    in a program session was removed: a program's mounts are at their host paths by
    construction.
- **Errors.** `inject` raises `TypeError` for a name that is no `str` and a `serialize` that is
  no `bool`. It raises `ValueError` for a name the configuration's rule refuses (an ASCII
  identifier, not a keyword, not `runtime`, `asyncio` or `outrig`, not a dunder), one already
  injected, or one the configuration declares a binding of, which the start's answer names. It
  raises `RuntimeError` while the session starts, once admission has closed or the session ended,
  and in a build without RPyC. The window during the start is refused rather than waited out: the
  caller may be the program's own loop, which the entry needs. A name injected before entry that
  the configuration declares fails the entry with `RuntimeError`, nothing started. The checks and
  the start's snapshot of what was injected before it share a lock, so an injection from another
  thread is never lost or doubled.
- **The free-name rule applies to every binding.** A global is installed only where the name is in
  neither the kernel's globals nor `builtins`, so a factory binding named after a builtin no longer
  replaces it. The check and the binding are one `dict.setdefault`: a live object's stub is bound on
  the interpreter's reader thread, while agent code may bind the same name on its kernel's. The stub
  joins the kernel's stubs after that, so agent code cannot take it from `outrig.bindings` and bind
  it first. No kernel tracks which names were taken: the orientation and the announcement always
  name `outrig.bindings["name"]`, which reaches a binding wherever its name holds something else.
- **Describing the object** reads the module and qualified name of `type(obj)` through `type`'s
  slots, and `__doc__` from the class's own `__dict__`: its first non-empty line, cut to 300
  characters. No descriptor, property or `repr` of the object runs.
- **`serialize` is per injection**, within its session.
- **One relay per endpoint.** A program's session has a second relay beside the bindings
  process's, made with `Relay::beside`: it serves no binding at first, gains each as it is
  injected, and numbers its requests with the first's counter. The interpreter's table holds both;
  a line goes to the relay holding its binding, else to the first, which refuses a binding none
  holds. `CALLS_MAX` counts per endpoint. Each relay sends the interpreter `close`, whose handler
  is idempotent. The report lists the bindings process's calls, then the program's.
- **A program's end takes its calls with it, at once.** When the program is gone -- in the
  session's command loop, or during a shutdown it asked for -- its relay ends each call it was
  serving `unknown`, `killed`, and tells the interpreter to close every kernel's connections to the
  program's objects alone, so agent code waiting on one raises rather than waiting out the drain.
  No `binding.exited` is published. A bindings process's exit likewise unbinds only its own.
- **The seal.** At terminating, after the bindings stop and before the container stops, the
  program's relay is sealed. A call whose dispatch was published, so may be running, ends
  `unknown`, `still_running`, and is named in the stop's `NotProven` reason by request, agent and
  binding; one never dispatched ends `cancelled`, `closed`, since the program refuses a dispatch it
  is not told was published. A line arriving after the seal is discarded.
- **An injection is a `bindings` line with an id.** The interpreter merges it into the bindings,
  binds the stubs in each kernel not released, and answers `bound` however that went; the engine
  waits `READY_TIMEOUT` at most, so an injection, which runs in the session's command loop,
  cannot hold it longer. Objects injected before entry take the same path once the session has
  started, before the start is answered; one that cannot be bound fails the start, the session
  stopped.
- **Announcements** follow the announcer's announced and kept counts: what a failed round
  announced is announced again, the count reset to the kept one as a round opens. No round opens
  for an injection. Only child agents are told, a program's session having no primary.
- **RPyC.** A program's session mounts RPyC and has a program relay whenever the build carries RPyC;
  the program imports it from the directory the start's answer names, at its first entry, as
  `_outrig_rpyc`. The serving code overrides that version's internals, so an `rpyc` the program
  imported first, of another version, cannot serve its objects; refusing such a program was the
  other choice, and would deny live objects to a program that uses RPyC itself. The copy's modules
  import each other as `rpyc.*`. Each runs with a builtins namespace of its own, whose `__import__`
  imports those names as `_outrig_rpyc`'s, and a finder first on `sys.meta_path` loads them that
  way. No module of the program's leaves `sys.modules`, and `sys.path` does not change. Importing
  the copy as `rpyc`, with the program's modules of those names out of `sys.modules` meanwhile, was
  rejected: an import of `rpyc` on another of the program's threads during it raised `KeyError`, or
  got the copy. A build without RPyC runs programs as before, and refuses injections.
- **Owner protocol version 3.** `inject` (`session`, `name`, `type`, `doc`, `serialize`) is
  answered once every kernel has the binding. A session's serving crosses as `rpc` and `event`
  lines from the program and `rpc`, `published` and `control` lines to it, each naming the
  `session`, never answered and never refused: their `id` is a connection's or a publication's.
  `session.start` carries `mounts`, `cwd` and `live`; its answer gains `bindings` and `rpyc`.
- **The program's side.** One `outrig._serve.Server` per session, made at entry; `binding.py`
  imports the same module from the package directory it is now given.
  - The connection's reader thread hands every serving line to one thread through a queue and
    never writes. That thread and the connections' threads write directly, under the send lock. A
    failure on a line is logged, and the thread goes on.
  - Each server holds its own dispatches waiting for their publication. `__aexit__` ends the
    server in a `finally`, once the shutdown is answered: each waiting dispatch is answered
    unpublished, so its request is refused; every connection closes; nothing more is written. A
    call still running runs on, and its reply goes nowhere.
- **Simplify.** `/simplify` compared this task's first build with an independent second one. The
  second was simpler -- about half the new logic, two new types to ten, no envelope around the
  serving lines and no router or shared ledger over the relays -- and was adopted. The first's
  fixes went into it where it fell short: the program's end unbinding its objects, the seal by
  dispatch, the server's end, the lock around injection and entry, a failed injection before
  entry failing the start, the bind's timeout, the announcements' counts, and `cwd` as the
  orientation's and bindings process's directory.
- **Out of scope**, as the task states: replacing or unbinding a live object, and `inject` from a
  Rust embedder, whose objects are services (`services.md`, `0003-44`).
