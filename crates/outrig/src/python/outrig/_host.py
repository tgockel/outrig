"""The host's half of `outrig`: a program's connection to the engine that runs it,
`outrig.config()`, and the sessions the program owns, `outrig.Session`. The bootstrap `outrig run
<script.py>` starts installs it before the program runs; nothing else imports it, and no kernel
ever does.

The connection is two pipes the engine passed down, one each way, each line one JSON object. They
are the program's alone: close-on-exec from here on, so no program this one starts inherits them,
and closed in a forked child. A thread of its own reads the engine's lines, so the program's
main thread, and the event loop it may run there, never serves the connection. The program's
first line is a greeting naming the protocol's version, and the script runs only once the engine
has answered with the same version. Each request, the greeting first, carries an id, and its one
reply the same id; a session's events come unasked, with none.

The engine's death ends the program and everything it started, as the bindings process ends with
its owner (`supervisor.rs`). Two things say the engine is gone: the parent-death signal, SIGHUP,
which the engine left blocked until the handler below is in place, and the end of the connection,
which the reading thread sees.

An `outrig.Agent` subclass constructed here is a root child of a session the program owns
("agent classes", below).
"""

import asyncio
import collections
import concurrent.futures
import contextlib
import contextvars
import copy
import dataclasses
import datetime
import inspect
import itertools
import json
import keyword
import os
import pathlib
import select
import signal
import threading
import time
import types
import weakref
from collections.abc import Mapping

import outrig

from . import _agents, schema

# The signal the engine's death delivers: `PR_SET_PDEATHSIG`, set between fork and exec and
# blocked until `install` has put the handler in place, so a death in between is pending rather
# than lost.
DEATH_SIGNAL = signal.SIGHUP
# How long the programs this one started have to leave on SIGTERM before SIGKILL, once the engine
# is gone and nobody else will send it.
DEATH_GRACE = 1.0
# The version of the protocol over the connection, which the greeting names; the engine refuses
# any other.
PROTOCOL = 2
# How many pushes of a session's events this program holds for a reader that falls behind before
# it drops the oldest. The engine holds as many for this program, and drops past it too, counted.
EVENT_CAPACITY = 4096
# The longest line the engine reads, in bytes, its newline aside: it discards a longer one
# unanswered, having no id to answer it by. A request's body, and an instance's inputs, are in one,
# bounded as a kernel's message is.
LINE_MAX = 1 << 20
# The longest grace the engine takes for a session's shutdown, in seconds: an hour.
GRACE_MAX = 3600
_dying = threading.Lock()  # taken by the one call to `_die` that runs
# The connection to the engine, or `None` in a process this program forked.
_connection = None


def _diag(text):
    """One line on stderr, written on a thread of its own. Stderr is the program's, a pipe OutRig
    neither owns nor drains, so a full one may never take the line; nothing here waits for it,
    least of all the way the program ends with the engine."""
    data = f"outrig-program: {text}\n".encode("utf-8", "backslashreplace")

    def write():
        with contextlib.suppress(OSError):
            _write_all(2, data)

    # No new thread once the interpreter is shutting down: the line is dropped.
    with contextlib.suppress(RuntimeError):
        threading.Thread(target=write, name="outrig-diagnostic", daemon=True).start()


def _die(reason):
    """End this program and everything it started: SIGTERM to its process group, a grace, then
    SIGKILL to the group, itself included. Once, and on the main thread, where the signal handler
    runs. `killpg` cannot fail here: the program leads its own group, and may signal itself."""
    if not _dying.acquire(blocking=False):
        return
    _diag(f"{reason}; stopping the program and its process group")
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    os.killpg(os.getpgrp(), signal.SIGTERM)
    time.sleep(DEATH_GRACE)
    os.killpg(os.getpgrp(), signal.SIGKILL)
    os._exit(1)


def _lost():
    """The connection ended, so the engine is gone: have the main thread run `_die` through the
    signal it would have received, and kill the group from here if it cannot -- native code
    holding the main thread, or a handler of the program's own in place of `_die`."""
    os.kill(os.getpid(), DEATH_SIGNAL)
    time.sleep(DEATH_GRACE + 1.0)
    _diag("the engine is gone and the program did not stop; killing its process group")
    os.killpg(os.getpgrp(), signal.SIGKILL)


def _write_all(fd, data):
    """Write every byte of `data`, even to a pipe something has made non-blocking."""
    view = memoryview(data)
    while view:
        try:
            written = os.write(fd, view)
        except BlockingIOError:
            poller = select.poll()
            poller.register(fd, select.POLLOUT)
            poller.poll()
            continue
        view = view[written:]


