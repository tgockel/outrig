# Hosted objects

How an agent uses a library that runs on the host as an ordinary Python object. The operator
declares a **binding** -- say `repo`, a GitPython `Repo` for the project -- and the agent writes
`repo.index.commit("...")` or `repo.remotes.origin.push()` as anyone using GitPython would. The
`Repo` is real. It runs in a process on the host, against the user's repository, remotes, and
credentials, and the agent holds proxies to it.

This page replaces `pyro-remote-objects.md`. It is the mechanism: what a binding is, where it runs,
how its packages arrive, why paths need no translating, how requests cross, what crosses, and what
the agent sees. `boundary-policy.md` decides what is allowed to cross, `security.md` says why the
host is where it runs and what that grants, and `lifecycle.md` says how it stops. The design was
settled in the planning round of 2026-09-30. The round of 2026-10-09 made hosted calls awaitable
through a facade over the same transport, which `0003-21` built and `0003-22` bound in every
kernel; the review of 2026-10-14 withdrew it, for the reason "Rejected alternatives" records, and
`0003-23` restores the synchronous calls `0003-17` proved ("Calls are synchronous"). `0003-16`
through `0003-18` are spikes that test the riskiest parts first -- `0003-16` ran the transport and
the interception on the host, with both sides started by tests, and its `## Decisions` records
what held -- `0003-20` and `0003-22` build the rest, and `0003-36` adds the live objects a host
program injects ("Live objects").

GitPython is the example throughout, and nothing here is specific to it. No mechanism on this page
names a library, a type, or a parameter: there are no per-library schemas, classification tables,
special cases, or binding kinds. A rule that would only make sense for Git is a rule this design
does not have.

## The shape

```text
  HOST                                          CONTAINER (primary)
  +--------------------------------------+      +----------------------------------+
  | Rust owner                           |  (1) | interpreter process              |
  |   relays frames, one bounded queue   |<---->|   reader thread routes `rpc`     |
  |   per serving process; admission;    |      |   frames to their kernel         |
  |   events                             |      |   kernel `root`:  proxy `repo`   |
  +--------------------------------------+      |   kernel `child`: proxy `repo`   |
         ^                      ^               +----------------------------------+
         | (2)                  | (3)
         v                      v
  +-------------------------+  +----------------------------+
  | bindings process        |  | host program (optional)    |   (1) the interpreter protocol,
  |   payload CPython, -I   |  | `outrig run-new script.py` |       RPyC frames as `rpc` lines
  |   a thread per          |  |   serves its own objects   |   (2) the process's stdin and
  |   Connection, up to 4   |  |   with the same module,    |       stdout
  |   per kernel and        |  |   a thread per Connection  |   (3) the owner connection
  |   binding, each         |  |   notes = Notes("...")     |
  |   intercepting every    |  +----------------------------+   The workspace is mounted at its
  |   request               |                                   host path; the package cache is
  |   repo = git.Repo(...)  |                                   mounted read-only.
  +-------------------------+
```

## A binding

A binding is a host-side record: what to construct -- or, for a live object, which object a host
program holds -- how to describe it to the model, which packages it needs, and which process holds
it. The variable the agent sees is a presentation of
that record and not the record itself. Its identity is assigned by the host and is never taken
from anything the container sends.

It is declared in config:

```toml
[bindings.repo]
description = "The project's Git repository, on the host, with your remotes and credentials."
requires    = ["GitPython==3.2.0"]
factory     = "git:Repo"
args        = [{ path = "." }]
serialize   = true
```

- `description` -- shown to the model beside the name.
- `requires` -- requirement specifiers, installed as "Packages" describes.
- `factory` -- `module:callable`, imported and called in the binding's process at session start.
- `args`, `kwargs` -- literal values passed to the factory. A path is written in the tagged form
  `{ path = "..." }` and reaches the factory as an absolute host path. A bare string is a string.
- `serialize` -- `true` when the library is not thread-safe: the binding then runs one call at a
  time ("Calls are synchronous"). It is `false` when absent. GitPython needs it.

The tag exists for the reason path translation was rejected (below): OutRig cannot tell which
strings are paths, so the operator says. `"."` might be a directory, a branch, or a message.

**Nothing is bound by default.** A session with no bindings starts no bindings process, installs no
package, and mounts its directories where it always did.

Bindings come from four places, ordered as listed and by name within each: the global config, the
repo config, the embedding API (`embedding.md`), and a host program's `session.inject`, which may
arrive after the session has started ("Live objects"). A repo entry replaces a global one of the
same name, as the config's merge rule does for every map, and is then repo-declared. **A
repo-declared binding starts only after the operator approves that exact declaration.** OutRig
prompts once,
remembers the approval by the declaration's digest, and prompts again after any edit. The reason is
what a factory is: code the host runs at session start, as the user, before the agent has done
anything -- `factory = "os:system"` with an argument is a shell command. The repo config,
`.agents/outrig/config.toml`, is repository content. A cloned project brings one, and the agent can
write it through the workspace mount, which is read-write. Without approval, either could choose
what the host runs the next time a session starts. Bindings from the global config or the embedding
API need no approval: they come from the operator or the embedder directly. `0003-20` built the
prompt and the digest store; when nobody can be prompted, the binding does not start and the
session runs without it, saying so.

## Where it runs

One process on the host -- the literal host, not a container -- holds every factory binding of a
session, using the same embedded static CPython the container's interpreter uses; a live object
stays in the host program that injected it ("Live objects"). It is a supervised child of
the Rust owner: started before the interpreter, in a process group of its own, with a parent-death
signal on which it kills that group, so the owner's death ends it and every program its libraries
started. At shutdown the owner kills the group itself. `lifecycle.md` describes the close sequence,
`0003-18` proved the supervision and built it as a crate-private module, and `0003-20` connects it
to the session. The process starts with `-I`, so neither its working directory nor the user site is
on its import path: it imports the standard library, the vendored RPyC ("Transport"), and the
bindings' package directories, and nothing the agent can write from the container. It calls each
binding's factory once, in the bindings' order, and serves each object under its binding's name.

One process for every binding, rather than one per binding, is the maintainer's call of 2026-10-13,
in planning `0003-22`: one process is simpler to start, relay to, account for and stop than one per
binding. What it gives up, to be fixed in pure Python inside the one process should any of it come
to matter:

- **One environment.** Every binding's library runs with the process's environment: under the CLI
  the user's -- the ssh-agent, the credential helpers, the git config -- and under an embedder the
  one it gives the session's bindings (`embedding.md`). An embedder cannot give two bindings
  different credentials.
- **One version of each package.** The process's `sys.path` holds the bindings' package
  directories in the bindings' order, as the container's does ("Packages"), so where two
  requirement sets hold different versions of one package, every binding imports the first one's.
- **One life.** A library that crashes the process, hangs it, or exhausts its memory ends every
  binding at once.

