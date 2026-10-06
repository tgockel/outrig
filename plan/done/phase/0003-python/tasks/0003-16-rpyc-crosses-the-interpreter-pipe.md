# 0003-16 -- RPyC crosses the interpreter pipe; every request is intercepted

## Context

`hosted-objects.md` runs each binding's library in a process on the host and gives agent code
proxies for its objects. RPyC 6.0.2 carries the requests, and its frames are sent as messages of
the protocol the interpreter already speaks, so nothing in the container listens on a socket. The
design was settled from reading RPyC's source, and none of it has run. This task runs the part the
rest depends on: whether every request an RPyC client can send is checked on the host, and what may
cross as an argument.

What RPyC 6.0.2 does, read from the wheel this task pins (`rpyc/core/protocol.py`, `consts.py`,
`brine.py`, `vinegar.py`, `netref.py`, `channel.py`):

- **Dispatch is one table per connection.** `Connection._request_handlers()` maps each of the 20
  `HANDLE_*` constants in `consts.py` to a handler, and `__init__` stores the result as
  `_HANDLERS`. A `Service` names the connection class it builds in `_protocol`, so a subclass can
  replace every entry.
- **Most handlers check nothing.** `getattr`, `setattr`, `delattr`, `callattr`, `cmp`,
  `oldslicing` and `ctxexit` pass through RPyC's attribute check, `_check_attr`. `call`, `repr`,
  `str`, `hash`, `dir`, `inspect`, `instancecheck`, `buffiter` and `del` act on whatever object
  the request names, and `pickle` is refused only while `allow_pickle` is off. Checking attribute
  access alone would leave most of the protocol open.
- **Both published vulnerabilities came from requests written by hand.** CVE-2019-16328 was a
  `cmp` request naming `__getattribute__` as its operator. CVE-2024-27758 was a server calling
  `__array__` on a proxy the client supplied, which fetched pickle bytes from the client and passed
  them to `pickle.loads`. In 6.0.2 `__array__` checks `allow_pickle`, but a proxy's
  `__reduce_ex__` still sends the same request and returns `pickle.loads` with the answer as its
  argument, which `copy.copy` then calls. Agent code runs in the same process as the container side
  of the transport, so it can write frames of its own, and the host's checks have to hold for
  those.
- **What crosses by value is decided by exact type.** `brine.dumpable` accepts `None`, `bool`,
  `int`, `float`, `complex`, `str`, `bytes`, and tuples, frozensets and slices of these, and it
  compares `type(obj)` exactly, so an `IntEnum` member or a named tuple is not among them.
  Everything else is sent as `LABEL_REMOTE_REF`, and the receiver builds a proxy for it in
  `_netref_factory`, which first sends a nested `inspect` request back for the object's method
  names. A host library holding such a proxy runs agent code whenever it uses it: reading an
  attribute, taking its `repr`, iterating it.
- **Defaults that matter here.** `sync_request_timeout` is 30 s. `include_local_traceback` is on,
  so the host's traceback text goes to the client. `import_custom_exceptions` and
  `instantiate_custom_exceptions` are off, so a remote exception becomes a subclass of RPyC's
  `GenericException` unless the receiver may import its module. `allow_pickle` is off. `Channel`
  compresses every frame over 3000 bytes with zlib, and the receiver decompresses it with no limit
  on the result.
- RPyC declares `plumbum` as a dependency. `import rpyc` does not import it; only
  `rpyc/utils/zerodeploy.py` and the registry's command-line tool do.

The container side has no RPyC transport yet. `interpreter.py`'s reader thread reads the host's
NDJSON and routes each line by agent through `_ROUTES`, answering on that thread or queueing work
to a kernel's loop. A hosted call is the first exchange in which a kernel's thread blocks waiting
for an answer that only the reader thread can deliver. Lines are not bounded on the way in -- the
reader uses `readline()` -- and the host reads interpreter lines of at most 16 MiB
(`REPLY_LINE_MAX` in `host.rs`), so a large frame has to be split.

`build.rs` already fetches, checks and embeds one archive, the payload, and `payload.rs` unpacks it
once into the user's cache under the archive's name. RPyC needs the same treatment under a key of
its own: unpacked into the payload's directory, a new RPyC pin would find that directory already
present and keep the old copy.

## Goal

Every request an RPyC client can send is checked on the host, and refused when the check does not
know it, and the rules for what crosses as an argument are settled by tests rather than by
reading. This is a spike: if an acceptance item cannot be met, the task stops and reports to the
maintainer rather than changing the transport or what crosses on its own.