class _Connection:
    """Requests to the engine, each answered by one reply with its id, notices it does not answer,
    and what it says of the program's sessions unasked -- their events, and what becomes of their
    agent class instances; all read on a thread of its own."""

    def __init__(self, fd_in, fd_out):
        self.fds = (fd_in, fd_out)
        self._send = threading.Lock()
        self._ids = itertools.count()  # from 0, the greeting's
        # id -> the `concurrent.futures.Future` its caller waits on. Callers add entries and the
        # reading thread pops them with no lock: each is one dict operation, atomic under the GIL.
        self._waiting = {}
        # A session's handle -> the `_Events` its pushes go to, from its entry to its
        # `events.end`, under the same rule.
        self.events = {}
        # Why requests can no longer be answered, once they cannot.
        self._broken = None
        threading.Thread(target=self._read, name="outrig-engine", daemon=True).start()

    def post(self, kind, /, **fields):
        """Send a `kind` request, and return the `concurrent.futures.Future` the reading thread
        settles with the engine's reply, which names the same kind, or with `RuntimeError` when
        the engine refused the request or the connection broke. A coroutine awaits it through
        `asyncio.wrap_future`."""
        ident = next(self._ids)
        reply = self._waiting[ident] = concurrent.futures.Future()
        try:
            self.tell(kind, id=ident, **fields)
        except BaseException:
            self._waiting.pop(ident, None)
            raise
        return reply

    def tell(self, kind, /, **fields):
        """Send a `kind` line: a notice, which the engine does not answer, unless it carries the
        `id` a request does. Raises `RuntimeError` once the connection is broken."""
        if self._broken is not None:
            raise RuntimeError(self._broken)
        # Encoded before it is sent, so a line the engine could not read is refused here rather
        # than sent to go unanswered: a str UTF-8 cannot hold -- a path that is not UTF-8, which
        # Python holds as lone surrogates -- raises `UnicodeEncodeError`, and a float JSON has no
        # form for, or a line longer than the engine reads, `ValueError`.
        line = json.dumps({"t": kind, **fields}, ensure_ascii=False, allow_nan=False).encode()
        if len(line) > LINE_MAX:
            raise ValueError(
                f"a {kind} request of {len(line)} bytes is longer than the {LINE_MAX} the engine "
                "reads"
            )
        try:
            with self._send:
                _write_all(self.fds[1], line + b"\n")
        except OSError as e:
            raise RuntimeError(f"the connection to the engine is broken: {e}") from None

    def request(self, kind, **fields):
        """Send a `kind` request and return the engine's reply, which names the same kind."""
        return self.post(kind, **fields).result()

    def _read(self):
        pending = b""
        while True:
            try:
                chunk = os.read(self.fds[0], 1 << 16)
            except OSError as e:
                # The program closed or replaced the descriptor: the engine is not gone, so
                # nothing is killed, but nothing more can be answered.
                self._break(f"the connection to the engine is broken: {e}")
                return
            if not chunk:
                self._break("the engine that runs this program is gone")
                _lost()
                return
            pending += chunk
            while b"\n" in pending:
                line, _, pending = pending.partition(b"\n")
                self._deliver(line)

    def _deliver(self, line):
        try:
            message = json.loads(line)
            if "id" not in message:
                # A push, for one session: an event, `events.end` their last, or what became of
                # one of its agent class instances or their requests.
                handle = message["session"]
                if message["t"] == "events.end":
                    self.events.pop(handle).push(message)
                elif message["t"] in ("event", "missed"):
                    self.events[handle].push(message)
                else:
                    _pushed(message)
                return
            reply = self._waiting.pop(message["id"])
        except (ValueError, KeyError, TypeError) as e:
            _diag(f"ignored a line from the engine ({e!r}): {line[:200]!r}")
            return
        # A future a coroutine stopped waiting for is cancelled, and takes nothing.
        with contextlib.suppress(concurrent.futures.InvalidStateError):
            if message.get("t") == "error":
                why = message.get("error", "the engine refused the request")
                reply.set_exception(RuntimeError(why))
            else:
                reply.set_result(message)

    def _break(self, why):
        self._broken = why
        while self._waiting:
            _, reply = self._waiting.popitem()
            with contextlib.suppress(concurrent.futures.InvalidStateError):
                reply.set_exception(RuntimeError(why))
        # Nor will any more of a session's events come, nor of its agent class instances.
        for events in list(self.events.values()):
            events.push({"t": "events.end"})
        _closed(list(_roots.values()), terminated=True)


def _forget():
    """In a child this program forked: the connection is the program's alone, and so is the
    handler that ends the program's group."""
    global _connection
    if _connection is not None:
        for fd in _connection.fds:
            with contextlib.suppress(OSError):
                os.close(fd)
        _connection = None
    with contextlib.suppress(ValueError):
        signal.signal(DEATH_SIGNAL, signal.SIG_DFL)


def install(fd_in, fd_out):
    """Connect `outrig` to the engine over `fd_in` and `fd_out`, end the program with the engine,
    and bind each name `outrig._HOST_ONLY` lists. Main thread, before the program runs. An
    engine that does not answer the greeting with this protocol's version ends the program with
    status 1, and says why itself."""
    global _connection
    for fd in (fd_in, fd_out):
        os.set_inheritable(fd, False)
    signal.signal(DEATH_SIGNAL, lambda *_: _die("the engine that runs this program is gone"))
    # Started while the signal is still blocked, so the reading thread inherits it blocked and the
    # signal goes to the main thread, where the handler runs.
    _connection = _Connection(fd_in, fd_out)
    # Greeted while the signal is still blocked too, so a greeting that fails -- refused, or the
    # connection ending first -- ends the program here, quietly: the end of the connection has the
    # reading thread send the signal, and `_die` would print a line of its own.
    try:
        hello = _connection.request("hello", protocol=PROTOCOL)
    except RuntimeError:
        hello = {}
    if hello.get("protocol") != PROTOCOL:
        os._exit(1)
    signal.pthread_sigmask(signal.SIG_UNBLOCK, {DEATH_SIGNAL})
    os.register_at_fork(after_in_child=_forget)
    for name in outrig._HOST_ONLY:
        setattr(outrig, name, globals()[name])
    _agents._launch = _Root


# ------------------------------------------------------------------------------- the configuration


@dataclasses.dataclass(frozen=True)
class Model:
    """A concrete model the configuration declares: a `[models.<name>]` row with a provider.

    `identifier` is what OutRig sends the provider: the row's `identifier`, or its name where the
    configuration leaves that out. `context_window` and `max_tokens` are the row's
    `context-window` and `max-tokens`, or `None` where it declares none. Each is a number the
    configuration states, not the model's capacity: OutRig does not ask the provider.
    """

    name: str
    provider: str
    identifier: str
    context_window: int | None
    max_tokens: int | None


