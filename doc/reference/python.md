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
`outrig.facility(name)` is a reference to the binding `name` that resolves the same way wherever
it is used, which is how one agent gives another a binding: see `help(outrig.Facility)`.

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

A channel the session's owner declares ends in the owner's own messaging rather than at an agent:
its `send` raises an `outrig.ChannelError` when the message was not taken, or may not have been,
and its `receive` when the channel was closed: see `help(outrig.ChannelError)`.

Each service the session's owner serves from Rust is an `outrig.Service`, its methods calls to
the host by value; a call that returns no result raises `outrig.ServiceError`: see
`help(outrig.Service)`. Each MCP server the session places is an `outrig.McpService`, a method per
tool by the tool's exact name, whose calls return an `outrig.McpResult` -- content blocks in
order, structured content -- and raise `outrig.ToolError` for a result the server marked an
error, and an `outrig.McpError` saying whether a call that failed reached the server --
`NotDispatched`, `MayHaveDispatched` or `ResponseReceived`: see `help(outrig.McpService)`,
`help(outrig.McpResult)` and `help(outrig.McpError)`.

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
sees, an `outrig.Application` the code its agent classes are in, and its `events()` yields an
`outrig.Event` for each record, with an `outrig.Missed` where records were lost. See [Host
programs](../usage/host-programs.md#sessions).

### `outrig.Session`

```python
class Session:
    def __init__(self, *, mounts=(), cwd=None, grace=None, application=None, mcp=(),
                 mcp_policy=None, mcp_identity=None): ...
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
- `application`: an `Application`, the code this program's agent classes are in, which every
  kernel of the session imports from; or `None`.
`session.questions` receives questions asked by its root agents: `async for q in
session.questions` (or `await session.questions.receive()`) yields an OpenQuestion with
`id`, `asker`, `body` (a Question), and `closed`. Run this receiver concurrently with
the agents. Answers have no timeout. Withdrawal marks the object closed; session end
closes the iterator. This endpoint is independent of the lossy events subscription.

- `mcp`: the MCP servers it places and binds, each an `McpCommand`, an `McpSidecar`, an
  `McpPrimaryView` or an `McpUrl`; `mcp_policy`, an `McpPolicy`, how they start; and
  `mcp_identity`, a
  mapping of header names to values carried unread on every request to a URL server this
  program declares. Each starts with the session, beside its interpreter, and one that does
  not start does not fail entry: `session.mcp` says how each start ended.

Relative paths are made absolute against the working directory now. A mount that does not
exist, is not a directory, or is `/`, a `cwd` that is not a directory, and an application
whose path is not a directory, fail entry.

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

#### `outrig.Session.reset_model_requests`

```python
async def reset_model_requests(self, agent, limit=None): ...
```

Set the model-request count of `agent`, an `outrig.Agent` this program constructed in
the session, to zero, keeping its limit, or replacing it with `limit`, an int of at least
1. An agent at its limit goes on: what is sent to it from now on opens its rounds again,
on what it kept. Its model is told once. What warrants a reset is this program's to
decide; only the session's owner can lift a limit, never an agent's code. Raises
`outrig.AgentReleased` for an agent released, and `outrig.SessionClosing` once the session
is closing.

#### `outrig.Session.raise_model_request_limit`

```python
async def raise_model_request_limit(self, agent, limit): ...
```

Replace the model-request limit of `agent` with `limit`, an int of at least 1, keeping
its count: otherwise as `reset_model_requests`. A limit at or below the count leaves the
agent at its limit.

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
`asyncio`, `outrig` or a dunder -- or one the session already binds, an MCP service's
among them; and `RuntimeError` while the session starts, and once it takes no more: its
admission closed, or it ended. A name injected before entry that the configuration
declares a binding or an MCP server of fails the entry.

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

### `outrig.Application`

```python
@dataclasses.dataclass(frozen=True)
class Application:
    path: str
    requires: tuple = ()
    imports: tuple = ()
```

A session's application code: the package holding this program's agent classes and what
they import, which the session's interpreter imports from in every kernel.

`path` is a directory whose top-level entries are packages and modules, as a project's `src/`
is, or a pure-Python wheel, a file whose name ends `.whl`: a str or path made absolute against
the working directory when the `Application` is made. A wheel's files are laid out as an
installer lays them out, its `.dist-info` with them; the `Requires-Dist` in its metadata is
not read. `requires` names the distributions it imports beyond the standard library and
`outrig`, as a binding's `requires` does: a name, extras, a version specifier and markers,
never a path or a URL. `imports` names the modules imported as the session starts, in order.

Entry reads the directory or the wheel with no Python run, refuses it if anything in it is
compiled or a link, or if the wheel's tags are not pure Python's, and copies it whole to a
directory named for its digest, which the container mounts read-only: the session runs that
copy, whatever happens to `path` meanwhile. The copy goes on
`sys.path` after the `outrig` package and before the working directory, so no file there takes
the place of one of its modules, and its `requires` -- pure-Python wheels only -- is installed
and placed beside it. Each module of `imports` is then imported, and one that raises, is not
found, reaches compiled code or takes more than 30 seconds fails entry, naming it and ending
with the end of its traceback. A module not named imports when something first imports it.

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

## Questions

Agents ask the session owner with `runtime.ask(outrig.Question(...))`, returning a request handle
immediately. See [Questions to the session
owner](../concepts/python-runtime.md#questions-to-the-session-owner).

### `outrig.Question`

```python
@dataclass
class Question:
    text: str
    choices: list[Choice] = field(default_factory=list)
    multiple: bool = False
    other: bool = True
    context: QuestionContext | None = None
```

A decision for your parent (the session owner for a primary), asked with `runtime.ask`.

Empty choices means free-form. Otherwise `multiple=False` permits one choice and
`multiple=True` permits zero or more; `other=False` forbids additional text.
Strings in choices become Choice(label). At ask, the limits are 32 choices,
65,536 UTF-8 JSON bytes, and 32 open questions per asker, separate from requests-max.
`context` optionally carries a QuestionContext: longer background text and file references,
included in the same size bound. There is no answer timeout: cancel a question you give
up on. A child asks its parent; a primary asks the session owner.

### `outrig.Choice`

```python
@dataclass
class Choice:
    label: str
    description: str = ''
```

An offered label and optional description. Labels must be nonempty and unique.

### `outrig.QuestionContext`

```python
@dataclass
class QuestionContext:
    description: str = ''
    references: list[QuestionReference] = field(default_factory=list)
```

Optional background for a short question prompt: longer prose, file references, or both.

`description` is plain text, not trusted markup. A string in `references` becomes a
QuestionReference(path). Context is data under the same total question-size bound.

### `outrig.QuestionReference`

```python
@dataclass
class QuestionReference:
    path: str
    line: int | None = None
    end_line: int | None = None
    description: str = ''
```

An inert file reference accompanying a question, never an instruction to open a file.

`path` is a nonempty path as the asker sees it. `line` and `end_line` are optional
one-based positions; an end requires a start and must not precede it. `description`
explains why the file matters. Owners display these as untrusted text, not markup.

### `outrig.Answer`

```python
@dataclass
class Answer:
    choices: list[str] = field(default_factory=list)
    text: str | None = None
    by: str = 'application'
    chain: list[str] = field(default_factory=list)
```

An explicit answer: choice labels, optional text, answerer, and attribution chain.

Choices are returned in the question's order. A host program passes one to
`await question.answer(answer)`; invalid answers leave the question open.

## Questions from children

A child asks its parent with `runtime.ask`. Every agent receives its children's questions on
`runtime.questions`, a `QuestionEndpoint`; taking one returns a `QuestionDelivery`. These are
agent-kernel values, not host-program APIs.

### `QuestionEndpoint`

```python
class QuestionEndpoint(RequestEndpoint):
    def __init__(self, agent): ...
```

`runtime.questions`, the inbox for questions from your children.

Separate from `runtime.channels`: `pending()` counts unread questions, and
`d = await runtime.questions.receive()` takes one. Arrivals open a round, are announced
as questions, and wake `runtime.wait` with `MessageAvailable`. Use `runtime.wait`, not
a bare await of a child's result, when that child might ask you something.

A parent that leaves a question unread or unanswered gets the ordinary unanswered-request
warning round, then a host cancel; the child receives `outrig.RequestCancelled`.
Delegation pauses that obligation while its hop is open. Cancelling the originating handle
withdraws every hop, including an owner question. Interrupting code awaiting delegation
does not withdraw it: explicitly cancel the original question to give up.

A child can end a round with its question open and remain `idle`; the handle shows the
question's state. Its ordinary answer obligations pause while it awaits an answer.
Questions have their own bound of 32 open per asker, separate from `requests-max`.
Decorated one-call children close questions left open when their call ends. Releasing either
asker or answerer closes the question.

#### `QuestionEndpoint.receive`

```python
async def receive(self): ...
```

Take the next child's question, waiting until one arrives.

The returned delivery identifies its sender and asker chain; answer, decline, escalate,
or forward it. Your children can ask even during your instance's first round.

### `QuestionDelivery`

```python
@dataclasses.dataclass(frozen=True)
class QuestionDelivery(Delivery): ...
```

A child's question. Answer explicitly, decline, escalate, or forward it.

`id`, `body` (a copy of the Question), `sender` (the immediate child's id), and
`received_at` identify the delivery. `chain` lists askers from the originating child
through each intermediary.
Delegation remains active if the awaiting submission is interrupted; cancelling the
original question withdraws every delegated request. Invalid answers leave it open.

#### `QuestionDelivery.chain`

```python
@property
def chain(self): ...
```

The originating asker followed by intermediary agents, in order.

#### `QuestionDelivery.answer`

```python
async def answer(self, answer=None, *, choices=None, text=None): ...
```

Settle with Answer(...), or choices= and text=. Attribution is supplied here.

The complete encoded settlement must fit 1 MiB, whether or not observation is enabled.
An oversized answer raises ValueError and leaves the question open for a smaller answer.

#### `QuestionDelivery.decline`

```python
async def decline(self, reason='not relevant'): ...
```

Fail the child's handle with the reason; the preset is 'not relevant'.

#### `QuestionDelivery.escalate`

```python
async def escalate(self): ...
```

Ask your parent (or the session owner), passing its outcome back unchanged.

#### `QuestionDelivery.forward`

```python
async def forward(self, target): ...
```

Ask one of your request children's channels, passing its outcome back.

`target` is a channel declared with `outrig.Request(outrig.Question, outrig.Answer)`,
for example `expert.channels['questions']`. A reply must satisfy the original question;
cancellation and release keep their typed errors across relays. A failure or invalid
reply fails the original handle.

## Answering questions

A host program reads `session.questions` independently of its events and explicitly answers,
declines, or cancels each question. A normal message never answers one.

### `OpenQuestion`

```python
class OpenQuestion:
    def __init__(self, session, message): ...
```

One question asked of a Session owner. Invalid answers leave it open.

`id`, `asker`, `chain`, and `body` identify the question; `closed` marks withdrawal or
settlement. `answer(Answer(...))`, `decline(reason)`, or `cancel()` settles it.
A late answer raises RuntimeError. There is no automatic timeout. Cancellation after
sending a settlement waits for its receipt, updates closed on success, then propagates.

#### `OpenQuestion.answer`

```python
async def answer(self, answer): ...
```

Answer explicitly. Invalid answers raise and leave the question open.

#### `OpenQuestion.decline`

```python
async def decline(self, reason='not relevant'): ...
```

Decline with a reason; the preset is 'not relevant'.

#### `OpenQuestion.cancel`

```python
async def cancel(self): ...
```

End the question cancelled by the owner.

## MCP services

A session places the MCP servers a program declares with `outrig.Session(mcp=[...])` -- an
`outrig.McpCommand`, an `outrig.McpSidecar`, an `outrig.McpPrimaryView` or an `outrig.McpUrl` each
-- beside those the configuration declares, and starts them as an `outrig.McpPolicy` says. After
entry, `session.mcp` maps each name to an `McpOutcome`, with an `McpCause` for one that did not
start, met only as values. See [MCP Servers](../concepts/mcp-servers.md#in-a-session-mcp-services).

### `outrig.McpCommand`

```python
@dataclasses.dataclass(frozen=True)
class McpCommand:
    name: str
    command: tuple
    env: Mapping = dataclasses.field(default_factory=dict)
    call_timeout_secs: int | None = None
    description: str | None = None
```

An MCP server a session places in its container, beside the interpreter, as the session's
user: `command` exec'd there, bound as `name` -- a Python identifier nothing else in the
session takes -- for agent code to reach as `outrig.bindings[name]`, its tools its methods.

`env` sets the server's environment: each value a literal, or `"${VAR}"`, which the engine
resolves. A server here may hold no credential -- its environment would be readable from
agent Python -- so one with a `"${VAR}"` value does not start, and `session.mcp` says why:
place it in an `McpSidecar`. `call_timeout_secs` bounds each of its calls in place of the
session's ten minutes; `description` is what agents are told it is, in place of the first line
of the server's own `instructions`.

### `outrig.McpSidecar`

```python
@dataclasses.dataclass(frozen=True)
class McpSidecar:
    name: str
    image: str
    command: tuple | None = None
    args: tuple = ()
    env: Mapping = dataclasses.field(default_factory=dict)
    workspace: str = 'none'
    mounts: tuple = ()
    call_timeout_secs: int | None = None
    description: str | None = None
```

An MCP server a session places in a container of its own, of `image`, bound as `name`:
`command` exec'd there, or, with no `command`, the image's entrypoint handed `args`.

`env` is as for `McpCommand`, and a `"${VAR}"` value is allowed here: the container holds it,
not the interpreter's. `workspace` is how much of the session's working directory, its `cwd`,
it sees -- `"none"`, the default, `"ro"` or `"rw"` -- at its path on this host, and `mounts`
the directories it sees besides, each a `Mount` or a path, at its path on this host. It never
sees more of the working directory than the session's mounts grant there, or anywhere inside
it -- a read-only mount inside a `"rw"` working directory stays read-only: one that asks for it
in a session with no `cwd`, or for `"rw"` where the session mounts it read-only, fails the
entry. `call_timeout_secs` and `description` are as for `McpCommand`.

### `outrig.McpPrimaryView`

```python
@dataclasses.dataclass(frozen=True)
class McpPrimaryView:
    name: str
    image: str
    args: tuple = ()
    env: Mapping = dataclasses.field(default_factory=dict)
    call_timeout_secs: int | None = None
    description: str | None = None
```

An MCP server a session places as `image`'s entrypoint, handed `args`, in a container that
sees the session container's whole filesystem, bound as `name`. As in an `McpCommand`, it may
hold no credential; `env`, `call_timeout_secs` and `description` are as there.

### `outrig.McpUrl`

```python
@dataclasses.dataclass(frozen=True)
class McpUrl:
    name: str
    url: str
    headers: Mapping = dataclasses.field(default_factory=dict)
    trust_anchor: str | os.PathLike | bytes | None = None
    trust: str = 'with-system-roots'
    allow_stateless: bool = True
    call_timeout_secs: int | None = None
    description: str | None = None
```

An MCP server a session dials at `url`, over streamable HTTP from this host, bound as
`name`: no container of the session's runs it, and it sees no file of the session's.

`headers` sets what each request carries: each value a literal, or `"${VAR}"`, which the
engine resolves; no value reaches this program or the agent. Each request also carries the
session's `mcp_identity` headers, which `headers` may not name. `trust_anchor` is a
certificate authority, PEM, its client trusts -- a path on this host, or the PEM as `bytes` --
beside the system's roots, or, with `trust="only"`, in their place. `allow_stateless` takes a
server that issues no session, as by default. A URL with a user name or password in it, a
scheme other than `http` or `https`, or a header HTTP or the protocol sets itself -- in
`headers` or `mcp_identity` -- does not start, and `session.mcp` says why.
`call_timeout_secs` and `description` are as for `McpCommand`.

### `outrig.McpPolicy`

```python
@dataclasses.dataclass(frozen=True)
class McpPolicy:
    connect_budget_secs: float | None = None
    reconnect_max: int | None = None
```

How a session's MCP servers start: `connect_budget_secs`, how long each has to start --
every attempt and every wait between them -- before it is `"unavailable"`, from 1 to 3600
seconds as the configuration's `mcp-connect-budget-secs` is, that or two minutes when `None`;
and `reconnect_max`, how many times one is connected again after its transport fails, three
when `None`. Raises `TypeError` for a value of another type, and `ValueError` for one out of
range.

### `McpOutcome`

```python
@dataclasses.dataclass(frozen=True)
class McpOutcome:
    service: str
    outcome: str
    tools: int | None = None
    listing: int | None = None
    attempts: int | None = None
    undecoded: tuple[tuple[str | None, str], ...] = ()
    cause: McpCause | None = None
```

How a declared MCP service's start ended: `session.mcp[name]` after entry. `outcome` is
`"ready"` -- started and bound, with `tools` tools in its first listing, generation `listing`,
after `attempts` attempts, and in `undecoded` each tool its server listed that did not decode,
left out, as `(name, reason)` -- or `"unavailable"`, `"refused"`, `"misconfigured"`,
`"unclassified"` or, for a repository's `url` entry nobody approved, `"unapproved"`, with the
`McpCause` in `cause`. The session starts with the ready ones; what the others mean is this
program's to decide.

### `McpCause`

```python
@dataclasses.dataclass(frozen=True)
class McpCause:
    service: str
    server: str | None
    placement: str
    declared_in: str
    command: str | None
    exit: str | None
    stderr_tail: str | None
    message: str
    kind: str | None
    attempts: int
```

Why an MCP service did not start, holding no value a `"${VAR}"` resolved to: the
`service`, the `server`'s own name where it differs, its `placement` -- `"command"`,
`"sidecar"`, `"primary_view"` or `"url"` -- who it was `declared_in`, the `command` run with
its environment as declared, how its transport ended (`exit`), the end of what it wrote to
stderr (`stderr_tail`), what went wrong (`message`), the `kind` of failure -- `"transport"`,
`"protocol"`, `"timeout"`, `"canceled"` or `"other"` -- and how many `attempts` were made; each
`None` where there is none.

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
    services: tuple[ServiceCall, ...]
    requests: tuple[RequestOutcome, ...]
    bindings: BindingStop | None
    events: EventDelivery
    mcp_services: tuple[McpServiceStop, ...] = ()
    mcp_uncertain: tuple[McpUncertainCall, ...] = ()
```

What a session's shutdown found: `session.shutdown_report` after the exit.

`verdict` is what it adds up to:

- `"clean"`: everything the session started is stopped, and every outcome is known.
- `"stopped_with_unknown"`: everything is stopped, and some execution, hosted call or service
  call was cut off, or an MCP call may have reached its server unanswered. Its outcome is
  unknown, never failed: its effect may stand.
- `"not_proven_stopped"`: something could not be confirmed stopped, and may still be acting;
  the exit raised `ShutdownException`. Do not reuse what the session ran on.

`closed_by`, a `ClosedBy`, is what closed admission; `stopped`, a `Stopped`, whether
everything is proven stopped; `executions`, each execution running when admission closed;
`hosted`, each hosted call whose outcome came after admission closed; `services`, each
`ServiceCall` whose outcome came after it; `mcp_services`, each `McpServiceStop`, every MCP
service and how it was stopped; `mcp_uncertain`, each `McpUncertainCall`, every MCP call that
may have reached its server unanswered during the session; `requests`, each
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

### `ServiceCall`

```python
@dataclasses.dataclass(frozen=True)
class ServiceCall:
    call: int
    service: str
    method: str
    agent: str
    outcome: str
    category: str | None
    reason: str | None
```

A call of a service's method whose outcome came after a session's admission closed: its
`call` id, the `service`, the `method` by its exact name, the `agent` that made it, its
`outcome` -- `"returned"`, `"raised"`, `"refused"`, `"cancelled"` or `"unknown"` -- the
service's `category` for a `"raised"` one, and the `reason` for one of the last three, such as
`"admission_closed"` or `"closed"`; each `None` where there is none.

### `McpServiceStop`

```python
@dataclasses.dataclass(frozen=True)
class McpServiceStop:
    service: str
    placement: str
    outcome: str
    how: str
    proven: bool
```

One MCP service at a session's close: the `service`, its `placement`, how its start ended
(`outcome`), `how` it was stopped -- `"stopped"`, its container stopped; `"with_primary"`, it
ended with the session's container; `"disconnected"`, the HTTP session to a URL server ended,
the server not the session's to stop; or `"not_started"` -- and whether that is `proven`.

### `McpUncertainCall`

```python
@dataclasses.dataclass(frozen=True)
class McpUncertainCall:
    call: int
    service: str
    tool: str
    agent: str
    failure: str | None
    reason: str | None
```

A call of an MCP service's tool that ended may have dispatched at any point in a session:
its request was handed to the server and no answer was read, so the server may have done what
it asked. Its `call` id, the `service`, the `tool` by its exact name, the `agent` that made it,
the `failure` that cut it off -- `"transport"`, `"timeout"`, `"canceled"`, `"other"` -- and the
`reason` it was told to stop, such as `"deadline"` or `"closed"`; each `None` where there is
none. Nothing called it again.

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
`Handle` at once, whose `status` is an `outrig.Status`; a program never constructs a `Handle`. A
class adds to its child's instructions by returning `outrig.Section`s from
`__outrig_instructions__`. See [Host programs](../usage/host-programs.md#agent-classes).

### `outrig.Agent`

```python
class Agent:
    def __init__(self, *, model=None, model_requests=None, session=None, facilities=None,
                 **inputs): ...
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
Methods not marked are ordinary ones. What a class cannot declare raises `TypeError` as the
class is made, naming the member.

`Reviewer(**inputs, model=None)` runs the class's `__init__` to its end, then its
`__outrig_attach__`, and only then attaches the instance: binds each input, which must be data,
in the child as a variable, launches the child in the background -- on `model`, or on yours --
and returns at once; a request made before the child exists waits for it. The child's first
round is its own, to get ready in: `await reviewer.ready()` returns once it has ended, and
raises what the launch met instead. `await reviewer.release()` ends the child and every request
still open on it; `async with Reviewer() as reviewer:` waits for `ready()` on entry and releases
on leaving. `model_requests=n`, or `(n, warnings)`, is the child's own model-request limit.

In the child, the instance is `self`: the same object, not a copy, so `self._task` there is what
`__init__` stored, and an ordinary method runs in whichever agent calls it. A request method
called on `self` there raises `TypeError`: requests are how its maker asks. The two agents share
`self` from two threads: one attribute read or write is atomic, a compound update is not. A
module's globals are shared by every agent that imports it; keep per-agent state on `self`. The
child holds `self`, so nothing collects an instance while its child lives: it ends as it is
released, as the agent that made it is, or as the session ends. A session's owner can construct
one as a root, in a kernel of its own, where it is `self` and needs no marked method.

`Reviewer(**inputs, facilities={"files": files})` gives the child, beside its data, the objects
it acts through, by name, kept as `reviewer.facilities`; a name is a variable's, not `outrig`,
`self`, an input's name or a bound class's. A binding's proxy becomes a reference to that
binding (`help(outrig.Facility)`); a proxy of an object a binding returned, a coroutine and a
generator are refused, naming the entry; anything else is the object itself, shared. The child
is told of these and of no other binding -- in its instructions, as globals where the name is
free, by `outrig.bindings` and `runtime.facilities` -- and without `facilities=` sees what this
agent sees. The mapping decides what an agent is told, not what it may reach, which is the
session's either way.

A subclass's `__init__` passes the base its keywords with `super().__init__(**inputs,
model=...)`, which launches nothing; one that does not raises `TypeError` once it returns.
Define `__outrig_attach__(self)` to finish what `__init__` cannot -- state a mixin's `__init__`
sets after the subclass's own, say: OutRig calls it on the class, so it needs no `super()`, and
ignores what it returns. Define `__outrig_instructions__(self)` to return the `outrig.Section`s
-- say, the task the constructor stored -- that the child's instructions carry after the
docstring: OutRig calls it once, after attachment's checks, so `self.facilities` reads as the
child's will. It should call no model and make no hosted call; nothing else of the instance is
read. A request method called before the instance is attached -- in `__init__` or either hook --
raises `TypeError`. A constructor or hook that raises, a return that is not `Section`s or a
section past its bound launches nothing, and the exception is the constructor call's; what it
did to your own objects stands, since OutRig rolls nothing back. An agent constructed inside one
is attached at once, and its child runs on if the constructor then raises, until its maker is
released or the session ends. The metaclass derives from `abc.ABCMeta`, so abstract mixins work;
a mixin whose metaclass is another raises `TypeError` as the class is made.

In a program `outrig run-new` runs on the host, `Reviewer(**inputs, model=None, session=None)`
is constructed on the running event loop its handles belong to, and its child is a root child of
`session`, the current `outrig.Session` when not given, on `model` or `default-model`. Its types
reach the child as data, and each reply is decoded here, into this program's own. The instance
stays here, so its child has no `self`, and one collected unreleased is released then. Construct
it, and leave its `async with`, inside the session's, whose exit releases it. In an agent's
kernel `session=` is refused; on the host, `facilities=` is, with `NotImplementedError`.

#### `outrig.Agent.facilities`

```python
@property
def facilities(self): ...
```

This agent's facilities, by name: a copy of the dict its constructor was given as
`facilities=`, which its child's view follows, read as it stands. Read-only as an
attribute; the dict is the agent's to change. Once the instance is attached, a binding's
proxy in it is a reference to that binding (`outrig.Facility`). Without `facilities=`, it
is a copy of its maker's, taken as it is attached, or `None` where its maker's view is the
session's, every binding.

#### `outrig.Agent.ready`

```python
async def ready(self): ...
```

Return once the child has ended its first round, which is its own, to get ready in.

Raises what the launch met instead -- `ValueError` for an unknown model,
`outrig.AgentLimitReached`, `outrig.SessionClosing` -- or `outrig.BudgetExhausted` or
`outrig.AgentRequestFailed` when the child cannot have that round, or
`outrig.AgentReleased` when the instance was released first. Awaiting it again answers
the same.

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

Cancel the request; whether this ended it. Not the child, which takes more work. A
request on a channel of the session's owner is not ended here: its far end is asked, once,
to withdraw it, and the handle settles on the answer, or reads `uncertain` with none; this
says whether it has been asked.

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
channel, and `accepted` or `failed` once it has ended; a request on a channel of the
session's owner is `sending` until the channel's far end has taken it, then `waiting`, or
`uncertain` while it is not known whether it took it or what became of a `cancel()`. `reason`
says why a failed one failed -- `rejected`, `failed`, `cancelled`, `released`, `budget`,
`closed`, `terminated`, or `limit` for a typed call whose child a limit refused, or a request
its child's model-request limit ended -- and is `None` otherwise. `child` is its child's
state: `running`, `idle`, `limit-reached`, `wedged` or `released`; `None` for a request on a
channel of the session's owner, which no child answers. Fixed once the request has ended.

### `outrig.Section`

```python
@dataclasses.dataclass(frozen=True)
class Section:
    title: str
    text: str
```

One titled part of an agent's instructions, which its class's `__outrig_instructions__`
returns: `Section("Task", f"Review {self._task.title}")`.

OutRig puts `text` in the instructions as it is, under `title`, after the class's docstring
and in the order returned; nothing is formatted into it. A title is 1 to 64 characters, a
text at most 16 KiB, and every title and text together at most 32 KiB, in UTF-8: past a bound
the agent's construction raises `ValueError`.

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
agents may nest to, the children a session holds, or the requests one child holds. Or the
child's model-request limit: at it, the child ends what it holds with this, and what is sent
to it, until the session's owner resets its count or raises its limit.

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

### `outrig.RequestEnded`

```python
class RequestEnded(AgentError):
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
builds alike. Every problem is reported with the JSON path where it is, up to `PROBLEMS_MAX`;
a path or a message past 1,024 characters -- one quoting a long key, say -- is shortened in its
middle, saying how much was left out.

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
- `outrig.facility` -- makes a reference, which `facilities=` refuses on the host.
- `outrig.Facility` -- what `outrig.facility` makes; each use of one raises `RuntimeError`.
- `outrig.Service` -- the class of a service's object, which only a kernel holds.
- `outrig.ServiceError` -- what a call of a service's method raises, which only a kernel makes.
- `outrig.ServiceMethod` -- what a service's object lists of its methods, which only a kernel holds.
- `outrig.McpService` -- the class of an MCP service's object, which only a kernel holds.
- `outrig.McpTool` -- what an MCP service's `tool` returns, which only a kernel holds.
- `outrig.McpRelisted` -- what an MCP service's `relist` returns, which only a kernel holds.
- `outrig.McpResult` -- what a call of an MCP service's tool returns, which only a kernel makes.
- `outrig.McpBlock` -- the base of an `McpResult`'s content blocks, which only a kernel makes.
- `outrig.McpText` -- an `McpResult`'s text block, which only a kernel makes.
- `outrig.McpImage` -- an `McpResult`'s image block, which only a kernel makes.
- `outrig.McpAudio` -- an `McpResult`'s audio block, which only a kernel makes.
- `outrig.McpResource` -- an `McpResult`'s embedded resource block, which only a kernel makes.
- `outrig.McpResourceLink` -- an `McpResult`'s resource link block, which only a kernel makes.
- `outrig.McpOther` -- an `McpResult`'s block of a kind not modeled, which only a kernel makes.
- `outrig.McpError` -- what a call of an MCP service's tool raises, which only a kernel makes.
- `outrig.NotDispatched` -- an `McpError` of a call its server never got, which only a kernel makes.
- `outrig.MayHaveDispatched` -- an `McpError` of a call its server may have done, only in a kernel.
- `outrig.ResponseReceived` -- an `McpError` of a call its server answered, only in a kernel.
- `outrig.UnknownTool` -- a `NotDispatched` for a tool not in the listing, only in a kernel.
- `outrig.InvalidArguments` -- a `NotDispatched` for arguments the schema refused, only in a kernel.
- `outrig.ServiceUnavailable` -- a `NotDispatched` for a server not connected, only in a kernel.
- `outrig.AdmissionClosed` -- a `NotDispatched` for a call made as the session closed, in a kernel.
- `outrig.Paused` -- a `NotDispatched` for a call of a paused MCP service, only in a kernel.
- `outrig.ProtocolError` -- a `ResponseReceived` for an error its server answered, only in a kernel.
- `outrig.ToolError` -- what a call raises for a result its MCP server marked an error, in a kernel.
- `outrig.ChannelError` -- the base of what an external channel raises, which only a kernel holds.
- `outrig.ChannelClosed` -- what an external channel raises once closed, which only a kernel holds.
- `outrig.SendRefused` -- what an external channel's `send` raises, which only a kernel holds.
- `outrig.SendNotAccepted` -- what an external channel's `send` raises, which only a kernel holds.
- `outrig.SendUncertain` -- what an external channel's `send` raises, which only a kernel holds.
- `outrig.Resolution` -- what an external channel's `resolve` returns, which only a kernel holds.
- `outrig.UnreadableDelivery` -- what an external channel's `receive` may raise, only in a kernel.
