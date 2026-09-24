"""The interpreter: one process per session, hosting one kernel per agent.

Started by `podman exec -i` inside the session container, with the primary agent's id as its
one argument, and spoken to in NDJSON: requests on stdin, replies on stdout, every message in
both directions naming the agent it concerns. Each agent is a **kernel** -- a session module,
an event loop on a thread of its own, an execution slot, and a bounded backlog of background
output. The process owns what they share: the protocol descriptors, the reader thread that
routes messages by agent id, and the descriptors executed code writes to. The primary agent's
kernel runs on the main thread, because that is the only thread a signal handler runs on.

Four things here are load-bearing rather than incidental:

- **The protocol never shares a descriptor with the program.** Both protocol streams are moved
  aside at start. fd 0 then reads `/dev/null`, and fds 1 and 2 are the exec's stderr, which the
  host records as diagnostics. Nothing executed code writes can forge a protocol message, and
  nothing it reads can consume one.
- **Session globals live in a module registered in `sys.modules`**, one per kernel.
  `@dataclass` and `typing.get_type_hints` resolve a class's annotations through
  `sys.modules[cls.__module__].__dict__`; a bare dict silently breaks introspection on classes
  the model defines.
- **Output is attributed to the execution that caused it**, not merely to its agent. A context
  variable names the execution, and asyncio copies it into every task the execution starts, so
  a task an earlier execution left running is billed to that execution rather than to whichever
  holds the slot. Each execution has a pipe of its own for the same reason: a child process's
  bytes carry no writer identity, so the descriptor it inherited is the only attribution there
  is.
- **Output is bounded at the descriptor, not at `print`.** Each execution's pipe is drained on
  its own thread against `OUTPUT_MAX`, and a result never waits for a descendant that still
  holds the pipe open.
"""

import ast
import asyncio
import builtins
import collections
import contextlib
import contextvars
import functools
import inspect
import io
import json
import os
import select
import subprocess
import sys
import threading
import traceback
import types

# ---------------------------------------------------------------------------- limits

OUTPUT_MAX = 16 * 1024  # bytes of one execution's own output
BG_MAX = 2 * 1024  # bytes of other executions' output held for the next result, tail kept
REPR_MAX = 1000  # one echoed value, or one inventory entry
INVENTORY_MAX = 200  # global names listed
DRAIN_TIMEOUT = 5.0  # seconds a result waits for its pipe to reach the end of the body's output

# musl gives a thread 128 KiB of stack, far short of what CPython's recursion limits assume: deep
# but legal recursion -- `json.loads` of a nested document, `repr` of a nested list -- overflows
# it and kills the process, every agent with it, rather than raising `RecursionError`. Every
# thread from here on, a sub-agent's or one the agent starts, gets the main thread's 8 MiB.
threading.stack_size(8 << 20)

# ---------------------------------------------------------------------------- descriptors

# The exec's real stdin and stdout, moved aside before anything can touch them. Every protocol
# message is read from and written to these and nothing else. `os.dup` makes both
# non-inheritable, so no child process receives either.
_PROTO_IN = os.dup(0)
_PROTO_OUT = os.dup(1)

_devnull = os.open(os.devnull, os.O_RDONLY)
os.dup2(_devnull, 0)
os.close(_devnull)

# fd 1 joins fd 2 on the exec's stderr. What reaches either without passing through the
# dispatcher below -- `os.write(1, ...)`, `os.system`, a thread no execution started -- cannot be
# billed to any agent, and the host records it instead of dropping it.
os.dup2(2, 1)
sys.__stdout__.reconfigure(line_buffering=True)


def _write_all(fd, data):
    """Write every byte of `data`, even to a pipe a child process has made non-blocking.

    `O_NONBLOCK` belongs to the open pipe rather than to one descriptor, so a child that sets it
    on its stdout sets it on ours too, and a full pipe then raises rather than waiting.
    """
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


def _diag(text):
    """One line on the exec's stderr, where the host logs it. Never a protocol message."""
    if len(text) > REPR_MAX:
        text = f"{text[:REPR_MAX]}..."
    with contextlib.suppress(OSError):
        _write_all(2, f"outrig-interpreter: {text}\n".encode("utf-8", "backslashreplace"))


