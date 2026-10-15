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
  exception classes never do. The interpreter converts it (fork 4): the binding's name rule sets
  the refused name on its `Refused` as `name`, and only a `getattr` refused that way is re-raised
  as a class made from RPyC's generic `outrig.Refused` and `AttributeError`. A refusal for any
  other reason -- a result the binding will not hand out, a request it could not record -- stays a
  plain `outrig.Refused`, so `hasattr` never reports an existing attribute as missing.
- **One request per read.** RPyC's proxy sends a second `getattr` from its `__getattr__` fallback
  whenever the first raised `AttributeError`, so a missing or refused read is two requests and two
  records. The interpreter's copy of the proxy re-raises the first error instead.
- **Tracebacks show the agent's code.** The error an execution reports leaves out RPyC's frames
  and the hosted machinery's between the agent's line and the exception, for the exception and
  every one chained to it; the agent's frames, a callback's, and other libraries' stay.
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
  thread, blocked in the call, or the worker a `to_thread` call runs on. A coroutine function is
  refused as a callback on every thread, before the call is sent, with an error saying to pass a
  plain function; a callback that returns a coroutine anyway is refused when the host calls it,
  and the coroutine is closed (fork 2).
- **Close.** `Kernel.close_hosted` wakes every wait on the kernel's pools with `EOFError` saying
  the outcome is unknown, as today. With no executor there is no queued job: "never sent" is a
  call woken while it waited for a free connection, and nothing else.
- **Removal.** `HostedObject`, `_Job`, the per-pool `ThreadPoolExecutor`, the path replay, `_set`,
  `_delete`, `_resolve`, `_sync`, `_isinstance`, `_netref`, `_binding`, the `_facades` cache, and
  the `_SERVING` and `within` serving chains with the serving of coroutine callbacks; the facade's
  tests, replaced by synchronous tests of the same shapes; `runtime.wait`'s refusal of a path. The
  header comment of `interpreter.py`'s hosted-objects section and the comments that describe
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
- `hasattr(obj, "missing")` and `hasattr(obj, "_private")` are both false, each one request;
  `except AttributeError` catches `obj._private`, `except` on the fixture's exception classes
  does not, and the read is recorded `refused` by the interception. A read refused for its result
  stays a plain `outrig.Refused`.
