"""The interpreter: one process per session, hosting one kernel per agent.

Started by `podman exec -i` inside the session container, with the primary agent's id as its
one argument, and spoken to in NDJSON: requests on stdin, replies on stdout, every message in
both directions naming the agent it concerns. Each agent is a **kernel** -- a session module,
an event loop on a thread of its own, an execution slot, a bounded backlog of background output,
and the endpoints of its channels, bound as `runtime.channels`. The process owns what they
share: the protocol descriptors, the reader thread that routes messages by agent id, and the
descriptors executed code writes to. The primary agent's kernel runs on the main thread, because
that is the only thread a signal handler runs on.

Five things here are load-bearing rather than incidental:

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
- **Running out of memory is a result, not the end of the process.** A ceiling set at start
  turns an allocation past it into a `MemoryError` in the thread that made it, and a reserve held
  while agent code runs leaves room to report it with. It is not isolation, for the reasons the
  memory section below gives.
"""

import ast
import asyncio
import builtins
import collections
import contextlib
import contextvars
import dataclasses
import datetime
import functools
import importlib
import inspect
import io
import json
import math
import mmap
import os
import resource
import select
import shlex
import shutil
import signal
import site
import subprocess
import sys
import threading
import time
import traceback
import types
import typing

# ---------------------------------------------------------------------------- limits

OUTPUT_MAX = 16 * 1024  # bytes of one execution's own output
BG_MAX = 2 * 1024  # bytes of other executions' output held for the next result, tail kept
REPR_MAX = 1000  # one echoed value, or one inventory entry
INVENTORY_MAX = 200  # global names listed in one inventory
HELP_MAX = 8 * 1024  # characters of one `help()` answer
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


def _encode(message):
    return (json.dumps(_clean(message)) + "\n").encode("utf-8")


def _write_line(line):
    """Write one encoded protocol line, whole.

    Agent code reaches this through `Endpoint.send`, so it is in `_MACHINERY`: an interrupt that
    lands here is dropped rather than raised, since a line cut short would glue the next one --
    perhaps an answer the host is waiting for -- onto its tail.
    """
    with _send_lock:
        _write_all(_PROTO_OUT, line)


def _send(message):
    _write_line(_with_room(_encode, message))


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
        # Made here rather than in the thread, where running out of memory would end the thread
        # before it had read anything.
        buffer = bytearray(1 << 16)
        read, write = os.pipe()
        try:
            threading.Thread(
                target=self._drain,
                args=(read, sentinel, buffer),
                name=f"drain-{self.id}",
                daemon=True,
            ).start()
        except BaseException:
            os.close(read)
            os.close(write)
            raise
        return write

    def _drain(self, fd, sentinel, buffer):
        """Read one pipe until everything holding it has closed it.

        Bytes before `sentinel` are the body's own output. The sentinel is written when the body
        finishes, so everything after it came from something the execution left running. With no
        sentinel, every byte is of that second kind.

        This thread has to outlive anything that goes wrong in it: a pipe nobody drains blocks
        its writers, and one of them may be holding the lock its execution needs to report. So it
        reads into `buffer`, which it brought with it, and what it then has no memory to keep is
        counted as dropped rather than left in the pipe. It does not give the reserve back to keep
        more: agent code runs meanwhile, and would take the reserve instead. And whatever ends the
        thread closes the pipe, so a writer gets an error rather than waiting for ever.

        Every byte is kept or counted dropped, once. Where there is not even the memory to count,
        a byte goes uncounted rather than counted twice.
        """
        view = memoryview(buffer)
        into = [buffer]
        # The end of the body's output so far, held back in case it begins the sentinel.
        held = b""
        try:
            while True:
                try:
                    count = os.readv(fd, into)
                except OSError as e:
                    _diag(f"execution {self.id!r}: reading its output failed: {e!r}")
                    break
                if not count:
                    break
                try:
                    if sentinel is None:
                        self._pass(view, 0, count)
                        continue
                    held = self._split(view, count, sentinel, held)
                    if held is None:
                        sentinel, held = None, b""
                except MemoryError:
                    # No memory even to count what was lost. What was held back goes uncounted
                    # with it, since some of it may already have been passed on.
                    held = b""
                except Exception as e:
                    _diag(f"execution {self.id!r}: attributing its output failed: {e!r}")
            self._pass(held, 0, len(held))
        finally:
            os.close(fd)
            self._end_body()

    def _split(self, view, count, sentinel, held):
        """Pass on `view`'s first `count` bytes, read after `held`, while the body's output could
        still end at `sentinel`. Returns the bytes to hold back now, or `None` once it has ended.

        The sentinel is looked for in place, so that nothing larger than it is copied to find it.
        """
        size = len(sentinel)
        try:
            # One that began in what was held back ends in the first bytes of this read.
            start = (held + view[: min(count, size - 1)]).find(sentinel)
        except MemoryError:
            start = -1
        if start >= 0:
            self._pass(held, 0, start)
            self._end_body()
            self._pass(view, start + size - len(held), count)
            return None
        found = view.obj.find(sentinel, 0, count)
        if found >= 0:
            self._pass(held, 0, len(held))
            self._pass(view, 0, found)
            self._end_body()
            self._pass(view, found + size, count)
            return None
        # All but the last `size - 1` bytes of `held` and this read are the body's; those are held
        # back, taken before anything is passed on so that each byte goes one way only.
        hold = min(size - 1, len(held) + count)
        cut = len(held) + count - hold
        try:
            if cut >= len(held):
                tail = bytes(view[cut - len(held) : count])
            else:
                tail = held[cut:] + view[:count]
        except MemoryError:
            tail = None
        if cut >= len(held):
            self._pass(held, 0, len(held))
            self._pass(view, 0, cut - len(held))
        else:
            self._pass(held, 0, cut)
        if tail is None:
            self._lose(hold)
            return b""
        return tail

    def _pass(self, data, start, end):
        """Attribute `data[start:end]`, or count it dropped if there is no memory to copy it out.

        Each byte once: `_take` keeps all of what it is given or none of it.
        """
        if start >= end:
            return
        try:
            part = bytes(data[start:end])
        except MemoryError:
            self._lose(end - start)
            return
        try:
            self._take(part)
        except MemoryError:
            self._lose(len(part))

    def _take(self, data):
        """Attribute `data`: all of it, or -- raising `MemoryError` -- none of it."""
        with self._buf_lock:
            if not self._drained.is_set():
                kept = data[: OUTPUT_MAX - len(self._captured)]
                dropped = self._dropped + len(data) - len(kept)
                self._captured += kept
                self._dropped = dropped
                return
        self.kernel.background(self.id, data)

    def _lose(self, count):
        """Count `count` bytes the drain had no memory to keep as dropped.

        Raises nothing: the drain has to carry on, and nobody to tell.
        """
        try:
            with self._buf_lock:
                if not self._drained.is_set():
                    self._dropped += count
                    return
            self.kernel.lose(self.id, count)
        except MemoryError:
            pass

    def _end_body(self):
        with self._buf_lock:
            try:
                self._drained.set()
            except MemoryError:
                # Set all the same: only waking a waiter failed, and `finish` does not wait for
                # ever.
                pass

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

    Running out of memory in the loop's own work gives the reserve back first. Reading its wakeup
    pipe fails that way while agent code holds memory at the ceiling, and would fail again at once
    on every turn for as long as it did.
    """
    if isinstance(context.get("exception"), MemoryError):
        _give_reserve()
    try:
        _explain_imports(context.get("exception"))
    except BaseException as e:
        # Named by its type, read around its metaclass: its repr is anyone's code.
        _diag(f"a failed import could not be explained: {_type_name(type(e))}")
    source = context.get("task") or context.get("future") or context.get("handle")
    get_context = getattr(source, "get_context", None)
    token = _CURRENT.set(None if get_context is None else get_context().get(_CURRENT))
    try:
        loop.default_exception_handler(context)
    finally:
        _CURRENT.reset(token)


# ---------------------------------------------------------------------------- memory

# Agents share this process, so without a ceiling one agent's runaway allocation would have the
# kernel kill it and every agent with it. The ceiling is `RLIMIT_DATA`, which counts private
# writable memory -- the heap, thread stacks -- and not file mappings or address space reserved and
# never made writable, which is how a JVM or V8 starts. Past it an allocation raises `MemoryError`
# in the thread that made it, and the execution reports it like any other failure.
#
# It is a ceiling, not isolation. It is the process's, so while one agent holds memory up to it
# every agent's allocations fail too, until that memory is let go. Shared anonymous memory
# (`mmap.mmap(-1, n)` without `MAP_PRIVATE`) is not counted. And `os._exit` ends the session
# whatever the ceiling.


def _memory_visible():
    """The memory this process can use, in bytes.

    Its cgroup's limit where it has one -- cgroup v2's `memory.max`, else v1's -- and never more
    than the machine has, which is `MemTotal`. A limit on a cgroup above the container's own is not
    visible from here.
    """
    found = [os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")]
    for path in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        # "max" when unlimited, which is not a number either.
        with contextlib.suppress(OSError, ValueError), open(path) as f:
            found.append(int(f.read()))
    return min(found)


def _set_ceiling():
    """Lower `RLIMIT_DATA`'s soft limit to half the memory the container can see.

    Half, so the interpreter runs out before the machine does and the kernel's OOM killer is not
    what answers; and half of what is there, so the ceiling grows with the machine as the
    programs an agent runs do. A lower soft limit already in place is kept.

    Every process the interpreter starts inherits the soft limit as a ceiling of its own -- in a
    threaded process there is no safe point between fork and exec to hand a child anything else.
    So the hard limit is left where it was: a program that needs more can lift its own, with
    `ulimit -d unlimited` or `resource.setrlimit`.
    """
    soft, hard = resource.getrlimit(resource.RLIMIT_DATA)
    ceiling = _memory_visible() // 2
    if soft != resource.RLIM_INFINITY:
        ceiling = min(ceiling, soft)
    try:
        resource.setrlimit(resource.RLIMIT_DATA, (ceiling, hard))
    except (OSError, ValueError) as e:
        _diag(f"no memory ceiling: setting it to {ceiling} bytes failed: {e!r}")


# Memory held back for the interpreter's own work. Agent code can use everything up to the
# ceiling, and would then leave nothing to report with -- no room to format a traceback, to start
# the thread the next execution's output needs, or to free the slot, which would refuse every
# submission after. So the reserve is held while agent code runs and given back while the
# interpreter works for itself: starting an execution, reading a request, reporting a result.
#
# Its maps are never written, so they count against the ceiling without costing memory. Taking
# them back can succeed in part: while agent code pins memory at the ceiling, the next execution
# runs with what is left, and fails at once unless it frees something first.
RESERVE = 32 << 20
RESERVE_PIECE = 1 << 20  # what is taken back at a time when all of it will not fit
_reserve = []
_reserve_lock = threading.Lock()


def _give_reserve():
    """Give the reserve back, so the interpreter's own work has room."""
    with _reserve_lock:
        while _reserve:
            _reserve.pop().close()


