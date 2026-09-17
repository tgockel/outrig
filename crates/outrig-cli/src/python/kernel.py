"""The agent's persistent Python execution environment.

One of these runs per agent, started by `podman exec` inside the session container and spoken
to over NDJSON. It owns the agent's globals, its asyncio event loop, its foreground execution,
and its channel endpoints -- everything that has to outlive a model turn.

Three things here are load-bearing rather than incidental:

- **The protocol does not share a file descriptor with the program.** fd 1 is handed to the
  executed code; the real exec stdout is dup'd aside first and is the only thing the host
  parses. Generated code writing to stdout therefore cannot forge a protocol message.
- **Session globals live in a module registered in `sys.modules`.** `@dataclass` and
  `typing.get_type_hints` resolve a class's annotations through
  `sys.modules[cls.__module__].__dict__`; a bare dict silently breaks introspection on classes
  the model defines.
- **Output is bounded at the file descriptor, not at `print`.** Raw `os.write(1, ...)` and an
  inherited subprocess's stdout both reach the model otherwise.
"""

import ast
import asyncio
import builtins
import collections
import json
import os
import signal
import sys
import threading
import traceback
import types

# ---------------------------------------------------------------------------- limits

OUTPUT_MAX = 16 * 1024  # aggregate captured bytes per execution
BG_MAX = 2 * 1024  # bytes of between-execution background output, tail kept
REPR_MAX = 1000  # one echoed value, or one inventory entry
INVENTORY_MAX = 200  # global names listed
DRAIN_TIMEOUT = 5.0  # seconds to wait for the capture pipe to settle

# ---------------------------------------------------------------------------- file descriptors

# The exec's real stdout, saved before anything can be written to it. Every protocol message
# goes here and nowhere else.
_PROTO_FD = os.dup(1)

_CAPTURE_R, _capture_w = os.pipe()
os.dup2(_capture_w, 1)
os.dup2(_capture_w, 2)
os.close(_capture_w)
sys.stdout.reconfigure(line_buffering=True)
sys.stderr.reconfigure(line_buffering=True)

# Marks the end of one execution's output. Non-textual so ordinary printed output cannot
# contain it by accident.
_SENTINEL = b"\x00\x1eOUTRIG-END-OF-EXECUTION\x1e\x00"

_captured = []
_captured_len = 0
_overflowed = False
_drained = threading.Event()

# True only while a submitted execution is running. Output written outside that window came
# from a background task between turns: it is attributed separately so it cannot evict the
# result of the execution that happens to run next.
_foreground = False
_bg = collections.deque()
_bg_len = 0
_bg_total = 0


def _stash_background(chunk):
    """Keep the most recent BG_MAX bytes of between-execution output, and count the rest."""
    global _bg_len, _bg_total
    _bg_total += len(chunk)
    _bg.append(chunk)
    _bg_len += len(chunk)
    while _bg_len - len(_bg[0]) >= BG_MAX:
        _bg_len -= len(_bg.popleft())
    if _bg_len > BG_MAX:
        trim = _bg_len - BG_MAX
        _bg[0] = _bg[0][trim:]
        _bg_len -= trim


def _capture(chunk):
    """Append up to the budget; read and discard the rest rather than growing a buffer."""
    global _captured_len, _overflowed
    if not _foreground:
        _stash_background(chunk)
        return
    room = OUTPUT_MAX - _captured_len
    if room <= 0:
        _overflowed = bool(chunk) or _overflowed
        return
    if len(chunk) > room:
        _captured.append(chunk[:room])
        _captured_len = OUTPUT_MAX
        _overflowed = True
    else:
        _captured.append(chunk)
        _captured_len += len(chunk)


def _drain_thread():
    pending = b""
    keep = len(_SENTINEL) - 1
    while True:
        try:
            chunk = os.read(_CAPTURE_R, 65536)
        except OSError:
            return
        if not chunk:
            return
        pending += chunk
        while _SENTINEL in pending:
            head, pending = pending.split(_SENTINEL, 1)
            _capture(head)
            _drained.set()
        # Hold back enough that a sentinel split across two reads is still found.
        if len(pending) > keep:
            _capture(pending[:-keep])
            pending = pending[-keep:]


def _collect_output():
    """Flush, mark the end of this execution's output, and take what was captured."""
    global _captured_len, _overflowed
    try:
        sys.stdout.flush()
        sys.stderr.flush()
    except Exception:
        pass
    _drained.clear()
    os.write(1, _SENTINEL)
    _drained.wait(DRAIN_TIMEOUT)
    text = b"".join(_captured).decode("utf-8", "replace")
    if _overflowed:
        text += f"\n[output truncated at {OUTPUT_MAX} bytes]"
    _captured.clear()
    _captured_len = 0
    _overflowed = False
    return _take_background() + text


