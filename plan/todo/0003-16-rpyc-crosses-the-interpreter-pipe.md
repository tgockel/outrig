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
  (`0003-24`) holds no vendored code. How an agent's own install of `rpyc` is kept from replacing
  this copy is fork 5.
- **The host program, `crates/outrig/src/python/binding.py`** (fork 1). It imports a factory named
  as `module:callable`, calls it, and serves RPyC over its stdin and stdout with the object the
  factory returned as the connection's root. A `Connection` subclass, installed through
  `Service._protocol`, replaces all 20 handlers:
  - a handler id outside the table is refused;
  - **the attribute rule**: a public name -- one without a leading underscore -- may be read,
    written, deleted and called, and any other name is refused unless it is in the pinned
    `safe_attrs`. The rule applies to every handler that names an attribute or an operator, `cmp`
    included, and it filters what `dir` and `inspect` answer -- except that an `inspect` answer
    for an object the host can call always includes `__call__`, which `safe_attrs` lacks. RPyC
    builds a proxy's class from that answer, so without `__call__` no hosted method could be
    called, and a call is checked by the `call` handler in any case;
  - `pickle` is refused, and `allow_pickle` stays off;
  - nothing the peer names is imported: `import_custom_exceptions` and
    `instantiate_custom_exceptions` stay off on the host;
  - no host traceback text goes to the container (`include_local_traceback` off), and the binding
    keeps the text for `0003-21`'s event;
  - `del` is refused when its count exceeds the references this connection handed out for that
    object;
  - an argument that arrives as `LABEL_REMOTE_REF` is refused unless the container marked it as a
    callable. A marked one becomes a proxy that supports a call and nothing else -- no attribute,
    no `repr`, no `__reduce_ex__` -- built without the nested `inspect`, and revoked when the call
    it was passed to returns;
  - a callback's return value that arrives as `LABEL_REMOTE_REF` is refused, marked or not, and
    the host's call of the callback raises.
- **The container side, in `interpreter.py`:**
  - an `rpc` message kind carrying RPyC frames in both directions, tagged by agent and binding;
  - a stream for RPyC's `Channel` whose `poll` waits on a condition variable that the reader thread
    signals when a frame arrives, so a kernel's thread waits for its reply without reading stdin;
  - **bounded frames**: a frame is split across protocol lines of bounded length and joined on the
    other side, and a frame larger than the frame bound is refused rather than buffered, in either
    direction;
  - **the argument shim**: before a request is sent, `list`, `dict`, `set` and `os.PathLike`
    arguments, nested ones included, are copied into a by-value form, which the host rebuilds as
    `list`, `dict`, `set`, and the `str` or `bytes` that `os.fspath` returned. An RPyC proxy
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
  - `cmp` naming `__getattribute__` as its operator, CVE-2019-16328's request;
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
  - a handler id outside the table.
- **Containers arrive as builtins.** A `list`, a `dict`, a `set` and a `pathlib.Path` passed to the
  fixture method that reports types arrive as `list`, `dict`, `set` and `str`.
- **A subclass of a by-value type crosses as fork 3 settles.** An `IntEnum` member, a named tuple
  and a `str` subclass passed to the fixture method that reports types arrive as `int`, `tuple`
  and `str` with the recommendation, or are each refused with an error naming the type with the
  alternative.
- **A container object sent by reference is refused.** An instance of a class the agent defined,
  passed as an argument, raises an error naming its type, and the host method is not called. The
  same holds for a hand-written client that skips the shim.
- **Another binding's proxy is refused, though it can be called.** A method proxy from a second
  binding process, passed as an argument, raises an error naming that binding before anything is
  sent, and the host method is not called.
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
  written.
- **A large result is bounded in transit.** A 64 MiB result of random bytes crosses whole, no
  protocol line in either direction exceeds the line bound, and the interpreter runs under a memory
  ceiling the test sets, as `0003-07`'s tests set one. A frame past the frame bound is refused.
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
