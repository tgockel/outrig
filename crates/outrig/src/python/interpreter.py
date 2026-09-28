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
- **An interrupt lands only in agent code.** The host's `interrupt` is handled on the reader
  thread, because a wedged loop drains no queue, and becomes a SIGINT aimed at the main thread.
  The handler raises only where the stack shows code a submission wrote, never in the loop's own
  bookkeeping or halfway through a protocol line. A `cancel` goes the other way, through the
  loop, and is the remedy for an execution suspended on an await rather than for a wedge.
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
import signal
import subprocess
import sys
import threading
import time
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
        # Whether the task has taken its first step. A cancel that arrives before then is held
        # here instead of delivered: a coroutine cancelled before it starts never enters the
        # wrapper's `try`, and would never report.
        self.started = False
        self.cancel_requested = False
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
    has no frame of the submission's at all, and is reported as the exception alone. Nor is the
    SIGINT handler's frame kept, where an interrupt ends the list: the interrupt is the result,
    and where the handler raised it is not.

    Formatting reads the exception itself, which is agent code. `traceback` guards `__str__`
    against anything, but its read of `__notes__` only against `Exception`, and a metaclass can
    answer for the type's own names. Whatever escapes those -- a `SystemExit`, or an interrupt
    aimed at a property that loops -- is caught here, so a failed execution always has something
    to report.
    """
    try:
        tb = exc.__traceback__
        while tb is not None and tb.tb_frame.f_code.co_filename != "<execution>":
            tb = tb.tb_next
        report = traceback.TracebackException(type(exc), exc, tb)
        handler = _on_sigint.__code__
        last = report.stack[-1] if report.stack else None
        if last and (last.filename, last.name) == (handler.co_filename, handler.co_name):
            report.stack.pop()
        return "".join(report.format())
    except BaseException as e:
        return (
            f"{_type_name(type(exc))}: its traceback could not be formatted "
            f"({_type_name(type(e))} while formatting it)\n"
        )


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
        # The thread the loop runs on, for its CPU clock and, for the primary, for aiming a
        # signal. Set before the thread starts.
        self.thread = None
        # `(exec_id, runaway)` while an interrupt is on its way to the handler, which consumes
        # it: one signal, one attempt. Then what the handler raised, the task it raised in, and
        # the execution that task belongs to, until whatever catches it has billed it.
        self._interrupting = None
        self._landed = None
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
                if not self._interrupted_task(e):
                    _diag(f"agent {self.agent!r}: {type(e).__name__} escaped its event loop")
        _diag(f"agent {self.agent!r}: its event loop was closed")

    def _interrupted_task(self, exc):
        """Report `exc` if it is an interrupt that ended a task rather than an execution's body.

        The task was holding the loop -- which is why the interrupt went where it did -- and it
        belongs to the execution that started it, so that is who is told, through the same route
        the task's own output takes. Retrieving the exception keeps asyncio from reporting the
        same death a second time when the task is collected; letting go of what the handler kept
        is what allows it to be collected at all.
        """
        landed, self._landed = self._landed, None
        if landed is None or landed[0] is not exc:
            return False
        _, task, owner = landed
        if task is not None:
            with contextlib.suppress(BaseException):
                task.exception()
        text = f"[outrig interrupted a task that was holding the event loop]\n{_format_error(exc)}"
        if owner is None:
            _diag(f"agent {self.agent!r}: {text}")
            return True
        try:
            owner.write(text.encode("utf-8", "backslashreplace"))
        except OSError as e:
            _diag(f"agent {self.agent!r}: {text} (not billed: {e})")
        return True

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
        # A `Task` rather than `create_task`, which would consult a task factory the agent can
        # set -- an eager one would run the body inside this callback.
        execution.task = asyncio.Task(
            self._run(execution, source), loop=self.loop, name=f"execution-{exec_id}"
        )
        self.running = execution

    async def _run(self, execution, source):
        """Run one execution to completion and report explicitly.

        Completion is tracked for the whole submission -- never inferred from printed output, and
        never from one particular `await`.
        """
        execution.started = True
        _CURRENT.set(execution)
        error = None
        try:
            if execution.cancel_requested:
                raise asyncio.CancelledError("stopped before it started")
            self.globals["__outrig_echo__"] = _echo
            result = eval(_compile(source), self.globals)  # noqa: S307 -- this is the feature
            if asyncio.iscoroutine(result):
                await result
        except asyncio.CancelledError as e:
            # Named, because it is a `BaseException` that an ordinary handler misses, and it is a
            # result: the host's remedy for a suspended await, or a cancellation the code let
            # through. The task carries on to report it.
            error = self._failure(e)
        except BaseException as e:
            # Everything else the body raised, `KeyboardInterrupt` included. The handler raises
            # one only inside agent code, so one arriving here is this execution's -- its body
            # was interrupted, or it awaited a task that was -- and never one meant to end the
            # interpreter. Letting any exception out would end the task with no reply, leaving
            # the host holding a slot this side has freed.
            error = self._failure(e)
        finally:
            # Spent either way. What the handler kept would otherwise hold an interrupted
            # frame -- and whatever a runaway built in it -- until the next interrupt.
            self._interrupting = self._landed = None
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

    def _failure(self, exc):
        """A failed execution's report. The interrupt that may have caused it is spent."""
        self._interrupting = self._landed = None
        error = _format_error(exc)
        if len(error) > OUTPUT_MAX:
            error = error[:OUTPUT_MAX] + f"\n[traceback truncated at {OUTPUT_MAX} bytes]"
        return error

    def cancel(self, exec_id):
        """Cancel `exec_id`'s task, if it still holds the slot. Runs on the reader thread.

        Delivered through the loop, since a task is cancelled at its next suspension point -- so
        this reaches an execution suspended on an await and does nothing for one that has stopped
        yielding, which is what `interrupt` is for. A request for an execution that has already
        finished is the ordinary race of a Ctrl-C with a result, and is dropped without comment.
        """
        if self._holds(exec_id):
            self.loop.call_soon_threadsafe(self._cancel, exec_id)

    def _holds(self, exec_id):
        with self._slot_lock:
            return self._holder == exec_id

    def _cancel(self, exec_id):
        execution = self.running
        if execution is None or execution.id != exec_id:
            return
        if not execution.started:
            execution.cancel_requested = True
            return
        execution.task.cancel()

    def interrupt(self, exec_id, runaway):
        """Aim a SIGINT at the loop's thread for `exec_id`. Runs on the reader thread.

        Handled here rather than through the loop, whose queue is exactly what a wedged loop is
        not draining. The signal is sent to the main thread itself: `signal.raise_signal` would
        signal this one, and a main thread blocked in `waitpid` or `sleep` would never wake to
        run the handler. `runaway` widens where it may land -- see `_on_sigint`.
        """
        if self is not _primary:
            # Python runs signal handlers on the main thread alone, and only the primary is
            # there. A wedged sub-agent is contained, not recoverable.
            _diag(f"agent {self.agent!r} cannot be interrupted: it is not on the main thread")
            return
        if not self._holds(exec_id):
            return
        if signal.getsignal(signal.SIGINT) is not _on_sigint:
            _diag(
                f"execution {exec_id!r} cannot be interrupted: SIGINT's handler has been "
                f"replaced, so only a cancel can reach it"
            )
            return
        self._interrupting, self._landed = (exec_id, runaway), None
        signal.pthread_kill(self.thread.ident, signal.SIGINT)

    def cpu(self, request_id):
        """Report the CPU time the loop's thread has used, in seconds. Runs on the reader thread.

        Answered here rather than on the loop, so it answers while the loop is not turning: what
        tells a loop spinning in Python from one blocked in a system call, such as the wait in a
        healthy `subprocess.run`. `None` when the clock cannot be read.
        """
        try:
            seconds = time.clock_gettime(time.pthread_getcpuclockid(self.thread.ident))
        except Exception as e:
            _diag(f"agent {self.agent!r}: its CPU clock could not be read: {e!r}")
            seconds = None
        _send({"t": "cpu", "agent": self.agent, "id": request_id, "seconds": seconds})

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