## Deliverables

- **RPyC 6.0.2, vendored through `build.rs`.** The published wheel,
  `rpyc-6.0.2-py3-none-any.whl` with SHA-256
  `8072308ad30725bc281c42c011fc8c922be15f3eeda6eafb2917cafe1b6f00ec`, is fetched, checked and
  embedded as the payload is, and unpacked on first use into a cache directory named for the pin.
  `plumbum` is not vendored. The pin is a row in a table rather than a set of constants, so that
  a later artifact can be pinned the same way. A session mounts that directory read-only in the
  container beside the payload, and the interpreter imports RPyC from it; `0003-21` adds the
  mount, and this task's tests run both sides on the host. The synthesized `outrig` package
  (`0003-24`) holds no vendored code. The interpreter imports RPyC at start, before any agent code
  runs, so `import rpyc` in agent code returns OutRig's copy for the interpreter's life (fork 5).
- **The host program, `crates/outrig/src/python/binding.py`** (fork 1). It imports a factory named
  as `module:callable`, calls it, and serves RPyC over its stdin and stdout with the object the
  factory returned as what `getroot` answers; the connection's RPyC root stays a `Service`. A
  `Connection` subclass replaces all 20 handlers:
  - a handler id outside the table is refused, before anything in the request is unboxed;
  - **the target rule**: every handler but `ping`, `close`, `getroot` and `inspect` acts on an
    object, and that object must arrive as a reference to one this connection handed out. A
    by-value target would let `str.format`, a public method, read attributes through its format
    string; `format` and `format_map` are also refused on a `str` target, since a library can
    return the `str` type itself;
  - **the attribute rule**: a public name -- one without a leading underscore -- may be read,
    written, deleted and called, and any other name is refused unless it is in the pinned
    `safe_attrs`. The rule applies to every handler that names an attribute or an operator, `cmp`
    included -- which takes a comparison operator and nothing else -- and it filters what `dir`
    and `inspect` answer, except that an `inspect` answer for an object the host can call always
    includes `__call__`, which `safe_attrs` lacks. RPyC builds a proxy's class from that answer,
    so without `__call__` no hosted method could be called, and a call is checked by the `call`
    handler in any case;
  - **frames and tracebacks never cross**: a result that is or holds a `types.FrameType` or
    `types.TracebackType` is refused, because a generator's `gi_frame` and a frame's `f_globals`
    are public names that reach the host process's globals;
  - `pickle` is refused, and `allow_pickle` stays off;
  - nothing the peer names is imported: `import_custom_exceptions` and
    `instantiate_custom_exceptions` stay off on the host, and a container exception that is not a
    builtin `Exception` is rebuilt as one plain `Exception` class carrying its name, args and
    public attributes;
  - no host traceback text goes to the container (`include_local_traceback` off), and the binding
    keeps the text for `0003-21`'s event; where the text would go, the host sends its one-line
    rendering of the exception (fork 4);
  - `del` is refused when its count exceeds the references this connection handed out for that
    object, and the object is named by the request's reference rather than read for its id;
  - an argument that arrives as `LABEL_REMOTE_REF` is refused unless the container marked it as a
    callable. A marked one becomes a proxy that supports a call and nothing else -- no attribute,
    no `repr`, no `__reduce_ex__` -- built without the nested `inspect`, never cached, and revoked
    before the reply of the call it was passed to is sent;
  - a callback's return value that arrives as `LABEL_REMOTE_REF` is refused, marked or not, and
    the host's call of the callback raises;
  - compression is off, and a frame flagged compressed closes the connection (fork 2).
