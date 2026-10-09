# A hosted object is awaited

## Context

`0003-17` built the pool of connections per kernel and binding, the wait a reader thread can
wake, and the serialize turn, and proved them with calls that blocked the calling thread. In that
design a hosted call blocked its kernel's loop until the host answered, and the orientation taught
`asyncio.to_thread` as the remedy. The planning round of 2026-10-09 chose instead an awaitable
facade over the same machinery: the name bound for a binding is a lazy proxy; attribute access,
item access and calls build a path with no round trip; `await` resolves the path on a worker
thread through the pool and settles the awaiting code. RPyC 6.0.2, the binding process, the wire,
the pool, the wake and `serialize = true` are unchanged (`hosted-objects.md`, "Calls are
awaited").

Why this and not the alternatives: RPyC has no asyncio support (`rpyc.async_` returns an
`AsyncResult` with no `__await__`, upstream tomerfiliba-org/rpyc#506), and a survey on the same
day found no maintained pure-Python library that is asyncio-native and hosts a transparent
object graph. The structural reason is that attribute syntax cannot be awaited, so any proxy
library chooses between blocking in `__getattr__` and returning a lazy path to await later. The
facade is that lazy path over the transport that exists. A protocol of OutRig's own is
`plan/phase/0003-python/potential/custom-hosted-object-protocol.md`.

Two facts of `crates/outrig/src/python/interpreter.py` shape the work. `Kernel._wake` finds the
wait to wake by the kernel thread's ident (`_waits`), and `_Pool.request` decides `interruptible`
from the calling thread's frames (`_agent_code_outward`); neither fits a worker thread, whose
frames hold no agent code. And unboxing a reply can issue a nested synchronous `inspect`
(`0003-16`), so a kernel-side thread must serve each in-flight call and the reader thread never
may (`0003-17`); that is why `plan/next/awaitable-hosted-calls.md`'s sketch, which had the reader
thread process replies, was wrong, and why this task keeps a thread per in-flight call.

## Goal

Agent code reads, calls and iterates a hosted object by awaiting it; its kernel's event loop
keeps turning while the host works; nothing an agent writes on a hosted object makes a request
without an `await`; and no agent code writes `asyncio.to_thread` for one.

## Deliverables

- **The facade.** For each binding a kernel has, a lazy proxy of three parts: the pool, a base
  that is `None` for the binding's root or a netref for a *resolved* proxy, and a path of steps.
  `x.name`, `x[k]` and `x(...)` append a step and cross nothing. `await x` submits one job to the
  pool's executor; the worker replays the steps as ordinary netref operations through
  `Hosted.sync_request` and `_Pool.request`, one request per step -- `getattr` for an attribute,
  `getattr` then `call` for a method call, as RPyC's proxies send them (`0003-17`) -- and settles
  the awaiting code through the kernel's loop. A by-value result is the value; a tuple is a tuple
  with proxies inside; anything else is a resolved proxy bound to the host object, which chains
  and awaits the same way. Awaiting a resolved proxy with an empty path returns itself; awaiting
  a path twice resolves twice. A hosted exception is raised where the call is awaited, typed as
  "Exceptions" says. Arguments: a resolved proxy unwraps to its netref; an unresolved one is
  resolved first on the same worker, recursing into containers, so `Hosted._box` sees netrefs.
  A proxy is awaitable and not a coroutine: `await` and `asyncio.gather` take it as it is, and
  `x._resolve()` returns a coroutine that awaits `x`, for `asyncio.create_task(..., name=)`,
  which takes nothing else; `asyncio.wait` and `runtime.wait` take the task.
- **`async for`** over a proxy: `__iter__` then one `__next__` per item, as `0003-17` counted;
  `[x async for x in proxy]` materializes. `for`, `iter`, `len`, truthiness, `in`, `==`, the
  operators and `with` raise `TypeError` naming the awaited spelling -- `async for`,
  `await x.__len__()`, `await x.__bool__()`, `await a.__eq__(b)`, `async with` -- because a proxy
  cannot know its length or truth without a round trip and must not make one silently. `hash`
  is identity, so proxies can be dict keys and set members. `callable` is always true.
- **Explicit assignment and deletion.** `await x._set(name, value)` and
  `await x._delete(name)` send `setattr` and `delattr`; `x.name = v` and `del x.name` raise with
  the pointer, since an assignment statement cannot be awaited. The facade's own API lives under
  single-underscore names: `_set`, `_delete`, `_resolve`, `_sync`, `_isinstance`, `_netref`,
  `_binding`. The host refuses every private name ("Interception"), so no hosted attribute can
  collide with them. The facade defines no public name: not `send`, `throw` or `close`, which a
  coroutine would carry, so `await repo.close()` and `await gen.send(v)` are steps like any
  other. Dunder names outside a fixed list of the facade's own (`__class__`, `__await__`,
  `__aiter__`, `__anext__`, `__aenter__`, `__aexit__`, and the object machinery) are steps, so
  `await x.__len__()` reaches RPyC's safe-list handler.
- **Local answers.** `repr`, `str`, `dir`, `help`, `type`, `isinstance` and `hash` make no
  request. An unresolved proxy prints its binding and path and says it is unresolved; a resolved
  one prints the host type's name from the netref's id pack, and `dir` lists the facade's API
  plus the netref class's method names, which RPyC put in the class when it built it. The host's
  forms are explicit and awaited: `await x.__repr__()`, `__str__()`, `__dir__()`, `__hash__()`,
  `__len__()`, `__contains__(y)`, `__getitem__(k)`, sent as RPyC's dedicated handler where one
  exists and recorded under the inventory's names (`0003-16`). pydoc's probes (`__name__`,
  `__doc__`, `__module__`) are answered locally. `isinstance(c, git.Commit)` holds for a
  resolved proxy through the netref's class descriptor, which resolves the local class through
  `sys.modules` as today (fork 1); `isinstance(a, x)` with `x` hosted answers locally for the
  exact class and otherwise points at `await x._isinstance(a)`. `hasattr(x, "name")` is always
  `True` and `getattr(x, "name", default)` never chooses its default: a lookup builds a path
  and raises nothing, and the facade cannot tell either from an ordinary lookup, so it cannot
  refuse them as it refuses `len`. The error arrives at the `await` and is an `AttributeError`
  -- the library's own, or the host's refusal of a private name -- so the spelling is
  `try: await x.name` / `except AttributeError`.
- **A worker per in-flight call.** Each pool owns a `ThreadPoolExecutor` of `POOL_MAX` workers,
  created lazily. A job is built on the loop thread at await time, so it carries what a worker's
  frames cannot: the awaiting execution from `_CURRENT`, `interruptible` from the awaiting frames
  through `_agent_code_outward`, the awaiting task, and the future. The worker runs under a copy
  of the awaiting task's context, so `_CURRENT`, `outrig.runtime` (`0003-24`) and an invocation
  id (`0003-28`) resolve on it and its output is attributed. It takes a connection as `0003-17`'s
  `to_thread` workers did: four per kernel and binding, and a fifth job waits in the executor's
  queue, with no thread and no stack, until a worker is free. `runtime.wait` returns on a message
  during a long call, and a timer in another task fires.
- **Callbacks.** A callable argument crosses as today; the host calls it on the connection the
  request took, which the worker serves. A plain function runs on the worker and reads the proxies
  it receives with `_sync()`, which resolves on that thread and connection -- `_Pool._pick` gives
  a thread that already holds a connection that same connection, so nested calls complete with
  every connection occupied, as `0003-17` showed. A coroutine function runs on the kernel's loop
  through `run_coroutine_threadsafe`, under the worker's context; the worker, instead of blocking
  on the result, serves the coroutine's nested awaits from a queue on its own connection (fork
  2), so no fifth connection or second worker is needed and under `serialize = true` the nested
  request still arrives on the connection whose host thread holds the re-entrant turn. An
  interrupt inside the coroutine lands in that task, is answered to the host and re-raised on
  the worker, and ends the awaiting execution as today. `_sync()` on the kernel's own loop
  thread with a coroutine callback is refused rather than deadlocking.