@dataclasses.dataclass(frozen=True)
class Alias:
    """A `[models.<name>]` row with `alias`: the concrete models it stands for, in the order a
    call tries them, an alias of aliases flattened."""

    name: str
    candidates: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class Provider:
    """A `[providers.<name>]` table: its `style`, `"chat-completions"`, `"responses"` or `"anthropic"`, and its `base-url`.
    Its key is not here, nor the variable that holds it."""

    name: str
    style: str
    base_url: str


@dataclasses.dataclass(frozen=True)
class Events:
    """The `[events]` table: `mode` is `"off"` or `"record"`."""

    mode: str


@dataclasses.dataclass(frozen=True)
class Config:
    """The configuration `outrig run-new` loaded before this program started: see
    `help(outrig.config)`."""

    models: Mapping[str, Model]
    aliases: Mapping[str, Alias]
    providers: Mapping[str, Provider]
    default_model: str | None
    workspace: pathlib.Path | None
    events: Events


def config():
    """The configuration `outrig run-new` loaded before this program started, as a frozen,
    read-only view.

    `outrig run-new` reads it once, from the directory it was invoked in -- with `--config` and the
    global file as for every command -- and never again: not from the script's directory, nor
    from a workspace a session later names. It is what a session this program starts is built
    from.

    - `models`: each concrete model by name, a `Model`: its provider, the identifier OutRig sends
      that provider, and its declared `context_window` and `max_tokens`.
    - `aliases`: each alias by name, an `Alias`: the concrete models it stands for, in the order
      a call tries them.
    - `providers`: each provider by name, a `Provider`: its `style` and `base_url`.
    - `default_model`: the model or alias `default-model` names, or `None`.
    - `workspace`: the configured `[workspace] host-path`, resolved to an absolute path on this
      host, or `None` where the configuration names none.
    - `events`: the `[events]` table, whose `mode` is `"off"` or `"record"`.

    A declared window is a number the configuration states, not the model's capacity: OutRig
    does not ask the provider. No API key, no `${VAR}` value and no other secret is in the view;
    the engine resolves those itself when a session calls a model.

    Every call asks the engine, and returns a new value.
    """
    if _connection is None:
        raise RuntimeError(
            "outrig.config() answers in the program `outrig run-new` started, not in a process it "
            "forked"
        )
    view = _connection.request("config")["config"]
    workspace = view["workspace"]
    return Config(
        models=types.MappingProxyType(
            {name: Model(name=name, **row) for name, row in view["models"].items()}
        ),
        aliases=types.MappingProxyType(
            {
                name: Alias(name=name, candidates=tuple(row["candidates"]))
                for name, row in view["aliases"].items()
            }
        ),
        providers=types.MappingProxyType(
            {name: Provider(name=name, **row) for name, row in view["providers"].items()}
        ),
        default_model=view["default_model"],
        workspace=None if workspace is None else pathlib.Path(workspace),
        events=Events(**view["events"]),
    )


# ---------------------------------------------------------------------------------------- sessions

# Each session's handle on the connection, which the program chooses.
_handles = itertools.count(1)
# The session whose `async with` block the code reading it runs in.
_current = contextvars.ContextVar("outrig.current_session", default=None)


@dataclasses.dataclass(frozen=True)
class Mount:
    """One more directory a session's container sees, as a `[[workspace.mounts]]` entry
    declares one.

    `host_path` is the directory on this host, a str or path made absolute against the working
    directory when the `Mount` is made. `container_path` is where the container sees it, an
    absolute path, or `None` for the same path as on the host, which a session with bindings
    requires of every mount. `access` is `"read-only"`, the default, or `"read-write"`.
    """

    host_path: str
    container_path: str | None = None
    access: str = "read-only"

    def __post_init__(self):
        object.__setattr__(self, "host_path", str(pathlib.Path(self.host_path).absolute()))
        if self.container_path is not None:
            container = pathlib.PurePosixPath(self.container_path)
            if not container.is_absolute():
                raise ValueError(f"container_path {str(container)!r} is not absolute")
            object.__setattr__(self, "container_path", str(container))
        if self.access not in ("read-only", "read-write"):
            raise ValueError(f"access is 'read-only' or 'read-write', not {self.access!r}")


@dataclasses.dataclass(frozen=True)
class Event:
    """One record of a session's events, as `events.jsonl` records it.

    `id` is its place in the session's sequence, from 1; `type` its type after `org.outrig.`,
    such as `session.state`; `subject` the agent it belongs to, or `None` for one about the
    session; `time` when it was published, an aware `datetime` in UTC; and `data` its payload, as
    decoded JSON. The event log's reference says what each type means.
    """

    id: int
    type: str
    subject: str | None
    time: datetime.datetime
    data: dict


@dataclasses.dataclass(frozen=True)
class Missed:
    """`count` records of a session's events lost between the item before this one and the item
    after it, because they were not read in time: see `help(outrig.Session.events)`."""

    count: int


@dataclasses.dataclass(frozen=True)
class ClosedBy:
    """What closed a session's admission: `by` is `"owner"` -- the program, or the shutdown --
    or `"interpreter_exited"`, with the exit's `cause`."""

    by: str
    cause: str | None = None


@dataclasses.dataclass(frozen=True)
class Stopped:
    """Whether what a session started is proven stopped: `state` is `"proven"`, or `"not_proven"`
    with the `reason`."""

    state: str
    reason: str | None = None


@dataclasses.dataclass(frozen=True)
class ExecutionOutcome:
    """An execution running when a session's admission closed, and how it ended: its `id`, and its
    `status`, `"ok"` or `"error"` when its result arrived, `"unknown"` when none did -- what it
    did may or may not have happened."""

    id: int
    status: str


@dataclasses.dataclass(frozen=True)
class HostedCall:
    """A hosted request whose outcome came after a session's admission closed: its `request` id,
    the `agent` that made it, its `binding`, its `outcome` -- `"returned"`, `"raised"`,
    `"refused"`, `"cancelled"` or `"unknown"` -- and the `reason` for one of the last three, such
    as `"admission_closed"` or `"killed"`, or `None`."""

    request: int
    agent: str
    binding: str
    outcome: str
    reason: str | None