- **The container side, in `interpreter.py`:**
  - an `rpc` message kind carrying RPyC frames in both directions, tagged by agent, binding and a
    connection number the kernel assigns; a close notice under the same kind, carrying the reason;
  - a channel for RPyC's `Connection` whose `poll` waits on a condition variable that the reader
    thread signals when a frame arrives, so a kernel's thread waits for its reply without reading
    stdin;
  - **bounded frames**: a frame is split across protocol lines of bounded length and joined on the
    other side, and a frame larger than the frame bound is refused rather than buffered, in either
    direction, by closing the connection with the reason;
  - **the argument shim**: before a request is sent, `list`, `dict`, `set` and `os.PathLike`
    arguments, nested ones included, are copied into a by-value form, which the host rebuilds as
    `list`, `dict`, `set`, and the `str` or `bytes` that `os.fspath` returned; a subclass of a
    by-value type -- an `IntEnum` member, a named tuple, a `str` subclass, an `OrderedDict` --
    is sent as its base value, read through the base type's own method (fork 3). An RPyC proxy
    sends its requests on the connection that produced it (`netref.syncreq`), and this task has
    one connection per kernel and binding, so a proxy goes as itself when it belongs to the
    connection the request is sent on, and is refused when it belongs to another -- another
    binding's, or another kernel's connection to this binding. `0003-17`'s pool widens that to
    any of the same kernel's connections to the binding, through a shared object table. That
    check runs before the callable rule, because a proxy of a host method is callable, and marking
    it would let one binding's call reach another binding's object. A callable is then marked as
    one. Any other object is refused before it is sent;
  - **a callback's return value** goes back to the host through the same shim, by value or as a
    copy; a proxy of this connection's own goes back as itself, and anything else, a callable
    included, is refused, so the host's call of the callback raises;
  - exceptions arrive with their type, `args` and public attributes. The container may import a
    module the host names (`import_custom_exceptions` and `instantiate_custom_exceptions` on, on
    this side only), so `except` can name the library's own class wherever its module imports in
    the container. Where it does not import, the exception arrives as RPyC's generic class with the
    same fields.
- **A test relay**: test code that passes `rpc` lines between an interpreter and one or more
  binding processes, all started on the host as `interpreter_tests.rs` starts the interpreter.
  `0003-21` replaces it with the relay in `host.rs`.
- **Fixtures** from a small local pure-Python library, built into a wheel by the test as
  `pip_installs_a_pure_package_that_imports_at_once` builds one: an object with public and private
  members and a public attribute that may be written and deleted, a nested object, a sequence, a
  context manager, a method that calls a callable it is given and returns the type of what the
  callable returned, a method that returns the types of its arguments, a method with a large
  result, an exception class with a public attribute, and an exception class whose `__str__` reads
  a private attribute, as GitPython's `GitCommandError` reads `_cmdline`.
- **The coverage inventory**, in this task's `## Decisions`: for each of the 20 handlers, what the
  subclass does with it, and whether `0003-21` should record it as a boundary request -- its
  receipt, dispatch and outcome events -- or as bookkeeping, which is recorded only when the
  interception refuses it.
- **The spike's record**, in the same `## Decisions`: the versions involved -- RPyC, the example
  library, Python, git, podman, the kernel and OS, as far as this task used them; where each
  process, mount and credential sits; what ran for real and what was mocked; what went untested;
  and which limits were reached and what happened.

## Acceptance

All through the test relay, with the fixture library in each binding process:

- **The protocol is covered.** A test enumerates `Connection._request_handlers()` and the
  `HANDLE_*` constants of the vendored RPyC, and fails if an id is not handled by the subclass, or
  if an id appears that the inventory does not name.
- **Ordinary use works.** From the container, a public attribute, a nested one, a method call, an
  index, iteration, a `with` block, and a proxy passed back to the host as an argument each give
  the host's answer. Writing and then deleting the fixture's writable public attribute each take
  effect on the host, as a read of it afterwards shows.
- **A client written by hand against the transport is refused, and the target is untouched**, for
  each of:
  - `cmp` naming `__getattribute__` as its operator, CVE-2019-16328's request, and `cmp` naming
    any operator that is not a comparison;
  - a host method that copies or pickles a callback proxy it was given: no `pickle` request reaches
    the container, and nothing the container sends is unpickled on the host -- CVE-2024-27758's
    path, through `__reduce_ex__`;
  - reading `__globals__` from a function and `__mro__` from a type, and calling `__subclasses__`;
  - `inspect` on a callable object with private methods, whose answer holds no name outside the
    public names and `safe_attrs` other than `__call__`, which it does hold;
  - `del` with a count larger than the references handed out, after which the object is still
    reachable through the proxy the client holds;
  - a callback answered with a reference into the container rather than a value: the host's call
    of the callback raises, and nothing is requested of the container through that reference;
  - a handler id outside the table;
  - a by-value target, and `format` on the `str` type;
  - a generator's frame, and a tuple holding one;
  - a compressed frame, after which the kernel's next use says why its connection closed, and the
    next resolution of the binding opens a new one.
- **Containers arrive as builtins.** A `list`, a `dict`, a `set` and a `pathlib.Path` passed to the
  fixture method that reports types arrive as `list`, `dict`, `set` and `str`.
- **A subclass of a by-value type crosses as its base value** (fork 3). An `IntEnum` member, a
  named tuple, a `str` subclass and an `OrderedDict` passed to the fixture method that reports
  types arrive as `int`, `tuple`, `str` and `dict`, with their values.