- `isinstance(obj.nested, Nested)` holds after importing the fixture; `await obj.a` raises
  `TypeError`; `obj.nested == obj.nested` asks the host. (Whether `obj.nested is obj.nested` holds
  depends on the connection each read took -- RPyC's proxy cache, `hosted-objects.md`'s "What
  crosses" -- so it is not asserted.)
- A hosted call's exception, and one chained to it, is reported without RPyC's frames or the
  hosted machinery's.
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
- A coroutine function passed as a callback, from a call on the kernel's thread and from one in a
  `to_thread` worker, is refused before the call is sent, and the fixture method is never called;
  a plain callback that returns a coroutine is refused when the host calls it. A plain callback
  runs on the thread serving its call, and a hosted call it makes is recorded under the outer
  call's id.
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

## Decisions

Forks 1 and 3 were taken as recommended. The maintainer decided the other two when asked.

- **Fork 2: the alternative.** A coroutine function is refused as a callback on every thread,
  before the call is sent. A synchronous library never awaits a callback, so this is the
  library's own convention: used locally, an `async def` callback returns a coroutine nobody runs.
  Keeping the worker case meant the worker and the kernel's loop thread sharing the outer call's
  connection, and a review of that design against the code found a startup race (the coroutine's
  first call reaching the pool before the worker marked its connection shared), calls taking
  another connection while the worker served a frame and deadlocking under `serialize = true`,
  and a cycle through two serialized bindings that deadlocks even when that is fixed. `0003-21`'s
  queue of nested calls, the other way to keep it, had needed several rounds of review fixes.
- **Fork 4: the interpreter converts**, and only the name rule's refusals, which `binding.py`
  marks; how is the bullet on the refusal of a name, below.
- **Fork 1, as recommended: `HostedRoot`.** `Kernel.stub` returns a `HostedRoot` (`outrig`
  module) holding the kernel's pool and nothing else. Each of its special methods is `_on_root`
  of an operation -- `getattr`, `setattr`, `delattr`, `len`, `repr`, the `operator` functions --
  applied to `pool.root()`, the root second for a reflected operator and for `isinstance`; the
  set is what an RPyC proxy's class can carry, its own and the Python 3 names of the safe list,
  so one the root's type lacks fails as it does on the proxy, and `with` is written out to fail
  as `with` does. It holds its pool rather than its kernel and binding, so after the bindings
  process ends a later use raises how it ended, as `0003-22`'s test requires. The cost: Python
  decides from the type, so `callable(repo)` is true of every stub, and `type(repo)` is
  `HostedRoot` where `type(repo.head)` is RPyC's class named for the host type. `help(repo)`
  describes the root's type: `_Help` hands pydoc the proxy, not the stub.
- **The refusal of a name, and only that, is an `AttributeError`.** A review of the plan against
  the code found that converting every `outrig.Refused` on a `getattr` would make `hasattr` call
  an attribute that exists missing whenever the binding refused the read for another reason -- a
  result it will not hand out, a request it could not record, a reply too large. `check_attr`
  sets the refused name on the exception as `name`, which crosses as an attribute and which no
  other refusal carries, and `Hosted.sync_request` -- every proxy request's one entry -- raises a
  refused `getattr` carrying it as `_attribute_error`, kept from `0003-21`, with `name` passed to
  `AttributeError`, whose `name` is a slot of its own. `0003-26` may replace the generic class
  with the package's.
- **One request per read.** RPyC's proxy asks the host from `__getattribute__`, and when the
  answer is an `AttributeError` Python calls its `__getattr__`, which asks again: a missing or
  refused attribute was two requests and two records -- for a missing one, in plain RPyC too.
  `_connection_class` replaces the proxy's `__getattribute__`, for the process, with one that
  answers what RPyC answers itself and raises `AttributeError` for everything else, leaving the
  one request to `__getattr__`; `__class__` is answered from the proxy's class when that names a
  class of this process and otherwise asked once, and `__doc__` is always asked. A name the proxy
  answers itself and lacks -- `__dict__` -- still goes to the host once, as before: a first
  version skipped it, and `0003-16`'s `names_that_reach_the_interpreter_are_refused`, which
  expects the host's refusal of `root.__dict__`, caught it.
- **Tracebacks.** `_format_error` formats with the frames of RPyC, by its directory, and of the
  hosted machinery, by the module's own globals and the first part of the code's qualified name
  (`_HOSTED_CODE`: the stub and `_on_root`, `_Pool`, `_Wait`, `FrameChannel`, everything built in
  `_connection_class`), left out of the traceback of the exception and of every one chained to it
  or grouped in it, rebuilt as tracebacks of their own and put back after formatting, since the
  exception is the agent's and may be held still. Not by filename: the interpreter runs from
  `-c`, as `<string>`, the name agent `exec` code has too. An error that passes through none of
  them formats as before.
- **The identity criterion is dropped.** "`obj.nested is obj.nested` is false" contradicts
  `hosted-objects.md`'s "What crosses" and RPyC 6.0.2: the proxy cache gives one proxy per host
  object per connection while the first lives, so two reads on one connection are the same proxy
  and two on two connections are not -- which connection a read takes depends on the pool. `==`
  asks the host, and the test asserts that.
- **`bool(obj)` needs `__bool__` on the host.** The fixture's `Root` had neither `__bool__` nor
  `__len__`, so `bool` of it answered `True` with no request, on the proxy as locally; it now has a
  counting `__bool__`.
- **What iteration costs, on the payload's 3.13.** A `for` statement is `__iter__` and one
  `__next__` per item plus the one that raises `StopIteration`. A list comprehension is one more
  `__iter__`, on the iterator: 3.13's inlined comprehension applies `GET_ITER` to it again, checked
  with plain RPyC on the payload's interpreter (3.12 sends one). `list(proxy)` asks `__len__` for
  its length hint. The request-count test measures a `for` statement.
- **Coroutine callbacks are refused before anything is sent** by `_coroutine_function`, which
  unwraps a `functools.partial` and then a bound method by exact type and reads `CO_COROUTINE`
  from the function's code, so a hosted proxy among the arguments is never asked anything;
  `inspect.iscoroutinefunction` would read `__class__` and `__func__`. A callable it does not see
  through -- a bound method of a partial, an object with an `async __call__` -- is caught where
  the host calls it: a native coroutine it returned is closed and the host gets the `TypeError`.
- **A never-returning callback at shutdown**, as observed: a function looping on the primary's
  thread, in a call made there, is interrupted by the shutdown's signal, which answers the outer
  call -- `raised`; one looping on a worker cannot be interrupted, and its call ends `unknown`,
  its binding killed. `0003-22` had the coroutine case `raised` by waking the facade's worker,
  which no longer exists.
- **`0003-21`'s tests removed, not rewritten**, since what they tested no longer exists: path
  laziness and its repr, `_resolve`/`create_task`, `gather` of paths, `_sync`, `_isinstance`,
  claims of no machinery type, the executor's workers and the interpreter exiting with idle ones
  (and the test relay's `close_stdin` and `exit_within`, which only it used), a call queued for a
  worker, an interrupt landing in several `gather`ed calls, cancelling the calls made inside an
  awaited call, the coroutine callbacks' serving and their sibling tasks, and the by-value
  `async for` after the close.
- **Where a proxy still behaves unlike the local object**, seen through `run-new` with GitPython
  3.2.0 in an Alpine container, `GIT_PYTHON_REFRESH=quiet`:
  - *Type.* `type(repo)` is `outrig.HostedRoot`; `type(c)` for a commit is RPyC's class,
    `<netref class 'rpyc.core.netref.git.objects.commit.Commit'>`.
  - *`isinstance` and `__class__`.* A proxy's class is fixed when it arrives. Arrived after
    `import git`, `isinstance(c, git.Commit)` holds and `c.__class__` is `git.Commit`. Arrived
    before -- the root, fetched at its first use and kept, among them -- `__class__` is a request
    the binding refuses: `isinstance(repo, git.Repo)` is false for the session, and
    `isinstance(c, git.Commit)` raises `outrig.Refused`, since `Commit` is an ABC whose check
    lets the error through. `plan/next/a-proxys-class-is-fixed-when-it-arrives.md`.
  - *Identity.* `repo.head is repo.head` and `c is repo.head.commit` are false: GitPython builds
    new objects; `c == repo.head.commit` asks the host, and is true.
  - *`repr`, `str`, `format`, `hash`, `dir`* each ask the host and give its answer:
    `<git.Commit "72bf82...">`, the sha, `f'{c}'`, equal hashes for equal commits, `hexsha` in
    `dir(c)`.
  - *Truthiness, `len`, `in`, iteration, indexing* are requests and answer as locally:
    `bool(repo.heads)`, `len(repo.heads)`, `[h.name for h in repo.heads]`, `repo.heads[0].name`.
  - *Values* come back by value: `c.binsha` is `bytes`, `c.parents` a `tuple` of proxies.
  - *Calling.* `callable(repo)` is true; `repo()` raises `TypeError: 'git.repo.base.Repo' object is
    not callable`.
  - *What reads the object's internals* is refused: `vars(c)` raises `TypeError`, `c.__dict__`
    `outrig.Refused`, `copy.copy(c)` the binding's refusal of pickling, and
    `inspect.signature(repo.index.commit)` the refusal of `__func__`, since a bound method's
    proxy claims `types.MethodType` -- `inspect.ismethod` is true of it, and `inspect.isgenerator`
    of a generator's. `help(git.Repo.index)` on the local copy is the orientation's advice.
  - *`help(c)` and `help(repo)`* work, as a run of requests: "Help on git.objects.commit.Commit in
    module rpyc.core.netref object".
  - *Exceptions.* `except git.GitCommandError` catches a failed command and `e.status` reads
    `128`; `str(e)` begins `<Unprintable exception>` and goes on with the host's own line, since
    GitPython's `__str__` reads attributes the rebuilt copy does not carry -- as in `0003-22`.
  - *Missing and private names.* `hasattr` is false for both, each one request.
- **Filed**: `plan/next/an-interrupted-nested-hosted-call-sends-no-cancel.md`, from the plan's
  review -- a call made inside a callback and interrupted sends no cancel line, which matters once
  `0003-24` can hold a request -- and `plan/next/a-proxys-class-is-fixed-when-it-arrives.md`.
- **What else changed with the facade.** `Kernel.interrupt`, `Kernel.cancel` and
  `_ctxexit_args` are what they were before `0003-21` (the signal armed just before it is sent),
  until the review below changed `interrupt`;
  `_close` and `_Pool.close` walk the waits alone; the refusals on the reader thread and in another
  kernel's execution moved into `_Pool.request`, so they cover every request; `_in_agent_task`
  and `Kernel.tasks` read a task's code only from a real coroutine, since `asyncio` will run a
  proxy whose host type claims to be one, and reading its `cr_code` would be a request, from
  inside the signal handler.
- **`/simplify`**: an independent version was written against the same tests, and a review
  judged it simpler -- about half the new logic, one helper where the first had about eight, no
  thread-local, no sets built at load, no class cache. It was taken, with four things from the
  first version that it lacked: `__class__` and `__doc__` asked once (it asked twice), the
  traceback left out without changing the agent's exception and through exception groups (it
  relinked the agent's traceback in place and skipped groups), a weak reference to the stub and
  its `__length_hint__`, and `_code_of`. Two assertions pin the first two:
  `each_operation_is_the_request_rpycs_proxy_sends`'s `class` count and the group and kept
  exception in `a_hosted_exception_is_raised_typed_where_the_call_is_made`.
- **An interrupt never raises inside a hosted call's own work**, from a review of the commit. The
  facade ran the transport on its workers; a synchronous call boxes, sends, waits and reads its
  answer on the kernel's thread, which on the primary is the one SIGINT lands on, and the handler
  raised wherever agent code lay further out. A signal that landed after `serve` had taken a reply's
  frame from the channel and before `_dispatch` popped its request -- in `brine.load`, say -- left
  the request outstanding with its reply already read: the pool never handed that connection out
  again, and four such interrupts left the binding's calls waiting for good. A release registered
  and not yet sent, and the pool's own bookkeeping, were exposed the same way. `_on_sigint` now
  declines wherever `_in_hosted_work` finds, walking out from the signal, a `_hosted_frame` -- the
  frames tracebacks leave out, this program's for the call and RPyC's -- before agent code, unless
  that frame is one of RPyC's request handlers (`_INTERRUPTIBLE_CODE`), beneath which a callback's
  code runs: a callback looping on the primary's thread is still interrupted at once, in a module's
  code too. Declining in a call, it leaves the interrupt on the call's wait, and the call raises it
  at its next check: before it waits, as it wakes, or -- new, in `_Pool.request`'s `finally` -- as
  it ends, saying the call ended first, with any error the call raised as its context. A child's
  interrupt that found its call busy, left by the reader thread, had been dropped when the call
  ended before waiting again, and is raised the same way. Outside a call -- a call taking the pool's
  lock before it has begun, a proxy's finalizer -- the interrupt is dropped, and the host's next one
  reaches the code; installing the wait before taking the lock would change what the lock guards.
  `Hosted._send` left `_MACHINERY`, which the rule covers. `an_interrupt_while_a_reply_is_decoded_*`
  hold a reply in RPyC's `load`, wrapped in code compiled under a name of its own, until the
  interrupt is left; on the committed code the primary's case stranded connection 1 and the child's
  dropped the interrupt.
- **The pool's lock is never left held by an interrupt.** The comparison `/simplify` ran (below)
  found that a first version of the fix, which told a callback's code by a flag on the innermost
  call's wait, raised in `Condition.__enter__` when a callback's own hosted call had taken the
  pool's lock but not yet entered its wait: the outer call's flag still said "callback". The
  `with` never registered its exit, so the main thread kept the lock, and the reader thread stopped
  at the next frame for the pool. The committed code has the same exposure for any call, since its
  handler raised anywhere in the transport. The test
  `an_interrupt_as_a_callbacks_call_takes_the_pools_lock_leaves_the_lock_free` arms the interrupt
  and aims the signal from the lock's `__enter__`, as the reader thread would, and a worker then
  takes the lock: on the committed code and on that first version it cannot.
- **One interrupt is raised once**, from the same review, there as a fault older than this task.
  The reader thread both left a `KeyboardInterrupt` on a busy call and sent the signal, so a
  callback that caught the signal's interrupt and returned got the other as its call next waited,
  and an interrupt the call raised first could be followed by the signal in the agent's cleanup.
  `_wake` now leaves a busy primary thread to the signal alone (`signalled`), which raises or
  leaves the interrupt, and it notifies the pool even when an exception was left already, so a
  second interrupt wakes a call the signal left one on as it went on to wait.
  `an_interrupt_raises_in_a_callbacks_code_once`, its callback compiled as a module's.
- **A refusal of a name is recognized however RPyC rebuilds it**, from the same review, a fault
  `0003-21`'s replay had too. Where a module `outrig` with a class `Refused` imports in the
  container -- the workspace's, say -- RPyC rebuilds the refusal as that class, named `Refused`, not
  as a class of its own named `outrig.Refused`; the name check missed it, and `hasattr` raised. The
  check accepts either name; the test is
  `a_refusal_rebuilt_from_a_local_outrig_module_is_an_attribute_error_too`.
- **The woken-call tests release the call through the binding.** `a_woken_call_runs_on` raised
  the slow call's release file once the interrupted execution's result arrived, but the cancel line
  the call sends ahead of that result reaches the binding through the relay on its own; in one full
  run of about twenty the call returned first and was recorded without the note that its caller
  was interrupted. The release is now a second hosted call, which the binding reads after the
  cancel line and handles only once the cancel is noted. With the cancel taken out of
  `_Pool.request`, the test still fails.
- **`/simplify` on the review's fixes**: an independent version was written from the committed
  code against the same tests, and a review found the two equally complex, each simpler in parts,
  and the first version's lock fault above. Taken from the alternative: telling a callback's code
  by RPyC's handler frames rather than a flag, the end-of-call raise in `_Pool.request`'s `finally`
  with one `stands` flag rather than a method and two call sites, and the one-line refusal check
  rather than a helper that knew how RPyC names its classes. Kept from the first: one route per
  interrupt rather than a token telling the handler whether the reader thread had also left one --
  with which a callback looping on nested calls kept an interrupt on its outer call until it
  returned -- the rule applied whether or not a call's wait is installed, which the alternative did
  only with one, leaving the same lock fault at a call's first `with`; leaving the interrupt on the
  call where `_MACHINERY` declines it too, rather than dropping it; raising at the end only an
  interrupt or a cancel, never a close's `EOFError` under another name; and putting the outer wait
  back before taking what was left.
- **Filed**, seen while testing that: #598. The error that agent code is shown for a host
  exception names RPyC's wrapper class, `_get_exception_class.<locals>.Derived`. `version/0.3.x`
  does the same.
- **An object's own code that boxing runs is interrupted**, from a second review. `_box` turns a
  path into its `__fspath__`, `isinstance` reads a `__class__` property, and RPyC's `get_id_pack`
  reads a callable's attributes. So boxing an argument or a callback's result runs the object's
  own code beneath `_box`, where `_on_sigint` declined: a `__fspath__` that never returned ignored
  every interrupt, each left on a call that never reached its next check, and a cancel does not
  reach it either. `_box` and `get_id_pack` join RPyC's request handlers in `_INTERRUPTIBLE_CODE`
  (`_HANDLER_CODE` until then). An argument's exception comes before anything of its request is
  written: `_async_request` takes back the request's callback and the callables marked for it,
  and no cancel line is needed. A result's exception is the callback's answer, which
  `_dispatch_request` sends before raising it. The test is
  `an_interrupt_raises_in_an_objects_own_code_as_a_call_boxes_it`; on the committed code its
  first interrupt never lands. The other places a call runs an object's own code are filed as
  `plan/next/an-objects-own-code-beyond-boxing-ignores-interrupts.md`: the host reading a
  callable's attribute, `HANDLE_INSPECT`, and a callback's error being boxed. Decoding a host
  exception was filed there too, and taken in a third review, below.
- **A waiting call looks again for an interrupt the signal left**, from the same review. The
  reader thread can find the primary's call busy and leave the interrupt to the signal, and the
  signal can land after the call has gone on to wait. Python runs the handler while the wait's
  lock is being taken. The handler takes no lock, so it leaves the exception without waking the
  wait, and the lock is taken again. The call then slept until the binding answered, and forever
  if the binding never does. A notify sent after `pthread_kill` races the handler the same way,
  and the reader thread cannot wait for the handler to run, since the main thread may stay in a C
  call for any length of time. So `_Wait.block` on the main thread waits at most `SIGNAL_CHECK`
  (0.1 s) at a time and looks again, keeping the caller's timeout. The handler cannot write a
  line either, so `take` now tells the host when a call the signal woke, while it waited for a
  free connection, was never sent, as the reader thread does when it wakes one. The tests
  `an_interrupt_the_signal_leaves_on_a_waiting_call_is_raised_from_the_wait` and
  `a_call_a_signal_interrupts_waiting_for_a_connection_is_cancelled` arm and aim the signal once
  the call waits, as the reader thread would. On the committed code, the first waits for its
  binding's call to return, and the second is never recorded `cancelled`.
- **Decoding a host exception is interrupted**, from a third review. RPyC's `vinegar.load`
  imports the module the host names, then runs the class's `__new__` and each attribute's setter,
  and `_get_exception_class` builds a subclass, which runs `__init_subclass__`. A hook there that
  never returned ignored every interrupt: the first frame outward was RPyC's, which the rule
  declines, and the call never reached the check the interrupt was left for. Both functions join
  `_INTERRUPTIBLE_CODE`. The frames that decode the reply itself, `brine.load` and `_dispatch`,
  stay off it, as `an_interrupt_while_a_reply_is_decoded_*` pins. That is safe because
  `_dispatch` has popped the request, and its callables, before it unboxes: an exception there
  ends the call in the reply's place and leaves the connection free. The test is
  `an_interrupt_raises_in_the_code_that_rebuilds_a_hosted_exception`, a local module's
  `__init_subclass__` that never returns; on the committed code it hangs.
- **A callback's own call is a check for what its outer call was woken with**, from the same
  review. The facade's one wait for a call and its nested steps held an exception left while a
  callback ran, and a nested call saw it. A synchronous call makes a wait of its own, so a cancel
  that found the thread running a callback's code, left on the outer wait, did not stop a call the
  callback then made: a write ran after the cancel was delivered. `_Pool.request` now takes the
  exception from the outer wait when it is a `KeyboardInterrupt` or a `CancelledError`, tells the
  host the call was never sent, as a call woken before it was is, and raises it there, so it is
  raised once. The test is `a_callbacks_call_after_a_cancel_landed_in_its_code_is_never_sent`, a
  callback that waits for the cancel and then touches a file on the host. The lock test above
  gained the same assertion: the callback's call, begun as the interrupt landed, is not sent.
- **`isinstance(stub, cls)` for a class the container cannot import**, from the same review.
  A binding that exposes a class, as `root_type`, hands the agent RPyC's proxy of the class. For
  a proxy of the root, RPyC compares identities; for any other object it compares the local class
  the proxy stands for, which a class the container cannot import has none of, so a stub, which is
  not a proxy, raised `AttributeError` where the root's own proxy answered `True`. The connection
  class wraps `BaseNetref.__instancecheck__` and swaps a stub for its root's proxy first. Done
  before the request, not in `_box`, since a same-host answer sends none. The fixture's `Root`
  gained `root_type`, for `a_binding_is_an_instance_of_the_class_its_member_names`.
- **Tracebacks leave hosted frames out through `BaseException`'s descriptors**, from the same
  review. `_without_hosted_frames` assigned `__traceback__` on every exception it met, so a frozen
  dataclass's `__setattr__` or a read-only `__traceback__` property raised, and one such cause or
  group member turned the whole report into "its traceback could not be formatted". It now
  changes only an exception that has a hosted frame, and reads, replaces and restores it through
  the descriptor, as `_explain_imports` already reads the chain through `_cause_of`,
  `_context_of` and `_members_of`; `_format_error`'s own read too. An exception's own setter never
  runs. The tests are `a_failure_whose_class_refuses_a_traceback_is_still_reported`, a frozen
  dataclass and a read-only property in one chain, and
  `a_hosted_exception_that_refuses_a_traceback_is_still_reported_without_hosted_frames`, where the
  filter does apply. `#598`, the wrapper class's name in the report, is unchanged.
- **A callback's error is always answered**, from a fourth review, a fault the second bullet above
  made reachable. RPyC boxes the exception a callback raised with `vinegar.dump`, which reads its
  public attributes and so runs the agent's properties. A property that made a hosted call, after
  a cancel landed in the callback, now raised the cancel from inside `dump`, which escaped
  `_dispatch_request` before the answer was sent. The host's call of the callback never ended, and
  under `serialize` it kept the binding's turn from every other connection. A property that
  looped was worse, since `dump` was off `_INTERRUPTIBLE_CODE`: the interrupt was left on a wait
  the primary thread never reached. `_box_callback_error` now catches a `BaseException` from the
  boxing and answers with a `RuntimeError` naming the exception and what failed, built by hand
  with `_raised`, which runs none of the agent's code; an interrupt or cancel that came meanwhile
  is raised after the answer as the callback's own would be, so the pending cancel is still raised
  once. Any other failure there used to close the connection as an "undecodable frame"; it now
  ends only the callback. `vinegar.dump` joins `_INTERRUPTIBLE_CODE`, which also covers the
  exception a proxy's `__exit__` sends, boxed before its request is written. Each of these tests
  fails on the committed code:
  `a_cancel_landing_as_a_callbacks_error_is_boxed_still_answers_the_host` waits on a second
  kernel's call until it times out; the looping getter hangs
  `an_interrupt_raises_in_a_property_read_to_box_a_callbacks_error` and
  `an_interrupt_raises_in_a_property_read_to_send_an_exception_to_exit`; and
  `a_callbacks_error_that_cannot_be_boxed_is_answered_with_what_failed` saw the connection close.
- **No answer the host waits for passes through `dump` unguarded**, from a fifth review. The bullet
  above assumed that an interrupt would land only in an object's own code beneath `dump`. It lands
  anywhere in it: `_agent_code_outward` asks only that agent code come before a frame of the
  machinery, and RPyC's frames are not machinery. The refusal of a callback the host calls after
  its caller gave up -- the request that arrived while the cancel notice was on its way -- boxed a
  fixed `RuntimeError` through `dump`, unguarded, so a signal there dropped the answer, and under
  `serialize` the host's call kept its binding's turn from every other connection. Both answers
  now come from `_inert_error`, a tuple built by hand that runs nothing, and the guard keeps the
  rest. The same review found `_box_callback_error` asking `isinstance(v, Exception)` of a
  control exception, which reads the instance's `__class__`: agent code, beneath a frame no
  interrupt ends, and before the guard. A getter that looped could not be interrupted, and one
  that raised lost the answer; the exception's type is asked instead, and its `__class__` is
  read only by `dump`, which can be interrupted and is guarded. The tests, each failing on the
  committed code, are `an_interrupt_cannot_lose_the_refusal_of_a_callback_the_host_calls_late`, a
  signal aimed at the first call of `dump` as the refusal is built, with the notice held back;
  `an_interrupt_raises_in_the_class_of_an_interrupt_a_callback_raised`, which hangs; and
  `a_class_hook_that_raises_on_a_control_error_a_callback_raised_is_answered`. `#598` is
  unchanged.