@dataclasses.dataclass(frozen=True)
class RequestOutcome:
    """A request made of a child agent that was open when a session's admission closed: its
    `request` id, the `child` it was made of, the `channel` it was sent on -- `None` for a
    submission -- and its `outcome`: `"closed"`, or whatever ended it first, or `"terminated"`
    when the interpreter died before anything did."""

    request: int
    child: str
    channel: str | None
    outcome: str


@dataclasses.dataclass(frozen=True)
class BindingStop:
    """How the process holding a session's bindings stopped: SIGTERM to its process group, a
    grace, and SIGKILL if anything was left. `names` are its bindings; `group_empty` whether its
    group is proven empty; `killed` whether SIGKILL was needed; `survivors` what was left in the
    group when it could not be proven empty, each `<pid> <command> <state>`; and `exited` how the
    process ended, when it ended while the session ran, or `None`."""

    names: tuple[str, ...]
    group_empty: bool
    killed: bool
    survivors: tuple[str, ...]
    exited: str | None


@dataclasses.dataclass(frozen=True)
class LogLoss:
    """Records a session's `events.jsonl`, at `path`, did not get: how many (`records`), why the
    first was lost (`first`), and why the file may hold a partial record when the writer could
    not prove its rollback (`integrity`), or `None`."""

    path: str
    records: int
    first: str
    integrity: str | None


@dataclasses.dataclass(frozen=True)
class EventDelivery:
    """How a session's events reached their subscribers: `last_sequence`, the id of the last one
    published; `missed`, how many each subscription lost, in the order they were made -- the
    first is `Session.events()`'s, the engine's drops and this program's together -- and `log`, a
    `LogLoss` when `events.jsonl` missed any, or `None`."""

    last_sequence: int
    missed: tuple[int, ...]
    log: LogLoss | None


@dataclasses.dataclass(frozen=True)
class ShutdownReport:
    """What a session's shutdown found: `session.shutdown_report` after the exit.

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
    """

    verdict: str
    closed_by: ClosedBy
    stopped: Stopped
    executions: tuple[ExecutionOutcome, ...]
    hosted: tuple[HostedCall, ...]
    requests: tuple[RequestOutcome, ...]
    bindings: BindingStop | None
    events: EventDelivery


def _report(raw, lost):
    """The `ShutdownReport` the engine's JSON `raw` describes, with the `lost` records this program
    dropped added to what its subscription missed."""
    bindings, events = raw["bindings"], raw["events"]
    if bindings is not None:
        names, survivors = tuple(bindings.pop("names")), tuple(bindings.pop("survivors"))
        bindings = BindingStop(names=names, survivors=survivors, **bindings)
    missed = list(events["missed"])
    missed[0] += lost
    return ShutdownReport(
        verdict=raw["verdict"],
        closed_by=ClosedBy(**raw["closed_by"]),
        stopped=Stopped(**raw["stopped"]),
        executions=tuple(
            ExecutionOutcome(id=row["execid"], status=row["status"]) for row in raw["executions"]
        ),
        hosted=tuple(HostedCall(**row) for row in raw["hosted"]),
        requests=tuple(RequestOutcome(**row) for row in raw["requests"]),
        bindings=bindings,
        events=EventDelivery(
            last_sequence=events["last_sequence"],
            missed=tuple(missed),
            log=None if events["log"] is None else LogLoss(**events["log"]),
        ),
    )


class ShutdownException(Exception):
    """A session's exit could not prove that everything the session started stopped: something
    it started may still be acting. Raised in the task that left the session's `async with`
    block, with the block's own exception, if any, as its `__context__`.

    `report` is the session's `ShutdownReport`, also its `shutdown_report`; `report.stopped`
    says what could not be confirmed stopped. Not an `outrig.AgentError`: it is about the
    session, not about an agent's work.
    """

    def __init__(self, report):
        super().__init__(f"the session is not proven stopped: {report.stopped.reason}")
        self.report = report


class _Events:
    """One session's events as the engine pushes them, held for `Session.events()`: at most
    `EVENT_CAPACITY` pushes, the oldest dropped and counted as more arrive, so the reading thread
    never waits for the program's reader."""

    def __init__(self):
        self.lock = threading.Lock()
        self.held = collections.deque()  # `event` and `missed` pushes, oldest first
        self.gap = 0  # records lost before the front of `held`, not yet reported
        self.lost = 0  # `event` pushes dropped here, in all
        self.ended = False  # `events.end` came
        self.waiter = None  # the future a reader awaits while nothing is held

    def push(self, message):
        """Take one push, on the reading thread."""
        with self.lock:
            if message["t"] == "events.end":
                self.ended = True
            else:
                if len(self.held) == EVENT_CAPACITY:
                    oldest = self.held.popleft()
                    if oldest["t"] == "missed":
                        self.gap += oldest["count"]
                    else:
                        self.gap += 1
                        self.lost += 1
                self.held.append(message)
            waiter, self.waiter = self.waiter, None
        if waiter is not None:
            # A loop that has closed has no reader left to wake.
            with contextlib.suppress(RuntimeError):
                waiter.get_loop().call_soon_threadsafe(_wake, waiter)

    def __aiter__(self):
        return self

    async def __anext__(self):
        while True:
            with self.lock:
                # The engine's own counts at the front join the gap, which one `Missed` reports.
                while self.held and self.held[0]["t"] == "missed":
                    self.gap += self.held.popleft()["count"]
                if self.gap:
                    gap, self.gap = self.gap, 0
                    return Missed(gap)
                if self.held:
                    record = self.held.popleft()["record"]
                    break
                if self.ended:
                    raise StopAsyncIteration
                self.waiter = waiter = asyncio.get_running_loop().create_future()
            await waiter
        return Event(
            id=int(record["id"]),
            type=record["type"].removeprefix("org.outrig."),
            subject=record.get("subject"),
            time=datetime.datetime.fromisoformat(record["time"]),
            data=record["data"],
        )