- **A container object sent by reference is refused.** An instance of a class the agent defined,
  passed as an argument, raises an error naming its type, and the host method is not called. The
  same holds for a hand-written client that skips the shim.
- **Another binding's proxy is refused, though it can be called.** A method proxy from a second
  binding process, passed as an argument, raises an error naming that binding before anything is
  sent, and the host method is not called. So is another kernel's proxy of the same binding.
- **A callback works only during its call.** A host method calls a callable it is given. After the
  method returns, calling the proxy it kept raises on the host, and nothing runs in the container.
  Reading an attribute of the proxy, taking its `repr`, and pickling it each raise during the call
  as well.
- **A callback's return value crosses by the argument rules.** Through the fixture method that
  reports the type of what its callable returned: a `list` arrives as a `list`, and a proxy of
  this connection's own as the host object it stands for; an instance of a class the agent
  defined, and a function, are each refused, and the host method's call of the callback raises.
- **Exceptions arrive typed.** A fixture exception raised on the host is caught in the container by
  its own class, with its `args` and its public attribute, and its text holds no host traceback.
  With the fixture's module not importable in the container, the same exception arrives as RPyC's
  generic class with the same fields.
- **A rebuilt exception says what the host said**, per fork 4. The fixture exception whose
  `__str__` reads a private attribute is raised on the host, and `str(e)` in the container holds
  the line `traceback.format_exception_only` gives for it on the host, and no host frame.
- **The host imports nothing the container names.** A callback raises an exception that claims a
  module which writes a file when imported, a module the binding process could import. No file is
  written, and the library's call of the callback raised an `Exception`.
- **A large result is bounded in transit.** A 64 MiB result of random bytes crosses whole, no
  protocol line in either direction exceeds the line bound, and the interpreter runs under a memory
  ceiling the test sets, as `0003-07`'s tests set one. A frame past the frame bound is refused:
  one the host would return, one written toward the interpreter, and one written toward the
  binding, and the container's own sender refuses one before a line crosses.
- The interpreter started without an RPyC directory, as `host.rs` starts it today, runs as before,
  and says so when a hosted object is asked for.
- A wheel whose digest differs from the pin fails the build before it is unpacked, and a build that
  cannot fetch the wheel degrades as one without the payload does.
- `crates/outrig/public-api.txt` is unchanged: nothing here is public.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The binding program as a second `-c` program, or a mode of `interpreter.py` -- Recommended: a
   second program.** Much of `interpreter.py` acts when the module loads: it moves the protocol
   descriptors aside, points fd 0 at `/dev/null` and fd 1 at stderr, sets the stack size of every
   later thread, and replaces `help`. Its `main` then sets a memory ceiling of half the visible
   memory and installs the SIGINT handler. None of that belongs in a host process, and a mode would
   need a branch in front of each piece. A second program, passed with `-c` as `host.rs` passes
   `interpreter.py`, costs a second copy of a few helpers, such as `_write_all` and the line
   encoding.
2. **Compression on the pipe -- Recommended: off on both sides, and a compressed frame from the
   container refused.** RPyC's `Channel` compresses a frame over 3000 bytes and the receiver
   decompresses it with no limit, so a small compressed frame from agent code could expand to
   gigabytes inside a host process, past any bound applied to what was sent. On a local pipe,
   compression saves nothing worth that. If this is taken, the hand-written-client tests add a
   compressed frame and expect it refused.
3. **A subclass of a by-value type -- an `IntEnum` member, a named tuple, a `str` subclass --
   Recommended: the shim sends its base value.** `hosted-objects.md` leaves this to this task.
   `brine` compares exact types, so without the shim these are sent by reference and refused, and
   every enum flag or named tuple an agent passes fails. Sending the base value -- the `int`, the
   plain `tuple`, the `str` -- is what by value means for them. The cost is that the host receives
   the base type, so a library that checks `isinstance(flag, ItsEnum)` or reads a named field gets
   the plain value. Refusing instead keeps what the host receives exact and makes the agent write
   `.value`.
4. **What a rebuilt exception's text says -- Recommended: the host sends its own one-line rendering
   of the exception where the traceback would go.** The container rebuilds an exception without
   running `__init__` and with public attributes only, and RPyC's `__str__` for it calls the
   class's own `__str__`; one that reads private state -- GitPython's `GitCommandError` reads
   `_cmdline` -- prints as `<Unprintable exception>`. With `include_local_traceback` off, the text
   that would have explained it is gone too. `traceback.format_exception_only` gives the host's
   message without its frames, and costs the model one line.
