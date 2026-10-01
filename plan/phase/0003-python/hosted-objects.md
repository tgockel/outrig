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
settled in the planning round of 2026-09-30 and none of it is built. `0003-16` through `0003-18`
are spikes that test the riskiest parts first; `0003-20` and `0003-21` build the rest.

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
  |   per binding; escalation; events    |      |   frames to their kernel         |
  +--------------------------------------+      |   kernel `root`:  stub `repo`    |
         ^                                      |   kernel `child`: stub `repo`    |
         | (2)                                  +----------------------------------+
         v
  +--------------------------------------+
  | binding process `repo`               |      (1) the interpreter protocol, with
  |   payload CPython, started with -I   |          RPyC frames as `rpc` messages
  |   one Connection per kernel, each    |      (2) the binding's stdin and stdout
  |   intercepting every request         |
  |   git.Repo("/home/alice/project")    |      The workspace is mounted at its
  +--------------------------------------+      host path; the package cache is
                                                mounted read-only.
```

## A binding

A binding is a host-side record: what to construct, how to describe it to the model, which
packages it needs, and which process holds it. The variable the agent sees is a presentation of
that record and not the record itself. Its identity is assigned by the host and is never taken
from anything the container sends.

It is declared in config:

```toml
[bindings.repo]
description = "The project's Git repository, on the host, with your remotes and credentials."
requires    = ["GitPython==3.2.0"]
factory     = "git:Repo"
args        = [{ path = "." }]
```

- `description` -- shown to the model beside the name.
- `requires` -- requirement specifiers, installed as "Packages" describes.
- `factory` -- `module:callable`, imported and called in the binding's process at session start.
- `args`, `kwargs` -- literal values passed to the factory. A path is written in the tagged form
  `{ path = "..." }` and reaches the factory as an absolute host path. A bare string is a string.

The tag exists for the reason path translation was rejected (below): OutRig cannot tell which
strings are paths, so the operator says. `"."` might be a directory, a branch, or a message.

**Nothing is bound by default.** A session with no bindings starts no binding process, installs no
package, and mounts its directories where it always did.

Bindings come from three places, ordered as listed and by name within each: the global config, the
repo config, and the embedding API (`embedding.md`). A repo entry replaces a global one of the same
name, as the config's merge rule does for every map, and is then repo-declared. **A repo-declared
binding starts only after the operator approves that exact declaration.** OutRig prompts once,
remembers the approval by the declaration's digest, and prompts again after any edit. The reason is
what a factory is: code the host runs at session start, as the user, before the agent has done
anything -- `factory = "os:system"` with an argument is a shell command. The repo config,
`.agents/outrig/config.toml`, is repository content. A cloned project brings one, and the agent can
write it through the workspace mount, which is read-write. Without approval, either could choose
what the host runs the next time a session starts. Bindings from the global config or the embedding
API need no approval: they come from the operator or the embedder directly. `0003-20` builds the
prompt and the digest store, and decides what happens when nobody can be prompted.

## Where it runs

Each binding runs in its own process on the host -- the literal host, not a container -- using the
same embedded static CPython the container's interpreter uses. It is a supervised child of the Rust
owner: started before the interpreter, in a process group of its own, with a parent-death signal on
which it kills that group, so the owner's death ends it and every program it started. At shutdown
the owner kills the group itself. `lifecycle.md` describes the close sequence, `0003-18` proves the
supervision, and `0003-20` builds it. The process starts with `-I`, so neither its working directory
nor the user site is on its import path: it imports the standard library, the vendored RPyC
("Transport"), and its package directory, and nothing the agent can write from the container.

One process per binding, rather than one for all of them:

- **Credentials stay per binding.** An embedder supplies an environment for each binding
  (`embedding.md`), and that environment is the only one its process has. The CLI supplies none,
  so under the CLI every binding inherits the user's environment -- the ssh-agent, the credential
  helpers, the git config.
- **Cross-binding references cannot exist.** A proxy from one binding names an object in another
  process, so `repo_b.index.add([blob_from_repo_a])` cannot pass one binding's object to another's
  credentials: it is refused like any other container object ("What crosses"). No rule about
  which binding may use which binding's objects is needed, because none can.
- **A library's resources end with its session.** GitPython's README says it is "not suited for
  long-running processes", because it relies on destructors to release system resources, and
  suggests running it in "a separate process which can be dropped periodically". A process per
  binding, ended with its session, is that.

Two consequences follow from running the payload on the host:

- **Hosted libraries must be pure Python.** The payload is a static build with no mechanism to load
  an extension module (`discovery.md`), on the host as in the container.
- **Linux hosts only.** The payload is a Linux executable and the parent-death signal is a Linux
  facility. `plan/next/macos-host-support.md` records it.

Why the host and not a sidecar: the repository, its remotes, and the user's credentials are on the
host, and the maintainer chose to run the library where they are rather than hand them to a
container built for it. This reverses the preference `security.md` used to state, and `security.md`
now records the decision and what it grants.

## Packages

`requires` is installed with the payload's own pip into a host cache keyed by the requirement set,
`~/.cache/outrig/bindings/<hash>/`: once per set, under a lock, and renamed into place when it is
complete (`0003-18`). Two rules limit what the install does on the host:

- **Wheels only** (`--only-binary=:all:`). pip never builds from source, so no package's build code
  runs on the host.
- **Every wheel is pure Python**: its platform tag is `any`, its ABI tag `none`, and its Python tag
  includes Python 3, as in `py3-none-any` or `py2.py3-none-any`. A wheel with any other tags is
  refused with the reason: one with compiled parts could be loaded by neither the binding process
  nor the container's interpreter.

GitPython 3.2.0 passes: it and its two dependencies, `gitdb` and `smmap`, are all `py3-none-any`. A
pure-Python package published only as a source distribution does not, and cannot be hosted until a
wheel of it exists. The cache is keyed by the requirement text, so an unpinned requirement resolves
once and stays resolved until its cache entry is removed.

The same directory is mounted read-only in the container and added to the agent's `sys.path`, so
agent code can `import git`. That is what makes the library's types useful on the agent's side.
`except git.GitCommandError` catches the host's exception, and `isinstance(c, git.Commit)` holds
for a proxy `c` of a host commit, because RPyC gives a proxy the local class of the same module and
name when that module has been imported in the container. The mount is read-only because the
binding process imports from the same directory: a writable mount would let the agent edit code
the host runs.

With several bindings, their directories go on the agent's `sys.path` in the order the bindings
have under "A binding". Where two requirement sets hold different versions of one package, the
container imports the earlier binding's version, while each binding process imports its own.
`0003-20` tests the order.

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
never sees the mount. What limits a binding is the host user's authority and policy, not the
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

**There is no socket.** RPyC frames are carried as messages of the interpreter protocol that
already connects the host to the interpreter (`harness-components.md`), as their own message kind,
`rpc`. The Rust owner relays them to the binding's process over its stdin and stdout, through a
bounded queue per binding, and replies return the same way. Every frame is bounded, and one over
the bound is refused rather than buffered (`0003-16`). Only the interpreter can reach a binding: a
program the agent starts inherits neither protocol descriptor, and a forked child closes both, so
a subprocess -- and everything else in the container -- has no channel to a binding at all. Code
inside the interpreter does, which is why enforcement does not trust the container side.

**One connection per kernel and binding.** Every kernel (`agent-placement.md`) gets its own
connection to every binding. RPyC serves an incoming request on whichever thread is reading the
connection when it arrives, so a connection two kernels shared could run one kernel's callback on
the other's thread -- in the wrong namespace, behind the wrong execution slot. RPyC's own
documentation recommends a connection per thread for this reason, and its thread-binding mode,
`bind_threads`, is marked experimental. `0003-17` proves the arrangement holds.

## Interception

Enforcement is on the host. The binding process installs a subclass of RPyC's `Connection`
through `Service._protocol`, and the subclass intercepts all 20 request handlers in RPyC's dispatch
table. Every request a kernel sends passes through it before RPyC acts on it, and it enforces
`boundary-policy.md`'s decisions. The rules it applies on its own:

- **An unknown handler is denied.** A conformance test enumerates the pinned table and fails if a
  handler is added, so an RPyC upgrade cannot bring a new request type in unexamined.
- **Members are public or denied.** A public name -- one without a leading underscore -- may be
  read, written, deleted, and called, and policy decides each request. Private and dunder names
  are denied, except those on RPyC's safe list: `__iter__`, `__next__`, `__len__`, `__getitem__`,
  `__contains__`, `__enter__`, `__exit__`, the operators, and a few more. That keeps `__class__`,
  `__dict__`, `__globals__`, `__mro__`, and `__subclasses__` -- the attributes through which one
  object reaches the rest of the host interpreter -- out of reach.
- **Pickle is off.** No request is answered with pickled bytes.
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

**Client-side wrappers are ergonomics only.** Anything installed on the container side -- the stub,
a patched proxy class -- runs in the process the agent's code runs in, and that code can send raw
frames past it. `0003-16`'s acceptance attacks the host with a raw client for that reason.

## What crosses

Arguments, from the container:

- **Immutable scalars and tuples of them** cross by value, as RPyC sends them: `None`, `bool`,
  `int`, `float`, `complex`, `str`, `bytes`, and tuples, frozensets, and slices of these.
- **`list`, `dict`, `set`, and `os.PathLike`** are copied by a shim on the container side into
  by-value form, which the host rebuilds as builtins: a list arrives as a `list`, and a path as the
  `str` or `bytes` that `os.fspath` returned. The copy is made once, in the agent's own execution,
  before the request is sent. The host never calls back into the container to read an argument,
  and what policy sees is what the library receives.
- **A proxy the host handed out** goes back as itself, if it belongs to the connection the request
  travels on: RPyC resolves it to the original host object. A proxy from another binding, or from
  another kernel's connection to this one, would cross as a reference into the container, and is
  refused. That check runs before the callable rule below: a hosted method is callable, and would
  otherwise cross as a callback.
- **A callable** becomes an opaque proxy that can only be called. It is valid only during the call
  it was passed to, and is revoked when that call returns. When the host library calls it, it runs
  in the container, on the thread that made the call, which is blocked waiting for it. Each
  invocation is a reverse interaction, evented with the id of the call it ran inside. A hosted call
  made from inside it is an ordinary request and goes through policy (`boundary-policy.md`). What
  it returns crosses back to the host by the same rules as an argument -- by value, or as the
  shim's copy -- and any other reference into the container is refused.
- **Anything else** -- a generator, a dataclass instance, any other container object -- is refused
  before it is sent. Accepting it would give the host a reference into the container, and any use
  of that reference would run agent code inside the host library's call; that is how
  CVE-2024-27758 worked. `list(generator)` is the spelling that works.

Results, from the host:

- Values of the by-value types come back by value.
- Everything else comes back as a proxy, a list or a dict included, so iterating a returned list is
  a request per item.
- A proxy's class is built from the host's listing of the object's members, which holds its public
  names, those on RPyC's safe list, and -- for an object the host can call -- `__call__`. The
  safe list does not include `__call__`, and a call is a request of its own, decided like any
  other; without it no proxy of a hosted method could be called, and listing it only for callable
  objects keeps `callable()` on a proxy truthful.
- The same host object returned twice is the same proxy. A library that builds a new object on
  every access yields a new proxy on every access: GitPython builds a new `HEAD` each time
  `repo.head` is read, so `repo.head is repo.head` is false. That is the library's behavior, not
  the transport's, and `==` asks the host.

## Exceptions

A host exception crosses as its type, its args, and its public attributes. The container can
import the type's module from the package mount, so `except git.GitCommandError as e` catches it
and `e.status` reads as it would on the host. A type the container cannot import arrives as RPyC's
generic exception class, carrying the same name, args, and attributes.

The host's traceback text is kept in the request's outcome event (`boundary-policy.md`) and dropped
from the error the agent sees. It describes the library's internals rather than the agent's code,
and it costs the model context; whoever reads the event has it.

Args and attributes carry whatever the library put in them -- a command line, its output, a remote
URL. Nothing scrubs them, for the reason nothing translates paths: no library-neutral rule can tell
a token from text.

## Presentation

Every kernel -- the primary and every child -- gets a local stub per binding, bound under the
binding's name, and the stub resolves its binding on the kernel's own thread the first time the
agent uses it. **Building a kernel resolves nothing.** The primary kernel is built before the
reader thread starts, and every other kernel is built on the reader thread itself, by `_open`.
Fetching a binding's root is a synchronous request whose reply only the reader thread can deliver,
so resolving during construction would wait forever in both cases.

The agent learns its bindings two ways, and neither crosses the boundary:

- **The orientation** lists each binding's name and description, and says that the name is an
  object on the host while an `import` of the same library is a local copy.
- **`runtime.bindings`** answers from a local manifest. Asking what is bound makes no remote call,
  runs no `repr`, and reads no property.

A binding name that is not a Python identifier, or that collides with `runtime`, `asyncio`,
`outrig`, or another binding, fails session startup rather than shadowing anything. The check runs
after the bindings from the global config, the repo config, and the embedding API are merged, in
that order ("A binding"). By then a repo entry has replaced a global one of the same name, so a
collision between bindings is an embedding-API binding that repeats a name already taken. Rebinding
the local name -- `repo = None`, `del repo` -- changes the namespace and nothing else: the binding,
its process, and its authority are untouched, and `runtime.bindings` still lists it.

**Asking about a proxy is a request.** `help(repo.index)` and `dir(c)` cross the boundary and are
intercepted, evented, and decided like anything else. So is echoing a proxy as an execution's
trailing expression, which calls `repr` inside the agent's own execution -- the line
`execution-and-rounds.md` draws for rendering. Automatic observation never touches a proxy: the
inventory reports its local class name, which RPyC built when the proxy arrived. `help(git.Repo)`
on the locally imported class answers without crossing at all.

## Calls are synchronous

A hosted call is synchronous, as the library's API is. While it runs:

- **Its kernel's thread is blocked, and so is that kernel's event loop.** Nothing else in that
  kernel runs: its background tasks stall, a message that arrives waits in the queue, and
  `runtime.wait` cannot raise `MessageAvailable` until the call returns. A synchronous
  `subprocess.run` costs the same, and the same remedy applies.
- **Other kernels continue.** The blocked thread waits on a condition variable, which releases the
  GIL (`0003-16`, `0003-17`). A call one of them makes to the same binding waits for this one, as
  below.
- **RPyC's request timeout is off.** Its default is 30 seconds, after which the caller stops waiting
  while the host carries on -- an `unknown` outcome chosen by a constant rather than by anyone. A
  push, a clone, or a call waiting for approval can legitimately take longer.

A long call that should not stop its kernel goes to a worker thread, in plain Python:

```python
info = await asyncio.to_thread(repo.remotes.origin.push)
```

The loop keeps running, messages arrive, and `runtime.wait` works. The worker uses the kernel's
connection, and **calls on one connection are serialized**: a second call from the same kernel
waits for the first. That is deliberate. A callback request arrives on the connection and is served
by whichever thread is reading it; with one caller waiting at a time, that is the thread whose
call the callback belongs to. `0003-17`'s acceptance runs two workers and checks that they never
receive each other's replies.

**A binding runs one request at a time**, across all its connections: a request from another
kernel waits until the one in progress has finished. A hosted library need not be thread-safe, and
GitPython documents that its `Git` objects are not. The exception is a request made inside a
callback of the request in progress, which runs at once, since the outer request cannot finish
until the callback does. `0003-17` carries this as a design fork, with this rule recommended. So a
long call delays every kernel's calls to its binding, but not their other work or their calls to
other bindings.

Nothing coordinates a binding's writes with the agent's. Agent code that writes a file while a
hosted call writes the same file -- the repository's index, a file in the working tree -- is not
serialized with that call, and the file holds whatever the two writes leave.

**An interrupt raises in the caller** -- by waking the waiting call, on every kernel the primary
included, rather than by the primary's SIGINT path, which could raise between a frame's header and
its body (`0003-17`). A call that had been dispatched keeps running on the host, because RPyC has
no request that cancels one. Its outcome is `unknown`, with the reason interrupted, in the sense
`execution-and-rounds.md` gives it: the host cannot say whether it took effect, and nothing retries
it. Its reply, if one comes, is discarded, and the connection stays usable. A call not yet
dispatched -- held for a decision, or queued behind another request -- is `cancelled`, with the
reason interrupted, and its target is never invoked (`boundary-policy.md`). Cancelling the
coroutine that awaits a `to_thread` call cancels a wait, the first of the four cancellations
`execution-and-rounds.md` distinguishes, and the call goes on.

Hosted calls that are awaitable without a worker thread need RPyC to deliver a reply to an event
loop, which it does not do, and are deferred to `plan/next/awaitable-hosted-calls.md`. Cancelling
a dispatched call is `plan/next/cancel-a-running-hosted-call.md`.

## Lifetime

A binding lives as long as its session (`0003-20`): its process starts before the interpreter and is
killed with its process group at shutdown. A proxy is usable while its session lives and not after.
Nothing reconnects, nothing rebinds an old proxy to a new process, and no record of calls outlives
the session. A call running at shutdown is given until the drain deadline, and one still running
then is `unknown`, its binding killed. A call not yet dispatched is never dispatched: one held for a
decision, or read by its binding and queued behind another request, is `cancelled`; one its binding
reads only after the close is `refused`; and one still waiting in the container for its connection's
lock is woken with the closing error (`lifecycle.md`). The interpreter's death ends the session,
bindings included: it is reported as an event, and the session then closes and reports as at
shutdown (`plan/next/interpreter-restart-with-a-reset-notice.md`).

A limit on live references, and on payloads beyond the frame bound, is
`plan/next/hosted-reference-and-payload-bounds.md`.

## What a binding grants

A hosted object acts with the host user's authority. Whatever its library does on the host is part
of the grant, including what the call expression does not show. With GitPython as the example:

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
operator decides which requests run with `boundary-policy.md`, whose default lets every call run
and events it. Confining what a hosted library's programs can do on the host is
`plan/next/hosted-effect-confinement.md`.

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
arbitrary imports. A binding grants a library's object graph, request by request, where policy can
see and decide each one.

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
pure-Python client that is hosted like any library. The trait idea is
`plan/next/rust-object-as-python-object.md`.

## Open questions

- How a relative tagged path resolves. The config's general rule is the directory of the file that
  declared it; `0003-20` states it for `[bindings]`.
- What a binding process dying mid-session does. Its calls in flight are `unknown`; whether the
  session continues without the binding, and how later uses fail, is `0003-20`'s and `0003-21`'s.
- Whether a `str` or `int` subclass -- an enum member, say -- crosses as its base value or is
  refused like other container objects. `0003-16` settles the argument rules.
- How a rebuilt exception describes itself. RPyC rebuilds one without running `__init__`, sends
  only public attributes, and appends the host's traceback to its `str()`. A class whose `__str__`
  reads private state then prints as `<Unprintable exception>` -- GitPython's `GitCommandError`
  reads `_cmdline` and `_cause` -- and the host's message was in the traceback text this design
  drops. Keeping the traceback's last line, which is the host's rendering of the message, is one
  answer; `0003-16` decides.
- Whether the stub iterates a returned container in batches with RPyC's `buffiter`, which would
  make one request per batch rather than per item.

## Unverified

- Every RPyC fact on this page is read from the 6.0.2 source, not exercised: the 20-handler table,
  the handlers that skip `_check_attr`, `Service._protocol`, the 30-second default, `bind_threads`
  marked experimental, a request served on whichever thread reads it, the per-thread connection
  advice in `serve_threaded`'s docstring, the by-value types in `brine`, class resolution through
  `sys.modules`, how `vinegar` dumps and rebuilds exceptions, a safe list without `__call__`, and
  the `_rpyc_getattr` hooks consulted where `_check_attr` is.
- The two CVEs are described from their published advisories and were not reproduced.
- The GitPython facts are read from the 3.2.0 wheel: `repo.head` building a new `HEAD` per access,
  commit hooks started with a copy of the environment, `execute` taking `env` and `shell`,
  `GitCommandWrapperType`, `CommandError.__str__`, the docstrings saying a `Git` object is not
  thread-safe, and the README's warning about long-running processes. That GitPython, `gitdb`, and
  `smmap` are `py3-none-any` was checked by downloading them for the payload's platform tag.
- That building a kernel cannot resolve a binding is reasoned from the interpreter's startup order
  and `_open`, not observed.
- What the spikes must confirm before anything is built on this page. A spike that cannot confirm
  its part stops and reports to the maintainer, who decides what follows:
  - `0003-16`: every handler is intercepted, the conformance test fails on a changed table, a raw
    client is refused (a comparison on `__getattribute__`, a proxy pickled, `__globals__`,
    `__mro__`, `__subclasses__`, a method listing of private names, an inflated reference count),
    copied containers arrive as builtins, and a large result is bounded.
  - `0003-17`: another kernel progresses while one blocks for ten seconds, a callback runs on the
    calling thread and cannot be invoked after its call, two `to_thread` workers never receive
    each other's replies, and an interrupted call leaves the connection usable.
  - `0003-18`: `kill -9` of the owner leaves nothing in the binding's process group, a requirement
    set is installed once, a compiled wheel is refused with the reason, and the cache imports
    inside a container.
