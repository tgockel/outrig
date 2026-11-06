# Hosted Objects

> Preview. Bindings belong to the system [Python Runtime](python-runtime.md) describes: the
> agents of `outrig run-new`, of a [host program](../usage/host-programs.md)'s sessions, and of
> the `outrig` library's `harness`. `outrig run` ignores them.

A **binding** is an object a process on the host holds for a session's agents, which their Python
uses under a name: a GitPython `Repo` as `repo`, say. The object stays on the host and acts there,
with the host user's authority. The agent's code holds a proxy of it, and each thing the code does
with the proxy -- an attribute read, a call, one step of an iteration -- is a request the host
runs and the session publishes as events. A binding is how an agent in the container reaches what
is only on the host: the repository's remotes, your credentials, a service your program holds.

## Factory bindings and live objects

A binding's object comes from one of two places:

- **A factory binding** is declared in the configuration, as a
  [`[bindings.<name>]`](../reference/config.md#bindingsname) table, or by a Rust program that
  embeds OutRig, with `SessionBuilder::bind`. It names the pure-Python packages its library needs
  and a `module:callable` factory with literal arguments. OutRig installs the packages, starts one
  process on the host that holds every factory binding of the session -- before the agent's
  interpreter starts -- calls each factory there, and stops that process, with every program it
  started, when the session ends.
- **A live object** is one a [host program](../usage/host-programs.md#live-objects) already holds
  and gives its session with `session.inject(name, obj)`. It stays in the program's process,
  which serves every request for it, to every agent of the session, including agents made later.

Nothing is bound by default.

## Where agent code finds one

In every kernel, a binding is the global of its name, where that name is free: a variable the
agent's code bound, an input of the same name, and a builtin such as `print` keep what they are.
`outrig.bindings[name]` is the binding whatever that global holds now, and is how a module's code,
which has no such global, reaches one. `runtime.bindings` lists each binding's name, description
and host-side type, from what the session said when it started, so reading it sends no request.

The agent is told each binding's name and description in its instructions, and a live object by
its type's qualified name and the first line of its class's docstring. An agent already running
when a live object is injected is told of it in its next tool result. Describing an object runs
none of its code: no `repr()`, no property.

## A call blocks its caller

A binding is used as its library documents: `repo.head.commit.hexsha` is plain Python, three
requests to the host and a value back. Each request holds the thread that made it until the host
answers, and the kernel's event loop with it: while a call runs, the agent's tasks do not run, no
message is delivered, and `runtime.wait` returns on nothing. Other agents' kernels run on.

`await asyncio.to_thread(repo.git.fetch, "origin")` runs a call on a worker thread and keeps the
loop turning, and a long one is best a task the agent waits on with `runtime.wait` and a timeout.
A Ctrl-C ends the caller's wait; a call already running on the host runs on, and its outcome notes
the interrupt.

A library that is not thread-safe is declared with `serialize = true`, or injected with
`serialize=True`, and its calls then run one at a time, every kernel's included.

## What crosses

Arguments, from agent code to the host:

- `None`, `bool`, `int`, `float`, `complex`, `str`, `bytes`, and tuples, frozensets and slices of
  these cross by value.
- A `list`, `dict`, `set` or path is copied, once, in the agent's execution, and arrives as a
  builtin `list`, `dict`, `set`, or the `str` its path is.
- A subclass of one of these crosses as its base value: an `IntEnum` member as its `int`, a named
  tuple as a `tuple`.
- A proxy the binding handed out goes back as the host's own object, when it belongs to the
  calling kernel.
- A callable becomes a callback the host can only call, during the call it was passed to. A
  coroutine function is refused, since the host would never await what it returns.
- Anything else -- a generator, a dataclass instance, any other object -- is refused before it is
  sent, since the host would otherwise hold a reference that runs agent code inside the library's
  call. `list(generator)` is what works.

Results, from the host: values of the by-value types come back as values, and everything else,
lists and dicts included, as another proxy, so iterating a returned list is a request per item. A
proxy is not the object: `type()` and identity differ, `repr`, `str`, `hash` and `dir` each ask the
host, and `isinstance` holds against the library's classes only where the container can import
them -- a binding's packages are mounted read-only under `/outrig/bindings/` for that.

Refused on the host whatever the code sends: a private or dunder name outside a short safe list
(`__iter__`, `__len__`, `__getitem__`, the operators, and a few more), which arrives as an
`AttributeError`; a frame or traceback; pickled data; `format` on a `str`; and a target the
binding never handed out. A binding's object cannot be a child agent's input or a message on a
channel either: those carry data. An exception the host raises crosses as its type, its arguments
and its public attributes, so `except git.GitCommandError` catches it; the host's traceback is in
the session's events, not in what the agent reads.

## The same paths on both sides

When a session has a binding, the workspace and every configured mount appear in the container at
their host paths, so a path the agent's code writes and a path the library reads name the same
file; a configured `container-path` that differs is a config error. A host program's mounts are
always at their host paths. The agent therefore sees host paths, and the host's directory layout
is not a secret from it.

A mount's access limits the container and nothing else. A binding acts on the host, so it can
write a directory the container has only read-only: the mount does not reach the host side.

## The host user's authority

The bindings process runs on the host, as you, in the workspace's host directory, with your
environment less every variable OutRig's configuration names through `${VAR}` -- its provider
keys, build arguments and MCP server environments. One process holds every factory binding of a
session, so every binding's library runs with that one environment, and a credential meant for one
is readable by another. A live object acts with the authority of the program that injected it.

Whatever a binding's library does on the host is part of what the binding grants, including what
the call does not show. With GitPython:

- Host `git` runs programs a repository names -- hooks on commit and push, `core.fsmonitor`, filter
  and diff drivers -- and `.git/` is in the workspace, which the agent can write. So a commit
  through the binding can run a hook the agent wrote, as you.
- `repo.git.execute([...])` runs any command.
- Your `ssh-agent` signs with every key it holds, for anything the binding runs; it cannot export
  a key.
- Your credential helpers return the secrets they hold, to whatever asks them.
- Your environment is readable by any program the library runs.
- OutRig's own files are yours too: the global configuration, the record of approved bindings,
  and the package cache. A binding through which the agent can run host programs is one through
  which it can change what later sessions are granted.

None of this is policed. For a consequential action, bind a narrow object you own rather than the
library that performs it: one whose methods name the effect -- `publish_commit(repo_id, oid,
destination)` -- resolve their identifiers through configuration you wrote, and check the state
they act on when they run. Binding the library itself is a decision to grant your authority with
the session's events as the record.

## The binding is the grant

Having a binding is permission to use it. Every request agent code makes of one runs, with nothing
asked, and every request is published as events on the session's stream:

- `hosted.received` -- as it arrives, before anything is decided: the operation, the member, the
  host-side type, and a preview of each argument, an object of the host's named by its type and a
  number, never by its `repr`.
- `hosted.dispatched` -- before its target runs.
- `hosted.ended` -- how it ended: `returned`, `raised` with the host's traceback, `refused`,
  `cancelled`, or `unknown`, with a reason.
- `hosted.callback` -- each call the library made of a callback.

The events are a record only where something keeps them: `outrig run-new` writes them to
`events.jsonl` under [`[events] mode = "record"`](../reference/config.md#events), a host program
reads them from `session.events()`, and an embedder from its subscriptions. The
[event log](../reference/events.md) lists their fields. No permission finer than the binding
exists in this release.

## A repository's binding needs your approval

A factory is code the host runs as you when the session starts, and a repository's configuration
is content a cloned project brings and the agent can edit through the workspace. So a binding the
repository's config declares starts only once you approve that exact declaration in that
repository: `outrig run-new` shows it whole and asks, and remembers a yes by a digest of the
declaration and the repository's root, so any edit asks again. With no terminal to ask on, the
binding does not start, the session runs without it, and a `binding.refused` event says why. A
binding in your global config, or one an embedder or a host program gives, needs no approval: it
comes from you. A host program's session asks nobody: a repository's binding starts there only if
you approved its exact declaration in that repository before, and is refused otherwise.
[Config -> `[bindings.<name>]`](../reference/config.md#bindingsname) has the details.

The agent cannot add a binding. A binding comes from the configuration, an embedder, or a host
program's `inject`, and no operation agent code can call creates one: the
[Trust Model](mcp-trust-model.md) rule that an agent cannot grow its own environment covers
bindings. It governs what the agent can add, not what a granted binding can do.

## When a binding stops

A factory binding lives as long as its session. When the session closes to new work, a request not
yet sent to the host is refused, raising `outrig.Closed` in its caller, and one already sent is
the binding's to finish. The shutdown waits for those with the agents' executions, then stops the
bindings process -- `SIGTERM` to its process group, a grace, `SIGKILL` -- and reports each call it
cut off as `unknown`: it may have acted. If the bindings process dies while the session runs, the
session goes on without its bindings: the calls it was running end `unknown`, `binding.exited`
says how it ended, and a later call raises an error that says so.

A live object's call still running when its session ends runs on in the program, since nothing can
end a thread: its record ends `unknown`, `still_running`, and the session is not proven stopped. A
program that dies takes its calls with it, and agent code waiting on one is told at once.

## See also

- [Config -> `[bindings.<name>]`](../reference/config.md#bindingsname) -- declaring a factory
  binding.
- [Host programs -> Live objects](../usage/host-programs.md#live-objects) -- injecting one.
- [Python Runtime](python-runtime.md) -- the agents that use them.
- [Trust Model](mcp-trust-model.md) -- the container boundary, and what stays outside it.
- [`SECURITY.md`](https://github.com/tgockel/outrig/blob/trunk/SECURITY.md) -- what is in scope
  as a vulnerability, and what is a boundary by design.