5. **How an agent's own `rpyc` is kept from replacing OutRig's copy -- Recommended: the
   interpreter imports RPyC from the read-only directory at start, before any agent code runs, so
   `sys.modules` holds OutRig's copy for the interpreter's life.** A later `pip install rpyc` then
   changes nothing inside the interpreter: `import rpyc` in agent code returns OutRig's copy, every
   submodule resolves through that package's own path, and the agent's copy is what a separate
   Python process it starts imports. The cost is that agent code cannot use another RPyC version
   inside the interpreter. The alternative, a private package name, keeps the two copies apart,
   but RPyC's modules import one another by the absolute name `rpyc` -- 83 import lines in 23
   files, with no relative import -- so each would be rewritten, or an import hook would map the
   name. Under either answer, agent code can remove OutRig's copy from `sys.modules`; that breaks
   only its own hosted calls, because no check on the host depends on the container side.

## Dependencies

- **Hard: none.** It builds on what `0003-01` through `0003-11` built: `build.rs`'s fetch and
  digest check, the payload's interpreter run on the host by tests, and the interpreter's protocol,
  reader thread and memory ceiling.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- the transport, the interception rules, what
  crosses, and the alternatives rejected for each.
- `crates/outrig/src/python/interpreter.py` -- `_ROUTES` and `_read`, which gain the `rpc` kind.
- `crates/outrig/build.rs` (`python_payload`, `fetch_python`) and
  `crates/outrig/src/python/payload.rs` (`unpack_once`) -- the fetch, check and unpack this repeats
  for a second artifact.
- `plan/next/hosted-reference-and-payload-bounds.md` -- limits on live references and on a call's
  payload, which the frame bound does not provide.

## Decisions

Every acceptance item was met, so the spike did not stop; what it found beyond the task is
recorded here and became rules. The five forks were taken as recommended, after the maintainer
confirmed each, with fork 3 widened to the subclasses of `list`, `dict` and `set` -- an
`OrderedDict`, a `defaultdict`, a `Counter` -- which the shim copies as plain builtins too.

- **Two paths the attribute rule alone left open, both now denied.** Found while reviewing the
  plan against RPyC's source, each confirmed by running it against the payload Python before the
  rule was written:
  - **A by-value target.** A handler acts on whatever its target unboxes to, and a `str` sent by
    value is a valid target, so a `callattr` of `format` on a format string -- a public name --
    with a host object as the argument reads that object's attributes on the host through
    `{0.attr}` syntax, with no attribute check. Now every handler but `ping`, `close`, `getroot`
    and `inspect` requires its target to arrive as `LABEL_LOCAL_REF`, the reference RPyC's own
    proxies always send; `cmp` takes a comparison operator only; and `format` and `format_map` are
    refused when the target is a `str` or the `str` type, which a library can return.
  - **A frame.** A generator's `gi_frame`, a coroutine's `cr_frame` and a traceback's `tb_frame`
    are public names, and a frame's `f_globals` and `f_builtins` are too, so a hosted generator
    reached `eval` in the binding process. A result that is or holds a `types.FrameType` or
    `types.TracebackType` is refused at every depth of the tuple being boxed. Objects reached
    later through a returned list are boxed, and checked, when they are read.
- **The root stays a `Service`.** RPyC's `_cleanup` calls `_local_root.on_disconnect(conn)`, so
  the factory's object is not the connection's root; the replacement `getroot` returns it.
- **An interrupt inside a callback answers the host, then ends the execution.** RPyC's default,
  `propagate_KeyboardInterrupt_locally=True`, re-raises without replying, which would leave the
  library mid-call for ever, since no timer runs. The container sets it off, sends the exception
  reply, and then raises the `KeyboardInterrupt` or `SystemExit` locally, so the execution ends
  with it as it would without a hosted call. The host rebuilds a container exception that is not a
  builtin `Exception` -- those two included, and any class from a module the container names --
  as `ContainerError`, an `Exception` carrying the name, args and public attributes, so the
  library's `except Exception` cleanup runs and nothing is imported to find out more.
- **Callback proxies are never cached**, since an outer request and a nested one must not share a
  proxy whose revocation ends both, and `__call__` checks revocation and queues its request under
  one lock that `revoke` also takes, before the reply is sent: no call is queued after revocation,
  and one queued before precedes the reply on the wire, so a host thread that called just in time
  gets its answer while the kernel still serves the connection. A worker thread the library
  started that calls after the reply raises on the host without sending.
