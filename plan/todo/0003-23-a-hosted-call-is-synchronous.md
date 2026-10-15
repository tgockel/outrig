# 0003-23 -- A hosted call is synchronous

## Context

`0003-21` built an awaited facade over the hosted-object transport: the name bound for a binding
was a lazy proxy, attribute access and calls built a path, and `await` resolved the path on a
worker thread. `0003-22` bound that facade under each binding's name in every kernel
(`bind_stubs`, `interpreter.py`) and relayed its requests. The maintainer withdrew the facade on
2026-10-14, in the review that produced the host-program requirements (R9): a hosted library keeps
its own calling convention. `repo.head.commit.hexsha` is plain code, `task.report_progress("...")`
runs when it is called, and a model learns nothing beyond "this object is on the host and its calls
block". The reason is API compatibility -- models already know GitPython and every other library
a binding may host, and an OutRig-specific asynchronous rewrite of each one is not acceptable --
not performance. The facade goes whole; nothing of it survives as an opt-in spelling.

What replaces it already exists. `0003-17` proved synchronous calls with several kernels, several
connections and interrupts, and `0003-21` kept its engine underneath the facade: `Kernel.hosted`,
`Hosted.sync_request` and `_Pool.request` on the calling thread, the wait on the pool's condition
variable that the reader thread wakes, callbacks served on the thread whose call they belong to,
`serialize = true`, and the pool of up to four connections per kernel and binding. RPyC's own
proxy answers `__len__`, `__bool__`, `__iter__`, the comparisons, `repr` and `str` as requests
through its safe list, resolves `__class__` through its class descriptor so `isinstance` holds,
and makes a request for `hasattr`. That is the behavior a library's users expect, and this task
binds it under the binding's name.

The relay, the bindings process, policy and the events do not change. `0003-22`'s receipt,
dispatch and outcome are recorded where frames are sent and served, which the synchronous path
shares, and the interpreter's labels and notices ride `Hosted._send` and the pool, not the facade.

## Goal

A binding's name, in every kernel, is RPyC's proxy of the host object, used with ordinary
synchronous Python; every operation on it still crosses the relay, is checked, recorded and
admitted as `0003-22` built; the calling thread blocks while the host works, and an interrupt still
wakes it.

## Deliverables

- **The name is the proxy.** `Kernel.stub(binding)` and `bind_stubs` bind the binding's root as
  RPyC's proxy, resolved at first use on the using thread -- never on the reader thread and never
  while a kernel is built, `0003-17`'s rule -- per fork 1. `outrig.bindings[name]` (`0003-26`) and
  `0003-33`'s injection read the same reference the runtime keeps, apart from the global the
  agent sees.
- **Ordinary semantics.** An attribute read, a call, item access, `for`, `len`, truthiness, `in`,
  a comparison, `with`, `x.a = v` and `del x.a` are the requests RPyC's proxies send, one
  boundary request each as `0003-16`'s inventory classifies them; results and exceptions follow
  `hosted-objects.md`'s "What crosses" and "Exceptions" unchanged. `repr`, `str`, `dir`, `hash`
  and whatever `help()` reads are requests, recorded under their operation names. An execution
  whose trailing expression is a proxy makes one `repr` request inside that execution, which
  `discovery.md`'s rule permits; the inventory, outside every execution, still reports names and
  type names and makes no request.
- **A refused attribute read is an `AttributeError`.** The binding's refusal of a private name
  reaches the kernel as an exception that is OutRig's and also an `AttributeError`, so
  `hasattr(x, "_private")` is false and `except AttributeError` catches it, while the library's own
  exception classes never do. How the type crosses before `0003-26`'s package exists is fork 4.
- **Blocking, stated.** A call blocks its kernel's thread: that kernel's loop does not turn, its
  `runtime.wait` returns on no message, and its timers do not fire until the host answers; other
  kernels run as before. `await asyncio.to_thread(fn, *args)`, with the whole expression inside
  the worker, is the optional way to keep the loop turning; the worker takes a connection of the
  kernel's pool, as `0003-17` proved. The orientation says both, plainly, and no longer teaches
  `await`, `_set`, `_resolve` or `async for`.
