# Python API Reference

The names of the `outrig` package a program `outrig run-new <script.py>` runs on the host uses, each
with its declaration and its docstring, which is what `help()` shows; the last section lists those
only agent code uses. [Host programs](../usage/host-programs.md) says how such a program runs, and
[Python Runtime](../concepts/python-runtime.md) describes the agents it drives. Generated from the
package's source by `scripts/python-api-reference.py`: change a docstring, then run it with
`--write`.

## `outrig`

The package's own docstring, which `help(outrig)` shows: what each part of the package is for, and
where it runs.

OutRig's own package, imported by agent code in a kernel and by a program on the host.

`outrig.schema` declares types, decodes JSON into them strictly, and renders their JSON Schema:
see `help(outrig.schema)`.

In an agent's kernel, `outrig.runtime` is the runtime of the execution reading it -- what the
`runtime` global is, reachable from a module's code too -- and `outrig.bindings[name]` is that
execution's own proxy of a binding, whatever the kernel's globals now hold under its name.

In a program `outrig run-new <script.py>` runs on the host, `outrig.config()` is a read-only view of
the configuration it loaded, and `outrig.Session` enters a session the engine starts for it: see
`help(outrig.config)` and `help(outrig.Session)`. An `outrig.Agent` subclass constructed there is
a child of the session, its replies decoded into the program's own classes; construct it, and
leave its `async with`, inside the session's. The explicit `runtime.spawn` has no counterpart
there.

Agent code launches a child agent with `runtime.spawn`, and the work it gives one ends with a
value or with an `outrig.AgentError`: see `help(outrig.AgentError)` and `help(outrig.Request)`.
`@outrig.agent` declares such work as a typed `async def`, each call of which runs in a child of
its own: see `help(outrig.agent)`. A subclass of `outrig.Agent` declares a long-lived child
instead, whose methods marked `@outrig.agent` are its request channels: see `help(outrig.Agent)`.

`outrig.skills` reaches the session's skills, whose code imports as `outrig_skills.<name>`: see
`help(outrig.skills)`.

## Configuration

`outrig.config()` returns the configuration `outrig run-new` loaded, a `Config`, whose parts are the
classes after it. A program meets these classes only as values: none is a name in `outrig`.

### `outrig.config`

```python
def config(): ...
```

The configuration `outrig run-new` loaded before this program started, as a frozen,
read-only view.

`outrig run-new` reads it once, from the directory it was invoked in -- with `--config` and the
global file as for every command -- and never again: not from the script's directory, nor
from a workspace a session later names. It is what a session this program starts is built
from.

- `models`: each concrete model by name, a `Model`: its provider, the style OutRig calls it
  with, the identifier OutRig sends that provider, and its declared `context_window` and
  `max_tokens`.
- `aliases`: each alias by name, an `Alias`: the concrete models it stands for, in the order
  a call tries them.
- `providers`: each provider by name, a `Provider`: its `style`, the default for its models,
  and its `base_url`.
- `default_model`: the model or alias `default-model` names, or `None`.
- `workspace`: the configured `[workspace] host-path`, resolved to an absolute path on this
  host, or `None` where the configuration names none.
- `mounts`: each configured `[[workspace.mounts]]` entry, a `Mount` of its host path. The
  workspace and these are the interactive session's: a session this program starts mounts
  them only where the program passes them to it.
- `events`: the `[events]` table, whose `mode` is `"off"` or `"record"`.

A declared window is a number the configuration states, not the model's capacity: OutRig
does not ask the provider. No API key, no `${VAR}` value and no other secret is in the view;
the engine resolves those itself when a session calls a model.

Every call asks the engine, and returns a new value.

### `Config`

```python
@dataclasses.dataclass(frozen=True)
class Config:
    models: Mapping[str, Model]
    aliases: Mapping[str, Alias]
    providers: Mapping[str, Provider]
    default_model: str | None
    workspace: pathlib.Path | None
    mounts: tuple['Mount', ...]
    events: Events
```

The configuration `outrig run-new` loaded before this program started: see
`help(outrig.config)`.

### `Model`

```python
@dataclasses.dataclass(frozen=True)
class Model:
    name: str
    provider: str
    style: str
    identifier: str
    context_window: int | None
    max_tokens: int | None
```