- **`ctxexit` sends the exception by value.** A proxy's `__exit__` sends only the exception's
  type. While a `with` block unwinds, `sys.exception()` is the exception, so the shim sends its
  type, args and public attributes when its type is the one sent, the type alone when it is not
  -- an `ExitStack` after an earlier callback raised, or a manual `__exit__` -- and `None` for
  `None`. The host rebuilds a builtin as itself, anything else as `ContainerError`, and calls
  `__exit__(type, value, None)`.
- **Nothing reachable from a finalizer does I/O.** `BaseNetref.__del__` sends a `del` request, and
  garbage collection can run it inside `_write_line` while that holds the process-wide send lock,
  which is not re-entrant; the request is queued -- a plain `deque`, no lock -- and sent before
  the connection's next request. RPyC's `close()` makes a synchronous `HANDLE_CLOSE` request of the
  other side, which may be gone, and `Connection.__del__` calls it; both programs override `close`
  to clean up and write nothing. A close *notice* is written by whichever thread decides to close,
  and the reader threads only close a channel, which wakes its waiter; `_cleanup`, which releases
  locks other threads may hold, runs on the serving thread alone. Every path through `_dispatch`
  releases RPyC's receive lock exactly once, including a frame that will not decode, which closes
  the connection with the reason instead of ending its thread.
- **The wire.** An `rpc` line is `{"t":"rpc","agent":A,"binding":B,"id":N,"data":<base64>,
  "more":bool}`, or `{..., "closed":<reason>}`; between the owner and a binding the same lines
  without `binding`. `N` numbers a kernel's connections to one binding from 1 for the kernel's
  life, so a frame for a closed connection is never taken for a new one's; the binding answers one
  with a close notice. Each line carries at most 512 KiB of frame bytes (`PART_MAX`), so a line
  stays under 1 MiB -- `host.rs` reads 16 MiB -- and a frame carries at most 128 MiB of data
  (`FRAME_MAX`), with a connection holding at most twice that unread. RPyC's `!LB` header and
  trailing newline stay on the wire, stripped at part boundaries so joining the parts yields the
  data without another copy, and the header's compressed flag is what refuses a compressed frame.
  A refused frame closes its connection with the reason; the kernel's next use raises `EOFError`
  with it, and `Kernel.hosted` opens a new connection the next time the binding is resolved.
- **The interpreter's source passed the 128 KiB a Linux argument may hold.** `MAX_ARG_STRLEN` is
  32 pages, and `interpreter.py` grew from 110,782 to 137,640 bytes here, so `python3 -c` refused
  it with `E2BIG`. `build.rs` now stages each program as a one-line bootstrap -- the source zlib-
  compressed and base64-encoded inside `exec(compile(zlib.decompress(base64.b64decode(...)),
  "<interpreter>", "exec"))` -- and asserts the result is under the limit, so the build fails
  rather than the first session. The interpreter's bootstrap is 55,050 bytes and the binding's
  15,222. The program runs in `__main__` exactly as the source did; its own frames show
  `<interpreter>` with no source text, and `_format_error` skips them as it skipped `<string>`.
  Should the compressed form approach the limit, the program moves to a mounted file.
- **The wheel travels as a tar.** Its members are deflated, and nothing in the dependency graph
  inflated, so `miniz_oxide` became a build and dev dependency: `build.rs` checks the digest, then
  reads the pinned wheel's zip layout -- one archive, no zip64, no data descriptors, members
  stored or deflated, anything else an error -- and writes the members under the pin's name as a
  zstd-compressed tar that `payload.rs` unpacks under `$XDG_CACHE_HOME/outrig/wheels/<pin>/` with
  the payload's lock and stage-and-rename. `OUTRIG_RPYC_WHEEL` names a local copy, and a fetch
  failure degrades under `OUTRIG_REQUIRE_PYTHON`, so CI changes nothing. Checked by hand, each
  with a rebuild: a wheel with one byte changed fails the build with `sha256 mismatch` before any
  parsing; with the download cache emptied and the proxy pointed at a dead port the build warns
  and embeds an empty tar, and `rpyc_dir()` reports the reason and names `OUTRIG_RPYC_WHEEL`; the
  same build with `OUTRIG_REQUIRE_PYTHON=1` panics; a rebuild with the cache restored and no
  network finds the cached wheel. The binding process writes `__pycache__` into the unpacked
  directory on the host, as any import from a writable directory does; the container's mount will
  be read-only, where Python skips the write.
