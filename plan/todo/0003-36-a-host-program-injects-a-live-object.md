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
  stated; a Rust embedder's live object is `plan/next/rust-object-as-python-object.md`.

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
