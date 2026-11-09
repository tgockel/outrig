# Host programs

> Preview. The script form is `outrig run-new`'s, the command of the sessions whose agents write
> Python; a later release makes that `outrig run`, and the script form comes with it. Until then
> `outrig run` and `outrig run-legacy` are the interactive session alone, and refuse a script.

A host program is a Python program of your own that OutRig runs on the host and that owns
sessions: it starts each one, drives the agents in it from its own code, hands them objects it
holds, and reads what each session's stop found. The agents act by writing Python in the
session's container, as [`outrig run-new`'s](../reference/cli.md#outrig-run-new) do; the program
has no agent of its own. [Python API](../reference/python.md) lists every name the `outrig`
package gives a program, with its `help()` text, and [Python
Runtime](../concepts/python-runtime.md) describes the agents it drives.

```sh
$ outrig run-new worker.py ./workspace "Fix the tests"
```

runs `worker.py` on the host as `python3 worker.py ./workspace "Fix the tests"` would, under the
Python OutRig carries rather than any on your system. Starting it starts no container and calls
no model; a [session](#sessions) the program enters does.

- **What it sees.** `sys.argv` is the script as you gave it and every argument after it,
  unchanged -- `--help` and OutRig's own flag names included -- and `__name__` is `"__main__"`,
  `__file__` the script, and the working directory, environment, stdin, stdout and stderr yours.
  OutRig reads none of what it prints.
- **What it imports.** The standard library, OutRig's own `outrig` package, and modules beside
  the script. The interpreter runs isolated: neither the working directory, `PYTHONPATH`, nor
  your user site is on `sys.path`. Like the agent's Python, it is a static build, so a package
  with compiled parts does not load.
- **The configuration.** Loaded and validated once, from the directory you ran `outrig` in --
  with `--config` and the global file as for every command -- before the program starts, and
  never again: not from the script's directory. `outrig.config()` is a read-only view of it: each
  model with its provider, the style OutRig calls it with -- its own, else its provider's -- the
  identifier OutRig sends and its declared window, each alias with its candidates in order, each
  provider's style and base URL, `default-model`, the resolved `[workspace] host-path`, each
  `[[workspace.mounts]]` entry, and `[events] mode`. No API key and no `${VAR}` value is in it.
- **Flags.** `--config`, `--global-config`, `--session-root` and `-v` go before the script, and
  so does `--grace`, which is the program's alone (see [Signals and the
  grace](#signals-and-the-grace)). The interactive session's flags -- `--agent`, `--model` and
  the rest -- are refused there, since the program chooses its own session's inputs; written after
  the script, they are the program's arguments.
- **The terminal.** The program does not read the terminal: because its process group is not the
  terminal's foreground group, a read from it fails at once with `EIO` (`input()` raises
  `EOFError`). Input piped or redirected into `outrig run-new` reads as usual, and writing to the
  terminal works.
- **The exit status** is the program's: `0` off the end, `sys.exit`'s value, `1` with Python's
  traceback for an uncaught exception, and 128 plus the signal's number for one a signal ended. A
  session OutRig had to stop for the program that is not proven stopped makes it `3`, whatever the
  program's was: something may still be running (see [What OutRig stops for the
  program](#what-outrig-stops-for-the-program)). A script that cannot be opened as a file, or a
  configuration that does not load, is an `error:` line and status `1`, with nothing started.

## Sessions

A program enters a session with `outrig.Session`, and OutRig starts it then: a container, the
Python interpreter in it, and the bindings the configuration declares, with no agent of its own.
OutRig owns each of those and stops them; the program owns its own control flow.

```python
repo = outrig.Mount("./repo", access="read-write")
async with outrig.Session(mounts=[repo], cwd="./repo") as session:
    ...                         # outrig.current_session() is session here

print(session.shutdown_report)
```

- **Making one starts nothing.** `outrig.Session(mounts=(), cwd=None, grace=None,
  application=None)` checks its inputs and returns. `mounts` are the directories of this host the
  container sees, each an `outrig.Mount(host_path, access="read-only")` or a path, which is
  mounted read-only. Each appears in the container at its own path, so a path names the same file
  in the program and in agent code; nothing else of the host is there. `cwd`, a directory inside
  one of them, is where agent code runs and what its imports find; without it, the image's
  working directory is. Relative paths are taken from the program's working directory when the
  session is made, and a directory given twice, or a `cwd` outside every mount, raises
  `ValueError`.
- **Its application code.** `application=outrig.Application(path, requires=(), imports=())` gives
  the session the package the program's agent classes are in: `path` a directory whose top-level
  entries are packages and modules, `requires` what it imports beyond the standard library and
  `outrig`, written as a binding's `requires` is, and `imports` the modules imported as the
  session starts. Entering copies the directory to a read-only snapshot every kernel imports from,
  ahead of `cwd`, and fails -- naming the module and ending with its traceback -- when a module of
  `imports` does not import, or when the directory does not exist or holds compiled code.
  [Application code](../concepts/python-runtime.md#application-code) says how.
- **The configuration's workspace is not mounted.** `[workspace]` and `[[workspace.mounts]]` are
  the interactive session's; `outrig.config().workspace` and `outrig.config().mounts` show them
  to a program that mounts them itself.
- **Entering it starts it.** It runs in the image `default-image` names -- or OutRig's built-in
  default, as for `outrig run-new` -- with the configured bindings; one a repository's config
  declares starts only if its exact declaration was approved in that repository before, since
  the session asks nobody. A mount that is not an existing directory, or is `/`, fails the entry,
  and so does a `cwd` that is not a directory. An entry that fails raises `RuntimeError` saying
  why, once what the start had made is stopped, and the body does not run.
- **Its events.** `session.events()` is the session's event stream, read with `async for`: each
  `outrig.Event` has the `type`, `subject`, `time` and `data` that [the event
  log](../reference/events.md) records, and an `outrig.Missed` says how many a slow reader lost.
  Reading is never required and never slows the session. With `[events] mode = "record"` the
  stream is also written to `logs/events.jsonl` in the session's directory under the session
  root, `<session-root>/<id>/`. That directory holds no `session.json`, so `outrig ls` does not
  list it.
- **Closing it.** `await session.close_admission()` returns once the session refuses new work: a
  call an agent makes of a binding from then on is refused. The `async with` exit closes
  admission, lets what is running finish for `grace` seconds -- five when it is `None`, an hour at
  most -- and stops the rest. It then sets `session.shutdown_report`, whose `verdict` is `"clean"`,
  `"stopped_with_unknown"` or `"not_proven_stopped"`. For the last, the exit raises
  `outrig.ShutdownException`, its `report` the same report and an exception the body raised its
  `__context__`: something the session started may still be running.

A task reading the events alongside the program's own work:

```python
async def watch(session):
    async for item in session.events():
        if isinstance(item, outrig.Missed):
            print(f"{item.count} events were not read in time")
        else:
            print(item.id, item.type, item.subject)

async def main():
    async with outrig.Session() as session:
        watcher = asyncio.create_task(watch(session))
        ...                     # the program's work with the session's agents
    await watcher               # ends once the session has stopped and its records are read
```

`session.events()` can be called once per session. [The event log](../reference/events.md)
says what each `type` means and what its `data` holds.

## Agent classes

A program drives agents by declaring them as classes, as an agent's own Python does: a subclass
of `outrig.Agent` whose docstring is the agent's instructions, and whose `async def` methods
marked `@outrig.agent` are the requests it answers. An instance constructed inside a session's
block is one agent of that session, whose parent is the program. It keeps its names and its
conversation from one request to the next, and its replies are the program's own classes.

```python
import asyncio
from dataclasses import dataclass
import outrig

@dataclass
class Summary:
    purpose: str
    languages: list[str]

class ProjectReader(outrig.Agent):
    """You read the project in your working directory, and answer questions about it."""

    @outrig.agent
    async def explain(self, question: str) -> Summary:
        """Answer `question` about the project."""
        ...

async def main():
    async with outrig.Session(mounts=["./repo"], cwd="./repo"):
        async with ProjectReader() as agent:
            summary = await agent.explain("What does this project do?")
    print(summary.purpose)

asyncio.run(main())
```

- **Construction starts it in the background.** `ProjectReader(**inputs, model=None,
  session=None)` returns at once. It runs on the event loop that is running, which its requests'
  handles belong to, in `session`, or else the session whose block it runs in; with no session,
  or no running loop, it raises. Each input must be data -- a str, number, bool, `None`, a list
  or str-keyed dict of these, or a dataclass instance -- and is a variable of that name in the
  agent's Python. The instance itself stays in your program: the agent's Python has no `self`,
  and the methods not marked run here. The agent's first round, in which it reads its
  instructions, follows: `await agent.ready()` waits for it, and `async with` waits for it on
  entry and releases the agent on exit. A subclass's own `__init__` calls
  `super().__init__(**inputs, model=...)`. The agent starts only after that `__init__` returns and
  after `__outrig_attach__(self)`, a method the class may define to finish its setup, so either
  can raise without starting anything.
- **Its model.** `model=` names a configured model or alias; without it, `default-model`. No
  `[agents.<name>]` table, `default-agent`, image or preamble applies: the class's docstring is
  the agent's instructions. An unknown model, a full `children-max`, or a session that has closed
  raises at `ready()` and at the first request, not in the constructor.
- **A call sends a request.** `agent.explain(...)` checks its arguments against their types --
  `explain(1)` raises `TypeError` and sends nothing -- and returns a handle at once: `await` it
  for the reply, or pass `handle.future` to `asyncio.wait`. Cancelling the code that awaits it
  leaves the request running; `handle.cancel()` cancels the request. `done()`, `result()` and
  `status` read it without waiting. A request made before the agent exists waits for it, and one
  made while `requests-max` requests are open on the agent raises `outrig.AgentLimitReached`.
- **Replies are your classes.** The types an agent class names reach the agent as descriptions --
  fields, types, `doc()` and `alias()` markers, defaults -- never as code. Each reply comes back
  as JSON and is decoded in your program, into your class. One that does not decode goes back to
  the agent with what is wrong, each problem with its path, and the agent corrects it without
  running anything again; after three, the request raises `outrig.CompletionRejected`.
- **Releasing it.** `await agent.release()`, or leaving `async with`, ends the agent, each request
  still open on it raising `outrig.AgentReleased`; so does dropping the instance, once it is
  collected. Construct agents inside the session's block: leaving the session releases each one
  still alive, ends its open requests with `outrig.SessionClosing`, and lists them in
  `shutdown_report.requests`.

The session's events record such an agent under a subject of its own, `agent/c1` and on, its
parent `owner`, the program (see [the event log](../reference/events.md)). The calls an agent's
own Python makes to launch children and give them work by hand -- `runtime.spawn`, `submit` --
have no counterpart in a program: it constructs agent classes.

## Live objects

A program can hand its agents an object it holds: `session.inject("notes", notes)` serves `notes`
to every agent of the session, those made later too. The object stays in the program; the agents'
Python holds a proxy of it, and each attribute read, call or other operation on it runs in the
program and is recorded in the session's events, as a configured binding's is. Injecting needs
no approval, the program being the session's owner.

```python
class Notes:
    """Notes the program keeps for its agents."""

    def __init__(self):
        self.lines = []

    def add(self, line: str) -> None:
        self.lines.append(line)

async def main():
    notes = Notes()
    async with outrig.Session() as session:
        session.inject("notes", notes)
        async with ProjectReader() as agent:
            await agent.explain("Note what each module does.")
    print(notes.lines)
```

- **Where agents find it.** As `outrig.bindings["notes"]` in every kernel that sees the
  session's bindings -- every one but an agent class's given `facilities=`, and its children --
  and as the global `notes` where that name is free: a variable the agent's code bound, an
  input of that name, and a builtin such as `print` keep what they are. `runtime.bindings`
  lists it. An agent's
  instructions name it with its type's qualified name and the first line of the type's
  docstring, and an agent already running is told of it in its next tool result. Nothing of the
  object itself is read to describe it: no `repr`, no property.
- **`inject` is synchronous**, and returns `None` once every agent can reach the object; before
  entry, the object is served from the session's start. The name must be a Python identifier,
  not a keyword, `runtime`, `asyncio`, `outrig` or a dunder, and neither one already injected
  nor one the configuration declares a binding of: otherwise `ValueError`, or, for an object
  injected before entry under a configured binding's name, a failed entry. While the session
  starts, and once it has closed to new work, `RuntimeError`. Nothing replaces or withdraws an
  injected object.
- **Threads, not the loop.** Each agent's operations run on a thread of their own, never on the
  program's event loop, which goes on running meanwhile. A method that needs the loop hands it
  work with `asyncio.run_coroutine_threadsafe(coroutine, loop).result()`; an `async def` method
  is called and its coroutine returned, never awaited. An object that is not safe to call from
  several threads at once is injected with `serialize=True`, which runs one call at a time.
- **Paths.** Injecting mounts nothing. A path the object returns names the same file in agent
  code only inside one of the session's `mounts`.
- **When the session ends.** Calls still running drain with the rest for `grace` seconds. One
  still running after that runs on in its thread, since nothing can end a thread: its record ends
  `unknown`, `still_running`, the report is `not_proven_stopped` and names the call, and leaving
  the `async with` raises `outrig.ShutdownException`. What the call returns later goes nowhere.
  A program that is killed takes its calls with it, each ending `unknown`, `killed`, and the
  agent code waiting on one is told at once.

## What the configuration applies to

A program's sessions are built from the configuration `outrig run-new` loaded, and not all of it
applies. The image `default-image` names, its `[images.<name>]` block, `[network]`, the factory
bindings, `[events]`, the models and providers an agent class's `model=` or `default-model`
names, and the top-level limits -- `children-max`, `model-concurrency-max`, `requests-max`,
`token-budget`, `subagent-depth-max`, `tool-call-max`, `tool-result-max` -- all do. The
configured workspace and its mounts are not mounted: a program passes `mounts=`. No
`[agents.<name>]` table, `default-agent` or `preamble` applies to an agent class, whose
docstring is its instructions. No MCP server or sidecar starts, as for `outrig run-new`, and a
program's session serves no skills. [Config -> What a host program's session
reads](../reference/config.md#what-a-host-programs-session-reads) has each setting.

## Signals and the grace

The program runs in a process group of its own. Ctrl-C, and `SIGINT` or `SIGTERM` sent to
`outrig`, are forwarded to that group, so the program sees them as it would from a terminal. So is
`SIGHUP` -- a closed terminal, a CI timeout -- which the program sees as the `SIGTERM` it asks
for. None of the three ends `outrig` where it stands: were one to, the sessions the program opened
would be left running, with no one to stop them. If `outrig` itself dies anyway -- `SIGKILL`, say
-- the program and everything it started end with it, and its sessions' containers remain.

The first signal also starts the program's grace: the time it has to end on its own, five seconds
unless `--grace <SECS>` or `[host] grace-secs` says otherwise. Time OutRig spends shutting down a
session the program asked it to shut down does not count against it, so a program that leaves its
sessions when signaled is not cut short by their shutdown. Past the grace, or at a second signal,
OutRig stops the program's process group -- `SIGTERM`, five seconds, `SIGKILL` -- and a further
signal sends `SIGKILL` at once. The grace bounds the program's own response, not the whole
shutdown: OutRig still stops whatever the program left open afterward.

A program that leaves its session when interrupted, and prints the report itself:

```python
import asyncio, signal
import outrig

async def main():
    leave = asyncio.Event()
    asyncio.get_running_loop().add_signal_handler(signal.SIGINT, leave.set)
    async with outrig.Session() as session:
        print(f"session {session.id} started", flush=True)
        await leave.wait()
    print(f"session {session.id}: {session.shutdown_report.verdict}")

asyncio.run(main())
```

Ctrl-C ends it with status `0`, once its session is shut down.

## What OutRig stops for the program

A program can end with a session still open: it returns, raises, is killed, or is stopped after
its grace. OutRig then closes the session's admission, shuts it down with the default five-second
drain, and prints its report on stderr, under a line naming the session. A session the program
was still entering has its start stopped where it stood, and what that start had made removed;
OutRig says so in one line. Either way `outrig` exits only once each of them is stopped. If
`outrig` itself is killed, none of this runs, and the sessions' containers remain.

The program is trusted code running with your authority, not a sandbox: it reads your files,
holds your environment, and runs host commands. Keep its imports and any credentials it reads
outside paths an agent can write.

## See also

- [Python API](../reference/python.md) -- every name the `outrig` package gives a program.
- [Python Runtime](../concepts/python-runtime.md) -- what an agent's Python is, and how agents,
  their requests and their limits work.
- [Hosted Objects](../concepts/hosted-objects.md) -- what an injected object grants an agent.
- [Reference -> CLI](../reference/cli.md#outrig-run-new-scriptpy) -- the script form's flags.
- [Config -> `[host]`](../reference/config.md#host) -- the program's grace.