def _wake(waiter):
    if not waiter.done():
        waiter.set_result(None)


async def _settled(reply):
    """Wait until `reply`, a request's future, is settled, through any cancellation of the task
    meanwhile. Return the cancellation that came, or `None`, for the caller to raise once it has
    read the reply."""
    waiting = asyncio.wrap_future(reply)
    cancelled = None
    while not waiting.done():
        try:
            await asyncio.wait([waiting])
        except asyncio.CancelledError as e:
            cancelled = e
    # Read here, so a refusal the caller reads from `reply` is not also logged as never read.
    waiting.exception()
    return cancelled


class Session:
    """A session this program owns, with no primary agent: a container, the interpreter in it,
    and the configuration's bindings, which the engine that runs this program starts and stops.

        async with outrig.Session(workspace="./repo") as session:
            ...
        print(session.shutdown_report.verdict)

    Making one checks its inputs and starts nothing:

    - `workspace`: the directory to mount as the session's workspace, a str or path made
      absolute against the working directory now. `None` takes the configuration's `[workspace]
      host-path` where it declares one; with neither, no workspace is mounted.
    - `mounts`: more directories, each a `Mount` or a path, which is `Mount(path)`, added to the
      configuration's `[[workspace.mounts]]`. A host or container path given twice fails entry.
    - `grace`: how many seconds the exit lets what is still running -- hosted calls, executions --
      finish before it stops what is left, at most an hour, or `None` for the engine's default,
      five. It bounds that wait only: interrupting kernels and stopping the bindings and the
      container after it take bounded times of their own.

    `async with` starts it in the image `default-image` names, with the configured factory
    bindings -- a repository's own are refused, there being nobody to approve them -- and records
    its events in its session directory under `[events] mode = "record"`. Inside the block it is
    `current_session()`, in the tasks created there too. A session is entered once. A start that
    fails raises `RuntimeError` with the engine's reason, once what it had started is stopped,
    and the block does not run; one asking for more mounts than the engine reads in a request
    raises `ValueError` before anything starts.

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

    `id` is the engine's id for the session once entered, which names its container and its
    directory under the session root, and `None` before.
    """

    def __init__(self, *, workspace=None, mounts=(), grace=None):
        if isinstance(mounts, str):
            raise TypeError("mounts takes Mount entries or paths, not one path")
        if grace is not None:
            if isinstance(grace, bool) or not isinstance(grace, (int, float)):
                raise TypeError(f"grace takes None or a number of seconds, not {grace!r}")
            if not 0 <= grace <= GRACE_MAX:
                raise ValueError(f"grace takes from 0 to {GRACE_MAX} seconds, not {grace!r}")
        self._workspace = None if workspace is None else str(pathlib.Path(workspace).absolute())
        self._mounts = [dataclasses.asdict(m if isinstance(m, Mount) else Mount(m)) for m in mounts]
        self._grace = grace
        self._handle = next(_handles)
        self._events = _Events()
        self._entered = False
        self._reading = False
        # How many requests an agent class instance may hold unsettled, which entry says.
        self._requests_max = float("inf")
        self.id = None
        self.shutdown_report = None

    async def __aenter__(self):
        if self._entered:
            raise RuntimeError("an outrig.Session is entered once; make another to enter again")
        self._entered = True
        try:
            if _connection is None:
                raise RuntimeError(
                    "a session starts in the program `outrig run-new` started, not a fork"
                )
            _connection.events[self._handle] = self._events
            start = _connection.post(
                "session.start",
                session=self._handle,
                workspace=self._workspace,
                mounts=self._mounts,
            )
        except BaseException:
            # Nothing was sent, so nothing but this ends the session's events.
            if _connection is not None:
                _connection.events.pop(self._handle, None)
            self._events.push({"t": "events.end"})
            raise
        try:
            answer = await asyncio.wrap_future(start)
        except asyncio.CancelledError:
            # Never left running: stopped, while it starts or after, before this propagates.
            stop = _connection.post("session.shutdown", session=self._handle, grace=self._grace)
            await _settled(stop)
            if stop.exception() is None and "report" in stop.result():
                self.shutdown_report = _report(stop.result()["report"], self._events.lost)
            raise
        self.id = answer["session"]["id"]
        self._requests_max = answer["session"]["requests_max"]
        self._token = _current.set(self)
        return self

    async def __aexit__(self, kind, error, traceback):
        # Left in another context than the one entered, the block leaves that context its own.
        with contextlib.suppress(ValueError):
            _current.reset(self._token)
        stop = _connection.post("session.shutdown", session=self._handle, grace=self._grace)
        cancelled = await _settled(stop)
        self.shutdown_report = _report(stop.result()["report"], self._events.lost)
        # Whatever of its agent class instances nothing ended, nothing will now.
        _closed([root for root in list(_roots.values()) if root.session is self], terminated=True)
        if self.shutdown_report.verdict == "not_proven_stopped":
            raise ShutdownException(self.shutdown_report)
        if cancelled is not None:
            raise cancelled

    async def close_admission(self):
        """Close the session to new work, and return once the engine has closed it: from then on,
        a call an agent makes of a binding is refused, admission closed, and so is an execution.
        What is already running is left to finish, or to the exit to stop. After the exit, it
        returns at once."""
        if self.shutdown_report is None:
            await asyncio.wrap_future(_connection.post("admission.close", session=self._handle))

    def events(self):
        """The session's events, as an async iterator, in the order the session published them.
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
        """
        if self._reading:
            raise RuntimeError("a session's events are read through one iterator, already made")
        self._reading = True
        return self._events