- **Interrupts and cancels** keep `0003-17`'s semantics. An interrupt or cancel aimed at a kernel
  whose thread waits on a hosted call raises there, `KeyboardInterrupt` or `CancelledError`, naming
  the binding and saying the outcome on the host is unknown or that the call was never sent; the
  host call runs on and its reply records `returned` or `raised` with the interrupted note
  (`0003-22`). A `to_thread` worker's call per fork 3.
- **Callbacks.** A plain-function callback runs on the thread serving the connection: the kernel's
  thread, blocked in the call, or the worker a `to_thread` call runs on. A coroutine callback
  cannot run while the kernel's loop is blocked in the call that passed it; what it does is fork 2.
- **Close.** `Kernel.close_hosted` wakes every wait on the kernel's pools with `EOFError` saying
  the outcome is unknown, as today. With no executor there is no queued job: "never sent" is a
  call woken while it waited for a free connection, and nothing else.
- **Removal.** `HostedObject`, `_Job`, the per-pool `ThreadPoolExecutor`, the path replay, `_set`,
  `_delete`, `_resolve`, `_sync`, `_isinstance`, `_netref`, `_binding`, the `_facades` cache, and
  the `_SERVING` and `within` serving chains except what fork 2 keeps; the facade's tests, replaced
  by synchronous tests of the same shapes; `runtime.wait`'s refusal of a path. The header comment of
  `interpreter.py`'s hosted-objects section and the comments in `binding.py` that describe
  awaiting are rewritten.
- **The orientation** (`crates/outrig/src/agent/orientation.rs`, the lines `0003-20` added):
  these names are objects on the host, used as their library documents; a call on one blocks this
  agent's loop until the host answers; `asyncio.to_thread(...)` keeps the loop turning for a long
  call; `import <lib>` is a local copy that knows nothing of them; `help(<lib>.<Class>)` describes
  the library locally; and, once `0003-26` lands, `outrig.bindings[name]` is the authoritative
  lookup, which that task adds to the line.
- `doc/reference/cli.md` and `doc/reference/events.md`: every sentence `0003-22` wrote about
  awaiting a hosted object is rewritten.

## Acceptance

- **Through `run-new`**, with a mock model and `0003-16`'s fixture binding: `obj.a.b` is two
  `getattr` requests, each with a receipt, a dispatch and an outcome sharing its id;
  `obj.method(2, y=3)` is `getattr`, `call` and a `del` after; `for x in obj.sequence` is
  `__iter__` and one `__next__` per item plus the one that ends it; `len(obj.sequence)`,
  `bool(obj)`, `1 in obj.sequence`, `obj.a == 1`, `obj.writable = "v"`, `del obj.writable` and
  `with obj.manager:` are each the corresponding request, and the fixture observes each effect;
  `repr(obj)` is one `repr` request, and an execution that ends in `obj` makes one; a method that
  raises the fixture's exception is caught by `except` on the fixture's class.
- `hasattr(obj, "missing")` and `hasattr(obj, "_private")` are both false;
  `except AttributeError` catches `obj._private`, `except` on the fixture's exception classes
  does not, and the read is recorded `refused` by the interception.
- `isinstance(obj.nested, Nested)` holds after importing the fixture; `await obj.a` raises
  `TypeError`; `obj.nested is obj.nested` is false and `obj.nested == obj.nested` asks the host.
- **The cost is stated and measured.** A 10 s call on the kernel's thread: a timer task in that
  kernel does not fire and a `runtime.wait` in another of its tasks does not return on a user
  message until the call returns, while a second kernel runs executions and answers `inv`
  throughout. The same call as `await asyncio.to_thread(obj.slow, 10)` leaves the timer firing
  and `runtime.wait` returning on the message, and takes a second connection of the pool.
- An interrupt during a call on the kernel's thread, on the primary and on a child kernel, raises
  `KeyboardInterrupt` there within a second with `0003-17`'s message; the connection is usable
  after; the fixture records that the host finished the call, and its outcome reads `returned`
  with the interrupted note. A cancel raises `CancelledError` the same way. A call woken while
  waiting for a free connection says it was never sent and is recorded `cancelled`. The
  `to_thread` case behaves as fork 3's choice says, and a test pins it.