def _hold_reserve(sparing=0):
    """Take back as much of the reserve as fits, before agent code runs again.

    In one map when all of it fits, which is the usual case, and a piece at a time when not.
    `sparing` bytes are left free after it, if they are free now.
    """
    with _reserve_lock:
        try:
            spared = mmap.mmap(-1, sparing, flags=mmap.MAP_PRIVATE) if sparing else None
        except (OSError, MemoryError):
            return
        missing = RESERVE - sum(map(len, _reserve))
        for size in (missing, RESERVE_PIECE):
            while missing >= size > 0:
                try:
                    _reserve.append(mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE))
                except (OSError, MemoryError):
                    break
                missing -= size
        if spared is not None:
            spared.close()


def _with_room(fn, *args):
    """`fn(*args)`, tried once more with the reserve given back if memory ran out."""
    try:
        return fn(*args)
    except MemoryError:
        _give_reserve()
    return fn(*args)


class _Loop(asyncio.SelectorEventLoop):
    """An event loop that does not lose a wakeup for want of memory.

    A future that completes schedules its callbacks -- a task's wakeup among them -- with
    `call_soon`, and asyncio drops one it could not schedule. The task waiting on it then never
    runs again, not even to take a cancel, and its execution never reports. So scheduling is the
    interpreter's work too, and runs on the reserve when it has to.
    """

    def call_soon(self, callback, *args, context=None):
        try:
            return super().call_soon(callback, *args, context=context)
        except MemoryError:
            _give_reserve()
        return super().call_soon(callback, *args, context=context)


# ---------------------------------------------------------------------------- imports

# What the agent imports, besides the standard library: modules in the workspace, and packages
# `pip install` adds. Running isolated (`-I`) keeps both off `sys.path`, so `_open_imports` puts
# them back, after the standard library, so that a project file named like a standard module --
# `types.py` -- cannot replace one this program imports later.
#
# `pip` is the payload's own. The payload is mounted read-only, so pip installs into the user site
# instead, `~/.local/lib/python3.13/site-packages`, which is on `sys.path` from the start. It is
# made here if it is missing: an import that finds no directory at a path remembers that, and would
# not look again when pip later made one there. Its `.pth` files are not read, so nothing
# installed runs when the interpreter starts.
#
# Compiled code never loads. This is a static build, with no way to load a shared object, so a
# compiled module fails however it was installed -- as `Dynamic loading not supported` when its
# file carries this interpreter's suffix, and as a bare "No module named" when it was built for
# another. `_explain_imports` says so where a failure is reported.

# The directory the interpreter started in: the workspace, since the host starts it there.
_WORKSPACE = None
# This interpreter's version, as the host is told it and the agent reads it.
_VERSION = sys.version.split()[0]


# The `pip` on the agent's `PATH`: this interpreter's pip, started clear of every `PYTHON*`
# variable but `PYTHONUSERBASE`. The image sets the others for its own Python, and one such as
# `PYTHONHOME` stops this one starting at all; `PYTHONUSERBASE` says where pip installs, which this
# interpreter reads as well. Cleared for pip and what it starts in turn, such as a build, and for
# nothing else the agent runs, so the image's Python still gets them. The first Python runs
# isolated, so they cannot stop it either. The second runs with `-P`, since `-m` would otherwise put
# the working directory -- the workspace -- ahead of everything, where a project's `pip.py` would
# run instead of pip and its `types.py` would stop it starting; not `-I`, which would also turn off
# the user site pip falls back to.
_PIP = """#!/bin/sh
exec {python} -I -c '
import os, sys
env = {{k: v for k, v in os.environ.items() if not k.startswith("PYTHON") or k == "PYTHONUSERBASE"}}
os.execve(sys.executable, [sys.executable, "-P", "-m", "pip", *sys.argv[1:]], env)
' "$@"
"""