# ---------------------------------------------------------------------------- interrupts

# The kernel on the main thread, the only thread Python runs a signal handler on.
_primary = None


def _on_sigint(signum, frame):
    """Turn an armed interrupt into a `KeyboardInterrupt` in the agent code holding the loop.

    Runs on the main thread, between two bytecodes or as a blocking call there returns early. It
    takes no lock and writes nothing, because the main thread may be anywhere, a lock or a write
    of its own included. It raises only when all three of these hold, and otherwise returns and
    leaves the main thread exactly where it was:

    - An interrupt is armed and the execution it names still holds the slot. A SIGINT nobody
      armed -- agent code running `pkill -INT python3` -- changes nothing.
    - Walking out from where the signal landed, code a submission wrote comes before any of the
      machinery below -- or the walk reaches the loop's dispatch from inside a task an agent
      started, which is agent code however it was compiled: a coroutine an imported module
      defines, scheduled with `create_task` or `gather`. Otherwise the main thread is choosing
      the next callback, writing a protocol line, or reporting a result, where an exception would
      lose a callback or tear a line -- or it is idle, and there is nothing to interrupt.
    - That code is the named execution's, including a task it started; or the execution has not
      started, so whatever holds the loop is what keeps it from starting; or the host found the
      loop spinning, which justifies ending whichever agent code is doing it. Without one of
      these, a user's interrupt for a suspended execution could end a healthy task belonging to
      another.
    """
    kernel = _primary
    armed, kernel._interrupting = kernel._interrupting, None
    if armed is None:
        return
    exec_id, runaway = armed
    if kernel._holder != exec_id:
        return
    running = kernel.running
    if (
        not runaway
        and running is not None
        and running.id == exec_id
        and running.started
        and _CURRENT.get() is not running
    ):
        return
    while frame is not None:
        code = frame.f_code
        if code.co_filename == "<execution>":
            break
        if code in _MACHINERY:
            if code is not _HANDLE_RUN or not _in_agent_task(kernel):
                return
            break
        frame = frame.f_back
    else:
        return
    exc = KeyboardInterrupt("interrupted by outrig")
    kernel._landed = (exc, asyncio.current_task(kernel.loop), _CURRENT.get())
    raise exc