What stays apart without a process per binding: a binding's objects live in tables of their own, one
per binding and kernel, so a proxy from one binding is refused by every other binding's request
("What crosses") as it was when they were separate processes. A serializing binding takes a turn of
its own, so it never waits behind another. A library's resources still end with its session:
GitPython's README says it is "not suited for long-running processes", because it relies on
destructors to release system resources, and suggests running it in "a separate process which can
be dropped periodically", which the process ended with its session is.

Two consequences follow from running the payload on the host:

- **Hosted libraries must be pure Python.** The payload is a static build with no mechanism to load
  an extension module (`discovery.md`), on the host as in the container.
- **Linux hosts only.** The payload is a Linux executable and the parent-death signal is a Linux
  facility. #295 records it.

Why the host and not a sidecar: the repository, its remotes, and the user's credentials are on the
host, and the maintainer chose to run the library where they are rather than hand them to a
container built for it. This reverses the preference `security.md` used to state, and `security.md`
now records the decision and what it grants.

## Live objects

A host program (`embedding.md`, "A Python program as owner") can expose an object it already
holds, after its session has started, with `session.inject(name, obj)` (`0003-36`). The object
stays in the program's process; agents hold proxies to it and use it as "Calls are synchronous"
describes, and `inject` returns once every existing agent, and every agent made later, can reach
it. The program needs no declaration approval, since it is the owner; every operation on the
object is still intercepted and recorded as events, as a factory binding's are.

What differs from a factory binding:

- **Where it is served.** The program's own process, by the serving module `binding.py`'s entry
  point is built from -- the interception, the per-kernel object tables, the serialize turn, the
  records -- on a thread per connection, carried as `rpc` frames on the owner connection. A live
  method that needs the program's asyncio loop uses `asyncio.run_coroutine_threadsafe` itself; an
  `async def` method is not awaited for it.
- **Lifetime.** The object outlives the session in its program. Nothing the engine does ends a
  call running in the program's thread: a thread cannot be killed, and exception injection is
  unreliable in native code and I/O. A live call still running when the session is terminated
  ends its record `unknown`, with the reason that it was still running; the session is not proven
  stopped, and the program's session exit raises `ShutdownException`; the program decides what to
  do with its own thread (`lifecycle.md`). A program the engine had to kill (R6) takes its calls
  with it, `unknown` as a killed binding's are.
- **No install, no mount.** The object's library is the program's; the container imports nothing
  for it, so a proxy's class has no local counterpart for `isinstance` unless the program's library
  is also in the image. Injection adds no mount, and a path a live object returns names a
  container file only where a mount makes it so.
- **The name.** Installed as a convenience global only where the name is free in a kernel, never
  overwriting a name agent code bound, and always reachable as `outrig.bindings[name]`
  ("Presentation"). A duplicate name raises in the program.
- **Authority.** The object acts with the program's authority and in its environment, which is the
  user's ("What a binding grants", `security.md`).

A Rust embedder has no `inject`: its objects are bindings through `bind`, and a Rust object reaches
agent Python as a service (`services.md`, `0003-44`).

## Packages

`requires` is installed with the payload's own pip into a host cache keyed by the requirement set,
`~/.cache/outrig/bindings/<hash>/`: once per set, under a lock, and renamed into place when it is
complete (`0003-18`). Two rules limit what the install does on the host:

- **Wheels only** (`--only-binary=:all:`), with pip's own source-distribution step disabled for
  the run. `--only-binary` constrains what pip resolves from an index, and a wheel may declare a
  dependency by URL on a source archive, which pip would fetch and whose build backend it would
  run to read its metadata. With the step disabled no package's build code runs on the host, and
  such a dependency fails with the reason instead (`0003-18`).
- **Every wheel is pure Python**: its platform tag is `any`, its ABI tag `none`, and its Python tag
  includes Python 3, as in `py3-none-any` or `py2.py3-none-any`. A wheel with any other tags is
  refused with the reason: one with compiled parts could be loaded by neither the bindings process
  nor the container's interpreter. So is a wheel tagged pure Python that carries an extension
  module among its files, since a tag is the publisher's claim.

`requires` holds requirement specifiers -- a name, extras, a version specifier, markers -- and
never a path or a URL: the cache is keyed by the requirement text, and a path whose file changes
would keep serving its first install. The install reads the user's pip configuration, so an index
URL, a proxy or a certificate configured for pip applies to it; the key stays the requirement set,
so changing the configured index reinstalls nothing (`0003-18`).

GitPython 3.2.0 passes: it and its two dependencies, `gitdb` and `smmap`, are all `py3-none-any`. A
pure-Python package published only as a source distribution does not, and cannot be hosted until a
wheel of it exists. The cache is keyed by the requirement text, so an unpinned requirement resolves
once and stays resolved until its cache entry is removed.

The same directory is mounted read-only in the container, at `/outrig/bindings/<hash>` beside the
payload rather than at its host path -- podman would make a host path's missing parents, such as
`~/.cache`, root-owned in the container user's home (`0003-20`) -- and added to the agent's
`sys.path`, after the workspace and before what the agent's own `pip` installs, so agent code can
`import git`. That is what makes the library's types useful on the agent's side.
`except git.GitCommandError` catches the host's exception, and `isinstance(c, git.Commit)` holds
for a proxy `c` of a host commit, because RPyC gives a proxy the local class of the same module and
name when that module has been imported in the container. The mount is read-only because the
bindings process imports from the same directory: a writable mount would let the agent edit code
the host runs.

With several bindings, their directories go on the agent's `sys.path` in the order the bindings
have under "A binding". Where two requirement sets hold different versions of one package, the
container imports the earlier binding's version, and so does the bindings process, whose
`sys.path` has the same order. `0003-20` tests the order.

The local import is a second copy of the library, and using it is ordinary container computation.
`git.Repo("/some/path")` in agent code builds a `Repo` in the container, which runs the image's
`git`, if it has one, on the container's files, with none of the host's credentials. Nothing about
it crosses the boundary or is evented. The two are easy to confuse, so the orientation says which
is which: `repo` is the hosted object, and `import git` is a local library.

## Same paths

When a session has bindings, the host directories it mounts -- the workspace and every
`[[workspace.mounts]]` entry -- are mounted in the container at their host paths. A path then means
the same thing on both sides: `repo.working_tree_dir` returns `/home/alice/project`, and
`open("/home/alice/project/README.md")` in agent code opens that file. Nothing is translated, in
either direction, anywhere.

The config follows from that. With bindings, an unset `container-path` takes the host path, and a
configured one that differs is a config error rather than a remapping (`0003-20`).

**Mount access limits the container and nothing else.** A directory mounted read-only cannot be
written by agent code and can be written by a hosted object, which runs on the host as the user and
never sees the mount. What limits a binding is the host user's authority and what was bound, not the
mount table (`security.md`, `boundary-policy.md`).

Two limits, stated rather than implied:

- **Same paths hold under the mounted directories only.** Elsewhere a path names a file on each
  side, and they are different files: a hosted call that writes `/tmp/out.txt` writes the host's,
  and the agent's `open("/tmp/out.txt")` does not see it.