def _open_imports():
    """Put the workspace and pip's install location on `sys.path`, and pip on `PATH`.

    `pip` goes first on `PATH`, from a directory of its own under the user base, so it means this
    interpreter's pip while `python3` still means the image's own Python; the scripts pip installs
    follow it. Every process the interpreter starts inherits the `PATH`, and nothing else in the
    container sees it.

    That `pip` is `_PIP`, not the payload's own script, which would start this Python in whatever
    `PYTHON*` variables the image sets for its own.
    """
    global _WORKSPACE
    try:
        _WORKSPACE = os.getcwd()
        sys.path.append(_WORKSPACE)
    except OSError as e:
        _diag(f"the working directory is not importable: {e!r}")
    user_site = site.getusersitepackages()
    try:
        os.makedirs(user_site, exist_ok=True)
    except OSError as e:
        _diag(f"packages pip installs will not import: {user_site} could not be made: {e!r}")
    sys.path.append(user_site)
    front = []
    pip = os.path.join(site.getuserbase(), "share", "outrig", "bin")
    script = _PIP.format(python=shlex.quote(sys.executable)).encode()
    try:
        os.makedirs(pip, exist_ok=True)
        for name in ("pip", "pip3", f"pip{sys.version_info.major}.{sys.version_info.minor}"):
            # Written beside the script and renamed over it, so an interpreter starting at the same
            # time under the same `HOME` never finds it missing or half written.
            path, made = os.path.join(pip, name), os.path.join(pip, f".{name}.{os.getpid()}")
            fd = os.open(made, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o755)
            try:
                _write_all(fd, script)
            finally:
                os.close(fd)
            os.replace(made, path)
        front.append(pip)
    except OSError as e:
        _diag(f"`pip` is not on PATH: {e!r}")
    front.append(os.path.join(site.getuserbase(), "bin"))
    os.environ["PATH"] = os.pathsep.join([*front, os.environ.get("PATH", os.defpath)])


def _image_python():
    """The image's own Python: the first `python3` or `python` on `PATH` that is not this one."""
    ours = os.path.realpath(sys.prefix) + os.sep
    for name in ("python3", "python"):
        found = shutil.which(name)
        if found and not os.path.realpath(found).startswith(ours):
            return found
    return None


# What every explanation of a failed import says, and how one already explained is recognized.
_NO_COMPILED = "this interpreter cannot load compiled code"
# A compiled module's file, built for any interpreter: this one's suffixes all end in `.so`.
_COMPILED_SUFFIXES = (".so", ".pyd")
# What a namespace package's `__path__` is.
_NamespacePath = importlib._bootstrap_external._NamespacePath


def _elsewhere():
    """Where code that needs compiled packages can run, as a sentence."""
    found = _image_python()
    python = f"the image's own Python, {found}," if found else "a Python the image provides"
    return (
        f"Code that needs a compiled package can run with {python} through subprocess, or as a "
        f"service."
    )


def _compiled_file(name):
    """A compiled file where module `name` would be, or `None`.

    Looked for where the import looked: its package's `__path__`, or `sys.path` for a top-level
    module. Read without running agent code: the package is taken only if it is a plain module, and
    its path only if it is a list, or importlib's namespace path as it last computed it --
    iterating one computes it again when its parent's path has changed, through every path hook
    and finder the agent installed.
    """
    parent, _, leaf = name.rpartition(".")
    if parent:
        package = sys.modules.get(parent)
        if type(package) is not types.ModuleType:
            return None
        where = package.__dict__.get("__path__")
    else:
        where = sys.path
    if type(where) is _NamespacePath:
        where = where._path
    if type(where) is not list:
        return None
    prefix = f"{leaf}."
    for directory in list(where):
        if type(directory) is not str:
            continue
        try:
            entries = os.listdir(directory or ".")
        except OSError:
            continue
        for entry in entries:
            if entry.startswith(prefix) and entry.endswith(_COMPILED_SUFFIXES):
                return os.path.join(directory, entry)
    return None


def _explain_import(exc):
    """Rewrite a failed import's message to say why it failed here, if the reason is this
    interpreter: compiled code, or a top-level module that is nowhere. Only `ImportError` and
    `ModuleNotFoundError` themselves are read, never a subclass, so no agent code runs."""
    kind = type(exc)
    if kind is not ImportError and kind is not ModuleNotFoundError:
        return
    name, message = exc.name, exc.msg
    if type(name) is not str or type(message) is not str or _NO_COMPILED in message:
        return
    path = exc.path
    if kind is ImportError:
        compiled = path if type(path) is str and path.endswith(_COMPILED_SUFFIXES) else None
        if compiled is None:
            return
    else:
        compiled = _compiled_file(name)
    if compiled is not None:
        text = (
            f"cannot import {name!r}: {compiled} is compiled code, and {_NO_COMPILED}, however it "
            f"is installed. {_elsewhere()}"
        )
    elif "." not in name and name not in sys.stdlib_module_names:
        text = (
            f"{message}. If a pure-Python package provides it, `pip install` that package and "
            f"import it again. A package with compiled parts will not import however it is "
            f"installed: {_NO_COMPILED}. {_elsewhere()}"
        )
    else:
        return
    exc.msg = text
    exc.args = (text,)


_cause_of = BaseException.__dict__["__cause__"].__get__
_context_of = BaseException.__dict__["__context__"].__get__
_members_of = BaseExceptionGroup.__dict__["exceptions"].__get__


def _explain_imports(exc):
    """Explain each failed import a traceback of `exc` would show: `exc`, its causes and contexts,
    and an exception group's members. Read through `BaseException`'s own descriptors, so a
    property an agent's exception class defines is never run.

    Done where a failure is reported rather than in the import system, so a library that tries an
    optional import behaves as it would anywhere, and an import that works costs nothing. An agent
    that catches the error and prints it sees Python's own message.
    """
    seen = set()
    stack = [exc]
    while stack and len(seen) < 100:
        exc = stack.pop()
        if exc is None or id(exc) in seen:
            continue
        seen.add(id(exc))
        _explain_import(exc)
        stack += (_cause_of(exc), _context_of(exc))
        if issubclass(type(exc), BaseExceptionGroup):
            stack += _members_of(exc)


class Python:
    """What this interpreter can run, readable before an import is tried.

    It is OutRig's own CPython, built static and mounted read-only, so it runs in an image that
    has no Python of its own.

    - The standard library imports, but for modules of other platforms such as `winreg`.
      `importlib.util.find_spec(name)` says whether a module is anywhere on `sys.path` without
      importing it.
    - `pip install` adds pure-Python packages, which import at once, with no restart. This `pip`
      is the interpreter's own: it installs into the user site, which is on `sys.path`, and puts a
      package's scripts on `PATH`. An image Python of the same version reads that user site too.
      `pip install --target DIR` works as well, once `DIR` is appended to `sys.path`.
    - Modules in the workspace -- the directory the interpreter started in, `workspace` below --
      import too. It comes after the standard library on `sys.path`, so a file of yours named like
      a standard module does not replace it.
    - Compiled code never loads: this is a static build, with no way to load a shared object. A
      package with compiled parts -- numpy, pandas, pydantic, lxml, cryptography -- does not
      import however it is installed, and `ctypes` imports but cannot load a library.
    - Code that needs such a package runs with the image's own Python, `image_python` below,
      through `subprocess`, where `python3 -m pip` installs for that Python. Or it runs as a
      service.
    """

    @property
    def version(self):
        """This interpreter's version, as `"3.13.15"`."""
        return _VERSION

    @property
    def workspace(self):
        """The directory this interpreter started in, whose modules import; `None` if unknown."""
        return _WORKSPACE

    @property
    def image_python(self):
        """The image's own Python on `PATH`, or `None` if it has none there."""
        return _image_python()

    def __repr__(self):
        workspace = "" if _WORKSPACE is None else f" ({_WORKSPACE})"
        return (
            f"<python: OutRig's static CPython {self.version}, with the standard library. `pip "
            f"install` adds pure-Python packages, which import at once, and modules in the "
            f"workspace{workspace} import too. Compiled code never loads, so numpy, pandas, and "
            f"other packages with compiled parts do not import however they are installed. "
            f"{_elsewhere()} help(runtime.python) says more.>"
        )