def _in_agent_task(kernel):
    """Whether the loop is stepping a task some agent code started.

    The only tasks this program starts are the executions' own wrappers, so any other task is
    agent code's doing -- including one whose coroutine a module defines rather than a submission.
    """
    task = asyncio.current_task(kernel.loop)
    return task is not None and getattr(task.get_coro(), "cr_code", None) is not _WRAPPER


# Machinery that can sit nearer the signal than agent code's frames; see `_on_sigint`. Every
# callback the loop runs, this program's own included, runs beneath `Handle._run`, so the loop's
# two dispatch frames cover them all -- including when agent code pumps the loop by hand. What else
# agent code triggers synchronously and must not be interrupted is the fork hooks. This program's
# output routing, by contrast, is deliberately absent: a runaway that prints spends most of its
# time there, and must still be interruptible.
_MACHINERY = frozenset(
    fn.__code__
    for fn in (
        asyncio.base_events.BaseEventLoop._run_once,
        asyncio.events.Handle._run,
        _before_fork,
        _after_fork_in_parent,
        _after_fork_in_child,
    )
)
_HANDLE_RUN = asyncio.events.Handle._run.__code__
_WRAPPER = Kernel._run.__code__


# ---------------------------------------------------------------------------- routing

def _open(agent):
    if not _valid_agent(agent):
        raise ValueError(f"{agent!r} cannot name an agent")
    if agent in _kernels:
        raise ValueError(f"agent {agent!r} is already open")
    kernel = Kernel(agent)
    kernel.thread = threading.Thread(target=kernel.serve, name=f"kernel-{agent}", daemon=True)
    try:
        kernel.thread.start()
    except BaseException:
        sys.modules.pop(kernel.module.__name__, None)
        kernel.loop.close()
        raise
    _kernels[agent] = kernel
    kernel.announce()


def _exec(kernel, request_id, message):
    source = message.get("src")
    if not isinstance(source, str):
        raise ValueError(f"exec {request_id} for {kernel.agent!r} has no source")
    kernel.admit(request_id, source)


# What each message addressed to an agent does, with the id it carries. `inv` is answered on the
# agent's loop; the rest are handled here, on the reader thread.
_ROUTES = {
    "exec": _exec,
    "inv": lambda kernel, request_id, _: kernel.loop.call_soon_threadsafe(
        kernel.inventory, request_id
    ),
    "cpu": lambda kernel, request_id, _: kernel.cpu(request_id),
    "cancel": lambda kernel, request_id, _: kernel.cancel(request_id),
    "interrupt": lambda kernel, request_id, message: kernel.interrupt(
        request_id, message.get("runaway") is True
    ),
}


def _route(line):
    """Hand one protocol line to the kernel it names, or raise saying why it cannot be."""
    message = json.loads(line)
    if not isinstance(message, dict):
        raise ValueError(f"not an object: {type(message).__name__}")
    kind, agent = message.get("t"), message.get("agent")
    if kind == "open":
        _open(agent)
        return
    handle = _ROUTES.get(kind) if isinstance(kind, str) else None
    if handle is None:
        raise ValueError(f"unknown message type {kind!r}")
    kernel = _kernels.get(agent) if isinstance(agent, str) else None
    if kernel is None:
        raise ValueError(f"no agent {agent!r}")
    request_id = message.get("id")
    if type(request_id) is not int:
        raise ValueError(f"{kind} for {agent!r} has no integer id")
    handle(kernel, request_id, message)


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
    global _primary
    primary = Kernel(sys.argv[1])
    primary.thread = threading.current_thread()
    _kernels[primary.agent] = primary
    _primary = primary
    # Before the reader starts, so no interrupt can arrive with nothing to handle it.
    signal.signal(signal.SIGINT, _on_sigint)
    primary.announce()
    threading.Thread(target=_read, name="reader", daemon=True).start()
    primary.serve()


if __name__ == "__main__":
    main()