def _take_background():
    """Background output since the last execution, as a labelled preamble.

    It is reported, because a background task's traceback is often the whole story -- but it is
    reported *before* and *apart from* this execution's own output, and bounded separately, so
    a chatty task can never push out the result the model actually asked for.
    """
    global _bg_len, _bg_total
    if not _bg_total:
        return ""
    body = b"".join(_bg).decode("utf-8", "replace")
    dropped = _bg_total - _bg_len
    _bg.clear()
    _bg_len = 0
    _bg_total = 0
    head = "[background output between executions"
    head += f", earlier {dropped} bytes dropped]" if dropped else "]"
    return f"{head}\n{body}\n[end background output]\n"


# ---------------------------------------------------------------------------- protocol

_send_lock = threading.Lock()


def _send(obj):
    line = (json.dumps(obj) + "\n").encode("utf-8")
    with _send_lock:
        os.write(_PROTO_FD, line)


# ---------------------------------------------------------------------------- channels

_ARRIVED = asyncio.Event()


class MessageAvailable(Exception):
    """Raised by `runtime.wait` when a channel has input. The operation is not cancelled."""


class UserText:
    """A line of text to or from the user."""

    __slots__ = ("text",)

    def __init__(self, text):
        self.text = str(text)

    def __repr__(self):
        return f"UserText({self.text!r})"


class Endpoint:
    """One end of a channel. Receiving consumes; notification does not."""

    def __init__(self, name):
        self.name = name
        self._queue = asyncio.Queue()

    async def receive(self):
        """Take the next message, waiting if there is none."""
        message = await self._queue.get()
        _refresh_arrival()
        return message

    async def send(self, message):
        """Send a message. Accepts a `UserText` or a plain string."""
        text = message.text if isinstance(message, UserText) else str(message)
        _send({"t": "send", "ch": self.name, "body": {"type": "UserText", "text": text}})

    def pending(self):
        """How many messages are queued. Does not consume any of them."""
        return self._queue.qsize()

    def __repr__(self):
        return f"<Endpoint {self.name!r} pending={self.pending()}>"


class Runtime:
    """The agent's connection to everything outside Python."""

    def __init__(self):
        self.channels = {"user": Endpoint("user")}

    async def wait(self, operation, label=None):
        """Await an operation while checking incoming channels.

        Returns the operation's result when it completes, or raises its exception if it
        failed.

        If input is pending, raises `MessageAvailable` naming the channel. The message is not
        consumed and the operation is not cancelled -- it keeps running under whatever name it
        is bound to, and a later execution can await it again to collect its result.

        Pending input takes priority when both input and completion are ready. `label` is
        descriptive text for status and exception reporting; it neither creates nor names a
        variable.
        """
        operation = asyncio.ensure_future(operation)
        while True:
            ready = [name for name, ep in self.channels.items() if ep.pending()]
            if ready:
                where = ", ".join(repr(name) for name in ready)
                raise MessageAvailable(
                    f"input pending on {where} while waiting for {label!r}. "
                    f"The operation was not cancelled."
                )
            if operation.done():
                return operation.result()
            arrival = asyncio.ensure_future(_ARRIVED.wait())
            try:
                await asyncio.wait({operation, arrival}, return_when=asyncio.FIRST_COMPLETED)
            finally:
                arrival.cancel()

    def __repr__(self):
        return f"<Runtime channels={sorted(self.channels)}>"


_runtime = Runtime()


def _refresh_arrival():
    if any(ep.pending() for ep in _runtime.channels.values()):
        _ARRIVED.set()
    else:
        _ARRIVED.clear()


# ---------------------------------------------------------------------------- session globals

_SESSION = types.ModuleType("outrig_session")
_SESSION.__dict__["__builtins__"] = builtins
sys.modules["outrig_session"] = _SESSION
G = _SESSION.__dict__


def _echo(value):
    """Print the repr of a trailing bare expression, the way a REPL does."""
    if value is not None:
        text = repr(value)
        if len(text) > REPR_MAX:
            text = f"{text[:REPR_MAX]}... [{len(text)} chars]"
        print(text)
    return None


G["__outrig_echo__"] = _echo
G["runtime"] = _runtime
G["MessageAvailable"] = MessageAvailable
G["UserText"] = UserText
G["asyncio"] = asyncio

# Names bound at boot are infrastructure, not the model's work, so the inventory hides them.
_BOOT_NAMES = frozenset(G)


# ---------------------------------------------------------------------------- execution

_running = False