_PYTHON = Python()


# ---------------------------------------------------------------------------- help

_HELP_GUIDE = (
    "help(x) describes x -- a function's signature and docstring, a class's methods, a module's "
    f"contents -- in at most {HELP_MAX} characters. help(runtime) describes what OutRig gives "
    "you: your channels, runtime.wait, runtime.names(), and your conversation, in runtime.history "
    "and runtime.context. help(runtime.python) says what this interpreter can import, and where "
    "code that needs more can run."
)


@functools.cache
def _pydoc_helper():
    """pydoc's `Helper`, writing every answer to its output.

    Its search, `help('modules <key>')`, prints each match itself instead, past the bound and ahead
    of its own heading. Redirecting `sys.stdout` around it would take other executions' output with
    it, so the search is done here, as pydoc does it, writing where the rest goes.
    """
    import pydoc
    import warnings

    class Helper(pydoc.Helper):
        def listmodules(self, key=""):
            if not key:
                return super().listmodules()
            output = self.output
            output.write(f"\nHere is a list of modules whose name or summary contains {key!r}.\n\n")

            def found(path, name, synopsis):
                if name.endswith(".__init__"):
                    name = f"{name.removesuffix('.__init__')} (package)"
                output.write(f"{name} - {synopsis}\n" if synopsis else f"{name}\n")

            with warnings.catch_warnings():
                warnings.filterwarnings("ignore")
                pydoc.ModuleScanner().run(found, key, onerror=lambda name: None)

    return Helper