- **The agent sees host paths.** The workspace's own path is one, and a hosted object returns
  others -- a home directory, a config file outside the workspace. The host's layout is not a
  secret under this design.

**Why not translate.** The alternative was a translator between container and host paths, applied
to arguments and results. It cannot be written without knowing which strings are paths:
`repo.index.commit("move /workspace/a to b")` must keep its message byte for byte, while
`repo.index.add(["/workspace/a"])` must have its argument rewritten, and the two look alike to
anything that does not know GitPython's signatures. Knowing them means a schema per library -- per
type, member, and parameter, per version -- which this design rules out. Same paths remove the
question instead of answering it with guesses.

## Transport

Requests cross on **RPyC 6.0.2**, vendored and pinned, without `plumbum`: RPyC declares it as a
dependency and uses it only in its command-line tools and SSH helpers. `build.rs` fetches RPyC,
checks it against a pinned hash, and unpacks it into a cache directory of its own, which binding
processes import from and the container mounts read-only beside the payload (`0003-16`). How agent
code's own install of a package named `rpyc` is kept from shadowing that copy in the interpreter is
a design fork of `0003-16`. Both sides run RPyC in service mode and never in classic mode, which
would give the container the host's interpreter -- `eval`, `execute`, and arbitrary imports.

**There is no socket.** RPyC frames are carried as messages of the interpreter protocol that already
connects the host to the interpreter (`harness-components.md`), as their own message kind, `rpc`.
The Rust owner relays them to the bindings process over its stdin and stdout, through one queue
bounded in bytes, and replies return the same way: a request that does not fit is refused whole,
raising in its caller, and a reply that does not fit closes its connection, since the call waiting
for it cannot be refused (`0003-22`). A frame travels in parts of at most 512 KiB, one per line, so
no protocol line passes 1 MiB; a frame holds at most 128 MiB of data, and one declaring more is
refused rather than buffered, in either direction, by closing the connection it arrived on with the
reason (`0003-16`), as is one declaring none, which RPyC never sends (`0003-22`). The relay holds
frames too, so it holds what the interpreter sends to the same bounds, counting what each line it
queues holds rather than what a label claims, its own lines included: a frame reserves what the
parts its declared size takes hold -- each part's data as text, and a fixed charge for the line --
and one declared past 128 MiB, or whose parts outrun that, in bytes or in number, closes its
connection. An agent holds at most 64 connections to a binding, and at most 1,024 requests that
have not ended, past which another is refused; the relay keeps 256 bytes of a member or target name
a label gives. The relay passes a close on only for a connection the process was sent a part of,
once, and a cancel only to such a connection, naming at most 16 requests, when the queue has room
(`0003-22`). The lines the host queues for the interpreter
are not bounded that way yet: an interpreter that stops reading can grow them
(`plan/next/bound-what-the-host-queues-for-the-interpreter.md`). RPyC's own compression is off on
both sides, and a frame flagged compressed is refused the same way: a small frame could otherwise
inflate without limit inside the bindings process. Only the interpreter can reach a binding: a
program the agent starts inherits neither protocol descriptor, and a forked child closes both, so a
subprocess -- and everything else in the container -- has no channel to a binding at all. Code
inside the interpreter does, which is why enforcement does not trust the container side.

**Connections belong to kernels, up to four per kernel and binding.** RPyC serves an incoming
request on whichever thread is reading the connection when it arrives, so a connection two kernels
shared could run one kernel's callback on the other's thread -- in the wrong namespace, behind the
wrong execution slot. RPyC's own documentation recommends a connection per thread for this reason,
and its thread-binding mode, `bind_threads`, is marked experimental. So no connection is shared
between kernels (`agent-placement.md`), one call is in flight on a connection at a time, and a
kernel opens more than one so that one worker's call delays no other ("Calls are synchronous").
On the host, the bindings process serves each connection on a thread of its own, which is RPyC's
ordinary server model, so requests from different connections run in the library at the same time
unless the binding is declared `serialize = true`. `0003-17` proved the arrangement holds, with
both programs started on the host by its tests; its `## Decisions` record what held.

## Interception

Enforcement is on the host. The bindings process installs a subclass of RPyC's `Connection`
through `Service._protocol`, and the subclass intercepts all 20 request handlers in RPyC's dispatch
table. Every request a kernel sends passes through it before RPyC acts on it, and it enforces
`boundary-policy.md`'s decisions. The rules it applies on its own:

- **An unknown handler is denied.** A conformance test enumerates the pinned table and fails if a
  handler is added, so an RPyC upgrade cannot bring a new request type in unexamined.
- **A request's target is an object the binding handed out.** Every handler but `ping`, `close`,
  `getroot` and `inspect` names the object it acts on, and that object must arrive as a reference
  to one this connection sent by reference. A by-value target would let `str.format`, a public
  method, read attributes through its format string; so would the `str` type itself, which a
  library can return, so `format` and `format_map` are also denied on a `str` target.
- **Members are public or denied.** A public name -- one without a leading underscore -- may be
  read, written, deleted, and called, and each request is recorded. Private and dunder names
  are denied, except those on RPyC's safe list: `__iter__`, `__next__`, `__len__`, `__getitem__`,
  `__contains__`, `__enter__`, `__exit__`, the operators, and a few more. That keeps `__class__`,
  `__dict__`, `__globals__`, `__mro__`, and `__subclasses__` -- the attributes through which one
  object reaches the rest of the host interpreter -- out of reach. A comparison request names
  one of the comparison operators and nothing else.
- **Frames and tracebacks never leave.** A generator's `gi_frame` is a public name, and a frame's
  `f_globals` another, so a result that is or holds a frame or traceback object is denied.
- **Pickle is off.** No request is answered with pickled bytes.
- **A request's number is one the owner can record.** RPyC numbers each request, and the record
  of it carries the number. The relay holds a request it admitted under the number its label
  gives, and sends that number with the frame. A request numbered outside 0 to 2^64 - 1, with the
  number of a request its connection is still serving, or with another than its label's -- or a
  frame labeled as a request that holds a reply -- closes the connection before anything is
  recorded or waited for, and the labeled request ends `refused`. A labeled frame left unread when
  its connection closes ends its request `cancelled`, `disconnected`, so the relay holds nothing
  for a frame that will never be served. A boundary request whose frame carries no label of the
  relay's is refused: run, it would be recorded under whatever request the relay holds by its
  number (`0003-22`).
- **The host never imports a module the container names.** An exception a container callback
  raises reaches the host as itself only if its type is a builtin; any other type arrives as RPyC's
  generic exception class. The container may import a module the host names, which is how the
  host's exceptions arrive typed: the host is the trusted side.