- **Interrupt, cancel and close.** Each pool keeps the set of its waits, sync and job alike.
  `Kernel._wake` keeps its kernel-thread lookup and also walks the kernel's pools, waking every
  job of the interrupted execution whose `interruptible` is set (fork 3): an in-flight job raises
  `KeyboardInterrupt` with `0003-17`'s message where the call is awaited; a job still queued is
  settled with the message that the call was never sent; a busy job -- boxing, sending, running
  a callback -- raises at its next wait. `Kernel.cancel` wakes the same way before the loop's
  `task.cancel()` lands, so the awaiting code sees the outrig `CancelledError`; a cancel of the
  awaiting task alone, by agent code, raises asyncio's plain `CancelledError`, cancels the wait
  only, and the host call runs on, its late reply dropped and the connection tidied by the next
  taker. `Kernel.close_hosted` wakes in-flight jobs with `EOFError` saying the outcome is unknown,
  settles queued ones as never sent, and shuts the executor down. `_Pool._pop_exc` uses the job's
  task rather than the loop's current task, which is racy from a worker.
- **What stays.** RPyC 6.0.2 and the vendored copy, `binding.py` untouched, the wire,
  `FrameChannel`, `_Pool.take`, `_pick`, `release` and `tidy`, `Hosted.serve` and `_dispatch`,
  the serialize turn, two requests per method call. `Kernel.hosted` and `_Pool.request` stay as
  the facade's engine and for tests, bound to no agent-visible name; `Kernel.stub(binding)`
  returns the proxy the relay task binds under the binding's name.
