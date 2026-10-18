"""The host's half of `outrig`: a program's connection to the engine that runs it, and
`outrig.config()`. The bootstrap `outrig run <script.py>` starts installs it before the program
runs; nothing else imports it, and no kernel ever does.

The connection is two pipes the engine passed down, one each way, each line one JSON object. They
are the program's alone: close-on-exec from here on, so no program this one starts inherits them,
and closed in a forked child. A thread of its own reads the engine's replies, so the program's
main thread, and the event loop it may run there, never serves the connection.

The engine's death ends the program and everything it started, as the bindings process ends with
its owner (`supervisor.rs`). Two things say the engine is gone: the parent-death signal, SIGHUP,
which the engine left blocked until the handler below is in place, and the end of the connection,
which the reading thread sees.
"""

import contextlib
import dataclasses
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


class _Reply:
    """What one request waits on: the engine's reply, or why none will come."""

    def __init__(self):
        self._done = threading.Event()
        self._message = None
        self._error = None

    def settle(self, message=None, error=None):
        self._message, self._error = message, error
        self._done.set()

    def wait(self):
        self._done.wait()
        if self._error is not None:
            raise RuntimeError(self._error)
        return self._message


class _Connection:
    """Requests to the engine, each answered by one reply with its id, read on a thread of its
    own."""

    def __init__(self, fd_in, fd_out):
        self.fds = (fd_in, fd_out)
        self._send = threading.Lock()
        self._ids = itertools.count(1)
        # id -> the `_Reply` its caller waits on. Callers add entries and the reading thread pops
        # them with no lock: each is one dict operation, atomic under the GIL.
        self._waiting = {}
        # Why requests can no longer be answered, once they cannot.
        self._broken = None
        threading.Thread(target=self._read, name="outrig-engine", daemon=True).start()

    def request(self, kind, **fields):
        """Send a `kind` request and return the engine's reply, which names the same kind."""
        if self._broken is not None:
            raise RuntimeError(self._broken)
        ident = next(self._ids)
        reply = _Reply()
        self._waiting[ident] = reply
        line = (json.dumps({"t": kind, "id": ident, **fields}) + "\n").encode()
        try:
            with self._send:
                _write_all(self.fds[1], line)
        except OSError as e:
            self._waiting.pop(ident, None)
            raise RuntimeError(f"the connection to the engine is broken: {e}") from None
        message = reply.wait()
        if message.get("t") == "error":
            raise RuntimeError(message.get("error", "the engine refused the request"))
        return message

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
            reply = self._waiting.pop(message["id"])
        except (ValueError, KeyError, TypeError) as e:
            _diag(f"ignored a line from the engine ({e!r}): {line[:200]!r}")
            return
        reply.settle(message)

    def _break(self, why):
        self._broken = why
        while self._waiting:
            _, reply = self._waiting.popitem()
            reply.settle(error=why)


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
    and bind `outrig.config`. Main thread, before the program runs."""
    global _connection
    for fd in (fd_in, fd_out):
        os.set_inheritable(fd, False)
    signal.signal(DEATH_SIGNAL, lambda *_: _die("the engine that runs this program is gone"))
    # Started while the signal is still blocked, so the reading thread inherits it blocked and the
    # signal goes to the main thread, where the handler runs.
    _connection = _Connection(fd_in, fd_out)
    signal.pthread_sigmask(signal.SIG_UNBLOCK, {DEATH_SIGNAL})
    os.register_at_fork(after_in_child=_forget)
    outrig.config = config


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