def _clean(value):
    """`value` with every string in it made safe to send.

    `json.dumps` passes a lone surrogate through as a `\\udXXX` escape, which a strict parser such
    as the host's rejects -- losing the whole message, not just the character. Tracebacks,
    names, and anything else the agent's code shaped can carry one.
    """
    if isinstance(value, str):
        return value.encode("utf-8", "backslashreplace").decode("utf-8")
    if isinstance(value, dict):
        return {key: _clean(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_clean(item) for item in value]
    return value


_send_lock = threading.Lock()


def _send(message):
    line = (json.dumps(_clean(message)) + "\n").encode("utf-8")
    with _send_lock:
        _write_all(_PROTO_OUT, line)


# ---------------------------------------------------------------------------- output

# The execution a write belongs to. Set at the top of an execution's task, so every task that
# execution creates carries it, and so does `asyncio.to_thread`, which copies the context. A
# thread started any other way starts with none.
_CURRENT = contextvars.ContextVar("outrig_execution", default=None)


def _output(data):
    execution = _CURRENT.get()
    if execution is None:
        _write_all(1, data)
    else:
        execution.write(data)


class _Routed:
    """What `sys.stdout` and its `.buffer` share: each write goes to whoever is writing."""

    def writable(self):
        return True

    def fileno(self):
        execution = _CURRENT.get()
        fd = None if execution is None else execution.fileno()
        return 1 if fd is None else fd

    def close(self):
        pass


class _BinaryOut(_Routed, io.RawIOBase):
    """`sys.stdout.buffer`: bytes, routed as the text stream above it routes text."""

    def write(self, data):
        data = bytes(data)
        _output(data)
        return len(data)


class _TextOut(_Routed, io.TextIOBase):
    """`sys.stdout` and `sys.stderr`, routed per write to the execution doing the writing.

    Unbuffered, because a buffer would be shared: a partial line one execution left in it would
    be flushed by the next writer and billed to that one instead. `reconfigure` is accepted and
    ignored for the same reason.
    """

    encoding = "utf-8"
    errors = "backslashreplace"

    def __init__(self, name):
        self.name = f"<{name}>"
        self.buffer = _BinaryOut()

    def write(self, text):
        if not isinstance(text, str):
            raise TypeError(f"write() argument must be str, not {type(text).__name__}")
        _output(text.encode("utf-8", "backslashreplace"))
        return len(text)

    def reconfigure(self, **_):
        pass


sys.stdout = _TextOut("stdout")
sys.stderr = _TextOut("stderr")
_STREAMS = (sys.stdout, sys.stderr, sys.stdout.buffer, sys.stderr.buffer)


class _Execution:
    """One submission, and every byte it causes to be written, wherever and whenever."""

    def __init__(self, kernel, exec_id):
        self.kernel = kernel
        self.id = exec_id
        self.task = None
        # The write end's lifetime, so nothing writes to or duplicates a number that has been
        # closed and reused. Held only around descriptor operations, never around agent code.
        self._fd_lock = threading.Lock()
        # What the drain has captured. The body's output ends when `_drained` is set: at the
        # sentinel, at end of file, or when `finish` stops waiting.
        self._buf_lock = threading.Lock()
        self._captured = bytearray()
        self._dropped = 0
        self._drained = threading.Event()
        # Random, so no output -- printed or binary, from Python or a child -- ends the body's
        # output early.
        self._sentinel = os.urandom(16)
        self._wfd = self._drained_pipe(self._sentinel)

    def _drained_pipe(self, sentinel):
        """A new pipe with a thread draining it into this execution; returns the write end."""
        read, write = os.pipe()
        try:
            threading.Thread(
                target=self._drain, args=(read, sentinel), name=f"drain-{self.id}", daemon=True
            ).start()
        except BaseException:
            os.close(read)
            os.close(write)
            raise
        return write

    def _drain(self, fd, sentinel):
        """Read one pipe until everything holding it has closed it.

        Bytes before `sentinel` are the body's own output. The sentinel is written when the body
        finishes, so everything after it came from something the execution left running. With no
        sentinel, every byte is of that second kind.

        This thread has to outlive anything that goes wrong in it: a pipe nobody drains blocks
        its writers, and one of them may be holding the lock its execution needs to report.
        """
        pending = b""
        while True:
            try:
                chunk = os.read(fd, 65536)
            except OSError as e:
                _diag(f"execution {self.id!r}: reading its output failed: {e!r}")
                break
            if not chunk:
                break
            try:
                if sentinel is None:
                    self._take(chunk)
                    continue
                pending += chunk
                head, found, tail = pending.partition(sentinel)
                if found:
                    self._take(head)
                    self._end_body()
                    sentinel, pending = None, b""
                    if tail:
                        self._take(tail)
                elif len(pending) >= len(sentinel):
                    # Hold back enough that a sentinel split across two reads is still found.
                    keep = len(sentinel) - 1
                    self._take(pending[:-keep])
                    pending = pending[-keep:]
            except Exception as e:
                _diag(f"execution {self.id!r}: attributing its output failed: {e!r}")
        try:
            if pending:
                self._take(pending)
        except Exception as e:
            _diag(f"execution {self.id!r}: attributing its output failed: {e!r}")
        self._end_body()
        os.close(fd)

    def _take(self, data):
        with self._buf_lock:
            if not self._drained.is_set():
                kept = data[: OUTPUT_MAX - len(self._captured)]
                self._captured += kept
                self._dropped += len(data) - len(kept)
                return
        self.kernel.background(self.id, data)

    def _end_body(self):
        with self._buf_lock:
            self._drained.set()

    def _spent(self, data):
        """Count `data` as dropped if the budget is already spent.

        The drain would read it only to throw it away, and a loop of prints past the budget
        would pay a pipe write and a thread wake-up for every one.
        """
        with self._buf_lock:
            if len(self._captured) < OUTPUT_MAX:
                return False
            self._dropped += len(data)
            return True

    def write(self, data):
        with self._fd_lock:
            if self._wfd is not None:
                if not self._spent(data):
                    _write_all(self._wfd, data)
                return
        self.kernel.background(self.id, data)

    def fileno(self):
        """The write end while the body runs; `None` once it has reported."""
        with self._fd_lock:
            return self._wfd

    def descriptor(self):
        """A write descriptor of its own, for a child process this execution starts.

        While the body runs, it is a copy of the execution's pipe. Once the body has reported, it
        is a new pipe drained straight to background: the child is still this execution's doing,
        even though it started after the result went out. The caller closes it.
        """
        with self._fd_lock:
            if self._wfd is not None:
                return os.dup(self._wfd)
        return self._drained_pipe(None)

    @contextlib.contextmanager
    def child_descriptor(self):
        """`descriptor()` for the length of one spawn, which runs with no lock held -- so nothing
        it calls out to, such as a warning through `sys.stderr`, can wait on one."""
        fd = self.descriptor()
        try:
            yield fd
        finally:
            os.close(fd)

    def adopt(self, fd):
        """In a forked child, make `fd` where everything this execution writes goes.

        The child is a copy, and nothing it holds for the execution is its own: the pipe may be
        closed, and a lock may have been held by a thread the fork did not copy. From here on it
        only writes, and the parent drains and bills what it writes.
        """
        self._fd_lock = threading.Lock()
        self._buf_lock = threading.Lock()
        self._captured = bytearray()
        self._wfd = fd

    def finish(self):
        """End the body's output and take it, as `(text, bytes dropped)`.

        Never waits for the pipe to close. A child the execution started may hold it open for as
        long as it likes, and what it writes from here on is background.
        """
        with self._fd_lock:
            try:
                _write_all(self._wfd, self._sentinel)
            except OSError as e:
                _diag(f"execution {self.id!r}: could not mark the end of its output: {e!r}")
                self._end_body()
            finally:
                os.close(self._wfd)
                self._wfd = None
        if not self._drained.wait(DRAIN_TIMEOUT):
            _diag(
                f"execution {self.id!r}: output did not settle within {DRAIN_TIMEOUT}s; "
                f"the rest is reported as background"
            )
            self._end_body()
        with self._buf_lock:
            text = self._captured.decode("utf-8", "replace")
            self._captured = bytearray()
            return text, self._dropped


# Every child process gets the current execution's pipe as the stdout and stderr it would
# otherwise have inherited, including one started through asyncio, which comes here too.
_popen_init = subprocess.Popen.__init__
_popen_signature = inspect.signature(_popen_init)


def _inherits(stream):
    return stream is None or any(stream is ours for ours in _STREAMS)


@functools.wraps(_popen_init)
def _attributed_popen_init(self, *args, **kwargs):
    execution = _CURRENT.get()
    if execution is None:
        return _popen_init(self, *args, **kwargs)
    try:
        bound = _popen_signature.bind(self, *args, **kwargs)
    except TypeError:
        return _popen_init(self, *args, **kwargs)
    streams = [name for name in ("stdout", "stderr") if _inherits(bound.arguments.get(name))]
    if not streams:
        return _popen_init(self, *args, **kwargs)
    with execution.child_descriptor() as fd:
        for name in streams:
            bound.arguments[name] = fd
        return _popen_init(*bound.args, **bound.kwargs)


subprocess.Popen.__init__ = _attributed_popen_init

# A fork -- `os.fork`, `multiprocessing` -- copies the forking thread's context, and with it the
# execution that context names. Its copy of that execution must write somewhere this process
# drains: otherwise, once the execution has reported, the child bills its output to a backlog of
# its own that nothing reads. Carried from before the fork to after it, per forking thread.
_fork = threading.local()


def _before_fork():
    # Cleared first, so a failure below cannot leave the previous fork's descriptor to be used.
    _fork.late = None
    execution = _CURRENT.get()
    if execution is not None:
        _fork.late = (execution, execution.descriptor())


def _after_fork_in_parent():
    late, _fork.late = _fork.late, None
    if late is not None:
        os.close(late[1])


def _after_fork_in_child():
    global _PROTO_IN, _PROTO_OUT
    # The child must neither write a protocol line nor hold the host's stdout open after the
    # interpreter has exited. The numbers are forgotten once closed: the child's own files will
    # reuse them, and its children must not close those in turn.
    for fd in (_PROTO_IN, _PROTO_OUT):
        if fd is not None:
            with contextlib.suppress(OSError):
                os.close(fd)
    _PROTO_IN = _PROTO_OUT = None
    late, _fork.late = _fork.late, None
    if late is not None:
        execution, fd = late
        execution.adopt(fd)


os.register_at_fork(
    before=_before_fork,
    after_in_parent=_after_fork_in_parent,
    after_in_child=_after_fork_in_child,
)


def _attributed_exception(loop, context):
    """Report an exception asyncio caught as output of the execution behind it.

    asyncio reports "Task exception was never retrieved" from wherever the task happens to be
    collected, and "Exception in callback" from its loop, with no execution in context either
    way; it would land in the unattributed bucket. The task or callback still carries the context
    it was created in, which names the execution -- and its traceback is often the whole story.
    """
    source = context.get("task") or context.get("future") or context.get("handle")
    get_context = getattr(source, "get_context", None)
    token = _CURRENT.set(None if get_context is None else get_context().get(_CURRENT))
    try:
        loop.default_exception_handler(context)
    finally:
        _CURRENT.reset(token)


# ---------------------------------------------------------------------------- kernels

_kernels = {}


def _module_name(agent):
    return f"outrig_session_{agent}"


def _valid_agent(agent):
    return isinstance(agent, str) and bool(agent) and _module_name(agent).isidentifier()


def _echo(value):
    """Print the repr of a trailing bare expression, the way a REPL does."""
    if value is not None:
        text = repr(value)
        if len(text) > REPR_MAX:
            text = f"{text[:REPR_MAX]}... [{len(text)} chars]"
        print(text)


def _compile(source):
    """Compile submitted source, rewriting a trailing bare expression to echo its repr.

    The rewrite is an AST edit rather than a separate `eval` of the last statement, so an
    `await` in that trailing expression still works.
    """
    tree = ast.parse(source, "<execution>", "exec")
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last = tree.body[-1]
        call = ast.Call(
            func=ast.Name(id="__outrig_echo__", ctx=ast.Load()), args=[last.value], keywords=[]
        )
        tree.body[-1] = ast.copy_location(ast.Expr(ast.copy_location(call, last.value)), last)
        ast.fix_missing_locations(tree)
    return compile(
        tree, "<execution>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT, dont_inherit=True
    )


def _format_error(exc):
    """The traceback from the submitted code's first frame on.

    The frames above it are this program's, and Python 3.13 quotes source for `-c` code, so
    keeping them would put the interpreter's own lines in front of every error. A compile error
    has no frame of the submission's at all, and is reported as the exception alone.
    """
    tb = exc.__traceback__
    while tb is not None and tb.tb_frame.f_code.co_filename != "<execution>":
        tb = tb.tb_next
    return "".join(traceback.format_exception(type(exc), exc, tb))


_MISSING = object()

# A type's own name, read without consulting its metaclass, which could run agent code.
_type_name = type.__dict__["__name__"].__get__


class Kernel:
    """One agent's execution environment: its namespace, its loop, its slot, its backlog."""

    def __init__(self, agent):
        self.agent = agent
        self.loop = asyncio.new_event_loop()
        self.loop.set_exception_handler(_attributed_exception)
        self.module = types.ModuleType(_module_name(agent))
        self.globals = self.module.__dict__
        self.globals["__builtins__"] = builtins
        self.globals["asyncio"] = asyncio
        sys.modules[self.module.__name__] = self.module
        # Names bound at boot are infrastructure, not the model's work. Hidden by identity, so a
        # name the model rebinds is listed again.
        self._boot = dict(self.globals)
        # The slot, twice over. `_holder` is the id admitted to it, decided on the reader thread;
        # `running` is the execution itself, including its task, once the loop has started it.
        self._slot_lock = threading.Lock()
        self._holder = None
        self.running = None
        self._bg_lock = threading.Lock()
        self._bg = collections.deque()
        self._bg_len = 0
        self._bg_dropped = collections.Counter()

    def announce(self):
        _send({"t": "ready", "agent": self.agent, "version": sys.version.split()[0]})

    def serve(self):
        """Run this kernel's loop on the calling thread for the life of the process."""
        asyncio.set_event_loop(self.loop)
        while not self.loop.is_closed():
            try:
                self.loop.run_forever()
            except BaseException as e:
                # A `SystemExit` or `KeyboardInterrupt` raised in a background task escapes the
                # loop rather than its task. It has ended that task; it must not end the agent.
                _diag(f"agent {self.agent!r}: {type(e).__name__} escaped its event loop")
        _diag(f"agent {self.agent!r}: its event loop was closed")

    def _reply(self, exec_id, status, **fields):
        result = {"output": "", "dropped": 0, "error": None, "background": [], **fields}
        _send({"t": "result", "agent": self.agent, "id": exec_id, "status": status, **result})

    def admit(self, exec_id, source):
        """Take the slot for `exec_id` and schedule it, or refuse it now.

        Decided on the reader thread rather than on the loop. A body running synchronous code
        holds the loop, and a check queued behind it would find the slot free once the body had
        ended -- running late a submission that should have been refused.
        """
        with self._slot_lock:
            holder = self._holder
            if holder is None:
                self._holder = exec_id
        if holder is not None:
            # Rejected rather than queued: the caller decides whether to wait or interrupt, and
            # nothing stacks up behind work it can no longer see. The backlog waits for a result.
            self._reply(exec_id, "refused", holder=holder)
            return
        try:
            self.loop.call_soon_threadsafe(self._start, exec_id, source)
        except BaseException:
            self._release()
            raise

    def _release(self):
        with self._slot_lock:
            self._holder = None

    def _start(self, exec_id, source):
        try:
            execution = _Execution(self, exec_id)
        except (OSError, RuntimeError) as e:
            self._release()
            self._reply(exec_id, "error", error=f"the execution could not start: {e!r}")
            return
        self.running = execution
        execution.task = self.loop.create_task(
            self._run(execution, source), name=f"execution-{exec_id}"
        )

    async def _run(self, execution, source):
        """Run one execution to completion and report explicitly.

        Completion is tracked for the whole submission -- never inferred from printed output, and
        never from one particular `await`.
        """
        _CURRENT.set(execution)
        error = None
        try:
            self.globals["__outrig_echo__"] = _echo
            result = eval(_compile(source), self.globals)  # noqa: S307 -- this is the feature
            if asyncio.iscoroutine(result):
                await result
        except BaseException as e:
            error = _format_error(e)
            if len(error) > OUTPUT_MAX:
                error = error[:OUTPUT_MAX] + f"\n[traceback truncated at {OUTPUT_MAX} bytes]"
        finally:
            try:
                output, dropped = execution.finish()
            except Exception as e:
                _diag(f"execution {execution.id!r}: collecting its output failed: {e!r}")
                output, dropped = "", 0
            self.running = None
            self._release()
        self._reply(
            execution.id,
            "ok" if error is None else "error",
            output=output,
            dropped=dropped,
            error=error,
            background=self.take_background(),
        )

    def background(self, exec_id, data):
        """Hold `data` for the next result, keeping the most recent `BG_MAX` bytes overall."""
        if not data:
            # Zero bytes count nothing against the bound, so they must not take a place either.
            return
        with self._bg_lock:
            self._bg.append((exec_id, data))
            self._bg_len += len(data)
            while self._bg_len > BG_MAX:
                owner, oldest = self._bg[0]
                cut = min(self._bg_len - BG_MAX, len(oldest))
                if cut == len(oldest):
                    self._bg.popleft()
                else:
                    self._bg[0] = (owner, oldest[cut:])
                self._bg_len -= cut
                self._bg_dropped[owner] += cut

    def take_background(self):
        """What other executions wrote since the last result, grouped by the one responsible.

        Reported apart from the result's own output and bounded separately, so a chatty
        background task can never push out what the model actually asked for.
        """
        with self._bg_lock:
            chunks, dropped = self._bg, self._bg_dropped
            self._bg, self._bg_len, self._bg_dropped = collections.deque(), 0, collections.Counter()
        grouped = {}
        for owner, data in chunks:
            grouped.setdefault(owner, []).append(data)
        for owner in dropped:
            grouped.setdefault(owner, [])
        return [
            {
                "id": owner,
                "output": b"".join(parts).decode("utf-8", "replace"),
                "dropped": dropped[owner],
            }
            for owner, parts in grouped.items()
        ]

    def inventory(self, request_id):
        """A bounded listing of what the session holds, always answered.

        Reports only names and type names. Nothing here calls `repr()`, a property, a descriptor,
        or an iterator -- all of those can execute code, block, or flood.
        """
        rows = []
        try:
            held = [
                (name, value)
                for name, value in list(self.globals.items())
                if type(name) is str
                and not name.startswith("__")
                and self._boot.get(name, _MISSING) is not value
            ]
            held.sort(key=lambda row: row[0])
            rows = [
                [name[:REPR_MAX], _type_name(type(value))[:REPR_MAX]]
                for name, value in held[:INVENTORY_MAX]
            ]
        except Exception as e:
            _diag(f"agent {self.agent!r}: the inventory failed: {e!r}")
        _send({"t": "inv", "agent": self.agent, "id": request_id, "globals": rows})


# ---------------------------------------------------------------------------- routing

def _open(agent):
    if not _valid_agent(agent):
        raise ValueError(f"{agent!r} cannot name an agent")
    if agent in _kernels:
        raise ValueError(f"agent {agent!r} is already open")
    kernel = Kernel(agent)
    try:
        threading.Thread(target=kernel.serve, name=f"kernel-{agent}", daemon=True).start()
    except BaseException:
        sys.modules.pop(kernel.module.__name__, None)
        kernel.loop.close()
        raise
    _kernels[agent] = kernel
    kernel.announce()


def _route(line):
    """Hand one protocol line to the kernel it names, or raise saying why it cannot be."""
    message = json.loads(line)
    if not isinstance(message, dict):
        raise ValueError(f"not an object: {type(message).__name__}")
    kind, agent = message.get("t"), message.get("agent")
    if kind == "open":
        _open(agent)
        return
    if kind not in ("exec", "inv"):
        raise ValueError(f"unknown message type {kind!r}")
    kernel = _kernels.get(agent) if isinstance(agent, str) else None
    if kernel is None:
        raise ValueError(f"no agent {agent!r}")
    request_id = message.get("id")
    if type(request_id) is not int:
        raise ValueError(f"{kind} for {agent!r} has no integer id")
    if kind == "inv":
        kernel.loop.call_soon_threadsafe(kernel.inventory, request_id)
        return
    source = message.get("src")
    if not isinstance(source, str):
        raise ValueError(f"exec {request_id} for {agent!r} has no source")
    kernel.admit(request_id, source)


def _read():
    """Read the host's messages off the protocol descriptor and route them by agent id.

    A thread rather than any kernel's loop: a loop running synchronous code cannot read, and
    this one keeps reading whatever the kernels are doing.
    """
    try:
        with os.fdopen(_PROTO_IN, "rb") as stream:
            for line in stream:
                if not line.strip():
                    continue
                try:
                    _route(line)
                except Exception as e:
                    _diag(f"ignored a message: {e}")
    finally:
        # The host is gone. Exiting here rather than on a kernel's loop means a loop that has
        # stopped turning cannot keep the process alive after its session has ended.
        os._exit(0)


def main():
    if len(sys.argv) != 2 or not _valid_agent(sys.argv[1]):
        _diag(f"usage: python3 -I -c <program> <agent-id>; got {sys.argv[1:]!r}")
        os._exit(2)
    primary = Kernel(sys.argv[1])
    _kernels[primary.agent] = primary
    primary.announce()
    threading.Thread(target=_read, name="reader", daemon=True).start()
    primary.serve()


if __name__ == "__main__":
    main()