def current_session():
    """The `Session` whose `async with` block the caller runs in, the innermost where blocks
    nest: in this task, and in a task or a `to_thread` call started inside the block, which copy
    the context; not in a `threading.Thread`, which starts with an empty one. A task that outlives
    the block still sees the session. Outside every session's block, raises `RuntimeError`."""
    session = _current.get()
    if session is None:
        raise RuntimeError(
            "no outrig.Session is entered here: enter one with `async with outrig.Session()`, "
            "or pass the session where it is needed"
        )
    return session


# ----------------------------------------------------------------------------------- agent classes
#
# An `outrig.Agent` subclass constructed here is a root child of a session: its parent is this
# program, which the engine names `owner`. The program speaks for the instance as a kernel's code
# speaks for its own -- a spawn, a release, what became of the instance -- and its requests reach
# the child's kernel through the engine, each numbered here: its body in JSON, decoded there into
# the classes the child made again from their descriptions, and each reply back in JSON, decoded
# here into the program's own, which the program's verdict accepts or sends back with what is
# wrong. What becomes of each comes unasked, on the connection's thread, and is handled on the
# loop the instance was constructed on, since handling it may run the program's code.

_numbers = itertools.count(1)  # each instance's and each request's, for the program's life
# How long `cancel()` waits for the child's kernel to say how a request it sent ended, in seconds.
CANCEL_WAIT = 10
_roots = {}  # each instance not yet released, by its number
_asks = {}  # each request not yet ended, by its number
# What a launch, `ready()` or a request ends with, by the engine's name for how it ended.
_ERRORS = {
    "model": ValueError,
    "limit": _agents.AgentLimitReached,
    "released": _agents.AgentReleased,
    "closed": _agents.SessionClosing,
    "terminated": _agents.SessionClosing,
    "budget": _agents.BudgetExhausted,
    "failed": _agents.AgentRequestFailed,
    "cancelled": _agents.RequestCancelled,
}
# A request's status when it ends with a launch's error, by the error's class.
_REASONS = {
    _agents.AgentLimitReached: "limit",
    _agents.SessionClosing: "closed",
    _agents.AgentReleased: "released",
}