- A coroutine callback passed from a call made on the kernel's thread is refused before the call
  is sent, with an error naming `asyncio.to_thread`; one passed from a `to_thread` call runs on
  the kernel's loop, and a hosted call it makes is recorded under the outer call's id.
- Four calls from one kernel take four connections; a fifth waits for one and opens none.
- `Kernel.close_hosted` during two calls raises `EOFError` saying the outcome is unknown in each,
  and the binding finishes both.
- e2e, with network: **the GitPython item of `0003-22`, as plain code.** Through `run-new` with
  a mock model and a GitPython 3.2.0 binding over a test repository: `repo.head.commit.hexsha`,
  `repo.index.commit("...")`, `repo.remotes.origin.push()`, `for c in repo.iter_commits()`,
  `len(c.parents)`, `isinstance(c, git.Commit)` under `GIT_PYTHON_REFRESH=quiet`, and
  `except git.GitCommandError`; the bare repository holds the commit and the stream holds every
  request. This task's `## Decisions` list each place a proxy still behaves unlike the local
  object, replacing `0003-22`'s list.
- **No agent-visible name resolves a hosted object through an awaitable**, checked over the names
  a kernel boots with.
- `0003-17`'s tests pass unchanged; `0003-21`'s are removed or rewritten synchronously;
  `crates/outrig/public-api.txt` is unchanged; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **How the root is resolved -- Recommended: a lazy root stub, resolved on first use on the using
   thread**, forwarding every operation RPyC's proxy answers to the proxy once fetched, so building
   a kernel resolves nothing and a kernel that never touches a binding pays nothing. The
   alternative fetches each root before the kernel's first execution, which binds the netref
   directly and needs no stub, at the cost of a round trip per binding per kernel before
   unrelated code runs, and of failing that execution when the bindings process is gone.
2. **A coroutine callback -- Recommended: refused when the serving thread is a kernel's loop
   thread, and run on the kernel's loop through `run_coroutine_threadsafe` when the call was made
   in a worker**, keeping `0003-21`'s serving of the coroutine's nested calls on that worker for
   this case only. The alternative refuses coroutine callbacks everywhere and deletes that code
   too; a library whose callbacks must await then needs a plain function and a queue.
3. **Whether a `to_thread` worker's call is woken by an interrupt -- Recommended: no, as `0003-17`
   built it.** The worker's frames hold no agent code, the awaiting coroutine receives the
   cancellation and its wait ends, and the worker's call runs on until the host answers, bounded
   by the pool. The alternative registers worker calls under the submitting execution, as
   `0003-21`'s jobs were, which keeps part of the job machinery.
4. **How a refusal crosses as an `AttributeError` before `outrig` exists in the kernel -- Open.**
   RPyC rebuilds a host exception by module and name and falls back to a generic `Exception`
   subclass; the binding can raise a type the container rebuilds as a builtin `AttributeError`
   subclass carrying OutRig's name and attributes, or the interpreter can map the generic class
   where the request returns. `0003-26` may replace either with the package's own class.

## Dependencies

- **Hard: 0003-17.** The engine this task binds: the pool, the wake, the serialize turn, callbacks
  on the serving thread, and the recording fixture.
- **Hard: 0003-21, 0003-22.** The facade this task removes, and the relay, stubs and events it
  binds the proxy into.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- "Calls are synchronous", "Presentation", and
  "Rejected alternatives", where the facade now is.
- `plan/phase/0003-python/execution-and-rounds.md` -- a hosted call blocks its caller.
- `plan/phase/0003-python/discovery.md` -- what the orientation says about a binding.
- `plan/done/phase/0003-python/tasks/0003-17-a-host-call-blocks-only-its-caller.md` -- the
  `## Decisions` that describe the engine.
- `crates/outrig/src/python/interpreter.py` -- `HostedObject`, `_Job`, `_Pool`, `Hosted`,
  `Kernel.stub`, `bind_stubs`, `Kernel.close_hosted`.
- `plan/next/one-request-per-hosted-method-call.md` -- making a method call one request, which a
  proxy subclass could do.
- `plan/phase/0003-python/potential/custom-hosted-object-protocol.md` -- a protocol of OutRig's
  own, which would keep this calling convention.