A concrete model the configuration declares: a `[models.<name>]` row with a provider.

`style` is the API OutRig calls it with: the row's `style`, or its provider's where the row
sets none. `identifier` is what OutRig sends the provider: the row's `identifier`, or its name
where the configuration leaves that out. `context_window` and `max_tokens` are the row's
`context-window` and `max-tokens`, or `None` where it declares none. Each is a number the
configuration states, not the model's capacity: OutRig does not ask the provider.

### `Alias`

```python
@dataclasses.dataclass(frozen=True)
class Alias:
    name: str
    candidates: tuple[str, ...]
```

A `[models.<name>]` row with `alias`: the concrete models it stands for, in the order a
call tries them, an alias of aliases flattened.

### `Provider`

```python
@dataclasses.dataclass(frozen=True)
class Provider:
    name: str
    style: str
    base_url: str
```

A `[providers.<name>]` table: its `style` -- `"chat-completions"`, `"responses"` or
`"messages"`, the default for the models on it -- and its `base-url`. Its key is not here,
nor the variable that holds it.

### `Events`

```python
@dataclasses.dataclass(frozen=True)
class Events:
    mode: str
```

The `[events]` table: `mode` is `"off"` or `"record"`.

## Sessions

A program runs agents in a session it owns: an `outrig.Session`, entered with `async with`, which
`outrig.current_session()` returns inside the block. An `outrig.Mount` is a directory its container
sees, and its `events()` yields an `outrig.Event` for each record, with an `outrig.Missed` where
records were lost. See [Host programs](../usage/host-programs.md#sessions).

### `outrig.Session`

```python
class Session:
    def __init__(self, *, mounts=(), cwd=None, grace=None): ...
```

A session this program owns, with no primary agent: a container, the interpreter in it,
and the configuration's bindings, which the engine that runs this program starts and stops.

    async with outrig.Session(mounts=["./repo"], cwd="./repo") as session:
        ...
    print(session.shutdown_report.verdict)

Making one checks its inputs and starts nothing:

- `mounts`: the directories its container sees, each at its path on this host: a `Mount`, or
  a path, which is `Mount(path)`, read-only. The configuration's `[workspace]` and
  `[[workspace.mounts]]` are not mounted: `outrig.config()` shows them, for a program that
  passes them here. A directory given twice raises `ValueError`.
- `cwd`: the interpreter's working directory, where agent code runs and what its imports
  find: a directory in one of `mounts`, or `ValueError`. `None` leaves the image's.
- `grace`: how many seconds the exit lets what is still running -- hosted calls, executions --
  finish before it stops what is left, at most an hour, or `None` for the engine's default,
  five. It bounds that wait only: interrupting kernels and stopping the bindings and the
  container after it take bounded times of their own.

Relative paths are made absolute against the working directory now. A mount that does not
exist, is not a directory, or is `/`, and a `cwd` that is not a directory, fail entry.

`async with` starts it in the image `default-image` names, with the configured factory
bindings -- a repository's own only where its exact declaration was approved in that
repository before, as a session asks nobody -- and records its events in its session
directory under `[events] mode = "record"`. Inside the block it is `current_session()`, in
the tasks created there too. A session is entered once. A start that fails raises
`RuntimeError` with the engine's reason, once what it had started is stopped, and the block
does not run; one asking for more mounts than the engine reads in a request raises
`ValueError` before anything starts.

An `outrig.Agent` constructed inside the block is a child of the session (see
`help(outrig.Agent)`): enter its `async with` inside this one, since leaving this block
releases it, and its requests still open end with `outrig.SessionClosing`.

Leaving the block, however it ends, closes admission, shuts the session down and sets
`shutdown_report`, a `ShutdownReport`. When the engine could not prove that everything the
session started stopped, leaving raises `ShutdownException`, with the block's own exception,
if any, as its `__context__`; otherwise the block's exception propagates.

A cancellation never leaves the session running. One while entry waits has the engine stop
the start, and what it had made, before it propagates; had the start just finished, the
session is shut down and `shutdown_report` set. One while the exit waits is raised once the
shutdown is done.

`inject` gives its agents an object this program holds.

`id` is the engine's id for the session once entered, which names its container and its
directory under the session root, and `None` before.

#### `outrig.Session.close_admission`

```python
async def close_admission(self): ...
```

Close the session to new work, and return once the engine has closed it: from then on,
a call an agent makes of a binding is refused, admission closed, and so is an execution.
What is already running is left to finish, or to the exit to stop. After the exit, it
returns at once.

#### `outrig.Session.inject`

```python
def inject(self, name, obj, *, serialize=False): ...
```

Give every agent of the session `obj`, an object this program holds, as the binding
`name`: in every kernel, now and made later, `outrig.bindings[name]` is a proxy of it,
and so is the global `name` wherever that name is free -- one agent code bound, or a
builtin's, is left as it is. Agents use it as its library documents; each operation on it
runs here, on a thread serving that agent's connection, never on this program's event
loop, and is evented as a factory binding's is. A method that needs the loop uses
`asyncio.run_coroutine_threadsafe` itself: an `async def` method is not awaited for it.
With `serialize`, its calls run one at a time, as a library that is not thread-safe
needs.

Before entry, it is installed as the session starts; after, this returns once every agent
can reach it. Agents are told its type's qualified name and the first line of its class's
docstring; nothing of `obj` itself is run to describe it. The object stays this
program's, and so does a call on it: one still running when the session ends is left to
finish here, its reply going nowhere, and the exit raises `ShutdownException`, the report
reading not proven stopped.

Raises `TypeError` for a `name` that is not a str or a `serialize` that is not a bool;
`ValueError` for a name a binding cannot have -- not an identifier, a keyword, `runtime`,
`asyncio`, `outrig` or a dunder -- or one the session already binds; and `RuntimeError`
while the session starts, and once it takes no more: its admission closed, or it ended. A
name injected before entry that the configuration declares a binding of fails the
entry.

#### `outrig.Session.events`

```python
def events(self): ...
```

The session's events, as an async iterator, in the order the session published them.
Once per session: a second call raises `RuntimeError`.

Each item is an `Event`, or a `Missed` where records were lost. The stream is lossy and
never slows the session. The engine drops records this program does not take in time,
and this program holds at most 4096 for a reader that falls behind, dropping the oldest
as more arrive; either way, the records lost are counted, in one `Missed(count)` placed
where they would have come, so the next record's `id` is the last one's plus `count` plus
one. The engine subscribes as the session is built, so records arrive from entry on, the
first a `session.state` of `starting`, whether or not anything reads them.

Iteration ends once the session has stopped, or its entry failed, and every record held
has been read; after the exit, it yields what is held and ends. `shutdown_report` never
depends on it, and its `events.missed[0]` counts what this stream lost.

### `outrig.Mount`

```python
@dataclasses.dataclass(frozen=True)
class Mount:
    host_path: str
    access: str = 'read-only'
```

A directory a session's container sees, at its path on this host, so a path means the
same file on both sides: one a live object returns names the file agent code opens.

`host_path` is the directory, a str or path made absolute against the working directory when
the `Mount` is made. `access` is `"read-only"`, the default, or `"read-write"`.

### `outrig.current_session`

```python
def current_session(): ...
```

The `Session` whose `async with` block the caller runs in, the innermost where blocks
nest: in this task, and in a task or a `to_thread` call started inside the block, which copy
the context; not in a `threading.Thread`, which starts with an empty one. A task that outlives
the block still sees the session. Outside every session's block, raises `RuntimeError`.

### `outrig.Event`

```python
@dataclasses.dataclass(frozen=True)
class Event:
    id: int
    type: str
    subject: str | None
    time: datetime.datetime
    data: dict
```

One record of a session's events, as `events.jsonl` records it.

`id` is its place in the session's sequence, from 1; `type` its type after `org.outrig.`,
such as `session.state`; `subject` the agent it belongs to, or `None` for one about the
session; `time` when it was published, an aware `datetime` in UTC; and `data` its payload, as
decoded JSON. The event log's reference says what each type means.

### `outrig.Missed`

```python
@dataclasses.dataclass(frozen=True)
class Missed:
    count: int
```

`count` records of a session's events lost between the item before this one and the item
after it, because they were not read in time: see `help(outrig.Session.events)`.

## Shutdown

Leaving a session's `async with` block shuts it down and sets its `shutdown_report`, an
`outrig.ShutdownReport`, whose fields hold the classes after it, met only as values. When the engine
cannot prove that everything the session started stopped, leaving raises `outrig.ShutdownException`.

### `outrig.ShutdownReport`

```python
@dataclasses.dataclass(frozen=True)
class ShutdownReport:
    verdict: str
    closed_by: ClosedBy
    stopped: Stopped
    executions: tuple[ExecutionOutcome, ...]
    hosted: tuple[HostedCall, ...]
    requests: tuple[RequestOutcome, ...]
    bindings: BindingStop | None
    events: EventDelivery
```

What a session's shutdown found: `session.shutdown_report` after the exit.

`verdict` is what it adds up to:

- `"clean"`: everything the session started is stopped, and every outcome is known.
- `"stopped_with_unknown"`: everything is stopped, and some execution or hosted call was cut
  off. Its outcome is unknown, never failed: its effect may stand.
- `"not_proven_stopped"`: something could not be confirmed stopped, and may still be acting;
  the exit raised `ShutdownException`. Do not reuse what the session ran on.

`closed_by`, a `ClosedBy`, is what closed admission; `stopped`, a `Stopped`, whether
everything is proven stopped; `executions`, each execution running when admission closed;
`hosted`, each hosted call whose outcome came after admission closed; `requests`, each
`RequestOutcome`, a request made of a child agent that was open then; `bindings`, a
`BindingStop`, how the process holding the bindings stopped, or `None` for a session with
none; and `events`, an `EventDelivery`, how the session's events were delivered. It never
depends on `Session.events()` having been read.

### `ClosedBy`

```python
@dataclasses.dataclass(frozen=True)
class ClosedBy:
    by: str
    cause: str | None = None
```

What closed a session's admission: `by` is `"owner"` -- the program, or the shutdown --
or `"interpreter_exited"`, with the exit's `cause`.

### `Stopped`

```python
@dataclasses.dataclass(frozen=True)
class Stopped:
    state: str
    reason: str | None = None
```

Whether what a session started is proven stopped: `state` is `"proven"`, or `"not_proven"`
with the `reason`.

### `ExecutionOutcome`

```python
@dataclasses.dataclass(frozen=True)
class ExecutionOutcome:
    id: int
    status: str
```

An execution running when a session's admission closed, and how it ended: its `id`, and its
`status`, `"ok"` or `"error"` when its result arrived, `"unknown"` when none did -- what it
did may or may not have happened.

### `HostedCall`

```python
@dataclasses.dataclass(frozen=True)
class HostedCall:
    request: int
    agent: str
    binding: str
    outcome: str
    reason: str | None
```

A hosted request whose outcome came after a session's admission closed: its `request` id,
the `agent` that made it, its `binding`, its `outcome` -- `"returned"`, `"raised"`,
`"refused"`, `"cancelled"` or `"unknown"` -- and the `reason` for one of the last three, such
as `"admission_closed"` or `"killed"`, or `None`.

### `RequestOutcome`

```python
@dataclasses.dataclass(frozen=True)
class RequestOutcome:
    request: int
    child: str
    channel: str | None
    outcome: str
```

A request made of a child agent that was open when a session's admission closed: its
`request` id, the `child` it was made of, the `channel` it was sent on -- `None` for a
submission -- and its `outcome`: `"closed"`, or whatever ended it first, or `"terminated"`
when the interpreter died before anything did.

### `BindingStop`

```python
@dataclasses.dataclass(frozen=True)
class BindingStop:
    names: tuple[str, ...]
    group_empty: bool
    killed: bool
    survivors: tuple[str, ...]
    exited: str | None
```

How the process holding a session's bindings stopped: SIGTERM to its process group, a
grace, and SIGKILL if anything was left. `names` are its bindings; `group_empty` whether its
group is proven empty; `killed` whether SIGKILL was needed; `survivors` what was left in the
group when it could not be proven empty, each `<pid> <command> <state>`; and `exited` how the
process ended, when it ended while the session ran, or `None`.

### `EventDelivery`

```python
@dataclasses.dataclass(frozen=True)
class EventDelivery:
    last_sequence: int
    missed: tuple[int, ...]
    log: LogLoss | None
```

How a session's events reached their subscribers: `last_sequence`, the id of the last one
published; `missed`, how many each subscription lost, in the order they were made -- the
first is `Session.events()`'s, the engine's drops and this program's together -- and `log`, a
`LogLoss` when `events.jsonl` missed any, or `None`.

### `LogLoss`

```python
@dataclasses.dataclass(frozen=True)
class LogLoss:
    path: str
    records: int
    first: str
    integrity: str | None
```

Records a session's `events.jsonl`, at `path`, did not get: how many (`records`), why the
first was lost (`first`), and why the file may hold a partial record when the writer could
not prove its rollback (`integrity`), or `None`.

### `outrig.ShutdownException`

```python
class ShutdownException(Exception):
    def __init__(self, report): ...
```

A session's exit could not prove that everything the session started stopped: something
it started may still be acting. Raised in the task that left the session's `async with`
block, with the block's own exception, if any, as its `__context__`.

`report` is the session's `ShutdownReport`, also its `shutdown_report`; `report.stopped`
says what could not be confirmed stopped. Not an `outrig.AgentError`: it is about the
session, not about an agent's work.

## Agents

A program's agents are instances of `outrig.Agent` subclasses, each a child agent in a session the
program owns, whose methods marked `@outrig.agent` are its request channels. A call of one returns a
`Handle` at once, whose `status` is an `outrig.Status`; a program never constructs a `Handle`. See
[Host programs](../usage/host-programs.md#agent-classes).

### `outrig.Agent`

```python
class Agent:
    def __init__(self, *, model=None, session=None, facilities=None, **inputs): ...
```

The base of an agent class: each instance is one long-lived child agent, and each method
marked `@outrig.agent` is one of its request channels.

    class Reviewer(outrig.Agent):
        """You review the repository in /workspace."""

        @outrig.agent
        async def review(self, path: str) -> Verdict:
            """Review the file at `path`."""

    reviewer = Reviewer()
    verdict = await reviewer.review("src/main.py")

The class's docstring is the first part of the child's instructions, as written. A marked
method is an `async def` whose docstring describes its channel, whose parameters after `self`
are each annotated, and which has a return annotation, each a type `outrig.schema` takes. A
call checks its arguments and sends them as one request -- none as `None`, one as itself,
several as a frozen dataclass the class holds, `<Class><Method>Message` -- and returns the
request's handle at once: await it for the reply, of the return type. The body is never run.
Methods not marked are ordinary ones, which run here. What a class cannot declare raises
`TypeError` as the class is made, naming the member.

`Reviewer(**inputs, model=None)` runs the class's `__init__` to its end, then its
`__outrig_attach__`, and only then attaches the instance: binds each input, which must be
data, in the child as a variable, launches the child in the background -- on `model`, a
configured model or alias, or on yours -- and returns at once; a request made before the
child exists waits for it. The child's first round is its own, to get ready in: `await
reviewer.ready()` returns once it has ended, and raises what the launch met instead. `await
reviewer.release()` ends the child and every request still open on it; `async with
Reviewer() as reviewer:` waits for `ready()` on entry and releases on leaving. An instance
collected unreleased is released then. `facilities=` is reserved: any value but `None`
raises `NotImplementedError`.

A subclass's `__init__` passes the base its keywords with `super().__init__(**inputs,
model=...)`, which launches nothing; one that does not raises `TypeError` once it returns.
Define `__outrig_attach__(self)` to finish what `__init__` cannot -- state a mixin's
`__init__` sets after the subclass's own, say: OutRig calls it on the class, so it needs no
`super()`, and ignores what it returns. A request method called before the instance is
attached -- in `__init__` or `__outrig_attach__` -- raises `TypeError`. A constructor or hook
that raises launches nothing, and its exception is the constructor call's; what it did to
your own objects stands, since OutRig rolls nothing back. An agent constructed inside
`__init__` or `__outrig_attach__` is attached at once, and its child runs on if the
constructor then raises, until something releases it. The class's metaclass derives from
`abc.ABCMeta`: a subclass that leaves an abstract method unimplemented raises `TypeError` at
the call, before its `__init__`; and mixing in a class whose metaclass is another, not
derived from it, raises `TypeError` as the class is made.

In a program `outrig run-new` runs on the host, `Reviewer(**inputs, model=None, session=None)`
is constructed on the running event loop its handles belong to, and its child is a root child
of `session`, the current `outrig.Session` when not given: its model is `model`, or
`default-model`, and no `[agents.<name>]` table applies. Its types reach the child as data,
made into classes again there, and each reply is decoded here, into this program's own.
Construct it, and leave its `async with`, inside the session's; the session's exit releases
it. In an agent's kernel, `session=` is refused: the child is in the kernel's own session.

#### `outrig.Agent.ready`

```python
async def ready(self): ...
```

Return once the child has ended its first round, which is its own, to get ready in.

Raises what the launch met instead -- `ValueError` for an unknown model,
`outrig.AgentLimitReached`, `outrig.SessionClosing` -- or `outrig.BudgetExhausted` or
`outrig.AgentRequestFailed` when the child cannot have that round, or
`outrig.AgentReleased` when the instance was released first. It settles once: awaiting
again answers the same at once.

#### `outrig.Agent.release`

```python
async def release(self): ...
```

End the child, and every request still open on it with `outrig.AgentReleased`, which
each call raises from now on. Returns once it has ended; a second call, once the first
has.

### `outrig.agent`

```python
def agent(fn=None, *, validate=None, model=None): ...
```

Declare `fn`, an `async def`, as work for a child agent: `@outrig.agent`, or
`@outrig.agent(validate=..., model=...)`.

Its parameters are the child's inputs, its docstring the child's instructions, and its return
annotation the result's type; each type is one `outrig.schema` takes. The body is never run.
Calling the declaration checks each argument against its parameter's type and copies it,
sends the work to a new child agent, and returns the handle at once: `await` it for the
result, checked against the return type. The child is released once the call settles,
awaited or not. Only agent code, in an agent's kernel, can call a declaration: called
anywhere else, a host program included, it raises `RuntimeError`. A host program gives a
child work through an `outrig.Agent` subclass instead.

`validate(result, **arguments)` runs on each answer the child gives, with the arguments as
they were at the call, and returns a list of problems, empty to accept; the child corrects
what it lists, within the attempt limit, reading the first 20, each cut to 1,024 characters.
It must be pure -- no hosted calls, writes or model calls -- since it runs again on every
attempt. One that raises ends the call, counting no attempt, and the handle raises what it
raised. `model` names a configured model or alias for the child; without it, the caller's.
A name that is not configured ends the call, and the handle raises `ValueError` naming the
configured ones. What a declaration cannot take -- a missing docstring, an unannotated
parameter, a type outside the subset -- raises `TypeError` at decoration, naming the function.

On a method of an `outrig.Agent` subclass it marks a request channel instead, and takes
neither keyword: see `help(outrig.Agent)`.

### `Handle`

```python
class Handle: ...
```

One submission or request an agent made of a child: what `child.submit(...)` and
`child.channels[name].request(...)` return, at once, the work already sent, and what a typed
call or an agent class's request returns.

`await handle` gives the accepted answer, an instance of the declared type, or raises how the
request ended -- an `outrig.AgentError` when the child's work or the session ended it -- and
awaiting again gives the same. A typed call or an agent class's request can also end on the
caller's side, raising what is not an `outrig.AgentError`: what the call's `validate=` raised,
or `ValueError` for a model that is not configured or work too long to send. Cancelling the
code awaiting it cancels that wait and never the request; `handle.cancel()` cancels the
request: one the child has not received never reaches it, and one it has ends so that its
answer raises `outrig.RequestCancelled`. `done()` and `result()` answer without waiting, as a
future's do, and `status` says what it is doing (see `help(outrig.Status)`). `future` is a
view `asyncio.wait` takes; `runtime.wait` and `asyncio.gather` take the handle itself. A
submission's `progress` is an endpoint of what the child reports while it works on it.

#### `Handle.id`

```python
@property
def id(self): ...
```

The request's id: what the child's delivery of it, and the session's events, name. In a
program on the host, `None` until the session has the request.

#### `Handle.future`

```python
@property
def future(self): ...
```

A future that settles as the request ends; cancelling it cancels nothing else.

#### `Handle.done`

```python
def done(self): ...
```

Whether the request has ended.

#### `Handle.result`

```python
def result(self): ...
```

The accepted answer, or the exception the request ended with, raised; raises
`asyncio.InvalidStateError` while it has not ended.

#### `Handle.cancel`

```python
def cancel(self): ...
```

Cancel the request; whether this ended it. Not the child, which takes more work.

#### `Handle.status`

```python
@property
def status(self): ...
```

What the request is doing, and its child: see `help(outrig.Status)`.

#### `Handle.progress`

```python
@property
def progress(self): ...
```

An endpoint of what the child reports on its `progress` channel while it works on this
submission: `await receive()` takes the next, as a `Delivery`. Only a submission has one.

### `outrig.Status`

```python
@dataclasses.dataclass(frozen=True)
class Status:
    state: str
    reason: object
    child: str
```

What a handle's request is doing, read without waiting.

`state` is `queued` or `running` for a submission, `waiting` or `received` for a request on a
channel, and `accepted` or `failed` once it has ended; `reason` says why a failed one failed --
`rejected`, `failed`, `cancelled`, `released`, `budget`, `closed`, `terminated`, or `limit`
for a typed call whose child a limit refused -- and is `None` otherwise. `child` is its
child's state: `running`, `idle`, `wedged` or `released`. Fixed once the request has ended.

## Errors

A request that ends without an accepted answer raises one of these from `await handle`, unless it
failed on the caller's side: a typed call whose `validate=` raised raises that exception, and a
model that is not configured, or work too long to send, raises `ValueError`. Each of these is an
`outrig.AgentError`, so `except outrig.AgentError` catches them all and lets the caller's errors
through.

### `outrig.AgentError`

```python
class AgentError(Exception): ...
```

The base of every error OutRig's child agents end work with.

`except outrig.AgentError` around `await handle` catches the child's failure -- rejected,
failed, cancelled, released, out of budget, the session closing, a limit -- and lets the
caller's own errors through: what a typed call's `validate=` raised, say, or the `ValueError`
of a model that is not configured.

### `outrig.CompletionRejected`

```python
class CompletionRejected(AgentError):
    def __init__(self, message, value=None, problems=()): ...
```

The child's answers failed their check past the request's attempt limit.

`value` is the last value it answered with and `problems` what was wrong with it, each a
`outrig.schema.Problem`.

### `outrig.AgentRequestFailed`

```python
class AgentRequestFailed(AgentError):
    def __init__(self, message): ...
```

The child failed the request on purpose, with `await delivery.fail(message)`, or a work
child its submission, with `await runtime.fail(message)`; `message` is why.

### `outrig.RequestCancelled`

```python
class RequestCancelled(AgentError): ...
```

The request was cancelled: with `handle.cancel()`, or by the host, when the child's rounds
left it unanswered after one that warned it would be. The message says which. In the child, an
answer to it raises this.

When the host cancelled a submission and a model call of the child's had failed and ended one
of its rounds -- the provider refused a reply, say -- the child did not stop of its own accord,
and `__cause__` is an `AgentRequestFailed` whose message says how the call failed. It is `None`
otherwise.

### `outrig.AgentReleased`

```python
class AgentReleased(AgentError): ...
```

The child, or an agent above it, was released before it answered.

### `outrig.AgentLimitReached`

```python
class AgentLimitReached(AgentError): ...
```

A limit the session holds every agent to refused at once, rather than waiting: the depth
agents may nest to, the children a session holds, or the requests one child holds.

### `outrig.BudgetExhausted`

```python
class BudgetExhausted(AgentError): ...
```

The session's token budget was spent before the child answered: no model call is made
from then on.

### `outrig.SessionClosing`

```python
class SessionClosing(AgentError): ...
```

The session closed to new work before the child answered, or before this could start.

### `outrig.RequestSettled`

```python
class RequestSettled(AgentError):
    def __init__(self, message, ended): ...
```

An answer to a request that had already ended -- answered, rejected, failed, released, or
closed -- refused. `ended` says how it had ended.

## `outrig.schema`

The types an agent class's requests and replies carry are ones `outrig.schema` takes. Its docstring,
which `help(outrig.schema)` shows, says which.

Declared types: strict decoding into them, and the JSON Schema that describes them.

`declare(T)` checks a type at once and returns its `Schema`. A JSON value then becomes an
instance of `T` only when it has exactly `T`'s shape:

    from dataclasses import dataclass
    from typing import Annotated, Literal, Optional
    from outrig.schema import alias, declare, doc

    @dataclass(frozen=True, kw_only=True)
    class Verdict:
        status: Annotated[Literal["approve", "reject"], doc("The outcome.")]
        next_step: Annotated[Optional[str], alias("next-step"), doc("What to do next, or null.")]

    verdict = declare(Verdict)
    verdict.decode({"status": "approve", "next-step": None})   # a Verdict
    verdict.text()                                             # its JSON Schema, as text

The types it takes: `str`, `bool`, `int`, finite `float`, `None`; `Literal` of str, int and bool
values; `list[T]`; `dict[str, T]`; unions of these, optionals included; dataclasses whose fields
are all of these; and `Annotated` around any of them. A type outside the set, or a field hint
that does not resolve in its module, fails at `declare`, naming the field.

`doc(text)` in a field's `Annotated` is its description, carried verbatim into the schema.
`alias(name)` is its JSON key; nothing else renames a field.

Decoding is strict. A key no field declares is rejected, a field's own name included when it has
an alias. A field without a default must be present, even when its type allows null. `null` is
accepted only where the type includes `None`. A field left out takes its dataclass default and
nothing else. A `Literal` matches by value and type, so `true` is not `1`, while an integer passes
as a float. A union takes a value exactly one member accepts, or that every member accepting it
builds alike. Every problem is reported with the JSON path where it is, up to `PROBLEMS_MAX`.

### `outrig.schema.declare`

```python
def declare(t): ...
```

`t`'s `Schema`, checking `t` now.

Raises `TypeError` for a type outside the set this module takes -- naming the field it is in
-- and for a field hint that does not resolve in its module.

### `outrig.schema.Schema`

```python
class Schema: ...
```

A declared type: `decode` a JSON value into it, `encode` an instance into JSON, and
`json_schema()` or `text()` for its JSON Schema. Made by `declare`.

#### `outrig.schema.Schema.decode`

```python
def decode(self, value): ...
```

`value`, a parsed JSON value, as a new instance of the declared type.

Raises `DecodeError` listing every problem, each with its JSON path. Nothing in `value` is
kept: containers are new, and a dataclass is built by calling it.

#### `outrig.schema.Schema.encode`

```python
def encode(self, value): ...
```

`value`, an instance of the declared type, as JSON: what `decode` takes back.

A dataclass becomes an object under its fields' keys. Raises `TypeError` listing every
place `value` is not of the declared type.

#### `outrig.schema.Schema.json_schema`

```python
def json_schema(self): ...
```

The JSON Schema of the declared type, as a new dict.

A dataclass is an object listing its fields under their keys, each with its `doc()` text
as `description`, requiring each without a default, and taking no other key. Dataclasses
other than the declared one are in `$defs`.

#### `outrig.schema.Schema.text`

```python
def text(self): ...
```

`json_schema()` as indented JSON text, every `doc()` text in it verbatim.

### `outrig.schema.doc`

```python
def doc(text): ...
```

A description for `Annotated`: `Annotated[str, doc("What it is.")]`.

On a dataclass field it is the field's description; around any other type, that type's. The
text reaches the schema verbatim.

### `outrig.schema.alias`

```python
def alias(name): ...
```

A field's JSON key, for the field's own `Annotated`: `Annotated[str, alias("next-step")]`.

Without one, the key is the field's name. The field's name is then not a key at all: decoding
rejects it as unknown.

### `outrig.schema.DecodeError`

```python
class DecodeError(ValueError):
    def __init__(self, message, problems=(), more=0): ...
```

A value that does not decode into its declared type.

`problems` is a tuple of `Problem`, at most `PROBLEMS_MAX` of them, and `more` counts those
past it. The message lists them.

### `outrig.schema.Problem`

```python
class Problem:
    def __init__(self, path, message): ...
```

One thing wrong with a value: `path`, where it is, as in `$.notes['retry-loop'].severity`,
and `message`, what is wrong and what was expected.

### `outrig.schema.PROBLEMS_MAX`

```python
PROBLEMS_MAX = 20
```

## Kernel-only names

Agent code in an agent's kernel uses these names, as [Python Runtime](../concepts/python-runtime.md)
describes; a program on the host has no use for them. What a program meets if it reads one:

- `outrig.runtime` -- reading it raises `RuntimeError`.
- `outrig.bindings` -- reading it raises `RuntimeError`.
- `outrig.skills` -- each of its functions raises `RuntimeError`.
- `outrig.Request` -- declares a channel for `runtime.spawn`, which a program does not have.