class _Root:
    """What the program keeps of an agent class instance it constructed, whose child is a root
    child of a session: its launch, the requests made before its child exists, and those open.
    Nothing here holds the instance, so that collecting it releases the child."""

    def __init__(self, agent, inputs, model, session):
        cls = type(agent)
        if session is None:
            session = current_session()
        elif type(session) is not Session:
            raise TypeError(f"session= takes an outrig.Session, not {type(session).__qualname__}")
        try:
            self.loop = asyncio.get_running_loop()
        except RuntimeError:
            raise RuntimeError(
                f"{cls.__qualname__}() is constructed on the running event loop its requests' "
                "handles belong to, and none runs here"
            ) from None
        self.methods = {
            name: send.__outrig_channel__ for name, send in cls._outrig_channels.items()
        }
        manifest, found, classes = [], set(), {}
        for name, value in inputs.items():
            if (not name.isidentifier() or keyword.iskeyword(name) or name.startswith("__")
                    or name in _agents.RESERVED):
                raise ValueError(f"{cls.__qualname__}(): input {name!r} cannot name a variable in "
                                 "the child")
            encoded, shape, used = _agents.walk(value, name)
            manifest.append({"name": name, "type": shape, "preview": _agents.preview(encoded)})
            found |= used
        bound = list(dict.fromkeys(c for method in self.methods.values() for c in method.classes))
        _agents.one_name_each(bound, inputs)
        _agents.one_name_each(bound + sorted(found, key=schema._type_qualname))
        for c in found:
            _agents.described(c, classes)
        channels = {
            name: {"request": _agents.described(m.request_type, classes),
                   "reply": _agents.described(m.reply_type, classes), "doc": m.doc}
            for name, m in self.methods.items()
        }
        self.id, self.session, self.child = next(_numbers), session, None
        self.name = f"{cls.__name__[:48]}-{self.id}"
        line = {
            "session": session._handle, "name": self.name, "prompt": inspect.cleandoc(cls.__doc__),
            "model": model, "kind": "request", "manifest": manifest,
            "channels": [
                {"name": name, "doc": m.doc, "request_type": schema._describe(m.request_type),
                 "reply_type": schema._describe(m.reply_type), "request": m.request.text(),
                 "reply": m.reply.text()}
                for name, m in self.methods.items()
            ],
            "instance": {"id": self.id, "module": cls.__module__, "qualname": cls.__qualname__,
                         "digest": cls._outrig_digest},
            # As text, so the engine, which parses a line into JSON values that sort an object's
            # keys and hold an integer past 64 bits as a float, changes none of it.
            "root": json.dumps({
                "name": self.name, "classes": classes, "channels": channels,
                "inputs": {name: _agents.tagged(value) for name, value in inputs.items()},
            }, allow_nan=False),
        }
        self.instance = None  # the session's id for the instance, which its launch answers
        self.life = "idle"  # what the engine last said the child is doing
        self.held = []  # the requests made before the child exists; `None` once the launch answers
        self.open = {}  # each request not yet ended, by its number
        self.requests_max = session._requests_max
        self.error = None  # what each call raises, once one must
        self.closed = False  # the session's close released the child
        self.oriented = self.loop.create_future()  # `ready()`'s: `None`, or the error, as a value
        self.releasing = None
        self.finalizer = weakref.finalize(agent, _collected, self)
        _roots[self.id] = self
        self.launching = self.loop.create_task(self.launch(line))

    def __str__(self):
        return f"child {self.name!r}" + ("" if self.child is None else f" ({self.child})")

    async def launch(self, line):
        """Have the engine launch the child, then send it what was asked of it meanwhile, in order;
        or end that as the launch did."""
        error = None
        try:
            answer = await asyncio.wrap_future(_connection.post("agent.spawn", **line))
        except RuntimeError as e:  # refused: the session is not running, or the connection broke
            error = _agents.SessionClosing(str(e))
        except Exception as e:  # not sent: longer than the engine reads, say
            error = e
        else:
            refused = answer.get("error")
            if refused is not None:
                error = _ERRORS.get(refused["kind"], RuntimeError)(refused["message"])
            else:
                self.child, self.requests_max = answer["child"], answer["requests_max"]
                self.instance = answer["instance"]
        self.error = self.error or error
        held, self.held = self.held, None
        for request in held:
            if self.error is None:
                self.send(request)
            else:
                request.end(_REASONS.get(type(self.error), "failed"), copy.copy(self.error))
        if self.error is not None:
            self.settle(self.error)
        if self.child is None:
            # Nothing to release, so nothing to say of a collection.
            self.finalizer.detach()
            _roots.pop(self.id, None)

    def settle(self, error):
        """Settle `ready()`, once: ready, or raising `error`."""
        if not self.oriented.done():
            self.oriented.set_result(error)

    async def ready(self):
        # Kept as a value and raised as a copy, as a kernel's instance does: one raised would hold
        # the frames that awaited it, the instance's among them, for as long as this lives.
        error = await asyncio.shield(self.oriented)
        if error is not None:
            raise copy.copy(error)

    def request(self, agent, name, message):
        """Send `message` on channel `name` as `agent`'s request, which holds `agent` until it ends
        -- held itself until the child exists -- and return its handle."""
        if self.error is None and self.session.shutdown_report is not None:
            self.error = _agents.SessionClosing(f"the session of {self} has ended")
        if self.error is not None:
            raise copy.copy(self.error)
        if len(self.open) >= self.requests_max:
            raise _agents.AgentLimitReached(
                f"{self} already holds {self.requests_max} unsettled requests, its requests-max, "
                "so nothing was sent; cancel one, or wait for an answer"
            )
        request = _Request(self, agent, name, self.methods[name].request.encode(message))
        if self.held is None:
            self.send(request)
        else:
            self.held.append(request)
        return _agents.Handle(request)

    def send(self, request):
        """Send `request` to the child's kernel, unless it was cancelled first; or end it with why
        it could not go."""
        with request.sending:
            with request.lock:
                if request.state in _agents._ENDED:
                    return
                request.sent = True
            try:
                # As text, so the engine, which sorts a JSON object's keys, keeps a dict's order.
                _connection.tell("ask", session=self.session._handle, agent=self.child,
                                 ask=request.ask, channel=request.channel,
                                 body=json.dumps(request.body, allow_nan=False))
            except (RuntimeError, ValueError) as e:
                request.end("failed", e)

    def tell(self, kind, **fields):
        """Tell the engine `kind` about the child, if it can still hear it."""
        with contextlib.suppress(RuntimeError, ValueError):
            _connection.tell(kind, session=self.session._handle, **fields)

    def dropped(self, request):
        """The handle of `request` was collected while it was open: said now, or once it is
        sent."""
        request.dropped = True
        if request.id is not None:
            self.said_dropped(request)

    def said_dropped(self, request):
        if request.state not in _agents._ENDED:
            self.tell("agent.instance.handle.collected", instance=self.instance, child=self.child,
                      request=request.id)

    def pushed(self, message):
        """What the engine says of the instance: the child's orienting round has ended, or cannot
        be had; or the child is doing something else."""
        if message["t"] == "oriented":
            error = message["error"]
            self.settle(error and _ERRORS.get(error["kind"], RuntimeError)(error["message"]))
        else:
            self.life = message["state"]

    def close(self):
        """The session closed, which released the child: `ready()`, and each call from now on,
        raise the closing error."""
        self.closed = True
        self.error = self.error or _agents.SessionClosing(f"the session closed with {self} open")
        self.settle(self.error)

    def collected(self):
        if self.releasing is None:
            self.releasing = self.loop.create_task(self._release("collected"))

    async def release(self, how):
        if self.releasing is None:
            self.releasing = self.loop.create_task(self._release(how))
        await asyncio.shield(self.releasing)

    async def _release(self, how):
        """Release the child `how` -- `explicit`, `async_with` or `collected` -- once its launch has
        answered, and say so: unless it never came to exist, or the session's close released it."""
        if how != "collected":
            self.finalizer.detach()
        self.error = self.error or _agents.AgentReleased(f"{self} was released")
        self.settle(self.error)
        await asyncio.wait([self.launching])
        if self.child is not None and not self.closed:
            # The engine records the release, and a collection, as it makes it.
            with contextlib.suppress(RuntimeError):
                await asyncio.wrap_future(_connection.post(
                    "agent.release", session=self.session._handle, children=[self.child],
                    how=how))
        _roots.pop(self.id, None)


def _sendable(text):
    """`text`, cut, as a line can carry it: a lone surrogate, which UTF-8 cannot hold, escaped."""
    return _agents.cut(text.encode("utf-8", "backslashreplace").decode("utf-8"))


def _collected(root):
    """An agent class instance was collected, on whatever thread dropped it: released from its
    loop."""
    with contextlib.suppress(RuntimeError):
        root.loop.call_soon_threadsafe(root.collected)


def _closed(roots, terminated):
    """The session of each of `roots` closed, which released its child. `terminated`, the
    interpreter died first, so nothing else ends the requests still open: they end here, on
    whatever thread heard so -- a `cancel()` the loop is in may be waiting for them -- and the rest
    on each instance's loop."""
    for root in roots:
        if terminated:
            for request in list(root.open.values()):
                request.end("terminated", _agents.SessionClosing(
                    f"the session's interpreter ended before {request} was answered"))
        with contextlib.suppress(RuntimeError):
            root.loop.call_soon_threadsafe(root.close)