- **`Refused` claims module `outrig`.** The binding's refusal class sets `__module__ = "outrig"`,
  so the container resolves it to the `outrig` package's class once `0003-24` synthesizes one, and
  until then sees RPyC's generic class named `outrig.Refused` with the reason as its text. A
  hosted exception's `str()` carries RPyC's "Remote Traceback" heading before the host's one line;
  `0003-21` may trim it.
- **The connection class is built by a function of the imported `rpyc`** in both programs, since
  neither can import RPyC at module level: the interpreter imports it in `main` from the directory
  it is given, after the memory ceiling and before `_open_imports` puts the workspace on
  `sys.path`, and the binding from its first argument. The conformance test builds the class the
  same way, without starting a binding.
- **Receiving costs about three copies of a result**, on the kernel thread: the joined frame, RPyC's
  `data[1:]` slice, and brine's read of the value, plus the 32 MiB reserve held while agent code
  runs. The bounds test runs the interpreter under a 512 MiB ceiling and prints the peak resident
  size for the record: 212 MiB after a 64 MiB result, measured on this machine.
- **The coverage inventory**, for `0003-21`'s events:

  | Handler            | What the binding does                                 | Record as    |
  | ------------------ | ----------------------------------------------------- | ------------ |
  | `ping`             | echoes by-value data                                  | bookkeeping  |
  | `close`            | RPyC's cleanup, no reply expected                     | bookkeeping  |
  | `getroot`          | the factory's object                                  | bookkeeping  |
  | `getattr`          | attribute rule, then `getattr`                        | boundary     |
  | `setattr`          | attribute rule, then `setattr`                        | boundary     |
  | `delattr`          | attribute rule, then `delattr`                        | boundary     |
  | `call`             | calls the target with rebuilt arguments               | boundary     |
  | `callattr`         | `getattr` then `call`                                 | boundary     |
  | `repr`             | `repr` of the target                                  | boundary     |
  | `str`              | `str` of the target                                   | boundary     |
  | `cmp`              | comparison operators only, through the type           | boundary     |
  | `hash`             | `hash` of the target                                  | boundary     |
  | `dir`              | `dir`, filtered by the attribute rule                 | boundary     |
  | `inspect`          | method names, filtered, plus `__call__` when callable | bookkeeping  |
  | `instancecheck`    | this connection's objects only, never a new proxy     | boundary     |
  | `buffiter`         | `islice` with an `int` count (size cap: `plan/next`)  | boundary     |
  | `oldslicing`       | attribute rule on both names                          | boundary     |
  | `ctxexit`          | `__exit__` with the by-value exception                | boundary     |
  | `pickle`           | always refused                                        | bookkeeping  |
  | `del`              | refused past the references handed out                | bookkeeping  |

  Bookkeeping is recorded only when refused. `inspect` is bookkeeping because it answers with
  names the host chose to hand out, as a proxy's class is built, and never runs library code
  beyond `dir`-style introspection.
- **The spike's record.**
  - Versions: RPyC 6.0.2 from the pinned wheel; the payload's CPython 3.13.15
    (`python-build-standalone` 20260901, x86_64, `+static`); the fixture library of this task's
    tests and no real library -- no GitPython, no git; no podman, no container; Linux 7.0.0-38 on
    the development machine; Rust 1.99.0.
  - Where things sit: every process on the host, started by the test binary -- the interpreter
    with `-I -c` and the RPyC directory, each binding with `-I -c`, the RPyC directory and the
    fixture's install directory -- in process groups of their own, with `HOME` under the system's
    temporary directory. No mount: the interpreter imports RPyC from the cache directory by path.
    No credential of any kind is involved.
  - Real: both programs as a session will run them, the vendored RPyC as `build.rs` embeds and
    `payload.rs` unpacks it, the fixture installed from a wheel by the payload's pip, and every
    frame crossing both programs' channels. Stood in for: the Rust relay in `host.rs`, by a test
    relay of the same shape (`relay.rs`), and the Rust owner's supervision of binding processes.
  - Untested: an interrupt landing while a kernel waits for a reply -- RPyC's stock path raises in
    the wait and leaves the connection's receive lock held once more by that thread; `0003-17`
    owns interrupts and the pool -- threads of one kernel sharing a connection, aarch64, the
    container mount, `plumbum`-free import inside a container, and a worker thread the library
    started calling back after the kernel stopped serving the connection, which waits until the
    kernel next serves it.
  - Limits reached: a 64 MiB result crossed under a 512 MiB ceiling in 128 parts, the longest line
    about 700 KB; a 128 MiB + 1 byte result, a frame declaring that many bytes toward the
    interpreter, and a 512 KiB + 1 byte part toward the binding were each refused with the bound
    named, and the kernel reopened its connection afterwards. A compressed frame of 5000 bytes
    was refused by the header flag. The interpreter's `-c` argument reached `MAX_ARG_STRLEN`,
    above.