def _compile(source):
    """Compile submitted source, rewriting a trailing bare expression to echo its repr.

    The rewrite is an AST edit rather than a separate `eval` of the last statement, so an
    `await` in that trailing expression still works.
    """
    tree = ast.parse(source, "<execution>", "exec")
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        tree.body[-1] = ast.Expr(
            ast.Call(
                func=ast.Name(id="__outrig_echo__", ctx=ast.Load()),
                args=[tree.body[-1].value],
                keywords=[],
            )
        )
        ast.fix_missing_locations(tree)
    return compile(tree, "<execution>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)


async def _execute(exec_id, source):
    """Run one submitted execution to completion and report explicitly.

    Completion is tracked for the whole submission -- the runtime never infers it from printed
    output, and never picks out a particular `await`.
    """
    global _running, _foreground
    _running = True
    _foreground = True
    error = None
    try:
        code = _compile(source)
        result = eval(code, G, G)  # noqa: S307 -- arbitrary execution is the feature
        if asyncio.iscoroutine(result):
            await result
    except BaseException:
        error = "".join(traceback.format_exception(*sys.exc_info()))
        if len(error) > OUTPUT_MAX:
            error = error[:OUTPUT_MAX] + f"\n[traceback truncated at {OUTPUT_MAX} bytes]"
    finally:
        _running = False
        output = _collect_output()
        _foreground = False
    _send(
        {
            "t": "result",
            "id": exec_id,
            "status": "error" if error else "ok",
            "output": output,
            "error": error,
        }
    )


def _describe(value):
    name = type(value).__name__
    return name if len(name) <= REPR_MAX else name[:REPR_MAX]


def _inventory(request_id):
    """A bounded listing of what the session holds.

    Reports only names and type names. Nothing here calls `repr()`, a property, a descriptor,
    or an iterator -- all of those can execute code, block, or flood.
    """
    names = []
    for name in sorted(G):
        if name.startswith("__") or name in _BOOT_NAMES:
            continue
        names.append([name, _describe(G[name])])
        if len(names) >= INVENTORY_MAX:
            break
    _send(
        {
            "t": "inv",
            "id": request_id,
            "globals": names,
            "channels": sorted(_runtime.channels),
            "pending": {
                name: ep.pending() for name, ep in _runtime.channels.items() if ep.pending()
            },
        }
    )


# ---------------------------------------------------------------------------- interrupt

def _interrupt():  # noqa: D401
    """Raise KeyboardInterrupt in the thread running the execution.

    Synchronous code that never yields -- `while True: pass`, a runaway comprehension -- blocks
    the event loop, and with it every path that could report the problem or accept a fix. The
    reader thread keeps running regardless, so this is called *directly from it* rather than
    through `call_soon_threadsafe`, which is exactly the queue a wedged loop is not draining.

    Pure-Python loops take the exception at the next bytecode boundary. A thread parked in a
    long C call (`time.sleep`, a blocking read) does not, and nothing short of killing the
    interpreter will free that; the host learns as much from the absence of a result.
    """
    if not _running:
        # Nothing to interrupt; raising here would tear down the loop itself.
        return
    signal.raise_signal(signal.SIGINT)


# ---------------------------------------------------------------------------- dispatch

def _dispatch(message):
    kind = message.get("t")
    if kind == "exec":
        if _running:
            _send(
                {
                    "t": "result",
                    "id": message["id"],
                    "status": "error",
                    "output": "",
                    "error": "an execution is already running in this session",
                }
            )
            return
        asyncio.ensure_future(_execute(message["id"], message["src"]))
    elif kind == "inv":
        _inventory(message["id"])
    elif kind == "msg":
        endpoint = _runtime.channels.get(message["ch"])
        if endpoint is not None:
            body = message.get("body") or {}
            endpoint._queue.put_nowait(UserText(body.get("text", "")))
            _ARRIVED.set()


def _reader_thread(loop):
    """Read host messages off fd 0 and hand them to the loop.

    A thread rather than `loop.connect_read_pipe`: neither can deliver while synchronous
    foreground code is blocking the loop, and this one has no coupling to loop state.
    """
    stdin = os.fdopen(0, "rb")
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue
        # Handled on this thread on purpose: a wedged loop cannot run callbacks, and this is
        # the message whose whole job is to un-wedge it.
        if message.get("t") == "interrupt":
            _interrupt()
            continue
        loop.call_soon_threadsafe(_dispatch, message)
    loop.call_soon_threadsafe(loop.stop)


def _on_sigint(signum, frame):
    """Turn a SIGINT into an exception inside the running execution, and nothing otherwise.

    Installed from inside the loop because `asyncio.run` installs its own handler first; that
    one defers the interrupt until the loop next runs a callback, which is precisely what a
    wedged loop never does.
    """
    if _running:
        raise KeyboardInterrupt


async def _main():
    signal.signal(signal.SIGINT, _on_sigint)
    threading.Thread(target=_drain_thread, daemon=True).start()
    threading.Thread(
        target=_reader_thread, args=(asyncio.get_running_loop(),), daemon=True
    ).start()
    _send({"t": "ready", "version": sys.version.split()[0]})
    await asyncio.Event().wait()


if __name__ == "__main__":
    try:
        asyncio.run(_main())
    except (KeyboardInterrupt, RuntimeError):
        pass