- Tests through `0003-16`'s test relay and `0003-17`'s fixture, with fixture additions in
  `relay.rs`: `pair()` returning `(self.nested, 1)`, `call_with_nested(fn)`, `call_twice(fn, x)`,
  and `iterable()` returning a generator. The header comment of `interpreter.py`'s hosted-objects
  section, and the comments in `binding.py`, `interpreter.py`, `payload.rs`, `relay.rs`,
  `supervisor.rs` and `binding_tests.rs` that name `0003-21`, `0003-22` or `0003-25`, are
  updated to the renumbered tasks.

## Acceptance

- `K.stub('fx')` gives a proxy; `repo.nested.value` and `repo.method(2, y=3)` cross no `rpc`
  line (the relay's count is unchanged) and print a local repr; `await` then yields the fixture's
  values. `await root.a.b.c` makes three `getattr`; `await root.method(1)` makes `getattr`, `call`
  and a `del` after.
- `await stub.nested` is a resolved proxy; `await stub.pair()` is a tuple of a proxy and `1`;
  `await (await stub.nested)` is the same proxy with no request; `await proxy.field` works;
  `isinstance(await stub.nested, Nested)` holds after importing the fixture.
- `await stub.takes(await stub.nested)` and `await stub.takes(stub.nested)` both return `True`,
  and the fixture's call count shows one call each.
- `[x async for x in stub.sequence]` is the list, with `__iter__` and one `__next__` per item in
  the fixture's counts; `for`, `len`, `if`, `in`, `==` and `with` on the proxy each raise
  `TypeError` naming the spelling; `await stub.sequence.__len__()` is its length; `await
  stub.__repr__()` is the host's text and makes one `repr` request.
- `await stub._set("writable", "after")` sends `setattr` and the fixture sees the value;
  `stub.writable = 1` and `del stub.writable` raise; `await stub._delete("writable")` sends
  `delattr`.
- `asyncio.create_task(stub.slow()._resolve(), name="slow")` runs, and `get_name()` is `"slow"`;
  `asyncio.create_task(stub.slow())` raises asyncio's `TypeError`;
  `await asyncio.gather(stub.a, stub.b)` needs no adapter.
- `await stub.close()` and `await stub.send(1)` reach the fixture, which defines both, as
  `getattr` and `call`; the proxy type has no local `send`, `throw` or `close`.
- `hasattr(stub, "missing")` is `True` and `getattr(stub, "missing", None)` is a path, not
  `None`; `await stub.missing` raises an error `except AttributeError` catches, and so does
  `await stub._private`, whose error is OutRig's refusal (`0003-21`).
- `async with stub.manager:` raising inside records `__exit__` with the exception type and text
  by value, as `ordinary_use` does today.
- A 10 s awaited call in a task leaves the loop running: `inv` answers, a timer in another task
  fires, and `runtime.wait` returns on a user message during it. Thread names show at most
  `POOL_MAX` workers for the pool and no thread per call.
- Four awaited calls from one kernel take four connections; a fifth opens none (no fifth id at
  the relay) and runs when one returns. `help` and echoing a proxy as a trailing expression
  cross nothing.
- An interrupt of the awaiting execution, on the primary and on a child kernel, raises
  `KeyboardInterrupt` where the call is awaited within a second, with `0003-17`'s message; the
  connection is usable after; the fixture records that the host finished the call. A cancel
  raises `CancelledError` the same way. An interrupt while a fifth call is queued says the call
  was never sent, and the fixture's call count is unchanged. Cancelling the awaiting task alone
  raises asyncio's `CancelledError`, and the next call tidies the connection.
- A plain-function callback runs on the worker (the fixture records the thread name), its
  `print` appears in the awaiting execution's output, and it reads a proxy with `_sync()` with
  every connection occupied. A coroutine callback runs on the kernel's loop, and a hosted call it
  awaits travels on the outer call's connection (one id at the relay) and, under
  `serialize = true`, returns while the outer call is in progress. An interrupt inside a
  coroutine callback ends the execution with `raised == "KeyboardInterrupt"`.
- `Kernel.close_hosted` during two awaited calls raises `EOFError` saying the outcome is unknown
  where each is awaited, settles a queued third as never sent, and the binding finishes both.
- A kernel with idle workers still exits when stdin closes.
- No agent-visible name resolves a hosted object synchronously, checked over the namespace the
  kernel boots.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **How a resolved proxy keeps `isinstance`.** Recommended: the proxy's `__class__` returns
   what the netref's class descriptor returns -- the local class when its module imports, else
   the proxy type -- and never falls back to a `getattr` of `__class__`, which the binding
   refuses. The alternative builds the proxy class in `Hosted._netref_factory` from a facade
   base, so a resolved proxy is a netref; it keeps RPyC's cache semantics but puts the facade's
   `__getattribute__` under RPyC's.
2. **Nested hosted calls from a coroutine callback.** Recommended: on the outer call's
   connection, performed by the worker that holds it, which the callback's task reaches through a
   context variable set before scheduling; the worker serves a queue of nested jobs until the
   coroutine settles. A new connection would deadlock under `serialize = true` and lose the
   parent id. A plain function cannot await and uses `_sync()`.
3. **How the wake finds a worker's wait.** Recommended: `_waits` stays keyed by thread ident for
   `serve` and nesting; each pool also keeps its waits, and `Kernel._wake` walks the kernel's
   pools under each pool's condition variable, in the lock order `close_hosted` uses, waking
   every job whose execution is the interrupted one. `runaway` does not widen job waking: a wedge
   is on the loop thread, never in a suspended await.
4. **Whether a step is a coroutine.** Settled: the proxy implements `__await__` and nothing of
   `collections.abc.Coroutine`, whose `send`, `throw` and `close` are public names that hosted
   objects own -- `repo.close()`, a generator's `send` -- and that `asyncio.create_task` would
   drive instead of building a path. `_resolve()` returns a coroutine for `create_task(name=)`.
   The alternative, `asyncio.ensure_future(x)` and `set_name`, adds no name to the facade but
   loses the one-line idiom `messages.md` prefers.
5. **Operators and item access.** Recommended: `x[k]` builds a step; comparisons, arithmetic,
   `in` and truthiness raise with the pointer; `hash` is identity. The alternative, operators that
   build steps to await, makes `x == y` an object that is always truthy, which is the silent wrong
   answer this task refuses.
6. **A worker per call or per connection.** Settled as per call by the maintainer, the cost
   `to_thread` paid; a serving thread per connection is
   `plan/next/hosted-calls-without-a-thread-per-call.md`.
7. **The executor.** Recommended: a `ThreadPoolExecutor` of `POOL_MAX` workers per pool, so a
   fifth concurrent await queues as a future and a queued job cancels cleanly, rather than the
   loop's default executor, which is the agent's own `to_thread` pool and which an hour-long
   hosted wait would otherwise occupy. The executor's threads are not daemon threads; the
   interpreter exits through `os._exit` from the reader thread, so no join blocks exit, which the
   acceptance checks.

## Dependencies

- **Hard: 0003-16.** The shim, the callback proxies, the interception, the test relay.
- **Hard: 0003-17.** The pool, the wake, the serialize turn, the recording fixture.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- "Calls are awaited" and "Presentation".
- `plan/phase/0003-python/execution-and-rounds.md` -- the retained task plus `runtime.wait`.
- `plan/phase/0003-python/runtime-protection.md` -- what the wake reaches.
- `plan/phase/0003-python/potential/custom-hosted-object-protocol.md` -- the alternative that
  removes the thread per call and the pool.
- `crates/outrig/src/python/interpreter.py` -- `_Pool`, `_Wait`, `Hosted`, `Kernel.hosted`,
  `Kernel._wake`, `_agent_code_outward`.
- `crates/outrig/src/python/binding_tests.rs` and `relay.rs` -- the tests and the fixture.
- `plan/next/hosted-calls-without-a-thread-per-call.md` and
  `plan/next/one-request-per-hosted-method-call.md` -- what this task leaves.