class _Help:
    """`help(x)`: what `pydoc` says of `x` -- its signature and docstring, and its members' --
    in at most `HELP_MAX` characters.

    Replaces `site`'s, whose `help()` starts an interactive utility that would read `/dev/null`,
    and whose answers are as long as the thing described: all of `asyncio` is 225 KB. It runs in
    the execution that asked, like any other code the agent calls.
    """

    def __repr__(self):
        return "Type help(x) for help about x, or help() for where to start."

    def __call__(self, *request):
        if not request:
            print(_HELP_GUIDE)
            return
        import pydoc

        answer = io.StringIO()
        _pydoc_helper()(input=io.StringIO(), output=answer)(*request)
        text = answer.getvalue()
        if len(text) > HELP_MAX:
            # At a line break if one falls in the bound's second half, so a single long line --
            # one module's summary -- is cut rather than dropped whole.
            cut = text.rfind("\n", HELP_MAX // 2, HELP_MAX) + 1 or HELP_MAX
            text = (
                f"{text[:cut]}[help cut at {cut} of {len(text)} characters: ask about one member, "
                f"as help(x.name), or read inspect.signature(x) or x.__doc__]\n"
            )
        sys.stdout.write(text)


builtins.help = _Help()


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

    Formatted on the reserve: the failure may be that memory ran out, and the traceback still holds
    whatever its frames had built. A failed import this interpreter is the reason for is explained
    first, and one that cannot be is reported as Python put it.
    """
    _give_reserve()
    try:
        _explain_imports(exc)
    except BaseException as e:
        # Named by its type, read around its metaclass: its repr is anyone's code.
        _diag(f"a failed import could not be explained: {_type_name(type(e))}")
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
_type_qualname = type.__dict__["__qualname__"].__get__


# ---------------------------------------------------------------------------- history

# The host keeps the agent's conversation, a turn at a time, and pushes each turn here as it
# commits it: one model call, what the model wrote, and each call it made with the result it read.
# Held here it is ordinary data, so the agent's code can read all of it and only what that code
# prints is seen. What each model call is sent of it is the host's to assemble; a promotion is how
# the agent asks for a turn to be sent again, and names it by id, never by content.


@dataclasses.dataclass(frozen=True)
class Call:
    """One call the model made: the `source` it submitted, and the `result` it read back."""

    source: str
    result: str

    def __repr__(self):
        return f"<call: {len(self.source)} characters of source, {len(self.result)} of result>"


@dataclasses.dataclass(frozen=True)
class Turn:
    """One model call and the calls it made, as the model saw them.

    `id` names the turn for good, and is what `runtime.context.promote` takes. `round` counts the
    conversation's rounds from 1. `prompt` is the line that opened the round, on its first turn,
    and `None` on the rest. `text` is what the model wrote, and `calls` each call it made, with the
    result it read -- clipped where the model's was.
    """

    id: int
    round: int
    prompt: str | None
    text: str
    calls: tuple[Call, ...]

    def __repr__(self):
        return (
            f"<turn {self.id}, round {self.round}: {len(self.calls)} calls, {len(self.text)} "
            f"characters of text>"
        )


class History:
    """Your whole conversation, a turn at a time: `runtime.history.turns`.

    A turn is one model call and the calls it made. `turns` holds every turn that has finished,
    oldest first, as `Turn` objects -- the round in progress too, up to its latest call. Reading
    it costs no context: only what your code prints is seen, so scan it with ordinary Python and
    print what you need. It is a tuple, and a turn finishing later is not added to one already
    read, so a scan sees what was there when it started.

    Not every turn is sent to the model on every call: see `help(runtime.context)`.
    """

    def __init__(self):
        self._turns = ()

    @property
    def turns(self):
        """Every finished turn, oldest first, as a tuple of `Turn`."""
        return self._turns

    def __repr__(self):
        turns = self._turns
        rounds = turns[-1].round if turns else 0
        return f"<history: {len(turns)} turns over {rounds} rounds>"

    def _add(self, turn):
        """Hold `turn`, which the host committed. Reader thread only.

        A turn is held only if it comes after the last one held. So a retry after running out of
        memory never holds one twice, and a turn whose line was lost leaves a gap in the ids
        rather than stopping every later one. Rebinding is the last step.
        """
        turns = self._turns
        if turns and turn.id <= turns[-1].id:
            return
        self._turns = turns + (turn,)


class Context:
    """What each model call is sent of your conversation.

    Each call is sent the first rounds of the conversation, the most recent ones, and the round in
    progress, whole. Everything else stays in `runtime.history.turns`, unsent, until you promote
    it: `runtime.context.promote(turn)`. A promoted turn is sent in its place in the
    conversation, not at the end, from the next model call on -- in this round, if your code
    promotes it while it runs.
    """

    def __init__(self, agent, history):
        self._agent = agent
        self._history = history
        self._lock = threading.Lock()
        self._promoted = set()

    @property
    def promoted(self):
        """The ids of the turns you have promoted, in order."""
        with self._lock:
            return tuple(sorted(self._promoted))

    def promote(self, *turns):
        """Send `turns` to the model from its next call on, each in its place in the conversation.

        Each is a `Turn` from `runtime.history.turns`, a turn's id, or an iterable of those. A turn
        is named by its id, so a copy promotes what the original said. Promoting a turn again
        changes nothing.
        """
        ids = set()
        for item in turns:
            many = not isinstance(item, (Turn, int)) and hasattr(type(item), "__iter__")
            for one in item if many else (item,):
                if isinstance(one, Turn):
                    ids.add(one.id)
                elif type(one) is int:
                    ids.add(one)
                else:
                    raise TypeError(f"a turn or a turn's id, not {_type_name(type(one))}")
        held = {turn.id for turn in self._history.turns}
        unknown = sorted(ids - held)
        if unknown:
            raise ValueError(f"no finished turn has id {', '.join(map(str, unknown))}")
        if ids:
            _write_line(_encode({"t": "promote", "agent": self._agent, "turns": sorted(ids)}))
            with self._lock:
                self._promoted |= ids

    def __repr__(self):
        return f"<context: {len(self.promoted)} turns promoted>"


# ---------------------------------------------------------------------------- channels

# A channel carries typed messages between two endpoints, each named locally. Today the other end
# of every channel is the host: an agent's `user` channel, carrying text both ways. A message from
# the host is queued on the reader thread, so `pending()` is exact even while the agent's loop is
# not turning; a message to it is one protocol line. Each endpoint also counts every message ever
# delivered to it, which is how the host tells a new arrival from one it has already announced.
#
# The host says when the user has received each message an endpoint sent, and a send waits while
# `SEND_WINDOW` are unreceived. So an agent that sends faster than the user reads is held back in
# its own code, under this process's memory ceiling, rather than piling messages up in the host's.
#
# `runtime.wait` watches every endpoint and takes nothing: a message arriving wakes it as it wakes
# a waiting receive, and it raises while any message waits unread.

QUEUE_MAX = 256  # unread messages one endpoint holds
MESSAGE_MAX = 1 << 20  # bytes of the protocol line one sent message becomes
SEND_WINDOW = 16  # messages one endpoint sends ahead of the other end receiving them

# What a contract may name: the types with an obvious JSON form, and what is built from them.
_SCALARS = (str, bool, int, float, type(None))
_UNIONS = (typing.Union, types.UnionType)

_SUBSET = (
    "str, bool, int, float, None, list[T], dict[str, T], unions of these, and dataclasses whose "
    "fields are all of these"
)


def _describe(t):
    """A type as a contract error names it: `bytes`, `set[int]`, `Row`."""
    if t is type(None):
        return "None"
    if typing.get_origin(t) is None and isinstance(t, type):
        return _type_qualname(t)
    return repr(t).removeprefix("typing.")


def _describe_all(contract):
    return " | ".join(map(_describe, contract)) if contract else "nothing"


def _contract(declared, where):
    """One direction's contract, as the tuple of types it admits.

    `declared` is a type, a union of types, or a tuple of them, `()` for a direction that carries
    nothing. Every type must be in the serializable subset, checked here rather than at the first
    send: anything else raises `TypeError` naming the type, and the field it is in when it is
    nested in a dataclass.
    """
    members = declared if isinstance(declared, tuple) else _union_members(declared)
    for member in members:
        _check_subset(member, where, "", set())
    return tuple(type(None) if member is None else member for member in members)


def _union_members(t):
    return typing.get_args(t) if typing.get_origin(t) in _UNIONS else (t,)


def _check_subset(t, where, inside, seen):
    """Raise `TypeError` unless `t` is in the serializable subset. `inside` names the field."""
    if t is None or t in _SCALARS:
        return
    origin, args = typing.get_origin(t), typing.get_args(t)
    if origin in _UNIONS:
        for member in args:
            _check_subset(member, where, inside, seen)
        return
    if origin is list and len(args) == 1:
        _check_subset(args[0], where, inside, seen)
        return
    if origin is dict and len(args) == 2 and args[0] is str:
        _check_subset(args[1], where, inside, seen)
        return
    if origin is None and isinstance(t, type) and dataclasses.is_dataclass(t):
        # A dataclass that refers to itself is checked once.
        if t in seen:
            return
        seen.add(t)
        for field, hint in _dataclass_fields(t, where):
            _check_subset(hint, where, f" (field {field!r} of {_type_qualname(t)})", seen)
        return
    raise TypeError(
        f"{where} {_describe(t)}{inside}, which is outside the subset a channel can carry: "
        f"{_SUBSET}"
    )


def _dataclass_fields(t, where):
    """`(name, type)` for each of dataclass `t`'s fields, its annotations resolved."""
    try:
        hints = typing.get_type_hints(t)
    except Exception as e:
        raise TypeError(
            f"{where} {_type_qualname(t)}, whose field types cannot be resolved: "
            f"{_type_name(type(e))}: {e}"
        ) from None
    return [(field.name, hints.get(field.name, field.type)) for field in dataclasses.fields(t)]


def _conforms(value, t):
    """Whether `value` is of contract type `t`.

    By exact type, so a subclass carrying behavior does not pass as its base and `True` is not an
    `int`; an `int` does pass as a `float`, as it does in Python's own annotations. Only a
    dataclass's fields are read, and only by the agent's own `send`: a body from the host is plain
    JSON, never a dataclass instance, so checking one runs no agent code.
    """
    if t is type(None):
        return value is None
    if t is float:
        return type(value) in (int, float) and math.isfinite(value)
    if t in _SCALARS:
        return type(value) is t
    origin, args = typing.get_origin(t), typing.get_args(t)
    if origin in _UNIONS:
        return any(_conforms(value, member) for member in args)
    if origin is list:
        return type(value) is list and all(_conforms(item, args[0]) for item in value)
    if origin is dict:
        return type(value) is dict and all(
            type(key) is str and _conforms(item, args[1]) for key, item in value.items()
        )
    return type(value) is t and all(
        _conforms(getattr(value, field), hint) for field, hint in _dataclass_fields(t, "")
    )


class _Refused(Exception):
    """A message the host sent that its endpoint does not take. Nothing was queued."""


@dataclasses.dataclass(frozen=True)
class Delivery:
    """One message taken from an endpoint.

    `body` is the message itself, `sender` who sent it -- `"user"` on the user channel -- and
    `received_at` when it arrived, in UTC.
    """

    body: object
    sender: str
    received_at: datetime.datetime


class MessageAvailable(BaseException):
    """Raised by `runtime.wait` while a message waits unread on one of this agent's channels.

    The wait ends, and nothing else does: the message is still queued for a receive, and what the
    wait was given is still running. `channels` names the channels holding messages.

    A `BaseException`, as `asyncio.CancelledError` is, so that an `except Exception:` written
    around a wait does not swallow it. Catch it by name, as `runtime.MessageAvailable`.
    """

    def __init__(self, channels):
        self.channels = tuple(channels)
        where = ", ".join(f"runtime.channels[{json.dumps(name)}]" for name in self.channels)
        super().__init__(
            f"input is waiting on {where}; it has not been read, and the wait cancelled nothing"
        )


def _wake(waiter):
    if not waiter.done():
        waiter.set_result(None)


def _wake_all(waiters, channel):
    """Wake each of `waiters`, an iterator taken before the change they wait on was made.

    Raises nothing. The change has been made, and `_handle` retries a message whose handling ran
    out of memory, which would make it twice. So nothing here is left to allocate outside a `try`:
    not the iterator, which the caller took, and not a `suppress`.
    """
    for waiter in waiters:
        try:
            _with_room(waiter.get_loop().call_soon_threadsafe, _wake, waiter)
        except BaseException as e:
            # Its loop has closed, or there is no memory even to schedule the wakeup.
            try:
                _diag(f"channel {channel!r}: a waiting receive or send could not be woken: {e!r}")
            except BaseException:
                pass


class Endpoint:
    """One end of a channel: `await receive()`, `await send(message)`, and `pending()`.

    The channel's contract -- what may travel each way -- was fixed when it was made, and reads as
    `receives` and `sends`. Messages arrive in the order they were sent.
    """

    def __init__(self, agent, name, *, receives, sends):
        self._receives = _contract(receives, f"channel {name!r} cannot receive")
        self._sends = _contract(sends, f"channel {name!r} cannot send")
        self._agent = agent
        self._name = name
        self._lock = threading.Lock()
        self._queue = collections.deque()
        # Every message ever queued here, received or not.
        self._delivered = 0
        # Messages sent that the other end has not yet received.
        self._unreceived = 0
        # Futures of receives waiting for a message -- and of waits watching for one, which take
        # nothing -- and of sends waiting for room in the window, each on its own loop.
        self._receivers = []
        self._senders = []

    @property
    def receives(self):
        """The types a message arriving here may have."""
        return self._receives

    @property
    def sends(self):
        """The types a message sent from here may have."""
        return self._sends

    def __repr__(self):
        return (
            f"<endpoint {self._name!r}: receives {_describe_all(self._receives)}, sends "
            f"{_describe_all(self._sends)}, {self.pending()} pending>"
        )

    def pending(self):
        """How many messages are waiting to be received. Takes none of them."""
        with self._lock:
            return len(self._queue)

    def _counts(self):
        """How many messages wait, and how many have ever been delivered, as of one moment."""
        with self._lock:
            return {"pending": len(self._queue), "delivered": self._delivered}

    async def receive(self):
        """Take the next message, waiting until there is one, as a `Delivery`.

        `.body` is the message itself. Receiving removes it: no later receive sees it again.
        """
        return await self._when(self._receivers, self._take)

    def _take(self):
        return self._queue.popleft() if self._queue else None

    def _take_room(self):
        if self._unreceived < SEND_WINDOW:
            self._unreceived += 1
            return True
        return None

    async def _when(self, waiters, take):
        """Wait until `take()`, called under the lock, gives something other than `None`, and
        return it. A waiting call parks a future in `waiters`, woken to try again."""
        loop = asyncio.get_running_loop()
        while True:
            with self._lock:
                taken = take()
                if taken is not None:
                    return taken
                waiter = loop.create_future()
                waiters.append(waiter)
            try:
                await waiter
            finally:
                with self._lock, contextlib.suppress(ValueError):
                    waiters.remove(waiter)

    def _watch(self, waiter):
        """Whether a message is waiting here, taking none of them. If none is, `waiter` is woken
        when one arrives, as a waiting receive is, until `_unwatch` takes it back."""
        with self._lock:
            if self._queue:
                return True
            self._receivers.append(waiter)
            return False

    def _unwatch(self, waiter):
        """Stop waking `waiter`, if it was watching. Checked rather than caught, so it raises
        nothing: a `ValueError` to catch would be one more allocation."""
        with self._lock:
            if waiter in self._receivers:
                self._receivers.remove(waiter)

    async def send(self, message):
        """Send `message` to the other end.

        It returns once the message is on its way: accepted for delivery, not yet read. At most
        `SEND_WINDOW` messages are on their way unreceived, and a send past that waits until the
        other end receives one. Sending does not end anything -- not the round, and not the code
        that sends.
        """
        if not any(_conforms(message, t) for t in self._sends):
            raise TypeError(
                f"channel {self._name!r} sends {_describe_all(self._sends)}, not "
                f"{_type_name(type(message))}"
            )
        # Encoded here rather than on the reserve: running out of memory while building the
        # agent's own message is the agent's to see.
        line = _encode({"t": "send", "agent": self._agent, "channel": self._name, "body": message})
        if len(line) > MESSAGE_MAX:
            raise ValueError(
                f"this message is {len(line)} bytes as sent, past the {MESSAGE_MAX} a channel "
                f"carries; send less, or send it in parts"
            )
        await self._when(self._senders, self._take_room)
        try:
            _write_line(line)
        except BaseException:
            # Not sent, so it takes no room -- on the reserve if it has to be, since room that is
            # never given back is lost to the channel for good.
            _with_room(self._received)
            raise

    def _received(self):
        """The other end received a message this endpoint sent, which makes room for another.

        Only what comes before the room is made can raise -- running out of memory, with nothing
        changed, so the caller can try again. After it nothing raises, so a retry never makes the
        room twice.
        """
        with self._lock:
            wake = iter(self._senders.copy())
            if self._unreceived:
                self._unreceived -= 1
        _wake_all(wake, self._name)

    def _deliver(self, body, sender):
        """Queue `body` from `sender` and return how many messages then wait. Reader thread only.

        Raises `_Refused` for a body the contract does not take or a queue that is full, having
        queued nothing. The append is the last step that can fail, so a retry after running out
        of memory never queues a message twice; waking the receivers comes after, and cannot undo
        it.
        """
        if not any(_conforms(body, t) for t in self._receives):
            raise _Refused(
                f"channel {self._name!r} receives {_describe_all(self._receives)}, not "
                f"{_type_name(type(body))}"
            )
        delivery = Delivery(body, sender, datetime.datetime.now(datetime.timezone.utc))
        with self._lock:
            if len(self._queue) >= QUEUE_MAX:
                raise _Refused(f"channel {self._name!r} already holds {QUEUE_MAX} unread messages")
            count, delivered = len(self._queue) + 1, self._delivered + 1
            wake = iter(self._receivers.copy())
            self._queue.append(delivery)
            # Only rebinding after the append, which cannot fail.
            self._delivered = delivered
        # Every receiver, since each re-checks and one may have been cancelled in the meantime.
        _wake_all(wake, self._name)
        return count


# What `return_when` may be: asyncio's own constants, which in CPython are strings.
_RETURN_WHEN = (asyncio.FIRST_COMPLETED, asyncio.FIRST_EXCEPTION, asyncio.ALL_COMPLETED)


class Runtime:
    """What this agent reaches beyond its own names.

    `runtime.channels` maps a name to the endpoint of a channel. `runtime.channels["user"]` is how
    the user reaches you and you reach them: see `help(type(runtime.channels["user"]))`.

    `await runtime.wait(fs)` waits as `asyncio.wait` does and watches every channel while it
    waits: see `help(runtime.wait)`.

    `runtime.names()` lists what you have bound, with the type of each value.

    `runtime.history.turns` is your whole conversation, to read with ordinary Python at no cost in
    context: see `help(runtime.history)`. Not all of it is sent to the model on each call, and
    `runtime.context.promote(turn)` sends a turn again: see `help(runtime.context)`.

    `runtime.python` says what this interpreter can import -- `pip install` adds pure-Python
    packages, and nothing compiled loads -- and where code that needs more can run.
    """

    MessageAvailable = MessageAvailable

    def __init__(self, channels, held, history, context):
        self._channels = types.MappingProxyType(channels)
        self._held = held
        self._history = history
        self._context = context

    @property
    def channels(self):
        """This agent's channel endpoints, by name. Read-only."""
        return self._channels

    @property
    def history(self):
        """Your whole conversation, a turn at a time."""
        return self._history

    @property
    def context(self):
        """What each model call is sent of your conversation, and how to add to it."""
        return self._context

    @property
    def python(self):
        """What this interpreter can import, and where code that needs more can run."""
        return _PYTHON

    def names(self):
        """Every name you have bound, as `{name: type name}`, sorted by name.

        The runtime's own names are left out, until you rebind one. Only names and the types of
        values are read -- no `repr()`, property, or iterator runs -- so it answers whatever the
        values are. Look at a value itself with an expression of your own.
        """
        return dict(self._held())

    async def wait(self, fs, *, timeout=None, return_when=asyncio.ALL_COMPLETED):
        """Wait for the futures in `fs` as `asyncio.wait` does, watching every channel meanwhile.

        It returns for asyncio's reasons and raises for the runtime's. `(done, pending)` comes
        back, two sets, once the futures satisfy `return_when` -- `asyncio.FIRST_COMPLETED`,
        `FIRST_EXCEPTION`, or `ALL_COMPLETED` -- or once `timeout` seconds have passed. A timeout
        cancels nothing and raises no `TimeoutError`, so `done` may be empty; giving up on what is
        still pending is a separate act. A task that failed is done, and its exception is in
        `task.exception()` rather than raised here.

        While a message waits unread on any channel, one there before the call included, it
        raises `runtime.MessageAvailable` instead, even if the futures are done as well. The
        message is still queued for a receive, and nothing in `fs` is cancelled, so a later wait
        can wait on them again. Nor are they cancelled when the code waiting here is, as a task it
        awaited directly would be.

        `fs` holds futures -- tasks, usually -- not coroutines. Name them, as in
        `asyncio.create_task(watch(), name="ci")`, so `done` says which is which.
        """
        # asyncio's own checks, in its own words, so what the mirror refuses is what asyncio does.
        if asyncio.isfuture(fs) or asyncio.iscoroutine(fs):
            raise TypeError(f"expect a list of futures, not {type(fs).__name__}")
        if not fs:
            raise ValueError("Set of Tasks/Futures is empty.")
        if return_when not in _RETURN_WHEN:
            raise ValueError(f"Invalid return_when value: {return_when}")
        fs = set(fs)
        if any(asyncio.iscoroutine(f) for f in fs):
            raise TypeError("Passing coroutines is forbidden, use tasks explicitly.")
        # An iterator is not known to be empty until it is read. asyncio trips an assertion here.
        if not fs:
            raise ValueError("Set of Tasks/Futures is empty.")
        loop = asyncio.get_running_loop()
        # Woken for asyncio's reasons, as the waiter in `asyncio.tasks._wait` is: by the timer, or
        # by the one callback each future gets, which counts completions as they come. So `N`
        # futures finishing one at a time cost `N` callbacks, not a look at every future still
        # pending each time one does.
        finished = loop.create_future()
        timer = None if timeout is None else loop.call_later(timeout, _wake, finished)
        remaining = len(fs)

        def completed(f):
            nonlocal remaining
            remaining -= 1
            # asyncio's own test. Only `FIRST_EXCEPTION` reads an exception, which marks it
            # retrieved, as asyncio's reading does.
            if (
                remaining <= 0
                or return_when == asyncio.FIRST_COMPLETED
                or (
                    return_when == asyncio.FIRST_EXCEPTION
                    and not f.cancelled()
                    and f.exception() is not None
                )
            ):
                _wake(finished)

        for f in fs:
            f.add_done_callback(completed)
        try:
            while True:
                # Read again on each pass, so a channel added during the wait is watched from the
                # next one on.
                endpoints = list(self._channels.items())
                arrived = loop.create_future()
                try:
                    # Input first, so it wins over futures that are done and a timeout that has
                    # passed; and without suspending, so code that catches this and waits again
                    # spins where the runaway probe detects it.
                    waiting = [name for name, endpoint in endpoints if endpoint._watch(arrived)]
                    if waiting:
                        raise MessageAvailable(waiting)
                    if finished.done():
                        done = {f for f in fs if f.done()}
                        return done, fs - done
                    await asyncio.wait({finished, arrived}, return_when=asyncio.FIRST_COMPLETED)
                finally:
                    for _, endpoint in endpoints:
                        endpoint._unwatch(arrived)
        finally:
            if timer is not None:
                timer.cancel()
            for f in fs:
                f.remove_done_callback(completed)

    def __repr__(self):
        return f"<runtime: channels {', '.join(map(repr, self._channels))}>"


class Kernel:
    """One agent's execution environment: its namespace, its loop, its slot, its backlog."""

    def __init__(self, agent):
        self.agent = agent
        self.loop = _Loop()
        self.loop.set_exception_handler(_attributed_exception)
        self.module = types.ModuleType(_module_name(agent))
        self.globals = self.module.__dict__
        self.globals["__builtins__"] = builtins
        self.globals["asyncio"] = asyncio
        # The user's channel carries text both ways. Every agent has one, and the host is at the
        # other end of it.
        self.channels = {"user": Endpoint(agent, "user", receives=str, sends=str)}
        self.history = History()
        self.globals["runtime"] = Runtime(
            self.channels, self.held, self.history, Context(agent, self.history)
        )
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
        _send({"t": "ready", "agent": self.agent, "version": _VERSION})

    def serve(self):
        """Run this kernel's loop on the calling thread for the life of the process."""
        asyncio.set_event_loop(self.loop)
        while not self.loop.is_closed():
            try:
                self.loop.run_forever()
            except BaseException as e:
                # A `SystemExit` or `KeyboardInterrupt` raised in a background task escapes the
                # loop rather than its task. It has ended that task; it must not end the agent --
                # and nor must running out of memory while saying so, which for the primary would
                # return from `main` and end the process. A `try` rather than `suppress`, which
                # would allocate.
                try:
                    if not self._interrupted_task(e):
                        _diag(f"agent {self.agent!r}: {type(e).__name__} escaped its event loop")
                except MemoryError:
                    pass
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
        # Starting is the interpreter's own work, and runs on the reserve: the thread that drains
        # the execution's output needs a stack, and the loop goes on to read its wakeup before the
        # body runs. The body takes the reserve back.
        _give_reserve()
        try:
            execution = _Execution(self, exec_id)
        except (OSError, RuntimeError, MemoryError) as e:
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
            code = _with_room(_compile, source)
            # Agent code runs with the reserve held, so that whatever it does to memory there is
            # some left to report it with.
            _hold_reserve()
            result = eval(code, self.globals)  # noqa: S307 -- this is the feature
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
            # From here the interpreter works for itself, on the reserve.
            _give_reserve()
            # Spent either way. What the handler kept would otherwise hold an interrupted
            # frame -- and whatever a runaway built in it -- until the next interrupt.
            self._interrupting = self._landed = None
            # Before anything that allocates, so nothing that fails below can leave it held.
            self.running = None
            self._release()
            try:
                output, dropped = execution.finish()
            except Exception as e:
                _diag(f"execution {execution.id!r}: collecting its output failed: {e!r}")
                output, dropped = "", 0
        try:
            self._reply(
                execution.id,
                "ok" if error is None else "error",
                output=output,
                dropped=dropped,
                error=error,
                background=self.take_background(),
            )
        except MemoryError:
            # Agent code still running -- a task, a thread -- took the room back. What the
            # execution wrote is lost; that it ended is not.
            self._reply(
                execution.id,
                "error",
                error=error or "MemoryError: memory ran out while this result was reported\n",
            )
        # Sparing a piece: the loop goes idle now, and its own machinery -- reading its wakeup
        # pipe, answering a probe -- must not find the ceiling where agent code left it.
        _hold_reserve(sparing=RESERVE_PIECE)

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
        """Hold `data` for the next result, keeping the most recent `BG_MAX` bytes overall.

        All of it, or -- raising `MemoryError` -- none of it. Each step of the trim changes nothing
        until everything it needs is allocated, so a byte is held or counted dropped, never both;
        a trim that runs out leaves the backlog over its bound until the next one.
        """
        if not data:
            # Zero bytes count nothing against the bound, so they must not take a place either.
            return
        with self._bg_lock:
            entry, total = (exec_id, data), self._bg_len + len(data)
            self._bg.append(entry)
            self._bg_len = total
            with contextlib.suppress(MemoryError):
                while self._bg_len > BG_MAX:
                    owner, oldest = self._bg[0]
                    cut = min(self._bg_len - BG_MAX, len(oldest))
                    rest = (owner, oldest[cut:]) if cut < len(oldest) else None
                    remaining = self._bg_len - cut
                    self._bg_dropped[owner] += cut
                    if rest is None:
                        self._bg.popleft()
                    else:
                        self._bg[0] = rest
                    self._bg_len = remaining

    def lose(self, exec_id, count):
        """Count `count` bytes of `exec_id`'s background output as dropped."""
        with self._bg_lock:
            self._bg_dropped[exec_id] += count

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

    def held(self):
        """What the agent has bound, as `(name, type name)` pairs sorted by name.

        Reads only the namespace's keys and the type of each value. Nothing here calls `repr()`, a
        property, a descriptor, or an iterator -- all of those can execute code, block, or flood --
        so it answers whatever the values are, and whoever asks.
        """
        return sorted(
            (name, _type_name(type(value)))
            for name, value in list(self.globals.items())
            if type(name) is str
            and not name.startswith("__")
            and self._boot.get(name, _MISSING) is not value
        )

    def inventory(self, request_id, after):
        """A bounded listing of what the session holds, always answered.

        The first `INVENTORY_MAX` names sorted after `after`, or from the first when it is `None`,
        with how many names there are in all and how many come after the last one listed. So a
        listing that stops short says so, and the next one starts from its last name.
        """
        rows, total, more = [], 0, 0
        try:
            held = self.held()
            total = len(held)
            if after is not None:
                held = [row for row in held if row[0] > after]
            more = max(len(held) - INVENTORY_MAX, 0)
            rows = [[name[:REPR_MAX], kind[:REPR_MAX]] for name, kind in held[:INVENTORY_MAX]]
        except Exception as e:
            _diag(f"agent {self.agent!r}: the inventory failed: {e!r}")
        _send(
            {
                "t": "inv",
                "agent": self.agent,
                "id": request_id,
                "globals": rows,
                "total": total,
                "more": more,
            }
        )


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
# agent code triggers synchronously and must not be interrupted is a protocol line on its way out,
# which `Endpoint.send` writes, and the fork hooks. This program's output routing, by contrast, is
# deliberately absent: a runaway that prints spends most of its time there, and must still be
# interruptible.
_MACHINERY = frozenset(
    fn.__code__
    for fn in (
        asyncio.base_events.BaseEventLoop._run_once,
        asyncio.events.Handle._run,
        _write_line,
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


def _msg(kernel, request_id, message):
    """Queue a message from the user on one of the agent's channels, and say how many then wait.

    Always answered, since the host waits for the answer. Everything that can fail before the
    message is queued is retried whole by `_handle` when memory ran out; nothing after it is, so a
    message is never queued twice.
    """
    channel = message.get("channel")
    endpoint = kernel.channels.get(channel) if isinstance(channel, str) else None
    try:
        if endpoint is None:
            raise _Refused(f"agent {kernel.agent!r} has no channel {channel!r}")
        pending = endpoint._deliver(message.get("body"), "user")
    except _Refused as e:
        _send({"t": "msg", "agent": kernel.agent, "id": request_id, "error": str(e)})
        return
    # Queued: from here nothing may reach `_handle`, whose retry would queue it again, so the
    # guards allocate nothing of their own.
    try:
        _send({"t": "msg", "agent": kernel.agent, "id": request_id, "pending": pending})
    except BaseException as e:
        try:
            _diag(f"message {request_id!r} was queued, but saying so failed: {e!r}")
        except BaseException:
            pass


def _received(kernel, _, message):
    """The user received a message the agent sent on a channel, which makes room for another.

    Running out of memory before the room is made reaches `_handle`, which tries again on the
    reserve: the host says so once, and room lost here would be lost for good.
    """
    channel = message.get("channel")
    endpoint = kernel.channels.get(channel) if isinstance(channel, str) else None
    if endpoint is None:
        raise ValueError(f"agent {kernel.agent!r} has no channel {channel!r}")
    endpoint._received()


def _turn(kernel, turn_id, message):
    """Hold a turn of the agent's conversation, which the host committed, for `runtime.history`.

    Not answered. The turn is built whole before anything changes, and `History._add` rebinds last,
    so `_handle`'s retry after running out of memory never holds it twice.
    """
    number, prompt, text, calls = (message.get(key) for key in ("round", "prompt", "text", "calls"))
    if (
        type(number) is not int
        or not isinstance(prompt, (str, type(None)))
        or not isinstance(text, str)
        or not isinstance(calls, list)
        or not all(
            isinstance(call, dict)
            and isinstance(call.get("source"), str)
            and isinstance(call.get("result"), str)
            for call in calls
        )
    ):
        raise ValueError(f"turn {turn_id} for {kernel.agent!r} is not a turn")
    turn = Turn(
        turn_id,
        number,
        prompt,
        text,
        tuple(Call(call["source"], call["result"]) for call in calls),
    )
    kernel.history._add(turn)


def _inventory(kernel, request_id, message):
    """List what the agent holds, on its loop, from past the name `after` when that is given."""
    after = message.get("after")
    kernel.loop.call_soon_threadsafe(
        kernel.inventory, request_id, after if isinstance(after, str) else None
    )


def _pending(kernel, request_id, _):
    """Say how many messages wait on each of the agent's channels, and how many have ever been
    delivered to it, reading none of them."""
    channels = {name: endpoint._counts() for name, endpoint in kernel.channels.items()}
    _send({"t": "pending", "agent": kernel.agent, "id": request_id, "channels": channels})


# What each message addressed to an agent does, with the id it carries. `inv` is answered on the
# agent's loop; the rest are handled here, on the reader thread.
_ROUTES = {
    "exec": _exec,
    "msg": _msg,
    "pending": _pending,
    "received": _received,
    "turn": _turn,
    "inv": _inventory,
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


def _handle(line):
    """Route one protocol line, or say why it could not be."""
    if not line.strip():
        return
    try:
        _with_room(_route, line)
    except Exception as e:
        _diag(f"ignored a message: {e}")


def _read():
    """Read the host's messages off the protocol descriptor and route them by agent id.

    A thread rather than any kernel's loop: a loop running synchronous code cannot read, and
    this one keeps reading whatever the kernels are doing.
    """
    try:
        with os.fdopen(_PROTO_IN, "rb") as stream:
            while True:
                try:
                    line = stream.readline()
                    if not line:
                        return
                    _handle(line)
                except MemoryError:
                    # Agent code holds the memory. A line that failed to read whole within the
                    # stream's buffer is read again once the reserve is given back, and waited on
                    # if even that is not enough; one that had been read is lost, which costs less
                    # than ending the session over it.
                    _give_reserve()
                    time.sleep(0.01)
    finally:
        # The host is gone. Exiting here rather than on a kernel's loop means a loop that has
        # stopped turning cannot keep the process alive after its session has ended.
        os._exit(0)


def main():
    if len(sys.argv) != 2 or not _valid_agent(sys.argv[1]):
        _diag(f"usage: python3 -I -c <program> <agent-id>; got {sys.argv[1:]!r}")
        os._exit(2)
    global _primary
    # Before any agent exists, and every process it starts inherits it.
    _set_ceiling()
    _open_imports()
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