class _Request:
    """One request the program made of an agent class instance's child: what its handle reads,
    and what the engine says of it. `ask` names it to the engine; `id` is the session's for it,
    once the child's kernel has it. That kernel decides how it ends, under the lock that orders
    this program's verdicts, a cancel, the release and the close; only one never sent ends here."""

    def __init__(self, root, agent, channel, body):
        self.root, self.instance, self.channel, self.body = root, agent, channel, body
        self.ask, self.id, self.loop = next(_numbers), None, root.loop
        self.lock = threading.Lock()
        self.state, self.final = "waiting", None
        self.value = self.error = self.progress = None
        self.decoded = {}  # each reply this program found nothing wrong with, by its number
        # Whether it was sent, its handle was collected, and a cancel of it was sent; and the lock
        # that keeps a cancel's line from going before the request's.
        self.sent = self.dropped = self.withdrawn = False
        self.sending = threading.Lock()
        self.has_ended = threading.Event()
        self.future = root.loop.create_future()
        root.open[self.ask] = _asks[self.ask] = self

    def __str__(self):
        return f"request {self.id} on {self.root}'s channel {self.channel!r}"

    def status(self):
        with self.lock:
            return self.final or _agents.Status(self.state, None, self.root.life)

    def pushed(self, message):
        """What the engine says of the request, on the instance's loop: the child's kernel has it,
        the child took it, or the child replied."""
        kind = message["t"]
        if kind == "agent.request":
            self.id = message["request"]
            if self.dropped:
                self.root.said_dropped(self)
        elif kind == "agent.received":
            with self.lock:
                if self.state == "waiting":
                    self.state = "received"
        else:
            self.judge(json.loads(message["value"]), message["reply"])

    def told_ended(self, message):
        """How the child's kernel ended the request, on the connection's thread: nothing of the
        program's runs in this."""
        outcome = message["outcome"]
        text = message.get("error") or f"{self} ended {outcome}"
        if outcome == "accepted":
            with self.lock:
                value = self.decoded.get(message["reply"])
            self.end(None, value=value)
        elif outcome == "rejected":
            last = message["last"]
            problems = tuple(schema.Problem(*problem) for problem in last["problems"])
            value = json.loads(last["value"])
            self.end("rejected", _agents.CompletionRejected(text, value, problems))
        else:
            self.end(outcome, _ERRORS.get(outcome, RuntimeError)(text))

    def judge(self, value, reply):
        """Decode `value`, the child's reply numbered `reply`, into the type this program declared,
        and give the child's kernel the verdict: what is wrong with it, or nothing, which accepts
        it. Here, on the instance's loop, since decoding runs the program's dataclasses."""
        try:
            decoded, problems = self.root.methods[self.channel].reply.decode(value), ()
        except Exception as e:
            decoded, problems = None, getattr(e, "problems", None)
            problems = problems or (schema.Problem("$", f"{type(e).__name__}: {e}"),)
        with self.lock:
            if self.state in _agents._ENDED:
                return  # the child's kernel ended it, which its reply hears
            if not problems:
                self.decoded[reply] = decoded
        self.root.tell("verdict", agent=self.root.child, ask=self.ask, reply=reply,
                       problems=[[_sendable(p.path), _sendable(p.message)] for p in problems])

    def cancel(self):
        """Cancel the request; whether this ended it, as a kernel's handle says. One not yet sent
        ends here. One sent ends in the child's kernel, which may have ended it otherwise first:
        this waits for its word, `CANCEL_WAIT` seconds at most, and says no without it."""
        with self.sending:
            if self.end("cancelled", _agents.RequestCancelled(f"{self} was cancelled"),
                        unsent=True):
                return True
            with self.lock:
                if self.state in _agents._ENDED:
                    return False
                first, self.withdrawn = not self.withdrawn, True
            if first:
                self.root.tell("withdraw", agent=self.root.child, ask=self.ask)
        self.has_ended.wait(CANCEL_WAIT)
        with self.lock:
            # Only the program cancels its requests, and only its first cancel asked to.
            return first and self.final is not None and self.final.reason == "cancelled"

    def end(self, reason, error=None, value=None, unsent=False):
        """End the request: accepted with `value` when `reason` is `None`, failed with `error`
        otherwise; with `unsent`, only if it was never sent. Whether this ended it: one that had
        ended stays as it ended. Any thread."""
        with self.lock:
            if self.state in _agents._ENDED or unsent and self.sent:
                return False
            self.state = "accepted" if reason is None else "failed"
            self.value, self.error = value, error
            self.final = _agents.Status(self.state, reason, self.root.life)
        self.root.open.pop(self.ask, None)
        _asks.pop(self.ask, None)
        self.has_ended.set()
        with contextlib.suppress(RuntimeError):
            self.loop.call_soon_threadsafe(self._settle)
        return True

    def _settle(self):
        # On the loop: letting go of the instance may collect it, which runs the program's code.
        self.instance = None
        if self.error is None:
            self.future.set_result(self.value)
        else:
            self.future.set_exception(self.error)
            self.future.exception()


def _pushed(message):
    """What the engine says of an agent class instance or of one of its requests, on the
    connection's thread. A request's end is taken here, where a `cancel()` the loop is in waits for
    it; the rest is handed to the loop the instance was constructed on, since handling it may run
    the program's code."""
    if "ask" in message:
        request = _asks.get(message["ask"])
        if request is not None and message["t"] == "agent.ended":
            request.told_ended(message)
        elif request is not None:
            with contextlib.suppress(RuntimeError):
                request.loop.call_soon_threadsafe(request.pushed, message)
    elif "instance" in message:
        root = _roots.get(message["instance"])
        if root is not None:
            with contextlib.suppress(RuntimeError):
                root.loop.call_soon_threadsafe(root.pushed, message)
    else:  # the session closed
        handle = message["session"]
        _closed([root for root in list(_roots.values()) if root.session._handle == handle],
                message["terminated"])
