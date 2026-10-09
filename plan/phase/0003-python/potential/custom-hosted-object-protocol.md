# A hosted-object protocol custom-built for OutRig

## Shipped

A hosted object is awaited through a facade over RPyC 6.0.2 (`hosted-objects.md`, "Calls are
awaited"). The facade is a lazy proxy on the container side: attribute access, item access and
calls build a path with no round trip, and `await` resolves the path on a worker thread through
the kernel's pool of connections, one RPyC request per step and two per method call. RPyC frames
ride the interpreter pipe as `rpc` lines, the binding process serves each connection on a thread
of its own and intercepts all 20 of RPyC's request handlers, the pool bounds a kernel at four
calls in flight per binding, and a worker thread per in-flight call is the cost. The maintainer
chose this in the planning round of 2026-10-09 as the way to get hosted objects rolling, and said
it is not the ideal long-term shape.

## Alternative

A hosted-object protocol of OutRig's own, asyncio-native on both sides, with RPyC gone.

- **The surface is the facade's**, as Cap'n Web does it: a proxy's attribute access yields a
  promise, `await` is the pull, and a chain such as `repo.head.commit.hexsha` crosses as one
  request carrying the path, so a step costs no round trip of its own. `async for` iterates in
  batches the host chooses.
- **The wire is JSON on the existing NDJSON lines**, as jsii's kernel API does it over a child's
  stdin and stdout: by-reference object ids, `get`, `set`, `delete`, `invoke`, `iter`, `release`,
  and callbacks as requests the host sends in place of a response. Every request carries a call
  id and, for a callback's nested request, the parent's id. No base64, no brine, no frame limit
  of RPyC's.
- **One multiplexed connection per kernel and binding.** Replies are matched by id, so any number
  of calls are in flight at once. The pool of four, the shared object table per kernel, the
  replaced `serve`, the wake through a pool's condition variable and the tidy of late replies,
  which exist only to work around RPyC's one-call-per-connection model, go away. The binding runs
  each call on a worker of its own and routes a callback's nested request to the worker waiting
  in that callback by the parent id, which RPyC's frames cannot carry.
- **A cancel message.** The host cannot stop a running Python call, but it can mark one abandoned,
  drop its reply, and record the outcome, which the awaiting side can be told at once.
- **Interception by construction.** The message set is the handful OutRig defines, each checked
  by policy as a boundary request, rather than RPyC's 20 handlers guarded against RPyC's own
  advisories (two CVEs so far) and pinned against upgrades by a conformance test. Nothing
  vendored, and no wheel to embed.
- **Servable from Rust.** The same messages can be answered by a Rust service directly, which is
  `plan/next/rust-object-as-python-object.md` without the hosted Python client it now needs, and
  which `mcp-wrappers.md` asks for.

What it costs: `binding.py`, the hosted half of `interpreter.py`, the hosted-object tests, and
the hosted-objects, boundary-policy and security pages are rewritten; and RPyC's conveniences are
re-created -- proxy classes built from the host's `inspect` so `isinstance` holds, exception
types rebuilt by name with their public attributes, the by-value type set and the copy shim,
reference counting and release. The survey of 2026-10-09 found no maintained library to take
instead: every pure-Python library with a transparent object graph (RPyC, rpyc-ng, Pyro5, Py4J,
`multiprocessing.managers`) is synchronous, every asyncio-native one (Callosum, jeepney,
dbus-fast, aiorpc, autobahn, pycapnp) is a service or schema model, and the three with the right
shape -- `py-capnweb` (a Cap'n Web port), `stateforward.proxyables`, `invisibles-py` -- are
single-author alpha projects with 0 to 38 stars. They are reference designs for this build, with
Cap'n Web's protocol page and jsii's kernel specification.

## Evaluation

What would decide it, once the facade and the relay task's events are in use:

- **Threads and memory.** Worker threads and resident memory under concurrent awaited calls across
  kernels and bindings, against the memory ceiling (`runtime-protection.md`), compared with one
  connection and no worker per call. If blocked workers dominate a session's memory, the
  alternative removes them.
- **Chain latency.** A multi-step chain at one request per step, against the 0.6 to 1.0 ms round
  trip `0003-17` measured, compared with one request per chain. If agents write long chains and
  the per-step cost shows in rounds, pipelining pays.
- **Nested callbacks.** Whether coroutine callbacks that await hosted calls hit the pool bound or
  the serve-a-queue path in practice. If they do, the parent id the alternative carries is the
  simpler mechanism.
- **RPyC upstream.** Its release cadence (one release a year, last 6.0.2 on 2025-04-18), its open
  asyncio issue, and any new advisory at the trust boundary. A second CVE in the handlers OutRig
  intercepts is a reason to stop vendoring them.
- **Rust services and MCP.** Whether `plan/next/rust-object-as-python-object.md` or the MCP
  wrapper is scheduled. Both want an awaitable protocol Rust can serve, and the alternative is
  that protocol; building the Rust service adapter on RPyC would mean writing RPyC's server side
  in Rust, which `hosted-objects.md` rejected.

## When

After the facade ships and the relay task's outcome events give real call counts and latencies,
or when the Rust service adapter is scheduled, whichever comes first. Until then the facade's
surface is the contract agent code is written against, and the alternative keeps that surface,
so adopting it later changes the wire and the binding process and not what agents write.
