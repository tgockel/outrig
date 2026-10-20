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
"""

import asyncio
import collections
import concurrent.futures
import contextlib
import contextvars
import dataclasses
import datetime
import itertools
import json
import os
import pathlib
import select
import signal
import threading
import time
import types
from collections.abc import Mapping

import outrig

# The signal the engine's death delivers: `PR_SET_PDEATHSIG`, set between fork and exec and
# blocked until `install` has put the handler in place, so a death in between is pending rather
# than lost.
DEATH_SIGNAL = signal.SIGHUP
# How long the programs this one started have to leave on SIGTERM before SIGKILL, once the engine
# is gone and nobody else will send it.
DEATH_GRACE = 1.0
# The version of the protocol over the connection, which the greeting names; the engine refuses
# any other.
PROTOCOL = 1
# How many pushes of a session's events this program holds for a reader that falls behind before
# it drops the oldest. The engine holds as many for this program, and drops past it too, counted.
EVENT_CAPACITY = 4096
# The longest request the engine reads, in bytes, its newline aside: it discards a longer line
# unanswered, having no id to answer it by.
LINE_MAX = 64 << 10
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
    """Requests to the engine, each answered by one reply with its id, and the events of the
    program's sessions, which come unasked; all read on a thread of its own."""

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

    def post(self, kind, **fields):
        """Send a `kind` request, and return the `concurrent.futures.Future` the reading thread
        settles with the engine's reply, which names the same kind, or with `RuntimeError` when
        the engine refused the request or the connection broke. A coroutine awaits it through
        `asyncio.wrap_future`."""
        if self._broken is not None:
            raise RuntimeError(self._broken)
        ident = next(self._ids)
        # Encoded before anything waits on it, so a request the engine could not read is refused
        # here rather than sent to go unanswered: a str UTF-8 cannot hold -- a path that is not
        # UTF-8, which Python holds as lone surrogates -- raises `UnicodeEncodeError`, and a
        # request longer than the engine reads, `ValueError`.
        line = json.dumps({"t": kind, "id": ident, **fields}, ensure_ascii=False).encode()
        if len(line) > LINE_MAX:
            raise ValueError(
                f"a {kind} request of {len(line)} bytes is longer than the {LINE_MAX} the engine "
                "reads"
            )
        reply = concurrent.futures.Future()
        self._waiting[ident] = reply
        try:
            with self._send:
                _write_all(self.fds[1], line + b"\n")
        except OSError as e:
            self._waiting.pop(ident, None)
            raise RuntimeError(f"the connection to the engine is broken: {e}") from None
        return reply

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
                # A push, for one session; `events.end` is its last.
                handle = message["session"]
                if message["t"] == "events.end":
                    self.events.pop(handle).push(message)
                else:
                    self.events[handle].push(message)
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
        # Nor will any more of a session's events come.
        for events in list(self.events.values()):
            events.push({"t": "events.end"})


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
    """A `[providers.<name>]` table: its `style`, `"openai"` or `"anthropic"`, and its `base-url`.
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
    """The configuration `outrig run` loaded before this program started: see
    `help(outrig.config)`."""

    models: Mapping[str, Model]
    aliases: Mapping[str, Alias]
    providers: Mapping[str, Provider]
    default_model: str | None
    workspace: pathlib.Path | None
    events: Events


def config():
    """The configuration `outrig run` loaded before this program started, as a frozen,
    read-only view.

    `outrig run` reads it once, from the directory it was invoked in -- with `--config` and the
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
            "outrig.config() answers in the program `outrig run` started, not in a process it "
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
    `hosted`, each hosted call whose outcome came after admission closed; `bindings`, a
    `BindingStop`, how the process holding the bindings stopped, or `None` for a session with
    none; and `events`, an `EventDelivery`, how the session's events were delivered. It never
    depends on `Session.events()` having been read.
    """

    verdict: str
    closed_by: ClosedBy
    stopped: Stopped
    executions: tuple[ExecutionOutcome, ...]
    hosted: tuple[HostedCall, ...]
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
        self.id = None
        self.shutdown_report = None

    async def __aenter__(self):
        if self._entered:
            raise RuntimeError("an outrig.Session is entered once; make another to enter again")
        self._entered = True
        try:
            if _connection is None:
                raise RuntimeError(
                    "a session starts in the program `outrig run` started, not a fork"
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
        self._token = _current.set(self)
        return self

    async def __aexit__(self, kind, error, traceback):
        # Left in another context than the one entered, the block leaves that context its own.
        with contextlib.suppress(ValueError):
            _current.reset(self._token)
        stop = _connection.post("session.shutdown", session=self._handle, grace=self._grace)
        cancelled = await _settled(stop)
        self.shutdown_report = _report(stop.result()["report"], self._events.lost)
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