RPyC's own attribute check is not enough, for a reason in its source: many handlers never consult
it. A call, `repr`, `str`, `hash`, `dir`, the method listing a proxy class is built from,
iteration in batches, `isinstance`, and reference counting all act on whatever object the request
names. RPyC has two published vulnerabilities, one of each kind this design guards against.
[CVE-2019-16328](https://nvd.nist.gov/vuln/detail/CVE-2019-16328) was a raw comparison request
naming `__getattribute__`, which the check did not cover.
[CVE-2024-27758](https://nvd.nist.gov/vuln/detail/CVE-2024-27758) was a server calling `__array__`
on a reference the client had supplied, which unpickled bytes the client chose. So interception
covers the handler table rather than the attribute check, and the host holds no general reference
into the container ("What crosses").

**Client-side wrappers are ergonomics only.** Anything installed on the container side -- a
patched proxy class, the stub that fetches a binding's root at first use -- runs in the process the
agent's code runs in, and that code can send raw frames past it. `0003-16`'s acceptance attacks
the host with a raw client for that reason.

## What crosses

Arguments, from the container:

- **Immutable scalars and tuples of them** cross by value, as RPyC sends them: `None`, `bool`,
  `int`, `float`, `complex`, `str`, `bytes`, and tuples, frozensets, and slices of these.
- **`list`, `dict`, `set`, and `os.PathLike`** are copied by a shim on the container side into
  by-value form, which the host rebuilds as builtins: a list arrives as a `list`, and a path as the
  `str` or `bytes` that `os.fspath` returned. The copy is made once, in the agent's own execution,
  before the request is sent. The host never calls back into the container to read an argument,
  and what the events record is what the library receives.
- **A subclass of a by-value type crosses as its base value**, read through the base type's own
  method so that no override runs: an `IntEnum` member arrives as its `int`, a named tuple as a
  `tuple`, a `str` subclass as a `str`, and an `OrderedDict`, `defaultdict` or `Counter` as a
  `dict`. The host receives the plain value, so a library that checks `isinstance(flag,
  ItsEnum)` sees an `int`; refusing instead would have made every flag and named tuple an error
  the agent fixes by hand (`0003-16`, fork 3).
- **A proxy the host handed out** goes back as itself, if it belongs to the kernel whose
  connection the request travels on: the binding keeps one object table per kernel, shared by that
  kernel's connections ("Calls are synchronous"), and resolves it to the original host object. A
  proxy from another binding, or from another kernel, would cross as a reference into the
  container, and is refused. That check runs before the callable rule below: a hosted method is
  callable, and would otherwise cross as a callback.
- **A callable** becomes an opaque proxy that can only be called. It is valid only during the call
  it was passed to, and is revoked when that call returns. When the host library calls it, it runs
  in the container on the thread serving the call's connection -- the kernel's thread, blocked in
  the call, or the worker a `to_thread` call runs on. A coroutine function is refused before the
  call is sent, on any thread: the host calls a callback as a plain function and never awaits
  what it returns ("Calls are synchronous"). Each invocation is a reverse interaction, evented with
  the id of the call it
  ran inside. A hosted call made from inside a callback is an ordinary request on the callback's
  connection, intercepted and recorded (`boundary-policy.md`). What it returns crosses back to the
  host by the same rules as an argument -- by value, or as the shim's copy -- and any other
  reference into the container is refused.
- **Anything else** -- a generator, a dataclass instance, any other container object -- is refused
  before it is sent. Accepting it would give the host a reference into the container, and any use
  of that reference would run agent code inside the host library's call; that is how
  CVE-2024-27758 worked. `list(generator)` is the spelling that works.

Results, from the host:

- Values of the by-value types come back by value.
- Everything else comes back as a proxy, a list or a dict included, so `for` over a returned list
  is a request per item, and `len` of it one more.
- A proxy's class is built from the host's listing of the object's members, which holds its public
  names, those on RPyC's safe list, and -- for an object the host can call -- `__call__`. The
  safe list does not include `__call__`, and a call is a request of its own, recorded like any
  other; without it no proxy of a hosted method could be called, and listing it only for callable
  objects keeps `callable()` on a proxy truthful.
- The same host object returned twice on one connection is the same proxy, and on two of a
  kernel's connections it is two proxies of one object ("Calls are synchronous"); `a == b` asks
  the host either way, and `is` compares the proxies. A library that builds a new object on every
  access yields a new proxy on every access: GitPython builds a new `HEAD` each time `repo.head`
  is read, so two reads of it give two proxies. That is the library's behavior, not the
  transport's.
- A method call is two requests, not one. RPyC's proxy fetches the bound method as a proxy of
  its own, calls that, and releases it afterwards; its one-request `callattr` is sent only for
  the special methods Python looks up on the type -- `__iter__`, `__next__`, `__getitem__` -- so
  iterating a returned list costs one request per item and per field read, and a release per
  proxy. `0003-17` measured both; making an ordinary call one request is
  `plan/next/one-request-per-hosted-method-call.md`.

## Exceptions

A host exception crosses as its type, its args, and its public attributes. The container can
import the type's module from the package mount, so `except git.GitCommandError as e` catches it
and `e.status` reads as it would on the host. A type the container cannot import arrives as RPyC's
generic exception class, carrying the same name, args, and attributes.

One the agent's code does not catch ends its execution, and the error the model reads names the
host's class on its type line -- `outrig_fixture.FixtureError: fixture failed`, or `KeyError: 999`
for a builtin -- whether or not the type's module imports in the container (#598). For RPyC's
generic class, `type(e).__module__` is the host's module, `outrig_fixture`, rather than the
`rpyc.core.vinegar/outrig_fixture` RPyC gives it; `type(e).__name__` is still the dotted
`outrig_fixture.FixtureError`, which is what the result's `raised` reports.

The host's traceback text is kept in the request's outcome event (`boundary-policy.md`) and dropped
from the error the agent sees. It describes the library's internals rather than the agent's code,
and it costs the model context; whoever reads the event has it. In its place the host sends its
own one-line rendering of the exception -- what `traceback.format_exception_only` gives -- so an
exception whose `__str__` reads private state the rebuilt copy lacks, as GitPython's
`GitCommandError` reads `_cmdline`, still says in the container what it said on the host
(`0003-16`, fork 4). RPyC prints that line under its "Remote Traceback" heading in `str(e)`.

Args and attributes carry whatever the library put in them -- a command line, its output, a remote
URL. Nothing scrubs them, for the reason nothing translates paths: no library-neutral rule can tell
a token from text. A projection by audience (`0003-68`) withholds such a field whole from an
embedder's subscription and scrubs nothing: `diagnostics.md`, "Audiences".

## Presentation

Every kernel -- the primary and every child -- gets each binding's name bound to RPyC's proxy of
the binding's root, resolved at first use on the thread that uses it. **Building a kernel resolves
nothing.** Every kernel, the primary's included, is built on the reader thread itself, by `_open`,
when the host opens it (`0003-28`) -- the primary's is then served on the main thread -- and the
reader thread, which delivers every reply, never waits for one; the first operation on a binding's
name opens the kernel's first connection and fetches the root, on the kernel's own thread or on
the worker a `to_thread` call runs on (`0003-23`, fork 1).

The agent learns its bindings three ways, and none crosses the boundary:

- **The orientation** lists each binding's name and description, says that the name is an object
  on the host used as its library documents while an `import` of the same library is a local
  copy, and says that a call on one blocks this agent's loop until the host answers -- a rule
  that cannot be learned by looking, since `repo.head.commit.hexsha` reads like a local attribute
  chain and is three round trips -- with `asyncio.to_thread(...)` as the way to keep the loop
  turning during a long call (`discovery.md`). For a binding whose name a kernel's own code had
  already taken, it names `outrig.bindings[...]` as the way to reach it.
- **`runtime.bindings`** answers from a local manifest: each binding's name, description and
  host-side type. Asking what is bound makes no remote call, runs no `repr`, and reads no
  property.
- **`outrig.bindings[name]`** (`0003-26`) is the authoritative lookup: the calling kernel's proxy of
  the binding, whatever the kernel's globals hold. The convenience global of the binding's name is
  installed only where the name is free, and a live object injected later never overwrites a global
  agent code bound (`0003-36`). In a kernel built from a facility dict (`0003-42`), all three
  present the dict's entries under its keys (`facilities.md`).

A binding name that is not a Python identifier, or that collides with `runtime`, `asyncio`,
`outrig`, or another binding, fails session startup rather than shadowing anything -- or, for a
live object, fails the injection. A relative `{ path }` in a binding's arguments resolves against
the repository's root, whichever layer declared it (`0003-20`). The check runs after the bindings
from the global config, the repo config, and the embedding API are merged, in that order ("A
binding"). By then a repo entry has replaced a global one of the same name, so a collision between
bindings is an embedding-API binding that repeats a name already taken. Rebinding the local name
-- `repo = None`, `del repo` -- changes the namespace and nothing else: the binding, its process,
and its authority are untouched, `runtime.bindings` still lists it, and `outrig.bindings["repo"]`
still returns it.

**Asking about a proxy is a request.** `repr`, `str`, `dir`, `hash` and whatever `help()` reads
cross to the host, as they would call methods of a local object, and each is recorded under its
operation name (`boundary-policy.md`); echoing a proxy as an execution's trailing expression is one
`repr` request, made inside that execution, which `discovery.md`'s rule allows. Automatic
observation never touches one: the inventory, outside every execution, reports the proxy's type
name and makes no request. `help(git.Repo)` on the locally imported class describes the library
without crossing at all, which is what the orientation recommends.

## Calls are synchronous

A hosted object is used as its library documents. An attribute read, a call, item access,
iteration, `len`, truthiness, a comparison, an assignment and a deletion each make their request
when they run, on the thread that runs them, and return the result or raise there:

```python
sha = repo.head.commit.hexsha        # three `getattr` requests
info = repo.remotes.origin.push()    # three `getattr`, a `call`, and a `del` after
for c in repo.iter_commits():        # `__iter__`, then one `__next__` per item
    ...
```

Nothing is awaited, deferred or rewritten: the model writes what the library's own users write,
and learns nothing beyond "this object is on the host and its calls block". That is the decision
of 2026-10-14, made in the review of the host-program requirements (R9), which withdrew the
awaited facade of 2026-10-09 ("Rejected alternatives"); API compatibility decided it, not
performance. `0003-17` proved the mechanism, and `0003-23` binds it under the binding's name.

- **What a call returns.** A result of the by-value types ("What crosses") is the value. Anything
  else is a proxy bound to the host object, used the same way: `c = repo.head.commit`, then
  `c.hexsha`. A tuple comes back as a tuple with proxies inside. A hosted exception is raised
  where the call is made, typed as "Exceptions" says.
- **Iteration is `for`.** One request to start and one per item, as `0003-17` measured, and
  `list(proxy)` materializes; `len`, truthiness, `in` and `==` are each a request too, since a
  proxy cannot know its length or truth without asking.
- **Assignment and deletion are plain.** `x.name = value` sends `setattr`, and `del x.name` sends
  `delattr`.
- **`hasattr` asks the host.** A lookup is a request, so `hasattr(x, "name")` is true or false as
  the host answers, and `getattr(x, "name", default)` chooses its default when the host raises
  `AttributeError` -- the library's own, or OutRig's refusal of a private name, which arrives as
  an `AttributeError` too ("Interception"; `0003-23`).
- **`isinstance` holds** against the library's local class, through the proxy's class descriptor,
  when that module was imported in the container before the proxy arrived ("Packages"): RPyC
  fixes a proxy's class as it unboxes the proxy (`0003-23`'s `## Decisions`).
- **RPyC's request timeout is off.** Its default is 30 seconds, after which the caller stops
  waiting while the host carries on -- an `unknown` outcome chosen by a constant rather than by
  anyone. A push, a clone, or a call waiting on a person can legitimately take longer.

**A call blocks its caller, and only its caller.** The calling thread waits for the host's
answer, so a call made on a kernel's own thread holds that kernel's event loop: no message is
delivered to it, a `runtime.wait` in one of its tasks returns on nothing, and its timers wait,
until the host answers. Other kernels run on, the interpreter's reader thread answers `inv` and
`cpu`, and a callback the host makes into the call runs on the blocked thread
(`execution-and-rounds.md`). The orientation says so plainly, and says what to do about it when it
matters: `await asyncio.to_thread(repo.remotes.origin.push)` runs the call on a worker thread of
the loop's executor, with the whole expression inside the worker, so the loop keeps turning while
the host works. That is ordinary Python, chosen by the caller for a long call, and never required.
A long call that must stay watchable for user input is the retained-task pattern written over it,
`push = asyncio.create_task(asyncio.to_thread(repo.remotes.origin.push), name="push")`, then
`runtime.wait({push})`.

**A kernel has a pool of connections per binding, up to four.** On one connection, one call is in
flight at a time: a callback request arrives on the connection and is served by whichever thread
is reading it, and with one caller waiting that is the thread whose call the callback belongs to.
So a call takes a connection with no call in flight, opens another when none is free and fewer
than four exist, and otherwise waits for one. Two `to_thread` workers' calls from one kernel then
run on the host at the same time, and a call that waits for an hour holds its connection and its
thread, and no other call. An RPyC proxy sends its requests on the connection that produced it,
and the object ids it names are that connection's; the pool works around both. The bindings
process keeps one object table per kernel and binding, shared by that kernel's connections to the
binding, so a proxy resolves on any of them, and the container side sends a proxy's request on
whichever connection its call took ("What crosses"). Every connection is opened by the thread
whose call needs it, never by the reader thread, and a thread waiting for a free connection waits
where the reader thread can wake it, as a thread waiting for its reply does.

**Callbacks run where the call is served.** A plain-function callback runs on the thread serving
the call's connection -- the kernel's thread, blocked in the call, or the worker a `to_thread`
call runs on -- in that call's context, so its output is the calling execution's
(`agent-placement.md`). A hosted call a callback makes goes on the callback's own connection,
which under `serialize = true` is what lets it through the re-entrant turn below, and what gives
it its parent id. A coroutine function is refused as a callback, on every thread, before the
call is sent: the host calls a callback as a plain function, as its library calls any, and a
coroutine it returned would never run (`0003-23`, fork 2).

**The bindings process serves each connection on a thread of its own.** That is RPyC's ordinary
server model -- its `ThreadedServer` starts a thread per connection -- so calls from different
kernels, and from one kernel's workers, run in the hosted library at the same time. A library that
is not thread-safe is declared so, and GitPython is one: its `cmd.py` says that "GitPython isn't
thread-safe".

```toml
[bindings.repo]
serialize = true
```

Under `serialize = true` the binding runs one call at a time across all its connections, in the
order they reached it. A call made inside a callback of the call in progress runs at once: the outer
call waits for the callback, and the callback waits for its request. Nothing special identifies it
-- the callback's request arrives on the outer call's connection and is served on the thread running
the outer call, which holds the binding's lock, and the lock is re-entrant. The lock is taken just
before the target is invoked. That is so for a callback the library calls on the thread running the
outer call. A library that calls it from a thread of its own, and joins that thread, makes the
request away from the lock's holder, and waiting there would be for ever: the holder keeps the lock
until the thread ends. Letting the request through instead would run two threads in a library
declared not thread-safe, so the binding refuses it at once, saying why, and the callback may catch
the refusal (#614). The cost is the one the default avoids: under `serialize = true`, a call that
blocks for an hour holds every other call to that binding for an hour, from every kernel.
`0003-20` delivers the setting; `0003-17` proved both modes, starting the binding program with
`--serialize`, which is how the setting reaches it. The lock is granted in arrival order and is
re-entrant for the thread holding it. A waiter stops waiting, and its request is recorded
`cancelled`, when a cancel names its request or when the binding observes the close; bookkeeping
still waits its turn, since a call the binding was forwarded may need it to finish (`0003-22`). A
live object declares the same with `inject(..., serialize=True)` ("Live objects").

**Why every binding is not serialized.** Until the review of 2026-10-02 this page said the
opposite: one request at a time per binding process, so that no hosted library had to be
thread-safe. RPyC itself is concurrent across connections, so the serialization was this design's
choice, not the transport's. The reviewer raised what it costs a service: a hosted method that
waits for a person -- a `wait_for_answer` that returns when someone answers, hours later -- would
have held every other call to that binding, from every kernel, for as long as it waited, and
running the call in a worker frees the caller's loop and not the binding. The maintainer agreed,
and the rule became the opt-in above, for the libraries that need it.

Nothing coordinates a binding's writes with the agent's, in either mode. Agent code that writes a
file while a hosted call writes the same file -- the repository's index, a file in the working tree
-- is not serialized with that call, and the file holds whatever the two writes leave.

**An interrupt raises where the call is made** -- by waking the calling thread's wait through the
pool's condition variable, on every kernel the primary included, rather than by the primary's SIGINT
path, which could raise between a frame's header and its body (`0003-17`). A call whose target had
been invoked keeps running on the host, because RPyC has no request that cancels one. Its reply,
when it comes, is not handed to anyone, but it is recorded: the call's outcome is `returned` or
`raised`, with the note that the caller had been interrupted, and the connection stays usable --
though not before that reply has arrived: the next call into the pool reads it and drops it, and the
connection is free again. A callback the library makes after its caller was interrupted is refused
the same way, when that next call reads it, so a library that calls back then waits for the kernel's
next call into the pool (`0003-17`). Only a call whose binding is killed or dies before replying is
`unknown`, in the sense `execution-and-rounds.md` gives it: the host cannot say whether it took
effect, and nothing retries it. A call not yet invoked -- waiting for a free connection, or waiting
for the serialize lock -- is `cancelled`, with the reason interrupted, and its target never runs
(`boundary-policy.md`). The interpreter tells the binding with a cancel line, which the relay sends
behind the request it names, so it can reach the binding before the connection's thread has taken
that request: the binding keeps it for the request's receipt while a frame on the connection is
untaken, at most 16 at a time, and closes a connection that would have it keep more (`0003-22`). A
`to_thread` worker's call is not woken: the worker's frames hold no agent code, so cancelling the
task that awaits it ends that wait -- a task's cancellation, the second of the four
`execution-and-rounds.md` distinguishes -- while the worker's call runs on until the host answers,
bounded by the pool (`0003-23`, fork 3); the call itself is the fourth, an accepted remote
operation.

A call made in a worker costs that worker thread for as long as the host takes, which is
`asyncio.to_thread`'s own cost and the caller's choice. Cancelling a call already invoked is
`plan/next/cancel-a-running-hosted-call.md`. A protocol without RPyC, which would remove the pool
and keep this calling convention, is `potential/custom-hosted-object-protocol.md`.

## Lifetime

A factory binding lives as long as its session (`0003-20`): its process starts before the
interpreter and is killed with its process group at shutdown. A live object lives as long as its
program; the session's close ends the agents' access to it and nothing else ("Live objects"). A
proxy is usable while its session lives and not after. Nothing reconnects, nothing rebinds an old
proxy to a new process, and no record of calls outlives the session. A call running at shutdown is
given until the drain deadline, and one still running then is `unknown`, its binding killed -- also
when a program the library forked out of the binding's process group holds the process's output open
past the stop, which the shutdown waits five seconds for before it takes the process as gone
(`lifecycle.md`). A call the owner still holds at the close is never forwarded: one queued in the
relay is refused there, and one still waiting in the container for a free connection is woken with
the closing error. A call the relay had forwarded is its binding's to finish: the binding reads it
ahead of its closed flag and runs it, which may be after the close has returned, unless it is still
waiting for the serialize lock when the binding observes the flag, which cancels it
(`lifecycle.md`). The interpreter's death ends the session, bindings included: it is reported as an
event, and the session then closes and reports as at shutdown
(`continuation.md`, "One continuation: a new session"). An MCP service, which hands out no
proxies, does reconnect for future calls (`mcp-presenter.md`, "Calls").

**A call that outlives its caller runs to completion.** A call still running in its binding when its
kernel is released (`0003-30`, through `Kernel.close_hosted`, which closes the kernel's pool and
wakes every call waiting on it), or when the connection that carried it closes for another reason --
the relay closing it after a reply that did not fit its queue (`0003-22`), the interpreter's death
-- cannot be stopped: RPyC has no cancel, so the binding runs it on its thread until it returns, and
the reply reaches nobody. Its caller is told `unknown`, since its wait ended with no result, while
the call's outcome event records `returned` or `raised` when the reply comes, and `unknown` only if
it never does. From then on no limit counts the call: the kernel's pool, which bounded it at four in
flight, is gone with the kernel; `children-max` stops counting the child once its kernel is gone;
and the agent that released the child is charged nothing. A child that is spawned, awaits in a
background task a call that never returns, and is released, repeated, accumulates threads and calls
in the bindings process for as long as the session lives: the container's thread is woken by
`close_hosted`, and the host's thread runs on. The maintainer documents this as unbounded
for now; bounding it is `plan/next/bound-surviving-binding-calls.md`.

A limit on live references, and on payloads beyond the frame bound, is
`plan/next/hosted-reference-and-payload-bounds.md`.

## What a binding grants

A hosted object acts with the host user's authority -- a factory binding's with the bindings
process's environment, a live object's with its program's. Whatever its library does on the host
is part of the grant, including what the call expression does not show. With GitPython as the
example:

- Host `git` runs programs that repository config names -- `core.fsmonitor` on most commands that
  read the working tree, hooks on commit and push -- and `.git/` is in the workspace, which the
  agent can write. `repo.index.commit("...")` can run a `pre-commit` hook the agent wrote, as the
  user: GitPython starts commit hooks itself, with a copy of its process's environment.
- `repo.git.execute([...])` runs any argv, and its `env` and `shell` arguments pass through.
- The binding inherits the user's ssh-agent and credential helpers, so code in the container can
  push with them, and `git credential fill` prints what a helper holds.
- The reachable objects are the public object graph, not only the root. `repo.GitCommandWrapperType`
  is GitPython's `Git` class, and calling it builds a command wrapper for any directory.

None of this is policed by the transport, and none of it is a defect of the mechanism: it is what
running a full library on the host means. It is documented here and in `security.md`, and the
operator decides what the agent can reach by what is bound: the binding is the grant
(`boundary-policy.md`), so every call runs and is published. Confining what a hosted library's
programs can do on the host is `plan/next/hosted-effect-confinement.md`.

## Rejected alternatives

**Rejected: Pyro5.** The deleted `pyro-remote-objects.md` chose it for being vendorable and for its
default-deny `@expose`. It has no authentication and no per-call hook -- the deleted
`call-inspection.md` needed a second process to add one -- and it lets the client choose the
serializer. Its model is also a service: exposed methods that return data. A full library is
nested objects, references passed back, attributes, iteration, and callbacks, which is what RPyC's
object model carries.

**Rejected: a socket in the container.** A Unix socket bind-mounted into the container needs an
answer to who may connect, and anything in the container can open it, subprocesses included. The
interpreter protocol already exists, belongs to the Rust owner, and is not inherited by
subprocesses.

**Rejected: RPyC classic mode.** It gives the client the host interpreter: `eval`, `execute`, and
arbitrary imports. A binding grants a library's object graph, request by request, where interception
can see and record each one.

**Rejected: wrapper objects on the host.** Wrapping every object the host returns in an
intercepting proxy keeps RPyC's public interface. It breaks identity -- a wrapper passed back must
be unwrapped, and the library's own `isinstance` checks see wrappers -- and it has to anticipate
every object a library can return. Intercepting the handler table covers every request in one
place.

**Rejected: per-type `_rpyc_getattr` and `_rpyc_setattr` hooks.** When the class of the object a
request names defines these methods, or `_rpyc_delattr`, RPyC calls them in place of its attribute
check. Making them the enforcement point would mean adding them to every type of every hosted
library -- wrapping or patching each class a library can return. That works against both aims of
this design: hosting a full library as published, and treating no library specially. The hooks are
also consulted only by the handlers that consult RPyC's attribute check, so they would leave every
other handler unchecked.

**Rejected: client-side proxy patching as enforcement.** It runs in the process agent code runs in,
and raw frames go past it. It is kept for ergonomics.

**Rejected: path translation.** "Same paths" says why.

**Rejected: a sidecar for the trusted side.** The repository, its remotes, and the user's
credentials are on the host. `security.md` records the decision.

**Rejected: a Rust service trait.** Presenting a Rust object to agent Python means writing RPyC's
server side in Rust, or an FFI layer under a Python class. A Rust service can instead ship a
pure-Python client that is hosted like any library. Reversed by `0003-44`: the trait is
`services.md`, a service model with no proxies.

**Rejected (2026-10-14): the awaited facade.** What this page said from the round of 2026-10-09,
and what `0003-21` built: the name bound for a binding was a lazy proxy, attribute access and calls
built a path, and `await` resolved the path on a worker thread, so a kernel's loop kept turning
while the host worked. The review of the host-program requirements rejected it for what it asked
of the model: an OutRig-specific asynchronous rewrite of every library a binding may host --
`await repo.head.commit.hexsha`, `async for`, `await x._set(...)`, `len` and truthiness refused,
`hasattr` always true -- where the model already knows the library as published. The
responsiveness it bought is available to the caller through `asyncio.to_thread`, and the thread
per in-flight call it paid is the one `to_thread` pays. `0003-23` removes it whole; nothing of it
survives as a second spelling.

**Rejected (2026-10-15): coroutine callbacks from a worker's call.** `0003-23` was to keep one
case of the facade's callback serving: a coroutine function passed from a call made in a
`to_thread` worker would run on the kernel's loop, and the hosted calls it made would go on the
worker's connection, for their parent id and the `serialize` turn. That meant the worker and the
loop's thread sharing one connection, and a review of the design against the code found a race
at its start -- the coroutine's first call reaching the pool before the worker had offered the
connection -- calls that took another connection while the worker served a frame, each a
deadlock under `serialize = true`, and a cycle through two serialized bindings that deadlocked
even with those fixed. A synchronous library never awaits a callback, so refusing one is the
library's own convention; used locally, an `async def` callback returns a coroutine that never
runs. The maintainer chose the refusal.

**Rejected: replacing RPyC with an asyncio-native transport.** The survey of 2026-10-09 found no
maintained pure-Python library that is asyncio-native and hosts a transparent object graph: every
transparent-proxy library (RPyC, rpyc-ng, Pyro5, Py4J, `multiprocessing.managers`) is synchronous,
every asyncio-native one (Callosum, jeepney, dbus-fast, aiorpc, autobahn, pycapnp) is a service or
schema model, and the three with the right shape are single-author alpha projects. The structural
reason: attribute syntax cannot be awaited, so `repo.head.commit` is three lookups Python performs
synchronously, and any proxy library chooses between blocking in `__getattr__` and returning a
lazy path that is awaited later. The facade was that lazy path over the transport that exists, and
it is rejected above. RPyC's own `async_` returns an `AsyncResult` that is not awaitable (upstream
tomerfiliba-org/rpyc#506, open since 2022) and needs a thread serving the connection to process
the reply. A protocol of OutRig's own, which would remove the pool and keep the synchronous
calling convention, is `potential/custom-hosted-object-protocol.md`.

**Rejected: a thread that both delivers frames and waits for a reply.** The sketch
`plan/next/awaitable-hosted-calls.md` held, since removed, and the facade's design revisited: the
interpreter's
reader thread, which delivers every frame, processing replies itself. Unboxing a reply can issue a
nested synchronous `inspect` to build a proxy's class (`0003-16`), so a thread that both delivers
frames and waits for one deadlocks; the reader thread must never wait for a reply (`0003-17`). A
call waits on the thread that made it, and the reader thread wakes it.

## Open questions

- ~~What the bindings process dying mid-session does.~~ Settled by `0003-22`, fork 3: its calls
  in flight are `unknown`, the process gone; `binding.exited` records how it ended, as the shutdown
  report's `bindings` does; and the session goes on without its bindings, a later call raising an
  error that names them and how their process ended.
- Whether `for` over a returned container fetches in batches with RPyC's `buffiter`, which would
  make one request per batch rather than per item, and what bounds a raw client's choice of batch
  size (`plan/next/hosted-reference-and-payload-bounds.md`).
- ~~Whether a binding's root is fetched at the first operation on its name or before the kernel's
  first execution.~~ Settled by `0003-23`, fork 1: at the first operation, on the thread making
  it, through a stub that forwards every operation to the proxy.
- Whether four connections per kernel and binding is the right bound. `0003-17`'s measurements of
  two kernels against one binding, and of two sessions against one service, are the first
  evidence.

## Unverified

- `0003-16` exercised these RPyC facts on 6.0.2, with both sides running on the host: the
  20-handler table and its replacement through `Service._protocol`, a proxy sending its requests
  on the connection that produced it (`netref.syncreq`), the object table per connection
  (`_local_objects`) and its reference counts, the by-value types in `brine`, class resolution
  through `sys.modules`, how `vinegar` dumps and rebuilds exceptions, a safe list without
  `__call__`, and the nested `inspect` a proxy's class is built from. `0003-17` exercised a
  request served on whichever thread reads the connection -- a callback runs on the thread whose
  call it belongs to -- and the binding's thread per connection, which the binding program
  starts itself rather than through `ThreadedServer`. Still read rather than exercised: the
  30-second default and `bind_threads` (both turned off in config), the per-thread connection
  advice in `serve_threaded`'s docstring, and the `_rpyc_getattr` hooks consulted where
  `_check_attr` is, which the interception never calls.
- That a kernel's connections share one object table without breaking RPyC's reference
  counting, and that the container side sends a proxy's request on a connection other than its
  own, was reasoned from `_box`, `_unbox` and the `del` handler, and `0003-17` tested each of
  the spike's questions about it: nested callbacks complete with every pooled connection
  occupied; closing one connection drops only its entries while a proxy on another connection
  of the same kernel keeps working; a kernel's pool closed under live worker calls tells their
  callers `unknown`, leaves the binding to finish the calls, and leaves its table gone; and a
  proxy released on one connection frees its object only when no connection holds it. All four
  held, so fork 4's alternative -- one connection per kernel and binding, with replies out of
  order -- was not measured. What stood in for the outcome event, which `0003-22` later built,
  was the binding's record of the finished call and the reply crossing to a closed connection.
- The two CVEs are described from their published advisories. `0003-16` sent each one's request
  by hand -- a comparison naming `__getattribute__`, and a host method copying and pickling a
  callback it was given -- and both are refused; neither was reproduced against stock RPyC.
- The GitPython facts are read from the 3.2.0 wheel: `repo.head` building a new `HEAD` per access,
  commit hooks started with a copy of the environment, `execute` taking `env` and `shell`,
  `GitCommandWrapperType`, `CommandError.__str__`, the docstrings saying a `Git` object is not
  thread-safe, and the README's warning about long-running processes. That GitPython, `gitdb`, and
  `smmap` are `py3-none-any` was checked by downloading them for the payload's platform tag.
- That building a kernel cannot resolve a binding is reasoned from the interpreter's startup order
  and `_open`, not observed.
- The synchronous proxy's claims. `0003-17` confirmed the engine with the fixture library and
  both programs on the host: a call blocks only its caller, a callback runs on the calling
  thread, an interrupt or a cancel wakes a call on any kernel and leaves the connection usable, a
  call woken while waiting for a connection was never sent, and nested callbacks complete with
  every connection occupied. `0003-23` confirmed, with the fixture and then with GitPython through
  `run-new`: a refused private name arriving as an `AttributeError`, one request; `hasattr` and
  `getattr` with a default answering as the host does; `repr`, `str`, `dir` and `hash` each a
  request, `help` a run of them, and the trailing-expression echo of a proxy one `repr` made inside
  its execution; a coroutine callback refused from a call on the kernel's thread and from a
  worker's; and the orientation's description of blocking, against a timer and a `runtime.wait`
  in the blocked kernel, with a second kernel running throughout. Its `## Decisions` list where a
  proxy still behaves unlike the local object.
- What the spikes must confirm before anything is built on this page. A spike that cannot confirm
  its part stops and reports to the maintainer, who decides what follows:
  - `0003-16` -- confirmed: every handler is intercepted, the conformance test fails on a changed
    table, a raw client is refused (a comparison on `__getattribute__`, a proxy pickled,
    `__globals__`, `__mro__`, `__subclasses__`, a method listing of private names, an inflated
    reference count), copied containers arrive as builtins, and a large result is bounded. Its
    `## Decisions` record what else the spike found: a by-value target reaching attributes
    through `str.format`, and a generator's frame reaching the host's globals, both now denied.
  - `0003-17` -- confirmed: another kernel progresses while one blocks, two kernels' calls to
    one binding overlap on the host and never overlap under `serialize = true`, two `to_thread`
    workers' calls from one kernel overlap and each receives its own reply, a proxy's request
    travels on any free connection of its kernel's pool, a callback runs on the calling thread
    and cannot be invoked after its call, an interrupt or a cancel wakes a call on a child's
    kernel and on the primary and leaves the connection usable, a call woken while waiting for
    a connection was never sent, nested callbacks complete with every connection occupied, a
    closed connection takes only its own entries from the kernel's table, a pool closed under
    live worker calls leaves them `unknown` and its table gone, and a proxy released on one
    connection frees its object only when no connection holds it. Its `## Decisions` record
    the measurements of the binding as a service client, and that a method call is two
    requests.
  - `0003-18` -- confirmed: `kill -9` of the owner during a call that started a grandchild
    leaves nothing in the binding's process group within 5 s; an owner killed between the fork
    and the setting of the parent-death signal leaves no binding; a tokio pool thread's exit
    signals nothing the supervisor started; `SIGINT` to the owner's group leaves the binding
    serving; frames for two agents interleave with an event line and a held decision request on
    one binding's stdio; a requirement set installs once; a compiled wheel pip would install, an
    sdist-only requirement and a pure-tagged wheel carrying an extension module are each refused
    with the reason and leave nothing in the cache; a binding that ignores `SIGTERM` is killed
    with its group after the grace and reported empty; the cache imports read-only inside a
    container; and GitPython 3.2.0 installs from the index, pure Python throughout, and a `Repo`
    the binding built answers `head.commit.hexsha`. Its `## Decisions` record what the spike
    found beyond the task.