- **Tests.** `relay.rs` starts the processes and relays lines on std threads, as
  `interpreter_tests.rs` does, counting the lines each way and remembering the longest; it can
  write a raw line toward either side. The fixture wheel is installed once per content hash under
  the temporary directory, so a test binary pays pip once and a rerun not at all. `python()` and
  `py()` moved from `interpreter_tests.rs` to `testing.rs`, and its `Interpreter` harness is
  `pub(super)` so one test starts the interpreter without an RPyC directory.
- **Simplified after review.** `/simplify` generated independent versions of the three
  self-contained units and compared them. The wheel reader was replaced by the alternative's:
  one function and a little-endian field reader in place of a parsed-member type and four typed
  readers, with this crate's error wording kept so the tests stood. The frame channel took the
  alternative's shape -- `close` returns the reason so a refusal is `return self.close(...)`,
  `poll` waits on a predicate, the frame is joined before any state changes, and the byte count
  is checked exactly on every part -- but kept its own `send`, which writes from segments rather
  than building the whole frame again, and its `BaseException` guard around a write. The `-c`
  bootstrap stayed on base64: the alternative's hex form needs no encoder, but puts the
  interpreter's bootstrap at 63% of the argument limit against base64's 42%, and the source grew
  25% in this one task. A separate review removed what nothing read: a connection object stored
  beside each channel in both programs, a `names` argument and the `_VoidService` global in the
  binding, RPyC attribute switches that only the replaced handlers consult, and a duplicate of
  the interpreter harness's spawn code in the relay, which now shares `Start`,
  `interpreter_command`, `python_command` and `capture` from `testing.rs`.
- **Six defects found by an external review of the landed commit, each fixed with a test:**
  - A builtin whose `__new__` needs arguments -- an exception group -- cannot be rebuilt by name,
    and `ctxexit` answered `Refused` without calling the library's `__exit__`, leaving whatever
    it held. The rebuild now falls back to `ContainerError`, and `__exit__` runs with it; a
    `UnicodeDecodeError`, which checks its arguments in `__init__`, rebuilds as itself.
  - A callback the library kept, read back after its request returned, was boxed as a reference
    the container had already released; the container's unboxing failed and closed the
    connection, every other proxy with it. Boxing a revoked callback is refused, and a reply the
    container cannot rebuild now fails only the call that asked.
  - A frame could be fed in unbounded parts: empty or tiny parts with more to come grew the
    parts list past any byte count, and empty frames queued for free. Every part before a frame's
    last must be full, which bounds a frame to 257 parts, and each frame held is charged 64 bytes
    beyond its data.
  - A reply refused part way -- a tuple holding a frame after an object, or one past the frame
    bound -- had already registered its earlier objects in the connection's table, where they
    stayed until the connection closed. Every reference a message hands out is recorded while it
    is boxed and taken back when the message is not sent, callback arguments included.
  - A write that failed part way closed the interpreter's channel and told the binding nothing,
    which kept the connection's thread, partial frame and objects until the session ended; the
    interpreter now sends a best-effort close notice when a send tears.
  - `vinegar` sends every public attribute of an exception, a method's repr included, and setting
    `add_note`'s repr on the rebuilt exception shadowed the method; a library or agent calling
    `add_note` then raised `TypeError`. Method attributes are dropped in both directions.
- **Two more from the review's second pass.** RPyC's `_send` appends to a queue that whichever
  thread holds the send lock drains, so a frame past the bound raised `ValueError` in whatever
  thread happened to send it, and that thread's rollback released references its own reply had
  delivered. Neither side needs the queue -- it exists for a send from a finalizer, and the
  interpreter defers those while the binding holds no proxies -- so both now serialize and write
  their own frame under the lock, and the error lands where the frame came from. And a frame
  whose last part could not be held for want of memory was dropped with the earlier parts kept
  and the caller waiting; the receiving side now tries once more on the reserve, then closes
  the channel with the reason and tells the other side.
- **Filed in `plan/next/`:** `binding-connections-are-uncounted.md` -- the binding starts a thread
  per connection id a kernel presents, with no bound of its own.
