"""The interpreter: one process per session, hosting one kernel per agent.

Started by `podman exec -i` inside the session container, with OutRig's package directory as its
argument -- and RPyC's after it, for a session with bindings -- and spoken to in NDJSON: requests
on stdin, replies on stdout, every message about an agent naming the agent it concerns. It starts
with no agent, greets, and builds one only when the host opens it. Each agent is a **kernel** --
a session module, an event loop on a thread of its own, an execution slot, a bounded backlog of
background output, and the endpoints of its channels, bound as `runtime.channels`. The process
owns what they share: the protocol descriptors, the reader thread that routes messages by agent
id, and the descriptors executed code writes to. The kernel the host opens onto the main thread
-- the primary agent's, in a session that has one -- runs there, because that is the only thread a
signal handler runs on.

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
  holds the slot. A thread carries the context it was started in, and an item a thread pool runs
  the one it was submitted from. Each execution has a pipe of its own for the same reason: a
  child process's bytes carry no writer identity, so the descriptor it inherited is the only
  attribution there is.
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
import base64
import builtins
import codecs
import collections
import concurrent.futures.thread
import contextlib
import contextvars
import copy
import dataclasses
import datetime
import functools
import importlib
import inspect
import io
import itertools
import json
import keyword
import mmap
import multiprocessing.pool
import operator
import os
import queue
import resource
import select
import shlex
import shutil
import signal
import stat
import struct
import subprocess
import sys
import sysconfig
import threading
import time
import traceback
import types
import typing
import weakref

# ---------------------------------------------------------------------------- limits

OUTPUT_MAX = 16 * 1024  # bytes of one execution's own output
BG_MAX = 2 * 1024  # bytes of other executions' output held for the next result, tail kept
REPR_MAX = 1000  # one echoed value, or one inventory entry
INVENTORY_MAX = 200  # global names listed in one inventory
TASKS_MAX = 50  # running tasks named in one listing
TASK_NAME_MAX = 200  # characters of one task's name
HELP_MAX = 8 * 1024  # characters of one `help()` answer
DRAIN_TIMEOUT = 5.0  # seconds a result waits for its pipe to reach the end of the body's output

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
# dispatcher below -- `os.write(1, ...)`, a thread started with `_thread` rather than
# `threading`, a pool's `initializer` -- cannot be billed to any agent, and the host records it
# instead of dropping it.
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
# execution creates carries it, and so does `asyncio.to_thread`, which copies the context. So do
# a thread it starts and an item it gives a thread pool, through the patches after the fork hooks.
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


# libc's `system()` ignores SIGINT for the whole process while it waits, as POSIX asks of it, so an
# interrupt the host sends then is discarded -- for every agent, since the disposition is
# process-wide. It also spawns without `Popen`, so the patch above never sees it. Through
# `subprocess.run` it ends with `KeyboardInterrupt` like any other child -- the direct child
# killed, what that child started not -- and its output is billed to its execution.
@functools.wraps(os.system)
def _system(command):
    # `fsencode` because `os.system` takes a path-like, which `run` refuses with `shell=True`.
    # `close_fds=False` because libc's `system()` passes on every descriptor agent code marked
    # inheritable, and `run` would close them. None of this program's own is inheritable.
    code = subprocess.run(os.fsencode(command), shell=True, close_fds=False).returncode
    # The wait status `os.system` returns: the exit code in the high byte, or the signal's number
    # where `returncode` is its negation.
    return code << 8 if code >= 0 else -code


# `wraps` copied the builtin's `posix`, which still holds the builtin: a pool pickles a function by
# the module it names, and finds it there.
_system.__module__ = os.__name__
os.system = _system

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

# A thread runs in a copy of the context it was started in, and so is billed to the execution
# that started it. This is Python 3.14's `thread_inherit_context`, except that the report of an
# exception the thread did not catch is made in that context too, rather than lost to stderr, and
# that a thread a pool starts for itself is left out, as `_pools_own` says. A thread started with
# `_thread.start_new_thread`, which bypasses `Thread.start`, carries none.
_thread_start = threading.Thread.start
_thread_bootstrap_inner = threading.Thread._bootstrap_inner
# 3.14 runs a thread's `run` inside `Thread._context`, which `start` fills by the flag's setting.
_THREAD_RUNS_IN_CONTEXT = sys.version_info >= (3, 14)

# The modules whose pools start threads for themselves: `ThreadPoolExecutor`'s workers, and a
# `multiprocessing.pool`'s workers and the threads that tend its queues and run its callbacks.
_POOL_MODULES = frozenset({"concurrent.futures.thread", "multiprocessing.pool"})


def _launched(thread):
    """Whether `thread`'s system thread exists: in `_limbo` until it takes its identity, then in
    `_active` until it ends, read under the lock `threading` keeps them with."""
    with threading._active_limbo_lock:
        return thread in threading._limbo or thread._ident in threading._active


def _pools_own(thread):
    """Whether `thread` is one a standard library pool starts for itself.

    A pool's worker is not the work it runs. Started for the first submission that needed it, it
    serves every later one, and the context it was started in would bill all of their work to the
    first -- which is what 3.14's flag, copying per thread, does to a pool. So a pool's own thread
    starts with no execution, and each item runs in the context it was submitted from, below. A
    thread is known by its target's module, read only when the target is a plain function or a
    method of one, so no callable the agent wrote is asked anything.
    """
    target = getattr(thread, "_target", None)
    if type(target) is types.MethodType:
        target = target.__func__
    return type(target) is types.FunctionType and target.__module__ in _POOL_MODULES


@functools.wraps(_thread_start)
def _attributed_thread_start(self):
    own = _pools_own(self)
    if not own:
        self._outrig_context = contextvars.copy_context()
    if _THREAD_RUNS_IN_CONTEXT and self._context is None:
        # Set either way, since the flag is on by default in a free-threaded build: an empty one
        # for a pool's thread, and for any other a copy of its own, because a context can be
        # entered by one frame at a time and the bootstrap runs inside the other.
        self._context = contextvars.Context() if own else contextvars.copy_context()
    try:
        _thread_start(self)
    except BaseException:
        # `start` raises before the thread exists -- the system refused one, or it was started
        # twice -- or after, interrupted while waiting for it to come up. Only a thread that runs
        # takes its context off, so one that never will is relieved of it here.
        if not _launched(self):
            self.__dict__.pop("_outrig_context", None)
        raise


@functools.wraps(_thread_bootstrap_inner)
def _attributed_bootstrap_inner(self):
    # Taken off the thread, so a `Thread` kept after its thread has ended holds no execution.
    context = self.__dict__.pop("_outrig_context", None)
    if context is None:
        _thread_bootstrap_inner(self)
    else:
        context.run(_thread_bootstrap_inner, self)


threading.Thread.start = _attributed_thread_start
threading.Thread._bootstrap_inner = _attributed_bootstrap_inner


def _submitted(fn):
    """`fn`, run in a copy of the context it was handed over in. `None` stays `None`.

    A new copy for each call, because `map` runs one function on several workers at once, and a
    context can be entered by only one thread at a time.
    """
    if fn is None:
        return None
    context = contextvars.copy_context()

    def submitted(*args, **kwargs):
        return context.copy().run(fn, *args, **kwargs)

    return submitted


# `ThreadPoolExecutor` is what `loop.run_in_executor` submits to by default, and
# `asyncio.to_thread` through that. Its work item is made inside `submit`, on the submitting
# thread, so the item carries the context it runs in, and what the pool was given is left as it
# was: 3.14's `InterpreterPoolExecutor` hands it to another interpreter, where a closure cannot go.
_WorkItem = concurrent.futures.thread._WorkItem
_work_item_init = _WorkItem.__init__
_work_item_run = _WorkItem.run


@functools.wraps(_work_item_init)
def _attributed_work_item_init(self, *args, **kwargs):
    _work_item_init(self, *args, **kwargs)
    self.context = contextvars.copy_context()


@functools.wraps(_work_item_run)
def _attributed_work_item_run(self, *args):
    return self.context.run(_work_item_run, self, *args)


_WorkItem.__init__ = _attributed_work_item_init
_WorkItem.run = _attributed_work_item_run


def _attributing(method):
    """`method`, with the function it is given -- its first argument, or `func` -- run as
    `_submitted` runs it."""

    @functools.wraps(method)
    def attributing(self, *args, **kwargs):
        if args:
            args = (_submitted(args[0]), *args[1:])
        elif "func" in kwargs:
            kwargs["func"] = _submitted(kwargs["func"])
        return method(self, *args, **kwargs)

    return attributing


# A `concurrent.futures` future's callbacks, a process pool's included, run in the context they
# were added in, as an asyncio future's do: they run on whichever thread finishes the work, which
# is a pool's own, or one a process pool started for whoever made it.
_LOGGER = concurrent.futures._base.LOGGER
_add_done_callback = concurrent.futures.Future.add_done_callback


def _logged(fn, future):
    """`fn(future)` as `Future` calls a done callback: an exception is logged, and not raised."""
    try:
        fn(future)
    except Exception:
        _LOGGER.exception("exception calling callback for %r", future)


def _attributed_done_callback(fn):
    """`fn`, run once in a copy of the context it was added in, and reported there if it raises.

    `Future` logs a callback's exception itself, but only once the callback's context has been
    left for the work's, so the report would be billed to whoever submitted the work. Logged here
    instead, and nothing reaches `Future`. The context is let go of once the callback has run: a
    future keeps its callbacks for as long as it is kept, and a callback kept by nothing else
    should not keep the execution that added it.
    """
    context = contextvars.copy_context()

    def callback(future):
        nonlocal context
        run, context = context, None
        if run is None:
            # Called again, which `Future` never does: in whatever context there is.
            run = contextvars.copy_context()
        run.run(_logged, fn, future)

    return callback


@functools.wraps(_add_done_callback)
def _attributed_add_done_callback(self, fn):
    _add_done_callback(self, _attributed_done_callback(fn))


concurrent.futures.Future.add_done_callback = _attributed_add_done_callback

# What a `multiprocessing.pool.ThreadPool` is given reaches a worker through these four methods,
# which `apply`, `map`, `starmap` and the rest call. `ThreadPool` alone: a process `Pool` pickles
# what it is given, and a context cannot be.
_ThreadPool = multiprocessing.pool.ThreadPool
_ThreadPool.apply_async = _attributing(_ThreadPool.apply_async)
_ThreadPool.imap = _attributing(_ThreadPool.imap)
_ThreadPool.imap_unordered = _attributing(_ThreadPool.imap_unordered)
_ThreadPool._map_async = _attributing(_ThreadPool._map_async)

# Callbacks stay in this process whichever pool runs the work, so every pool's, a process pool's
# included, are wrapped where its results are made. A result keeps its callbacks for as long as it
# is kept, so their context is let go of when the result settles, whether or not it ran either:
# its event is set then, after whichever callback it ran, and at once for a `map` of nothing.
_apply_result_init = multiprocessing.pool.ApplyResult.__init__


class _Settled(threading.Event):
    """An `ApplyResult`'s event, set when the result settles: the moment its callbacks' context can
    be let go of."""

    def __init__(self, release):
        super().__init__()
        self.release = release

    def set(self):
        self.release()
        super().set()


def _settling(callback, error_callback):
    """`callback` and `error_callback`, each run in a copy of the context they were given in, and
    the function that lets go of it. `None` stays `None`."""
    context = contextvars.copy_context()

    def release():
        nonlocal context
        context = None

    def attributed(fn):
        if fn is None:
            return None

        def settled(value):
            run = context
            if run is None:
                # After the result settled, which `multiprocessing.pool` never does: in whatever
                # context there is.
                run = contextvars.copy_context()
            return run.run(fn, value)

        return settled

    return attributed(callback), attributed(error_callback), release


@functools.wraps(_apply_result_init)
def _attributed_apply_result_init(self, pool, callback, error_callback):
    callback, error_callback, release = _settling(callback, error_callback)
    _apply_result_init(self, pool, callback, error_callback)
    # Replaced before the work is queued, which the caller does next, so nothing has set the
    # event it replaces.
    self._event = _Settled(release)


multiprocessing.pool.ApplyResult.__init__ = _attributed_apply_result_init


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
# `pip` is the payload's own, and installs into an environment of this interpreter's own: a virtual
# environment laid over the payload, made under `HOME` when the interpreter starts
# (`~/.local/share/outrig/python3.13`), since the payload itself is mounted read-only. No other
# Python in the container reads that directory, and this interpreter reads no other Python's
# installs. The user site, `~/.local/lib/python3.13/site-packages`, would be read by an image Python
# of the same version too, ahead of its own site-packages, so a dependency pip pulled in for this
# interpreter would have replaced the one the image installed for the project. The environment's
# `site-packages` is made here if it is missing: an import that finds no directory at a path
# remembers that, and would not look again when pip later made one there. Its `.pth` files are not
# read, so nothing installed runs when the interpreter starts.
#
# Compiled code never loads. This is a static build, with no way to load a shared object, so a
# compiled module fails however it was installed -- as `Dynamic loading not supported` when its
# file carries this interpreter's suffix, and as a bare "No module named" when it was built for
# another. `_explain_imports` says so where a failure is reported, and `_PIP_GUARD` where `pip`'s
# build of a package with compiled parts fails.

# The directory the interpreter started in: the workspace, since the host starts it there.
_WORKSPACE = None
# The directory packages the agent's pip installs land in, once `_open_imports` has made it. A
# binding's package directory goes on `sys.path` ahead of it (`_bindings`).
_SITE = None
# This interpreter's version, as the host is told it and the agent reads it.
_VERSION = sys.version.split()[0]
# Its minor version, which names the environment's directories, as `3.13`.
_MINOR = f"{sys.version_info.major}.{sys.version_info.minor}"


# The `pip` on the agent's `PATH`: this interpreter's pip, run through the environment's interpreter
# link so that it installs there, and started clear of every `PYTHON*` variable. The image sets
# those for its own Python, and one such as `PYTHONHOME` stops this one starting at all. Cleared for
# pip and what it starts in turn, such as a build, and for nothing else the agent runs, so the
# image's Python still gets them. Both Pythons run isolated: the first so the variables cannot stop
# it; the second so that `-m` does not put the working directory -- the workspace -- ahead of
# everything, where a project's `pip.py` would run instead of pip and its `types.py` would stop it
# starting, and so that `--user`, with no user site in view, fails rather than installing where this
# interpreter does not look. The second runs `_PIP_GUARD`, which is pip's own `main` with a note
# on its failures, in place of `-m pip`.
_PIP = """#!/bin/sh
exec {python} -I -c '
import os, sys
env = {{k: v for k, v in os.environ.items() if not k.startswith("PYTHON")}}
os.execve(sys.executable, [sys.executable, "-I", *sys.argv[1:]], env)
' {guard} "$@"
"""

# What the agent's `pip` runs: this interpreter's pip, saying why when a package this interpreter
# cannot load is the reason pip failed. `_make_environment` puts two lines ahead of it,
# `NO_COMPILED` and `ROUTE`: the reason, and where such code can run.
#
# pip decides which wheels fit from the tags its interpreter supports, and this one has no glibc or
# musl loader to read, so it supports only `linux_<arch>`. No package with compiled parts has a
# wheel for that, so pip falls back to its source distribution, and the build fails on a dependency
# or makes a module that cannot load. What pip rejects on the way is evidence: a project whose
# index page lists wheels for a version, none of them `*-none-any`, has compiled parts. Only
# evidence, never grounds to refuse. MarkupSafe lists such wheels and its source builds to pure
# Python, so `pip install jinja2` works because pip builds.
#
# Four pieces of pip are wrapped. Each runs pip's own function first, and only what it observes is
# guarded, so the guard cannot change what pip does. The evaluation of each link records the
# evidence. Reading a source distribution's metadata and building its wheel, where pip starts its
# build backend and the two places a build can fail, remember that one did; a failure before them,
# such as a hash that does not match, is not a failed build. The error pip raises when nothing
# fits says why for a package whose compiled wheels are all there is of the versions asked for, by
# the requirement and by a `-c` constraint, where nothing could be built. Where `--only-binary`
# leaves out a source distribution that pip would otherwise take, pip could have built it, and
# whether that works is not known: the error says what the flag did and nothing of where compiled
# code can run. The note for the rest is written after pip has exited with an error, as the last
# of its output.
#
# The supported tags are left alone: widening them would make pip prefer a compiled wheel to a pure
# fallback wheel of the same version, as `charset-normalizer` publishes. The rule for a pure
# wheel -- every tag is `*-none-any` -- is the one `check_pure` in `install.rs` applies to a
# binding's installed packages.
#
# The seams are pip's own and move with its version. If one is gone when pip starts the guard
# stands down, and pip runs as it would have: a pip that changed costs the note, not the install.
# Only the commands that reach the seams get the guard, since importing them is most of what it
# costs.
_PIP_GUARD = r'''
import copy
import sys


def guard():
    from pip._internal import wheel_builder
    from pip._internal.exceptions import DistributionNotFound
    from pip._internal.distributions.sdist import SourceDistribution
    from pip._internal.index.package_finder import LinkEvaluator, LinkType
    from pip._internal.models.candidate import InstallationCandidate
    from pip._internal.resolution.resolvelib.factory import Factory
    from pip._vendor.packaging.utils import canonicalize_name
    from pip._vendor.packaging.version import Version

    evaluate_link = LinkEvaluator.evaluate_link
    prepare_distribution_metadata = SourceDistribution.prepare_distribution_metadata
    build_one = wheel_builder._build_one
    get_installation_error = Factory.get_installation_error

    # project -> the versions, as their wheels' file names write them, that have a wheel this pip
    # cannot match that is not pure Python
    compiled = {}
    # project -> the versions, as pip reads them, that have a candidate pip could install
    usable = {}
    # project -> the source distributions that would be candidates but for the format control,
    # which `--only-binary` sets to allow no sources, as pip makes a candidate of a link. A list of
    # sightings, not a set: a `Link` compares and hashes by its URL and ignores whether it is
    # yanked, so a file a directory holds and a page in it marks yanked would be one element, and
    # the one kept would be the page's, which pip reads first. pip takes the one not yanked.
    left_out = {}
    # a source distribution's URL -> (project as named, version as written)
    sources = {}
    # the source distributions with compiled wheels that pip could not prepare or build, in order
    failed = {}

    def why(project, rest):
        return f"{project} has compiled parts, and {NO_COMPILED}: {rest} {ROUTE}"

    def evaluate(self, link):
        result = evaluate_link(self, link)
        try:
            kind, detail = result
            name = canonicalize_name(self.project_name)
            if kind is LinkType.candidate:
                usable.setdefault(name, set()).add(detail)
                if not link.is_wheel:
                    sources[link.url_without_fragment] = (self.project_name, detail)
            elif kind is LinkType.platform_mismatch and link.is_wheel:
                if not link.filename.endswith("-none-any.whl"):
                    compiled.setdefault(name, set()).add(link.filename.split("-", 2)[1])
            elif kind is LinkType.format_unsupported and "source" not in self._formats:
                # Not every link rejected here is one the format control cost: an egg, an old
                # macOS archive, and any file pip cannot use are unsupported for what they are,
                # and a source distribution can still be rejected for the Python it needs. Ask
                # again as if sources were allowed, and take only what then fits, with the
                # version pip reads. pip logs such a link as found, at debug level, as well as
                # skipped. Being a candidate is not being taken: pip chooses among candidates by
                # its release policy and passes over a yanked one, which `taken` asks of pip.
                allowing = copy.copy(self)
                allowing._formats = self._formats | {"source"}
                fit, version = evaluate_link(allowing, link)
                if fit is LinkType.candidate:
                    candidate = InstallationCandidate(self.project_name, version, link)
                    left_out.setdefault(name, []).append(candidate)
        except Exception:
            pass
        return result

    def remember(req):
        try:
            link = req.original_link or req.link
            source = sources.get(link.url_without_fragment)
            if source is not None:
                project, text = source
                versions = compiled.get(canonicalize_name(project), ())
                if Version(text) in {Version(version) for version in versions}:
                    failed[source] = None
        except Exception:
            pass

    def prepare(self, *args, **kwargs):
        try:
            return prepare_distribution_metadata(self, *args, **kwargs)
        except Exception:
            remember(self.req)
            raise

    def build(req, *args, **kwargs):
        wheel = build_one(req, *args, **kwargs)
        if wheel is None:
            remember(req)
        return wheel

    def fits(specifier, versions):
        return any(specifier.contains(Version(version), prereleases=True) for version in versions)

    def taken(finder, specifier, name):
        """Whether pip would take a source distribution of the project that `--only-binary` left
        out, were the option not given. pip's own candidate evaluator says which of them its
        release policy applies, for the specifier: a release candidate while a final release
        fits, and any a `--pre` or `--only-final` decides. Then its resolver passes over a yanked
        release unless the specifier pins a version and every release left is yanked (PEP 592).
        Nothing that fits is usable where this is asked, so those left out are all there would
        be. Hashes are not asked of pip, so a source whose hash does not match still counts."""
        evaluator = finder.make_candidate_evaluator(name, specifier=specifier)
        applicable = evaluator.get_applicable_candidates(left_out.get(name, []))
        yanked = [candidate.link.is_yanked for candidate in applicable]
        pinned = any(
            s.operator == "===" or (s.operator == "==" and not s.version.endswith(".*"))
            for s in specifier
        )
        return bool(yanked) and (not all(yanked) or pinned)

    def stuck(finder, cause, constraints):
        """The project of a requirement whose versions that fit are all compiled, if it is one,
        and whether pip would have taken a source distribution of such a version that
        `--only-binary` left out. The versions that fit are those the requirement and the
        project's `-c` constraint allow."""
        try:
            requirement = cause.requirement
            specifier = requirement.get_candidate_lookup()[1].req.specifier
            name = requirement.project_name
            if name in constraints:
                specifier &= constraints[name].specifier
            if fits(specifier, compiled.get(name, ())):
                if not fits(specifier, usable.get(name, ())):
                    return name, taken(finder, specifier, name)
        except Exception:
            pass

    def installation_error(self, e, *args, **kwargs):
        error = get_installation_error(self, e, *args, **kwargs)
        try:
            constraints = kwargs["constraints"] if "constraints" in kwargs else args[0]
            finder = self._finder
            found = sorted(filter(None, (stuck(finder, cause, constraints) for cause in e.causes)))
            if found and isinstance(error, DistributionNotFound):
                project, omitted = found[0]
                if omitted:
                    note = (
                        f"{project} has wheels for compiled code that this interpreter does not "
                        "match, and `--only-binary` leaves out its source distribution. Without "
                        "that option pip can build the source distribution, which is usable here "
                        f"only if it builds to pure Python, since {NO_COMPILED}."
                    )
                else:
                    note = why(
                        project,
                        "the versions asked for are published only as wheels for platforms this "
                        "interpreter does not match.",
                    )
                error = DistributionNotFound(f"{error}\n{note}")
        except Exception:
            pass
        return error

    def report(status):
        if status:
            for project, text in failed:
                rest = (
                    "its wheels are all for platforms this interpreter does not match, so pip "
                    "had to build its source distribution, and a package with compiled parts "
                    "will not import here however it is built."
                )
                sys.stderr.write(f"note: {why(f'{project} {text}', rest)}\n")

    LinkEvaluator.evaluate_link = evaluate
    SourceDistribution.prepare_distribution_metadata = prepare
    wheel_builder._build_one = build
    Factory.get_installation_error = installation_error
    return report


report = None
if {"install", "download", "wheel"} & set(sys.argv[1:]):
    try:
        report = guard()
    except Exception:
        pass
from pip._internal.cli.main import main

sys.argv[0] = "pip"
status = main()
if report is not None:
    report(status)
sys.exit(status)
'''

# The environment's interpreter link. Its `bin/` goes on `PATH` for the scripts pip installs there,
# so the name is one no image provides: `python3.13` would shadow the image's own Python of that
# version.
_LINK = "outrig-python"


def _replace(path, make):
    """Put the entry `make` creates at `path`, whole or not at all: made under a name beside it and
    renamed over it, so an interpreter starting at the same time under the same `HOME` never finds
    it missing or half made."""
    made = os.path.join(os.path.dirname(path), f".{os.path.basename(path)}.{os.getpid()}")
    make(made)
    os.replace(made, path)


def _write(path, data, mode):
    """Write `data` to a new file at `path` with `mode`."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    try:
        _write_all(fd, data)
    finally:
        os.close(fd)


def _guard_source():
    """`_PIP_GUARD`, ahead of which go the reason its refusals give and where such code can run."""
    return f"NO_COMPILED = {_NO_COMPILED!r}\nROUTE = {_elsewhere(pip=True)!r}\n{_PIP_GUARD}"


def _make_environment():
    """Lay this interpreter's environment over the payload, under `~/.local/share/outrig`, with
    `pip` beside it. Returns its interpreter link, its `site-packages`, and the directories for
    `PATH` in order: `pip`'s own, then the environment's `bin/`, where the scripts pip installs
    land. Every part is made or replaced whole, so interpreters starting together under one `HOME`
    all find it complete. A process run through the link sees this environment and the payload,
    and no user site."""
    share = os.path.expanduser("~/.local/share/outrig")
    root = os.path.join(share, f"python{_MINOR}")
    bin_dir = os.path.join(root, "bin")
    site_packages = os.path.join(root, "lib", f"python{_MINOR}", "site-packages")
    pip = os.path.join(share, "bin")
    for directory in (bin_dir, site_packages, pip):
        os.makedirs(directory, exist_ok=True)
    # The system site stays out, which also keeps the user site out of every process run through
    # the link, `sys.executable -m pip` included: pip would otherwise count a dependency an image
    # Python of this version installed for the user as met, and this interpreter cannot import it.
    # The payload's own site-packages, where pip lives, comes in through a `.pth` file instead.
    config = (
        f"home = {os.path.dirname(sys.executable)}\n"
        "include-system-site-packages = false\n"
        f"version = {_VERSION}\n"
    )
    _replace(os.path.join(root, "pyvenv.cfg"), lambda made: _write(made, config.encode(), 0o644))
    payload = f"{sysconfig.get_path('purelib')}\n".encode()
    _replace(os.path.join(site_packages, "outrig.pth"), lambda made: _write(made, payload, 0o644))
    link = os.path.join(bin_dir, _LINK)
    _replace(link, lambda made: os.symlink(sys.executable, made))
    guard = os.path.join(share, "pip-guard.py")
    _replace(guard, lambda made: _write(made, _guard_source().encode(), 0o644))
    script = _PIP.format(python=shlex.quote(link), guard=shlex.quote(guard)).encode()
    for name in ("pip", "pip3", f"pip{_MINOR}"):
        _replace(os.path.join(pip, name), lambda made: _write(made, script, 0o755))
    return link, site_packages, [pip, bin_dir]


def _open_imports():
    """Put the workspace and pip's install location on `sys.path`, and pip on `PATH`.

    `pip` goes first on `PATH`, from a directory of its own, so it means this interpreter's pip
    while `python3` still means the image's own Python; the environment's `bin/`, where the scripts
    pip installs land, follows it. Every process the interpreter starts inherits the `PATH`, and
    nothing else in the container sees it.

    That `pip` is `_PIP`, not the payload's own script, which would start this Python in whatever
    `PYTHON*` variables the image sets for its own. `sys.executable` becomes the environment's
    link, so `sys.executable -m pip` installs there as well, and a process started from it -- a
    `multiprocessing` child, a subprocess -- imports the same packages.
    """
    global _WORKSPACE, _SITE
    try:
        _WORKSPACE = os.getcwd()
        sys.path.append(_WORKSPACE)
    except OSError as e:
        _diag(f"the working directory is not importable: {e!r}")
    try:
        link, site_packages, bins = _make_environment()
    except OSError as e:
        _diag(f"packages pip installs will not import: the environment could not be made: {e!r}")
        return
    sys.path.append(site_packages)
    _SITE = site_packages
    sys.executable = link
    os.environ["PATH"] = os.pathsep.join([*bins, os.environ.get("PATH", os.defpath)])


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


def _elsewhere(pip=False):
    """Where code that needs compiled packages can run, as a sentence; with `pip`, and the image's
    own Python found, a second sentence on how to install for it."""
    found = _image_python()
    python = f"the image's own Python, {found}," if found else "a Python the image provides"
    text = (
        f"Code that needs a compiled package can run with {python} through subprocess, or as a "
        f"service."
    )
    if pip and found:
        text += f" `{found} -m pip` installs packages for it."
    return text


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


def _compiled_cause(exc):
    """The module a failed import could not load, and the compiled file found where it looked, or
    `None` when nothing says compiled code is why: a compiled file found and not loaded, or one
    beside a module that was not found -- including `from p import m`, which reports `m` as a name
    `p` lacks rather than as a module. Only `ImportError` and `ModuleNotFoundError` themselves are
    read, never a subclass, so no agent code runs."""
    kind = type(exc)
    if kind is not ImportError and kind is not ModuleNotFoundError:
        return None
    name = exc.name
    if type(name) is not str:
        return None
    if kind is ModuleNotFoundError:
        compiled = _compiled_file(name)
        return None if compiled is None else (name, compiled)
    path = exc.path
    if type(path) is str and path.endswith(_COMPILED_SUFFIXES):
        return name, path
    name_from = exc.name_from
    if type(name_from) is not str:
        return None
    module = f"{name}.{name_from}"
    compiled = _compiled_file(module)
    return None if compiled is None else (module, compiled)


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
    found = _compiled_cause(exc)
    if found is None and _skills is not None:
        # A skill's files are in its listing, not on disk.
        found = _skills._compiled_in_skill(exc)
    if found is not None:
        module, compiled = found
        text = (
            f"cannot import {module!r}: {compiled} is compiled code, and {_NO_COMPILED}, however "
            f"it is installed. {_elsewhere()}"
        )
    elif kind is ModuleNotFoundError and "." not in name and name not in sys.stdlib_module_names:
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
_traceback_of = BaseException.__dict__["__traceback__"].__get__
_set_traceback = BaseException.__dict__["__traceback__"].__set__


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
      is the interpreter's own: it installs into an environment of this interpreter's under your
      home, which is on `sys.path` and which no other Python in the container reads, and puts a
      package's scripts on `PATH`. `sys.executable -m pip` installs there too, without the note on
      a compiled package below. `pip install --target DIR` works as well, once `DIR` is appended
      to `sys.path`. What the image's own Python has installed, or installs with `python3 -m pip`,
      is not on this interpreter's `sys.path`.
    - Modules in the workspace -- the directory the interpreter started in, `workspace` below --
      import too. It comes after the standard library on `sys.path`, so a file of yours named like
      a standard module does not replace it.
    - Compiled code never loads: this is a static build, with no way to load a shared object. A
      package with compiled parts -- numpy, pandas, pydantic, lxml, cryptography -- does not
      import however it is installed, and `ctypes` imports but cannot load a library. `pip` finds
      no wheel of such a package that fits this interpreter, so it builds the source distribution,
      which fails or makes a module that cannot load; when it fails, `pip` says why at the end of
      its output. A package that lists compiled wheels and builds to pure Python from source, as
      MarkupSafe does, installs and imports.
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

        # A binding's name stands for the binding's root, which is what is described.
        request = [
            object.__getattribute__(x, "_pool").root() if type(x) is HostedRoot else x
            for x in request
        ]
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


# ---------------------------------------------------------------------------- hosted objects

# A binding is a library running in a process on the host, which agent code reaches through RPyC
# proxies: the name bound for a binding is a `HostedRoot`, standing for the proxy of its root, and
# each operation on one is a synchronous request, which blocks the thread making it until the host
# answers. Its frames travel as `rpc` lines of this protocol, tagged by agent, binding and
# connection, and the host relays them to the binding's process: nothing here listens on a
# socket. The reader thread routes each part of a frame to its connection's channel, and the
# thread waiting for a reply waits on the condition variable the channel shares with its pool, so
# it never reads the protocol itself and releases the GIL while it waits. A kernel holds a pool
# of up to `POOL_MAX` connections per binding, one call in flight on each, so a call that blocks
# for an hour blocks its thread and no other; the reader thread can wake a waiting thread with
# an interrupt through the same condition variable, on any kernel.
#
# Nothing on this side is enforcement. Agent code runs in this process and can write frames of its
# own, so every check that matters is the binding's, on the host. What this side does is make the
# ordinary case work: it copies a `list`, `dict`, `set` or path into a by-value form the host
# rebuilds as a builtin, sends a subclass of a by-value type as its base value, marks a callable so
# the host can call it back while the request runs, and refuses -- before anything is sent -- a
# coroutine function, an object that could only cross as a reference into this process, or a proxy
# that belongs to another connection. The frame channel is a copy of the binding program's; keep
# them the same.

PART_MAX = 512 * 1024  # bytes of frame data in one `rpc` line
FRAME_MAX = 128 << 20  # bytes of data in one RPyC frame, either direction
BUFFER_MAX = 2 * FRAME_MAX  # bytes one connection holds unread, the frame in progress included
FRAME_OVERHEAD = 64  # bytes charged for each frame held, for the object that holds it

# RPyC's own frame header: a 4-byte length and a 1-byte compressed flag, then the data and a
# newline. Kept on the wire as RPyC's `Channel` writes it, so a compressed frame is refused by a
# direct check rather than inferred.
FRAME_HEADER = struct.Struct("!LB")
FLUSHER = b"\n"

# Boxing labels of this transport's own, beside RPyC's four: the shim's copies of the builtin
# containers, and a callable marked for the host to call back.
LABEL_LIST = 101
LABEL_DICT = 102
LABEL_SET = 103
LABEL_FROZENSET = 104
LABEL_CALLABLE = 105

# `outrig.schema`, from OutRig's own package, which `main` imports from the directory it is given:
# what checks a channel's contract, and turns a message into what crosses.
_schema = None
# `outrig.skills`, from the same package: what serves `outrig_skills`, given the catalog.
_skills = None
# The vendored RPyC, imported by `main` from the directory it is given, and the connection class
# built over it; `None` when the interpreter was started without one.
_rpyc = None
_Hosted = None
_rpyc_missing = "this interpreter was started without RPyC, so it has no hosted objects"
# The reader thread, which must never wait for a reply it alone delivers.
_reader_thread = None
# Every open connection's channel, for the reader thread: (agent, binding, number) -> channel.
_rpc_channels = {}
# A kernel holds at most this many connections to one binding, one call in flight on each.
POOL_MAX = 4
# Seconds between a waiting hosted call's looks, on the main thread, for an interrupt its signal
# left: the handler takes no lock, so it cannot wake the wait.
SIGNAL_CHECK = 0.1
# The hosted call each thread is inside, by thread ident -- the innermost one, which is the one
# the reader thread wakes. Installed and removed under the pool's condition variable.
_waits = {}
# The operations recorded only when refused; every other one is a boundary request.
_BOOKKEEPING = frozenset(("ping", "close", "getroot", "inspect", "pickle", "del"))
# The session's closing error, once the host has said it closed to new work: a boundary call
# from then on is refused here, raising `outrig.Closed`.
_closing = None
# Why a coroutine function is refused as a callback, before the call it is passed to is sent, and
# a coroutine a plain callback returns, when the host calls it.
_COROUTINE_CALLBACK = (
    "a coroutine function cannot be a hosted object's callback: the host calls a callback as a "
    "plain function and never awaits what it returns. Pass a plain function."
)
# What a hosted call runs through here, by the first part of the qualified name of its code: the
# stub, the pool, the waits, the channel and the connection class. A traceback agent code is shown
# leaves out their frames, and RPyC's.
_HOSTED_CODE = frozenset(
    ("HostedRoot", "_on_root", "_Pool", "_Wait", "FrameChannel", "_connection_class")
)
# The hosted code that runs an object's own code, where an interrupt may raise: RPyC's request
# handlers, beneath which a callback's code runs and whose exception is the answer, and the boxing
# of an argument or a callback's result -- `_box`, and RPyC's `get_id_pack` it calls -- which can
# run an object's `__fspath__`, `__class__` or `__getattr__`. An argument's exception comes before
# any of its request is written, and a result's is the answer. So is rebuilding the exception a
# reply carries, in RPyC's `vinegar.load` and `_get_exception_class`, which import the module the
# host names and run the class's `__new__`, `__init_subclass__` and attribute setters: `_dispatch`
# has taken the reply from its channel and popped its request before either runs, so an interrupt
# there ends the call in the reply's place. So is boxing an exception for the host, in
# `vinegar.dump`, which reads its public attributes: the one a callback raised, where the answer is
# an error that says it could not be sent (`_box_callback_error`), and the one a proxy's `__exit__`
# sends, which comes before its request is written. An interrupt may land anywhere in `dump`, since
# RPyC's frames are not machinery, so no answer the host waits for is boxed through it unguarded:
# the refusal of a callback it calls late is built without it (`_inert_error`). The frames that
# decode the reply -- `brine.load`, `_dispatch` -- are not on this list, and an interrupt landing
# in them is left for the call.
_INTERRUPTIBLE_CODE = frozenset()


def _say(lines):
    """Send each of `lines` that is one, giving up on any that cannot be written: the host learns
    of the call from its other lines then."""
    for line in lines:
        if line is not None:
            with contextlib.suppress(Exception):
                _send(line)


def _inert_error(text):
    """A `RuntimeError` of `text` as an answer to the host, boxed as RPyC's `vinegar.dump` boxes one
    but without calling it: `dump` reads an exception's attributes, which is code an interrupt may
    raise in, and an answer the host waits for is never to be lost to one."""
    return (("builtins", "RuntimeError"), (text,), (), "<traceback not sent>")


def _named_as_host(exc, raw):
    """`exc`, which RPyC's `vinegar.load` rebuilt from `raw`, with its class named as the host's.

    `load` returns a subclass of the class `raw` names that has the base's `__name__` and
    `__module__` but not its `__qualname__`, and `traceback` prints a type by those two: the error
    that ended an execution read `_get_exception_class.<locals>.Derived`. The subclass takes the
    base's `__qualname__`, and where the base is RPyC's generic class, whose module reads
    `rpyc.core.vinegar/<module>`, the host's module and class too. `__name__` stays, since the
    result's `raised` reports it.

    Only RPyC's own subclass is renamed. What else `load` returns is left as it is: `StopIteration`,
    a string, and whatever the class's `__new__` made in the subclass's place, which is the
    library's -- an instance of its own class, or an object that is no exception. The subclass is
    told by identity against those RPyC has made, never by asking the object its class or hashing
    a class to look it up: that runs the library's code, and nothing here can be interrupted.
    """
    vinegar = _rpyc.core.vinegar
    derived = type(exc)
    # A snapshot, which another thread's `_get_exception_class` cannot change as it is read.
    if not any(derived is made for made in tuple(vinegar._exception_classes_cache.values())):
        return exc
    base = _type_bases(derived)[0]
    qualname = _type_qualname(base)
    if issubclass(base, vinegar.GenericException):
        modname, qualname = raw[0]
        type.__setattr__(derived, "__module__", modname)
    type.__setattr__(derived, "__qualname__", qualname)
    return exc


def _closed(text):
    """`outrig.Closed(text)`, for a call the close kept from running: the class the bindings
    process's own `Closed` arrives as through RPyC -- RPyC's generic class for the name, made
    without importing anything, since the reader thread makes one too."""
    raw = (("outrig", "Closed"), (), (), "")
    return type(_named_as_host(_rpyc.core.vinegar.load(raw, False, False, False), raw))(text)


class FrameChannel:
    """RPyC's channel, over `rpc` lines rather than a stream.

    Frames arrive in parts, each one line, from the thread reading the protocol; `put` joins them
    and `poll` waits for a whole one on a condition variable, so the thread serving the connection
    never reads the protocol itself. A frame is sent in parts the same way, each written whole
    under the process's send lock.

    Bounds are enforced here, in both directions: a part over `PART_MAX`, a part before a frame's
    last that is not full, a frame whose header claims no data or more than `FRAME_MAX`, a
    compressed frame, or more than `BUFFER_MAX` bytes held unread -- each frame charged
    `FRAME_OVERHEAD` beyond its bytes -- closes the channel with the reason, and the waiting
    reader raises `EOFError` saying why. A frame is never half-delivered: `recv` returns whole
    frames or raises.
    """

    def __init__(self, write, cond=None):
        # `write(part, more, label)` writes one part as one protocol line, `label` the fields of
        # the frame's first line beyond the part. `cond` is the condition variable to wait on,
        # when the caller shares one between channels.
        self._write = write
        self._cond = threading.Condition() if cond is None else cond
        self._reason = None  # why the channel closed; `None` while it is open
        self._frames = collections.deque()  # whole frames, data only
        self._held = 0  # bytes in `_frames`
        self._parts = []  # the frame in progress, header and flusher stripped
        self._received = 0  # bytes of it so far, header and flusher included
        self._expected = 0  # bytes it declares, header and flusher included

    @property
    def closed(self):
        return self._reason is not None

    @property
    def reason(self):
        return self._reason

    @property
    def ready(self):
        """Whether `recv` would return at once: a frame is held, or the channel has closed."""
        return bool(self._frames) or self._reason is not None

    def fileno(self):
        raise OSError("a frame channel has no descriptor")

    def close(self, reason="closed"):
        """Mark the channel closed -- the first reason stands -- drop what it holds, and wake every
        waiter. Writes nothing. Returns the reason in effect."""
        with self._cond:
            if self._reason is None:
                self._reason = reason
            self._frames.clear()
            self._parts = []
            self._held = self._received = 0
            self._cond.notify_all()
            return self._reason

    def put(self, part, more):
        """Take one part of a frame, from the reading thread. Returns `None`, or the reason this
        closed the channel, which the caller reports to the other side.

        Everything that can fail is decided, and a finished frame joined, before anything changes,
        so a caller that retries after running out of memory never applies a part twice.
        """
        with self._cond:
            if self._reason is not None:
                return None
            if len(part) > PART_MAX:
                return self.close(f"a part of {len(part)} bytes is past the {PART_MAX}-byte bound")
            expected = self._expected
            if not self._parts:
                # The first part carries the header.
                if len(part) < FRAME_HEADER.size:
                    return self.close("a frame shorter than its header")
                length, compressed = FRAME_HEADER.unpack_from(part)
                if compressed:
                    return self.close("a compressed frame; compression is off on this transport")
                if not length:
                    # RPyC sends none, and serves none: it would be taken and never dispatched.
                    return self.close("a frame with no data")
                if length > FRAME_MAX:
                    return self.close(
                        f"a frame of {length} bytes is past the {FRAME_MAX}-byte bound"
                    )
                expected = FRAME_HEADER.size + length + len(FLUSHER)
            if more and len(part) != PART_MAX:
                # Every part but the last is full, which bounds how many parts hold one frame.
                return self.close(
                    f"a part before a frame's last carries {PART_MAX} bytes, not {len(part)}"
                )
            received = self._received + len(part)
            if received > expected or (not more and received != expected):
                return self.close(
                    f"a frame's parts carry {received} bytes against the {expected} its header "
                    f"declares"
                )
            if not more and not part.endswith(FLUSHER):
                return self.close("a frame without its trailing newline")
            if self._held + received + FRAME_OVERHEAD > BUFFER_MAX:
                return self.close(f"more than {BUFFER_MAX} bytes of frames held unread")
            data = memoryview(part)[FRAME_HEADER.size if not self._parts else 0 :]
            if more:
                self._parts.append(data)
                self._received, self._expected = received, expected
                return None
            frame = b"".join([*self._parts, data[: len(data) - len(FLUSHER)]])
            self._frames.append(frame)
            self._held += len(frame) + FRAME_OVERHEAD
            self._parts = []
            self._received = 0
            self._cond.notify_all()
            return None

    def poll(self, timeout):
        """Whether a frame is ready, waiting up to `timeout` -- RPyC's `Timeout`, seconds, or
        `None` -- for one. True once the channel has closed, so the reader learns why."""
        left = timeout.timeleft() if hasattr(timeout, "timeleft") else timeout
        with self._cond:
            return self._cond.wait_for(lambda: self.ready, left)

    def recv(self):
        """The next whole frame's data, or `EOFError` with the reason the channel closed."""
        with self._cond:
            if self._frames:
                data = self._frames.popleft()
                self._held -= len(data) + FRAME_OVERHEAD
                return data
            raise EOFError(self._reason or "no frame is ready")

    def send(self, data, label=None):
        """Send one frame of `data`, in parts, `label` on the first. Refuses a frame past the
        bound before writing any of it; a write that fails part way closes the channel, since the
        other side holds a torn frame."""
        if len(data) > FRAME_MAX:
            raise ValueError(f"a frame of {len(data)} bytes is past the {FRAME_MAX}-byte bound")
        if self._reason is not None:
            raise EOFError(self._reason)
        segments = collections.deque(
            [FRAME_HEADER.pack(len(data), 0), memoryview(data), FLUSHER]
        )
        while True:
            part = bytearray()
            while segments and len(part) < PART_MAX:
                segment = segments.popleft()
                take = PART_MAX - len(part)
                part += segment[:take]
                if take < len(segment):
                    segments.appendleft(segment[take:])
            more = bool(segments)
            try:
                self._write(bytes(part), more, label)
            except BaseException:
                self.close("a frame could not be written whole")
                raise
            label = None
            if not more:
                return


class _Context(threading.local):
    """Per thread: the callables marked for the request being sent, or `None` while a callback's
    return value is being boxed, where a callable is refused."""

    marks = None


class _Wait:
    """One thread's hosted call in progress, for the reader thread to wake.

    `conn` is the connection the call took, `None` while it waits for a free one; `waiting` is
    true while the thread is in the condition variable's wait, as against running a callback's
    code, boxing, sending or reading a frame. `exc` is what the call is to raise at its next
    check -- before it waits, as it wakes, or as it ends: a `KeyboardInterrupt` or
    `CancelledError` the reader thread or an interrupt's signal left, or the closing `EOFError` --
    and `by` the kernel that left it, for the report when the call was a background task's.
    `interruptible` is decided once, from the calling thread's own frames, by the rule
    `_on_sigint` applies: a hosted call this program makes for itself is not an interrupt's to
    end. `asking` is what a boundary request it is waiting to send asks -- its operation, member
    and target -- while it waits for a free connection: a call woken then was never sent, which
    the waker tells the host. `seqs` are the requests the call sent, which a cancel names, and
    no other call's: a callback's own call shares its connection with the call it is inside.
    """

    __slots__ = (
        "pool",
        "execution",
        "invocation",
        "interruptible",
        "waiting",
        "exc",
        "by",
        "conn",
        "asking",
        "seqs",
    )

    def __init__(self, pool, execution, interruptible):
        self.pool = pool
        self.execution = execution
        # The skill invocation the calling code runs inside, read on its own thread: the waker
        # that tells the host of a call never sent runs on another.
        self.invocation = _invocation()
        self.interruptible = interruptible
        self.waiting = False
        self.exc = None
        self.by = None
        self.conn = None
        self.asking = None
        self.seqs = []  # the requests this call sent, which a cancel names

    def never_sent(self, outcome, reason):
        """Under the pool's condition variable: the line that tells the host this call was never
        sent -- `cancelled` as it is woken, or `refused` -- if it is to send a boundary request,
        for `_say` once the condition variable is let go; else `None`. Once."""
        if self.asking is None:
            return None
        (operation, member, target), self.asking = self.asking, None
        execution = self.execution
        return {
            "t": "hosted",
            "agent": self.pool.kernel.agent,
            "binding": self.pool.binding,
            "exec": None if execution is None else execution.id,
            "invocation": self.invocation,
            "operation": operation,
            "member": member,
            "target": target,
            "outcome": outcome,
            "reason": reason,
        }

    def block(self, ready, timeout=None):
        """Under the pool's condition variable: wait until `ready()`, or raise what the reader
        thread or an interrupt's signal left, which comes first. False when `timeout` seconds
        pass with neither. On the main thread it looks every `SIGNAL_CHECK` seconds, since the
        signal can land after the reader thread found the thread busy and leave its exception
        once the wait has begun, waking nothing."""
        deadline = None if timeout is None else time.monotonic() + timeout
        main = threading.current_thread() is threading.main_thread()
        while True:
            if self.exc is not None:
                raise self.pool._pop_exc(self)
            if ready():
                return True
            left = None if deadline is None else deadline - time.monotonic()
            if left is not None and left <= 0:
                return False
            if main:
                left = SIGNAL_CHECK if left is None else min(left, SIGNAL_CHECK)
            self.waiting = True
            try:
                self.pool.cond.wait(left)
            finally:
                self.waiting = False


def _woken_text(wait, verb):
    binding = wait.pool.binding
    if wait.conn is None:
        return (
            f"{verb} by outrig while waiting for a free connection to binding {binding!r}: the "
            f"call was never sent"
        )
    return (
        f"{verb} by outrig while waiting for binding {binding!r} to answer: the call's outcome "
        f"on the host is unknown"
    )


class _Pool:
    """One kernel's connections to one binding, up to `POOL_MAX`, with one call in flight on
    each.

    A connection has an owner -- the thread whose call is in flight on it, which alone reads or
    sends on it -- and a depth, for the nested calls a callback served on it makes; a request is
    outstanding on it while RPyC holds the request's callback, until its reply is read. It is
    free with no owner and nothing outstanding, and abandoned with no owner and a reply still to
    come: its caller was woken by an interrupt, and the host's thread may still be busy with the
    call. A call takes a free connection, opens one when none is free and fewer than `POOL_MAX`
    exist, and otherwise waits on the pool's condition variable -- the one every channel of the
    pool wakes on a frame, and the reader thread wakes with an exception -- where a call woken
    was never sent. On its way a taker tidies an abandoned connection whose frames have arrived:
    the late replies are dropped unread and a request made meanwhile is refused, so the
    connection is free again.

    Nothing dispatches, boxes or writes a frame while holding the condition variable.
    """

    def __init__(self, kernel, binding):
        self.kernel = kernel
        self.binding = binding
        self.cond = threading.Condition()
        self.connections = []
        self.closed = None  # the reason, once `close` has run
        self._root = None
        self._clock = itertools.count()  # orders the connections by their last take

    @property
    def names(self):
        return f"binding {self.binding!r} on agent {self.kernel.agent!r}"

    def root(self):
        """The binding's root, fetched on a connection of this pool at first use, and again
        when the connection it came on has closed."""
        root = self._root
        if root is not None and not object.__getattribute__(root, "____conn__").closed_either():
            return root
        self._root = root = self.request(_rpyc.core.consts.HANDLE_GETROOT)
        return root

    def request(self, handler, *args):
        """One synchronous request on whichever connection of the pool this thread may use.
        Refused on the reader thread, which delivers the reply it would wait for, and in an
        execution of another kernel: an interrupt of a kernel reaches only its own pools' calls.
        An interrupt or cancel left for the call and not raised by the time it ends is raised
        then, in place of what the call returned or after the error it raised."""
        if threading.current_thread() is _reader_thread:
            raise RuntimeError("a hosted request cannot be made on the reader thread")
        execution = _CURRENT.get()
        if execution is not None and execution.kernel is not self.kernel:
            raise TypeError(
                f"a hosted object of agent {self.kernel.agent!r} cannot be used by agent "
                f"{execution.kernel.agent!r}: each agent reaches a binding through its own"
            )
        wait = _Wait(self, execution, _agent_code_outward(sys._getframe(), self.kernel))
        asking = _Hosted.asked(handler, args)
        if asking[0] in _BOOKKEEPING:
            asking = None
        ident = threading.get_ident()
        with self.cond:
            outer = _waits.get(ident)
            _waits[ident] = wait
        stands = False
        try:
            self._refuse_after_wake(outer, wait, asking)
            conn = self.take(wait, asking)
            try:
                return conn._request_here(handler, *args)
            except BaseException:
                # The caller gave up on replies still to come -- interrupted, say -- so the host
                # is told, and drops a request of this call's it has not run: one waiting for its
                # binding's turn, say. At any depth, since the cancel names this call's requests
                # alone, and not those of the call a callback's request is made inside.
                seqs = [seq for seq in wait.seqs if seq in conn._marks]
                if seqs and not conn.closed_either():
                    conn.cancel(seqs)
                raise
            finally:
                self.release(conn)
        except (KeyboardInterrupt, asyncio.CancelledError):
            # One the call raised stands for one left for it.
            stands = True
            raise
        finally:
            with self.cond:
                # Put back first: a signal from here on leaves its interrupt on the outer call,
                # never on this one once it has been taken.
                if outer is None:
                    _waits.pop(ident, None)
                else:
                    _waits[ident] = outer
                left = self._pop_exc(wait) if wait.exc is not None and not stands else None
            if isinstance(left, (KeyboardInterrupt, asyncio.CancelledError)):
                verb = "cancelled" if isinstance(left, asyncio.CancelledError) else "interrupted"
                left.args = (f"{verb} by outrig after the call to binding {self.binding!r} ended",)
                raise left

    def _refuse_after_wake(self, outer, wait, asking):
        """Raise, as `wait`'s call and before anything is sent, the cancel or interrupt left for
        `outer`, the call this thread is inside, when it is a callback's own call: a thread running
        a callback's code is not waiting, so what woke its call is left for the call to raise once
        the callback has returned, and a call the callback made meanwhile would run after the
        cancel was delivered. It is taken from `outer`, which then has nothing left to raise, and
        the host is told this call was never sent, as it is of one woken before it was."""
        if outer is None:
            return
        with outer.pool.cond:
            if not isinstance(outer.exc, (KeyboardInterrupt, asyncio.CancelledError)):
                return
            wait.asking = asking
            said = wait.never_sent("cancelled", "interrupted")
            woken = self._pop_exc(outer)
        _say([said])
        raise woken

    def take(self, wait, asking=None):
        """A connection for this thread's call, as the class comment describes. While it waits,
        `asking` -- what its request asks, when that is a boundary request -- is the wait's; after
        the session closed, such a request is refused instead. Under the condition variable, so a
        close either finds the wait to wake or has set what refuses it."""
        ident = threading.get_ident()
        while True:
            refused = interrupted = None
            try:
                with self.cond:
                    wait.asking = asking
                    try:
                        # A call already woken raises what woke it, and was told of then --
                        # unless an interrupt's signal left it, which writes no line.
                        if asking is not None and _closing is not None and wait.exc is None:
                            refused = wait.never_sent("refused", "admission_closed")
                        else:
                            wait.block(lambda: self._pick(ident, wait))
                    except KeyboardInterrupt:
                        interrupted = wait.never_sent("cancelled", "interrupted")
                        raise
                    finally:
                        wait.asking = None
            finally:
                _say([interrupted])
            if refused is not None:
                _say([refused])
                raise _closed(_closing)
            conn = wait.conn
            # Its own, or free: the call's. Otherwise abandoned, with frames to drop -- outside
            # the condition variable, since refusing a request writes -- and then another look.
            if conn.depth > 1 or not conn._request_callbacks:
                return conn
            try:
                conn.tidy()
            finally:
                wait.conn = None
                self.release(conn)

    def _pick(self, ident, wait):
        """Under the condition variable: the connection for `wait`'s thread, taken, or `None` to
        wait for one. The thread's own when it is inside a callback served on it -- the nested
        call goes where the host's thread is waiting -- else a free one or an abandoned one
        whose frames have arrived, else a new one while the pool is not full."""
        if self.closed is not None:
            raise EOFError(f"{self.closed}: the call was never sent")
        for conn in list(self.connections):
            if conn.owner is None and conn.closed_either():
                self._drop(conn)
            elif conn.owner == ident:
                conn.depth += 1
                wait.conn = conn
                return conn
        free = [
            c
            for c in self.connections
            if c.owner is None and (not c._request_callbacks or c._channel.ready)
        ]
        if free:
            # One with releases queued goes first, and of those the one taken least recently: a
            # proxy's `del` is sent before the next request on the connection that produced the
            # proxy, so a connection the kernel's calls stopped taking would otherwise hold its
            # releases until the kernel closes -- and one that requeues a release on every call,
            # a factory's result discarded each time, must not keep the others from sending.
            queued = [c for c in free if c._pending_dels]
            conn = min(queued, key=lambda c: c.used_at) if queued else free[0]
        elif len(self.connections) >= POOL_MAX:
            return None
        else:
            conn = self.kernel._open_hosted(self.binding, self)
            self.connections.append(conn)
        conn.owner, conn.depth = ident, 1
        conn.used_at = next(self._clock)
        wait.conn = conn
        return conn

    def release(self, conn):
        with self.cond:
            conn.depth -= 1
            if conn.depth == 0:
                conn.owner = None
                if conn.closed_either():
                    self._drop(conn)
            self.cond.notify_all()

    def close(self, reason):
        """Close every connection, waking every call waiting on them: one in flight is left to
        the binding to finish and its caller told the outcome is unknown; one waiting for a
        connection was never sent. A connection a thread holds is closed by that thread when its
        wait ends, since cleaning up releases locks only the holder may release."""
        unknown = f"{reason}: the call's outcome on the host is unknown"
        with self.cond:
            self.closed = reason
            conns = list(self.connections)
            for conn in conns:
                _rpc_channels.pop(conn.key, None)
                conn._channel.close(unknown)
                if conn.owner is None:
                    conn.close()
            for wait in list(_waits.values()):
                if wait.pool is self and wait.conn is None and wait.exc is None:
                    wait.exc = EOFError(f"{reason}: the call was never sent")
            self.cond.notify_all()
        for conn in conns:
            _rpc_notice(self.kernel.agent, self.binding, conn.number, reason)

    def _drop(self, conn):
        self.connections.remove(conn)
        _rpc_channels.pop(conn.key, None)
        if conn.owner is None:
            conn.close()

    @staticmethod
    def _pop_exc(wait):
        """Take the exception the reader thread left, noting where it lands as `_on_sigint`
        does, so an interrupt that ends a background task's call is reported to its owner."""
        exc, wait.exc = wait.exc, None
        if wait.by is not None:
            wait.by._landed = (exc, asyncio.current_task(wait.by.loop), wait.execution)
        return exc


class HostedRoot:
    """A binding's root -- an object in a process on the host -- as the name bound for the binding
    holds it. Every operation on it is the same operation on RPyC's proxy of the root, a request
    made then and there that blocks the calling thread until the host answers, and one the root's
    type lacks fails as it would on the proxy. Nothing about it is awaitable. The proxy is fetched
    at first use, on the thread using it, and again once the connection it came on has closed;
    making a stub sends nothing."""

    __module__ = "outrig"
    __slots__ = ("_pool", "__weakref__")

    def __init__(self, pool):
        object.__setattr__(self, "_pool", pool)

    def __enter__(self):
        root = object.__getattribute__(self, "_pool").root()
        enter = getattr(type(root), "__enter__", None)
        if enter is None:
            raise TypeError(
                f"'{_type_name(type(root))}' object does not support the context manager protocol"
            )
        return enter(root)

    def __exit__(self, *exc_info):
        root = object.__getattribute__(self, "_pool").root()
        return type(root).__exit__(root, *exc_info)


def _on_root(operation, reflected=False):
    """A method of `HostedRoot`: `operation` applied to the root and the method's arguments, the
    root second for a reflected operator and for `isinstance`."""

    def method(self, *args, **kwargs):
        root = object.__getattribute__(self, "_pool").root()
        if reflected:
            return operation(args[0], root, *args[1:])
        return operation(root, *args, **kwargs)

    return method


# The rest of `HostedRoot`'s special methods, each named here without its underscores and each the
# same operation on the root: what RPyC's proxy answers, its numeric operators those of RPyC's safe
# list.
_BINARY = "add sub mul truediv floordiv mod lshift rshift and or xor".split()
HostedRoot.__getattribute__ = _on_root(getattr)
for _name in "setattr delattr iter next len bool hash repr str format dir int float abs".split():
    setattr(HostedRoot, f"__{_name}__", _on_root(getattr(builtins, _name)))
for _name in [
    *"call getitem setitem delitem contains eq ne lt le gt ge index neg pos invert".split(),
    *_BINARY,
    *(f"i{op}" for op in [*_BINARY, "pow"]),
]:
    setattr(HostedRoot, f"__{_name}__", _on_root(getattr(operator, f"__{_name}__")))
for _name in _BINARY:
    setattr(HostedRoot, f"__r{_name}__", _on_root(getattr(operator, f"__{_name}__"), True))
HostedRoot.__divmod__, HostedRoot.__rdivmod__ = _on_root(divmod), _on_root(divmod, True)
HostedRoot.__pow__, HostedRoot.__rpow__ = _on_root(pow), _on_root(pow, True)
HostedRoot.__instancecheck__ = _on_root(isinstance, True)
HostedRoot.__length_hint__ = _on_root(operator.length_hint)
del _BINARY, _name


def _target_type(obj):
    """The host type a proxy names, as a request's target says it; `None` for anything else."""
    if issubclass(type(obj), _rpyc.core.netref.BaseNetref):
        return str(object.__getattribute__(obj, "____id_pack__")[0])
    return None


def _attribute_error(e):
    """The binding's refusal `e` of an attribute's name, as an `AttributeError` of the same class
    name, text and `name`."""
    cls = type(e)
    namespace = {"__module__": cls.__module__, "__qualname__": _type_qualname(cls)}
    error = type(_type_name(cls), (cls, AttributeError), namespace)(*e.args, name=e.name)
    error.__dict__.update(e.__dict__)
    return error


def _coroutine_function(fn):
    """Whether `fn` is an `async def` function, a bound method of one, or a `functools.partial`
    of either -- read through their types alone, so a hosted proxy inside is asked nothing."""
    if type(fn) is functools.partial:
        fn = fn.func
    if type(fn) is types.MethodType:
        fn = fn.__func__
    return type(fn) is types.FunctionType and bool(fn.__code__.co_flags & inspect.CO_COROUTINE)


def _load_outrig(directory):
    """Import OutRig's own package from `directory`, and give it this process's executions:
    `outrig.runtime` and `outrig.bindings` are those of the execution reading them. Its skills
    module gets the way to ask the host for a skill's files, and puts its finder in place."""
    global _schema, _agents, _skills
    sys.path.append(directory)
    import outrig._agents
    import outrig._kernel
    import outrig.schema
    import outrig.skills

    outrig._kernel.install(_calling)
    outrig._agents._call = _call
    outrig._agents._submitted_source = _submitted_source
    outrig.skills._install(
        _ask_host, _notify, _compiled_cause, lambda: next(_ids), _refuse_closing,
        lambda: _observing,
    )
    _schema = outrig.schema
    _agents = outrig._agents
    _skills = outrig.skills


def _calling():
    """The kernel of the execution this code belongs to, or `None` outside every execution."""
    execution = _CURRENT.get()
    return None if execution is None else execution.kernel


def _invocation():
    """The id of the skill invocation the calling code runs inside, or `None`: diagnostic
    context the host attaches to what that code asks of it, which the code can change."""
    if _skills is None:
        return None
    ids = _skills._current()
    return ids[0] if ids else None


# The source of each submission, by the id of every code object compiled from it -- its own, and
# each function's in it. `outrig.agent` digests a declaration compiled in a submission from this,
# whichever execution runs the declaring: one a factory from an earlier submission makes is still
# that submission's. Keyed by id, since code objects compare by value; an entry goes with its
# code, so a reused id never finds a dead one's source.
_sources = {}


def _remember_source(code, source):
    """Record `source` as what compiled `code`, and every code object nested in it."""
    # A worklist, not recursion: what compiled may nest functions deeper than the recursion limit.
    pending = [code]
    while pending:
        code = pending.pop()
        _sources[id(code)] = source
        weakref.finalize(code, _sources.pop, id(code), None)
        pending.extend(const for const in code.co_consts if type(const) is types.CodeType)


def _submitted_source(code):
    """The source of the submission that compiled `code`, or `None` when none did."""
    return _sources.get(id(code))


def _load_rpyc(directory):
    """Import RPyC from `directory` and build the connection class over it."""
    global _rpyc, _Hosted, _rpyc_missing, _INTERRUPTIBLE_CODE
    sys.path.append(directory)
    try:
        import rpyc
    except Exception as e:
        _rpyc_missing = f"RPyC could not be imported from {directory}: {e!r}"
        _diag(_rpyc_missing)
        return
    _rpyc = rpyc
    _Hosted = _connection_class(rpyc)
    _INTERRUPTIBLE_CODE = frozenset(
        [
            *(fn.__code__ for fn in _Hosted._request_handlers().values()),
            _Hosted._box.__code__,
            rpyc.lib.get_id_pack.__code__,
            rpyc.core.vinegar.load.__code__,
            rpyc.core.vinegar._get_exception_class.__code__,
            rpyc.core.vinegar.dump.__code__,
        ]
    )


def _connection_class(rpyc):
    """The connection class a kernel opens to a binding, built over the imported `rpyc`, whose
    proxies it makes ask the host once for an attribute."""
    consts = rpyc.core.consts
    brine = rpyc.core.brine
    vinegar = rpyc.core.vinegar
    netref = rpyc.core.netref
    Connection = rpyc.core.protocol.Connection
    get_id_pack = rpyc.lib.get_id_pack

    # A proxy's `__getattribute__` asks the host for every name it does not answer itself, and
    # when the answer is an `AttributeError` Python calls its `__getattr__`, which asks again. So
    # the asking is left to `__getattr__` alone: a read is one request, whatever the host answers.
    # `__class__` is answered here when the proxy's class names a class of this process, and
    # `__doc__` is always asked.
    answered_here = netref.BaseNetref.__getattribute__
    local = (netref.LOCAL_ATTRS | {"__call__", "__array__"}) - {"__doc__"}

    def __getattribute__(proxy, name):
        if name == "__class__":
            cls = object.__getattribute__(proxy, "__class__")
            if cls is not None:
                return cls
        elif name in local:
            return answered_here(proxy, name)
        raise AttributeError(name)

    netref.BaseNetref.__getattribute__ = __getattribute__

    # RPyC tells an object of the proxied class from another by its proxy's identity, and any
    # other object by the local class it stands for, which a class the container cannot import has
    # none of. A binding's name is the proxy of its root, so it is asked as that proxy is.
    stock_instancecheck = netref.BaseNetref.__instancecheck__

    def __instancecheck__(proxy, other):
        if type(other) is HostedRoot:
            other = object.__getattribute__(other, "_pool").root()
        return stock_instancecheck(proxy, other)

    netref.BaseNetref.__instancecheck__ = __instancecheck__

    # The container may import a module the host names, so a host exception arrives as the
    # library's own class where its module imports here. No timer: a hosted call takes as long as
    # the library takes.
    config = dict(
        allow_pickle=False,
        import_custom_exceptions=True,
        instantiate_custom_exceptions=True,
        propagate_SystemExit_locally=False,
        propagate_KeyboardInterrupt_locally=False,
        sync_request_timeout=None,
        bind_threads=False,
    )
    # A subclass of a by-value type crosses as its base value, read through the base type's own
    # method so that no override of the subclass's runs.
    bases = (
        (int, int.__int__),
        (float, float.__float__),
        (complex, complex.__complex__),
        (str, str.__str__),
        (bytes, lambda value: memoryview(value).tobytes()),
        (tuple, lambda value: tuple.__getitem__(value, slice(None))),
    )
    # The containers the shim copies, nested ones included, read through the base's own methods.
    copies = (
        (dict, LABEL_DICT, dict.items),
        (list, LABEL_LIST, list.__iter__),
        (set, LABEL_SET, set.__iter__),
        (frozenset, LABEL_FROZENSET, frozenset.__iter__),
    )
    # Each handler's operation, as a request's label names it -- its `HANDLE_` name, lower-cased
    # -- and the handlers whose second argument names an attribute.
    operations = {
        value: name[len("HANDLE_") :].lower()
        for name, value in vars(consts).items()
        if name.startswith("HANDLE_")
    }
    naming = frozenset(
        [
            consts.HANDLE_GETATTR,
            consts.HANDLE_SETATTR,
            consts.HANDLE_DELATTR,
            consts.HANDLE_CALLATTR,
            consts.HANDLE_OLDSLICING,
        ]
    )

    class Hosted(Connection):
        """One of a kernel's connections to one binding, a member of a `_Pool`."""

        def __init__(self, channel, *, agent, binding, number, pool):
            self.agent = agent
            self.binding = binding
            self.number = number
            super().__init__(
                rpyc.core.service.VoidService(),
                channel,
                dict(config, connid=f"{agent}/{binding}#{number}"),
            )
            self._context = _Context()
            # Releases of proxies, from `BaseNetref.__del__`, held until the next request: a
            # finalizer can run inside `_write_line`, which holds the process's send lock.
            self._pending_dels = collections.deque()
            # The pool's bookkeeping, kept under the pool's condition variable. A request is
            # outstanding while its callback is in RPyC's `_request_callbacks`.
            self._pool = pool
            self.owner = None  # ident of the thread whose call is in flight here
            self.depth = 0  # that thread's nested calls on this connection
            self.used_at = -1  # the pool's clock when a call last took this connection
            # The callables each request handed out, by seq: released once its reply has been
            # delivered, or at once when the reply is dropped.
            self._marks = {}

        @property
        def names(self):
            return self._pool.names

        @property
        def key(self):
            return (self.agent, self.binding, self.number)

        def closed_either(self):
            """Closed on this side, or by the host, whose notice closes the channel alone until
            the thread serving this connection sees it."""
            return self._closed or self._channel.closed

        # ------------------------------------------------------------------ requests

        @staticmethod
        def asked(handler, args):
            """What a request asks, as its label and a notice of it say: its operation, the
            attribute it names, and the host type of its target."""
            if handler in naming:
                member = args[1] if len(args) > 1 else None
            else:
                member = args[2] if handler == consts.HANDLE_CMP and len(args) > 2 else None
            return (
                operations.get(handler, "unknown"),
                member if type(member) is str else None,
                _target_type(args[0]) if args else None,
            )

        def _send(self, msg, seq, args, label=None):
            """RPyC's, without its shared send queue -- a frame past the bound raises in the thread
            that asked, and a finalizer's release is queued by `_async_request` rather than sent --
            and when a frame could not be written whole, which closed the channel, the binding is
            told, so it releases the connection's objects and thread. `label`, with the frame's
            size, goes on its first line."""
            data = brine.I1.pack(msg) + brine.dump((seq, args))
            if label is not None:
                label = {**label, "size": len(data)}
            try:
                with self._sendlock:
                    self._channel.send(data, label)
            except BaseException:
                if self._channel.closed and not self._closed:
                    self.close_with(self._channel.reason)
                raise

        def _async_request(self, handler, args=(), callback=(lambda a, b: None)):
            if handler == consts.HANDLE_DEL:
                proxy, count = args
                self._pending_dels.append(
                    (object.__getattribute__(proxy, "____id_pack__"), count)
                )
                return
            self._flush_dels()
            if handler == consts.HANDLE_CTXEXIT:
                args = self._ctxexit_args(args)
            operation, member, target = self.asked(handler, args)
            execution = _CURRENT.get()
            saved = self._context.marks
            self._context.marks = marks = []
            try:
                seq = self._get_seq_id()
                self._request_callbacks[seq] = callback
                self._marks[seq] = marks
                wait = _waits.get(threading.get_ident())
                if wait is not None and wait.conn is self:
                    wait.seqs.append(seq)
                label = {
                    "request": seq,
                    "op": operation,
                    "member": member,
                    "target": target,
                    "exec": None if execution is None else execution.id,
                    "invocation": _invocation(),
                }
                try:
                    self._send(consts.MSG_REQUEST, seq, (handler, self._box(args)), label)
                except BaseException:
                    self._request_callbacks.pop(seq, None)
                    self._release(self._marks.pop(seq, ()))
                    raise
            finally:
                self._context.marks = saved

        def _release(self, marks):
            for id_pack in marks:
                with contextlib.suppress(KeyError):
                    self._local_objects.decref(id_pack)

        def _flush_dels(self):
            while True:
                try:
                    id_pack, count = self._pending_dels.popleft()
                except IndexError:
                    return
                seq = self._get_seq_id()
                self._request_callbacks[seq] = lambda is_exc, obj: None
                boxed = (
                    consts.LABEL_TUPLE,
                    ((consts.LABEL_LOCAL_REF, id_pack), (consts.LABEL_VALUE, count)),
                )
                label = {"request": seq, "op": "del"}
                self._send(consts.MSG_REQUEST, seq, (consts.HANDLE_DEL, boxed), label)

        def _ctxexit_args(self, args):
            """A proxy's `__exit__` sends only the exception's type. The exception itself is
            `sys.exception()` while a `with` block unwinds, and goes by value when its type is the
            one sent; otherwise the type alone, and `None` stays `None`."""
            proxy, exc = args
            if exc is None:
                return (proxy, None)
            current = sys.exception()
            if current is not None and type(current) is exc:
                dump = vinegar.dump(
                    exc, current, None, include_local_traceback=False, include_local_version=False
                )
            else:
                dump = ((exc.__module__, exc.__name__), (), (), "")
            return (proxy, dump)

        # ------------------------------------------------------------------ boxing

        def _box(self, obj, seen=frozenset()):
            """The shim: box an argument, or raise `TypeError` naming what cannot cross."""
            if brine.dumpable(obj):
                return consts.LABEL_VALUE, obj
            if type(obj) is tuple:
                return consts.LABEL_TUPLE, tuple(self._box(item, seen) for item in obj)
            # Before anything else: a stub, or a proxy of a host list, answers `isinstance(x,
            # list)`, and a proxy of a host method is callable. A stub sends its root, fetched
            # here if need be, nested on this thread's connection.
            if type(obj) is HostedRoot:
                pool = object.__getattribute__(obj, "_pool")
                if pool is not self._pool:
                    raise self._foreign(pool.names)
                obj = pool.root()
            if isinstance(obj, netref.BaseNetref):
                conn = object.__getattribute__(obj, "____conn__")
                if getattr(conn, "_pool", None) is self._pool:
                    # Any connection of the same kernel to the same binding: the binding keeps
                    # one object table per kernel.
                    return consts.LABEL_LOCAL_REF, object.__getattribute__(obj, "____id_pack__")
                raise self._foreign(getattr(conn, "names", "another connection"))
            for base, label, read in copies:
                if isinstance(obj, base):
                    if id(obj) in seen:
                        raise TypeError("a container that contains itself cannot be sent")
                    inner = seen | {id(obj)}
                    if base is dict:
                        return label, tuple(
                            (self._box(key, inner), self._box(value, inner))
                            for key, value in read(obj)
                        )
                    return label, tuple(self._box(item, inner) for item in read(obj))
            if isinstance(obj, os.PathLike):
                return self._box(os.fspath(obj), seen)
            for base, read in bases:
                if isinstance(obj, base):
                    return self._box(read(obj), seen)
            if callable(obj):
                marks = self._context.marks
                if marks is None:
                    raise TypeError(
                        f"a callable of type {_type_name(type(obj))} cannot be returned from a "
                        f"callback: only values go back to the host"
                    )
                if _coroutine_function(obj):
                    raise TypeError(_COROUTINE_CALLBACK)
                id_pack = get_id_pack(obj)
                self._local_objects.add(id_pack, obj)
                marks.append(id_pack)
                return LABEL_CALLABLE, id_pack
            raise TypeError(
                f"an object of type {_type_name(type(obj))} cannot be passed to a hosted object: "
                f"only values, lists, dicts, sets, paths, callables, and the binding's own proxies "
                f"cross. `list()` a generator, or pass what the object holds."
            )

        def _foreign(self, names):
            return TypeError(
                f"a proxy of {names} cannot be passed to {self.names}: a hosted object crosses "
                f"only to its own binding, from the kernel that holds it"
            )

        def _unbox_exc(self, raw):
            try:
                return _named_as_host(super()._unbox_exc(raw), raw)
            except Exception as e:
                return vinegar.GenericException(
                    f"an exception from {self.names} could not be rebuilt: {e!r}"
                )

        # ------------------------------------------------------------------ requests

        def sync_request(self, handler, *args):
            """A proxy's request, on whichever of its kernel's connections to this binding the
            pool gives: `netref.syncreq` calls this on the connection that produced the proxy,
            and the binding resolves the proxy on any of them. A proxy of a closed connection
            raises the reason without sending. A read the binding refuses by its rule for names
            raises an `AttributeError`, as a read of a missing name does."""
            if self.closed_either():
                raise EOFError(self._channel.reason or f"the connection to {self.names} is closed")
            try:
                return self._pool.request(handler, *args)
            except Exception as e:
                # That rule's refusal, alone of the binding's, names what it refused. It arrives
                # as RPyC's own class named `outrig.Refused`, or, where a module `outrig` with a
                # class `Refused` imports here, as that class.
                name = _type_name(type(e))
                if (
                    handler != consts.HANDLE_GETATTR
                    or "outrig.Refused" not in (name, f"{type(e).__module__}.{name}")
                    or getattr(e, "name", None) is None
                ):
                    raise
                raise _attribute_error(e) from None

        def _request_here(self, handler, *args):
            """RPyC's synchronous request on this connection, which the caller has taken."""
            return super().sync_request(handler, *args)

        def serve(self, timeout=None, wait_for_lock=True, waiting=lambda: True):
            """Serve one frame: the reply this thread waits for, or a request the host makes
            meanwhile -- a callback -- which runs here, on the thread whose call it belongs to.

            Replaces RPyC's, whose receive lock and event let several threads read one
            connection: the pool gives a connection to one thread at a time, and a nested `serve`
            -- a callback's own hosted call -- runs on that same thread. The wait is on the
            pool's condition variable, where the reader thread wakes it with a frame or with the
            exception an interrupt, a cancel or a close carries; that exception is raised here,
            between frames, or by `_Pool.request` as the call ends -- never while a frame is
            read. A request the host refused is answered by the reader thread (`refused`), which
            ends the wait with no frame to serve.
            """
            wait = _waits.get(threading.get_ident()) or _Wait(self._pool, None, False)
            channel = self._channel
            with self._pool.cond:
                if not wait.block(
                    lambda: channel.ready or not waiting(), rpyc.lib.Timeout(timeout).timeleft()
                ):
                    return False
                if not waiting():
                    return True
            try:
                data = channel.recv()
            except EOFError:
                self.close()
                raise
            self._dispatch(data)
            return True

        def tidy(self):
            """Dispatch the frames that have arrived for calls this side gave up on, without
            serving them: a late reply is dropped unread, a request refused. The connection is
            free once nothing is outstanding. Called by a taker that owns the connection."""
            with contextlib.suppress(EOFError):
                while self._request_callbacks and self._channel.ready:
                    self._dispatch(self._channel.recv(), serving=False)

        def refused(self, seq, reason, error):
            """The host refused request `seq` and never sent it on: answered here -- with the
            closing error when admission had closed, else `EOFError` -- its caller woken and the
            callables it handed out given back. Reader thread only."""
            exc = _closed(error) if reason == "admission_closed" else EOFError(error)
            with self._pool.cond:
                callback = self._request_callbacks.pop(seq, None)
                self._release(self._marks.pop(seq, ()))
                if callback is not None:
                    callback(True, exc)
                self._pool.cond.notify_all()

        def cancel(self, seqs):
            """Tell the host the caller of requests `seqs`, outstanding here, gave up on them."""
            with contextlib.suppress(Exception):
                _send(
                    {
                        "t": "rpc",
                        "agent": self.agent,
                        "binding": self.binding,
                        "id": self.number,
                        "cancel": list(seqs),
                    }
                )

        # ------------------------------------------------------------------ dispatch

        def _dispatch(self, data, serving=True):
            """Route one frame. A frame that cannot be decoded closes the connection with the
            reason rather than leaving a call waiting. With `serving` off, the frame belongs to
            a call this side gave up on: a reply is dropped unread, with the references it holds
            released, and a request is refused rather than run."""
            try:
                msg = data[0]
                seq, args = brine.load(data[1:])
                if msg == consts.MSG_REQUEST:
                    self._dispatch_request(seq, args, serving)
                elif msg in (consts.MSG_REPLY, consts.MSG_EXCEPTION):
                    # Popped first: the request is outstanding until its reply is read -- before
                    # unboxing, which can wait for a nested reply and be woken there.
                    callback = self._request_callbacks.pop(seq, None)
                    marks = self._marks.pop(seq, ())
                    if callback is None or not serving:
                        # Nobody will unbox the reply: the references it holds go back, and so
                        # do the callables the request handed out, which the host revoked
                        # before it replied.
                        self._release(marks)
                        if msg == consts.MSG_REPLY:
                            self._queue_dels_of(args)
                        return
                    try:
                        if msg == consts.MSG_EXCEPTION:
                            is_exc, obj = True, self._unbox_exc(args)
                        else:
                            try:
                                obj = self._unbox(args)
                            except Exception as e:
                                # A reference this side no longer holds, say: the call that
                                # asked fails, and the connection carries on.
                                is_exc, obj = True, vinegar.GenericException(
                                    f"a reply from {self.names} could not be rebuilt: {e!r}"
                                )
                            else:
                                is_exc = False
                        callback(is_exc, obj)
                    finally:
                        # After the reply is unboxed and delivered: a method that returns the
                        # callable it was given sends it back as a reference into this table,
                        # which must still hold it then.
                        self._release(marks)
                else:
                    raise ValueError(f"message type {msg!r}")
            except EOFError:
                raise
            except Exception as e:
                reason = f"an undecodable frame from {self.names}: {type(e).__name__}: {e}"
                self.close_with(reason)
                raise EOFError(reason) from None

        def _queue_dels_of(self, boxed):
            """Queue a release for every reference in a boxed reply nobody will unbox, so the
            binding's count of what this connection holds stays right."""
            label, value = boxed
            if label == consts.LABEL_REMOTE_REF:
                self._pending_dels.append(((str(value[0]), value[1], value[2]), 1))
            elif label == consts.LABEL_TUPLE:
                for item in value:
                    self._queue_dels_of(item)

        def _dispatch_request(self, seq, raw_args, serving=True):
            """Serve the host's call of a callback, here, on the thread whose call it belongs to.
            What it returns goes back by the argument rules with callables refused too, and a
            coroutine is closed and refused as a coroutine function is; an interrupt, a cancel or
            an exit it raised is answered and then raised here, so the execution ends with it. An
            error that cannot be boxed is answered by saying so (`_box_callback_error`). A request
            for a call this side gave up on is refused, and the callback does not run."""
            if not serving:
                refusal = _inert_error(
                    "the call this callback belongs to was interrupted in the container; the "
                    "callback did not run"
                )
                self._send(consts.MSG_EXCEPTION, seq, refusal, {"reply": seq})
                return
            saved = self._context.marks
            self._context.marks = None
            raised = None
            try:
                try:
                    handler, args = raw_args
                    args = self._unbox(args)
                    res = self._HANDLERS[handler](self, *args)
                    if type(res) is types.CoroutineType:
                        res.close()
                        raise TypeError(_COROUTINE_CALLBACK)
                    reply = (consts.MSG_REPLY, self._box(res))
                except BaseException:
                    t, v, tb = sys.exc_info()
                    self._last_traceback = tb
                    boxed, raised = self._box_callback_error(t, v, tb)
                    reply = (consts.MSG_EXCEPTION, boxed)
            finally:
                self._context.marks = saved
            try:
                self._send(reply[0], seq, reply[1], {"reply": seq})
            except Exception:
                # The interrupt is the result; a channel that closed meanwhile is not.
                if raised is None:
                    raise
            if raised is not None:
                raise raised

        def _box_callback_error(self, t, v, tb):
            """A callback's exception boxed as the answer to the host's call, and what is to be
            raised here once it is sent: the exception when it is not an `Exception` -- an
            interrupt, a cancel, an exit -- and otherwise nothing.

            RPyC reads the exception's public attributes to box it, which runs the agent's code: a
            property can raise, be interrupted, or make a hosted call that raises the cancel
            waiting for the callback's own call. The answer is then an error that says which
            exception could not be sent and what failed, written without running any of the
            agent's code -- the host's call of the callback waits for it, holding its binding's
            turn -- and an interrupt or a cancel that came while boxing is raised as one the
            callback raised is. Any other failure leaves the connection as it was, rather than
            closing it as an undecodable frame does.

            An exception is classified by its type, never by `isinstance`, which reads the
            instance's `__class__`: agent code, here beneath frames an interrupt does not end."""
            raised = None if issubclass(t, Exception) else v
            try:
                return self._box_exc(t, v, tb), raised
            except BaseException as failed:
                if raised is None and not issubclass(type(failed), Exception):
                    raised = failed
                return (
                    _inert_error(
                        f"a callback raised {_raised(v)}, which could not be sent to the host: "
                        f"{_raised(failed)}"
                    ),
                    raised,
                )

        # ------------------------------------------------------------------ closing

        def close(self):
            """Mark closed and clean up, writing nothing: RPyC's `close` makes a synchronous
            request of the other side, which may be gone, and `__del__` reaches here."""
            if self._closed:
                return
            self._closed = True
            self._cleanup(_anyway=True)

        def close_with(self, reason):
            """Close for `reason`, telling the host first."""
            _rpc_channels.pop((self.agent, self.binding, self.number), None)
            with contextlib.suppress(Exception):
                _write_line(
                    _encode(
                        {
                            "t": "rpc",
                            "agent": self.agent,
                            "binding": self.binding,
                            "id": self.number,
                            "closed": reason,
                        }
                    )
                )
            self._channel.close(reason)
            self.close()

    return Hosted


# ---------------------------------------------------------------------------- kernels

_kernels = {}

# Whether the host has asked to be told of every take, which it asks once, before any kernel
# exists (`_observe`), so a kernel opened later is told as one opened first is.
_observing = False


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


def _hosted_frame(frame):
    """Whether `frame` is RPyC's, or of what a hosted call runs through here (`_HOSTED_CODE`)."""
    code = frame.f_code
    if frame.f_globals is globals():
        return code.co_qualname.partition(".")[0] in _HOSTED_CODE
    return _rpyc is not None and code.co_filename.startswith(
        os.path.dirname(_rpyc.__file__) + os.sep
    )


def _in_hosted_work(frame):
    """Whether, walking outward from `frame`, a hosted call's own work comes first: a
    `_hosted_frame` other than one that runs an object's own code (`_INTERRUPTIBLE_CODE`)."""
    while frame is not None:
        if _hosted_frame(frame):
            return frame.f_code not in _INTERRUPTIBLE_CODE
        frame = frame.f_back
    return False


def _kept(tb):
    """`tb` without its `_hosted_frame`s, as a traceback of its own: `tb` is left as it was, and is
    what comes back when none of its frames is one."""
    kept, dropped, link = [], False, tb
    while link is not None:
        if _hosted_frame(link.tb_frame):
            dropped = True
        else:
            kept.append(link)
        link = link.tb_next
    if not dropped:
        return tb
    rebuilt = None
    for link in reversed(kept):
        rebuilt = types.TracebackType(rebuilt, link.tb_frame, link.tb_lasti, link.tb_lineno)
    return rebuilt


@contextlib.contextmanager
def _without_hosted_frames(exc):
    """`exc`, and every exception chained to it or grouped in it, with `_hosted_frame`s left out of
    their tracebacks while the block formats it, and put back after: the exception is the agent's,
    which may hold it still. Only an exception that has such a frame is changed, and it is read and
    set through `BaseException`'s own descriptor, so that a class that refuses the assignment --
    a frozen dataclass, a read-only property -- reports as it would have."""
    saved, seen, todo = [], set(), [exc]
    try:
        while todo:
            e = todo.pop()
            if e is None or id(e) in seen:
                continue
            seen.add(id(e))
            tb = _traceback_of(e)
            kept = _kept(tb)
            if kept is not tb:
                saved.append((e, tb))
                _set_traceback(e, kept)
            todo += [_cause_of(e), _context_of(e)]
            if issubclass(type(e), BaseExceptionGroup):
                todo += _members_of(e)
        yield
    finally:
        for e, tb in saved:
            _set_traceback(e, tb)


def _format_error(exc):
    """The traceback from the submitted code's first frame on.

    The frames above it are this program's, and Python 3.13 quotes source for `-c` code, so
    keeping them would put the interpreter's own lines in front of every error. A compile error
    has no frame of the submission's at all, and is reported as the exception alone. Nor is the
    SIGINT handler's frame kept, where an interrupt ends the list: the interrupt is the result,
    and where the handler raised it is not. Nor are the frames a hosted call runs through, RPyC's
    and this program's, in the exception or in any chained to it or grouped in it: the agent's
    own -- where it made the call, and the callback the host called -- say what it needs.

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
        with _without_hosted_frames(exc):
            tb = _traceback_of(exc)
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
_type_bases = type.__dict__["__bases__"].__get__


def _raised(exc):
    """The name of `exc`'s type, as a plain `str`, for the result that reports it.

    Through `_type_name`, never `__name__`, which a metaclass can make raise; and copied by
    `str.__str__`, since the name itself can be a `str` subclass whose `encode` raises when the
    result is sent. Either would end the reply that has to report this, leaving the host's slot
    held for good.
    """
    return str.__str__(_type_name(type(exc)))


# ---------------------------------------------------------------------------- history

# The host keeps the agent's conversation, a turn at a time, and pushes each turn here as it
# commits it: one model call, what the model wrote, and each call it made with the result it read.
# Held here it is ordinary data, so the agent's code can read all of it and only what that code
# prints is seen. What each model call is sent of it is the host's to assemble; a promotion is how
# the agent asks for a turn to be sent again, and a demotion how it stops asking, each naming the
# turn by id, never by content.
#
# All of it is held for the rest of the session, under the same memory ceiling as everything
# else. An eighth of the ceiling is where that is said once on stderr, which the host logs; nothing
# is let go, and fetching bodies only when read is the response past it.


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

    `incomplete` is true when the round ended while the turn's calls ran, so some results are
    OutRig's note rather than what the code did. A call whose note says it had not returned may
    still have run, and its outcome may arrive later: do not run it again to find out.
    """

    id: int
    round: int
    prompt: str | None
    text: str
    calls: tuple[Call, ...]
    incomplete: bool = False

    def __repr__(self):
        incomplete = ", incomplete" if self.incomplete else ""
        return (
            f"<turn {self.id}, round {self.round}: {len(self.calls)} calls, {len(self.text)} "
            f"characters of text{incomplete}>"
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
        self._held = 0  # bytes of text held, roughly
        self._told = False

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
        rather than stopping every later one. Rebinding is the last step that can lose the turn;
        after it only its size is counted.
        """
        turns = self._turns
        if turns and turn.id <= turns[-1].id:
            return
        calls = (text for call in turn.calls for text in (call.source, call.result))
        size = sum(map(sys.getsizeof, (turn.prompt or "", turn.text, *calls)))
        self._turns = turns + (turn,)
        self._held += size
        self._tell_of_size()

    def _tell_of_size(self):
        """Say once, on stderr, when what is held passes an eighth of the memory ceiling."""
        if self._told:
            return
        ceiling = resource.getrlimit(resource.RLIMIT_DATA)[0]
        if ceiling == resource.RLIM_INFINITY or self._held <= ceiling // 8:
            return
        self._told = True
        _diag(
            f"runtime.history holds about {self._held >> 20} MiB, past an eighth of the "
            f"interpreter's {ceiling >> 20} MiB memory ceiling. It keeps every turn, so a longer "
            f"session leaves less of the ceiling for the agent's own work."
        )


class Context:
    """What each model call is sent of your conversation.

    Each call is sent the round in progress, the first rounds of the conversation, and the most
    recent ones. Everything else stays in `runtime.history.turns`, unsent, until you promote it:
    `runtime.context.promote(turn)`. A promoted turn is sent in its place in the conversation, not
    at the end, from the next model call on -- in this round, if your code promotes it while it
    runs -- until you demote it: `runtime.context.demote(turn)`.

    Each call is also held to the model's context window. When what was chosen does not fit, the
    window's turns are left out first, those nearest the unsent middle before the rest, then
    promoted turns, oldest first, then the round's own earlier turns. The turn your latest call
    made is always sent. So `promoted` is what you asked for, not a promise of what was sent; the
    line that opens a round says how many turns its first call leaves out.
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
        changes nothing. A promotion lasts until the turn is demoted.
        """
        self._change("promote", self._ids(turns))

    def demote(self, *turns):
        """Stop sending `turns` for having been promoted, from the model's next call on.

        Takes what `promote` takes. A demoted turn is still sent while the conversation's window
        holds it; demoting one that was never promoted changes nothing.
        """
        # Every id, not only those recorded as promoted: a promotion interrupted after its line was
        # written and before it was recorded here is still the host's.
        self._change("demote", self._ids(turns))

    def _change(self, kind, ids):
        """Tell the host to `kind` -- promote or demote -- `ids`, and record it here, as one step.

        The line is encoded first, so nothing that can fail sits between telling and recording. Both
        are done under the lock, so changes made at once from several threads reach the host in
        the order `promoted` records them, and the two cannot end up disagreeing.
        """
        if not ids:
            return
        line = _encode({"t": kind, "agent": self._agent, "turns": sorted(ids)})
        with self._lock:
            _write_line(line)
            if kind == "promote":
                self._promoted |= ids
            else:
                self._promoted -= ids

    def _ids(self, turns):
        """The ids `turns` name, each of a turn that has finished, or raise."""
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
        return ids

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

# What a contract may name -- the types with an obvious JSON form, and what is built from them --
# is `outrig.schema`'s to say, so a channel and a declared result never take different types.


def _describe_all(contract):
    return " | ".join(map(_schema._describe, contract)) if contract else "nothing"


def _contract(declared, where):
    """One direction's contract: the tuple of types it admits, and their `outrig.schema` schema.

    `declared` is a type, a union of types, or a tuple of them, `()` for a direction that carries
    nothing. Every type must be in the serializable subset, checked here rather than at the first
    send: anything else raises `TypeError` naming the type, and the field it is in when it is
    nested in a dataclass.
    """
    members = declared if isinstance(declared, tuple) else _union_members(declared)
    members = tuple(type(None) if member is None else member for member in members)
    return members, _schema._contract(members, where)


def _union_members(t):
    return typing.get_args(t) if typing.get_origin(t) in _schema._UNIONS else (t,)


class _Refused(Exception):
    """A message the host sent that its endpoint does not take. Nothing was queued."""


@dataclasses.dataclass(frozen=True)
class Delivery:
    """One message taken from an endpoint.

    `id` names the message -- on a request channel, the request it carries, which its handle's
    `id` names too -- `body` is the message itself, `sender` who sent it -- `"user"` on the user
    channel, an agent's id between agents -- and `received_at` when it arrived, in UTC. `skill`
    names the skill a `/name` the user typed resolved to, which asks you to run it, and is
    `None` on every other message.
    """

    id: int
    body: object
    sender: str
    received_at: datetime.datetime
    skill: object = dataclasses.field(default=None, kw_only=True)


@dataclasses.dataclass(frozen=True)
class RequestDelivery(Delivery):
    """A request taken from one of a request child's channels: answer it with
    `await delivery.reply(value)`, or end it unanswered with `await delivery.fail(message)`.

    `reply` checks `value` against the channel's reply type -- an instance of it, or its JSON form
    -- and returns once it is accepted; one that is not raises what is wrong, with each problem's
    path, and counts one of the request's attempts. Past the last, the request ends
    `outrig.CompletionRejected`. A request that has already ended -- answered, cancelled, released
    -- takes no answer: `reply` and `fail` raise.
    """

    _work: object = dataclasses.field(default=None, repr=False, compare=False)

    async def reply(self, value):
        """Answer the request with `value`; see `help(type(delivery))`."""
        await self._work.answer(value)

    async def fail(self, message):
        """End the request unanswered, its sender's handle raising `outrig.AgentRequestFailed`
        carrying `message`, a str saying why."""
        await self._work.fail(message)


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
        self._receives, self._receiving = _contract(receives, f"channel {name!r} cannot receive")
        self._sends, self._sending = _contract(sends, f"channel {name!r} cannot send")
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
        message, delivery = await self._when(self._receivers, self._take)
        if _observing:
            # Taken already, so nothing here may raise and lose it: what the host is told is a
            # record of the take, and a take it never hears of is one the record lacks.
            try:
                line = self._took(message)
                if line is not None:
                    _write_line(_encode(line))
            except BaseException as e:
                try:
                    _diag(f"took a message on {self._name!r}, but saying so failed: {e!r}")
                except BaseException:
                    pass
        return delivery

    def _took(self, message):
        """The line telling the host this endpoint's code took `message`, or `None` when the
        host keeps no record of takes here."""
        return {"t": "took", "agent": self._agent, "channel": self._name, "message": message}

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
        # As JSON: a dataclass crosses as its fields' values, under their keys, never as itself.
        body, problems = self._sending._encoding(message)
        if problems:
            kind = type(message)
            if self._sending._node.fits(message):
                # Its type is one the channel sends, so what is wrong is inside it.
                raise TypeError(
                    f"channel {self._name!r} cannot send this {_type_qualname(kind)}:\n"
                    f"{problems.listing()}"
                )
            raise TypeError(
                f"channel {self._name!r} sends {_describe_all(self._sends)}, not "
                f"{_type_name(kind)}"
            )
        # Encoded here rather than on the reserve: running out of memory while building the
        # agent's own message is the agent's to see.
        line = _encode({"t": "send", "agent": self._agent, "channel": self._name, "body": body})
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

    def _deliver(self, body, sender, message, skill=None):
        """Queue `body` from `sender`, posted as `message` -- a directive naming `skill`, when
        that is given -- and return how many messages then wait. Reader thread only.

        Raises `_Refused` for a body the contract does not take or a queue that is full, having
        queued nothing. The append is the last step that can fail, so a retry after running out
        of memory never queues a message twice; waking the receivers comes after, and cannot undo
        it.
        """
        # Checked as `send` checks an instance, which a body from the host -- plain JSON -- never
        # is: a dataclass the contract names is refused rather than built, since building one
        # runs the agent's code, and this is the reader thread. What is queued is the copy the
        # check makes.
        copy, problems = self._receiving._encoding(body)
        if problems:
            raise _Refused(
                f"channel {self._name!r} receives {_describe_all(self._receives)}, not "
                f"{_type_name(type(body))}"
            )
        delivery = Delivery(
            message, copy, sender, datetime.datetime.now(datetime.timezone.utc), skill=skill
        )
        with self._lock:
            if len(self._queue) >= QUEUE_MAX:
                raise _Refused(f"channel {self._name!r} already holds {QUEUE_MAX} unread messages")
            count, delivered = len(self._queue) + 1, self._delivered + 1
            wake = iter(self._receivers.copy())
            # Beside the id it was posted under, which a take is reported by.
            self._queue.append((message, delivery))
            # Only rebinding after the append, which cannot fail.
            self._delivered = delivered
        # Every receiver, since each re-checks and one may have been cancelled in the meantime.
        _wake_all(wake, self._name)
        return count


# What `return_when` may be: asyncio's own constants, which in CPython are strings.
_RETURN_WHEN = (asyncio.FIRST_COMPLETED, asyncio.FIRST_EXCEPTION, asyncio.ALL_COMPLETED)

# What a runtime says of the channels its agent has, and of how its work is answered: the one
# part of the text that differs between agents, so each runtime class states its own.
_USER_CHANNELS = """`runtime.channels` maps a name to the endpoint of a channel.
`runtime.channels["user"]` is how the user reaches you and you reach them: see
`help(type(runtime.channels["user"]))`."""
_WORK_CHANNELS = """`await runtime.complete(value)` answers the submission your parent gave you,
and is the only way to answer it: see `help(runtime.complete)`.

`runtime.channels` maps a name to the endpoint of a channel, and yours is
`runtime.channels["progress"]`, which says how far you have got. No user reaches you."""
_REQUEST_CHANNELS = """`runtime.channels` maps a name to the endpoint of a channel, and yours are
the request channels your parent declared: `d = await runtime.channels[name].receive()` takes
the next request, which you answer with `await d.reply(value)`, or end with
`await d.fail(message)`. You have no `runtime.complete`, and no user reaches you."""


def _runtime_doc(channels):
    """A runtime's docstring, `channels` being what that agent is told of its own channels.

    Every runtime's docstring is whole. A kernel binds `runtime` and none of the classes behind
    it, so one that deferred to another class would send the agent reading it to a name it cannot
    reach -- and the text deferred to would describe channels the agent may not have.
    """
    return f"""What this agent reaches beyond its own names.

{channels}

`await runtime.wait(fs)` waits as `asyncio.wait` does and watches every channel while it
waits: see `help(runtime.wait)`.

`runtime.names()` lists what you have bound, with the type of each value.

`runtime.history.turns` is your whole conversation, to read with ordinary Python at no cost in
context: see `help(runtime.history)`. Not all of it is sent to the model on each call:
`runtime.context.promote(turn)` sends a turn again until `runtime.context.demote(turn)`, and
every call is held to the model's context window: see `help(runtime.context)`.

`runtime.python` says what this interpreter can import -- `pip install` adds pure-Python
packages, and nothing compiled loads -- and where code that needs more can run.

`runtime.bindings` lists the objects the session hosts for you on the host, by name, with what
each is; asking makes no request to any of them.

`await runtime.spawn(name, prompt=...)` launches a child agent to give work to, and
`await runtime.release([child])` ends it: see `help(runtime.spawn)`.

Code in a module reaches the same runtime after `import outrig`, as `outrig.runtime`, and
each binding as `outrig.bindings[name]`; `outrig.schema` declares types, decodes JSON into
them strictly, and renders their JSON Schema."""


class Runtime:
    __doc__ = _runtime_doc(_USER_CHANNELS)

    # These and nothing else. The agent reads a runtime and never adds to one, and without this
    # `help(runtime)` spends a tenth of its bound on `__dict__` and `__weakref__`.
    __slots__ = ("_channels", "_held", "_history", "_context", "_kernel")

    MessageAvailable = MessageAvailable

    def __init__(self, channels, held, history, context, kernel):
        self._channels = types.MappingProxyType(channels)
        self._held = held
        self._history = history
        self._context = context
        self._kernel = kernel

    def __getattr__(self, name):
        if name == "complete":
            child = self._kernel.child
            if child is None:
                raise AttributeError(
                    "runtime.complete is a work child's, which completes the submission its parent "
                    "gave it; this agent is no child"
                )
            raise AttributeError(
                "this child answers requests, and has no runtime.complete: take one with "
                "`d = await runtime.channels[name].receive()`, then answer it with "
                "`await d.reply(value)`, or end it with `await d.fail(message)`"
            )
        raise AttributeError(f"'runtime' has no attribute {name!r}")

    async def spawn(self, name, *, prompt, model=None, inputs=None, requests=None):
        """Launch a child agent, and return its `Child` once it exists.

        The child is a kernel of its own in this interpreter -- its own names, the session's
        bindings, no user channel -- and the host runs its rounds, each model call one of the
        session's. `prompt` is the first part of its instructions. `model` names a configured
        model or alias; without it, the child runs on yours. `inputs`, a dict of names to values,
        is bound in its namespace as variables, copies, before it starts: each must be data.

        Without `requests`, it is a work child: give it work with `child.submit(inputs,
        result=T)`, and it completes each with `runtime.complete`. With `requests`, a dict of
        channel names to `outrig.Request(request_type, reply_type, doc=...)` or a bare
        `(request_type, reply_type)`, it is a request child, answering requests on those
        channels: `child.channels[name].request(body)` sends one. Either way you get a handle
        to await.

        A spawn past the session's `children-max`, or from an agent at `subagent-depth-max`,
        raises `outrig.AgentLimitReached` at once rather than waiting; an unknown model raises
        `ValueError`, naming the configured ones, and so does a prompt that with the rest of the
        spawn comes to more than 1 MiB.
        """
        kernel = self._kernel
        if type(name) is not str or not name or len(name) > 64:
            raise ValueError(f"a child's name is a non-empty str of at most 64 characters, "
                             f"not {name!r}")
        if type(prompt) is not str:
            raise TypeError(f"prompt is a str, not {_type_qualname(type(prompt))}")
        if model is not None and type(model) is not str:
            raise TypeError(f"model names a configured model, a str, not "
                            f"{_type_qualname(type(model))}")
        _refuse_closing()
        manifest, copies, classes, _ = _inputs(inputs, "spawn")
        channels, said, bound = {}, [], []
        if requests is not None:
            if type(requests) is not dict or not requests:
                raise TypeError("requests is a non-empty dict of channel names to "
                                "outrig.Request(request_type, reply_type)")
            for channel, declared in requests.items():
                if type(channel) is not str or not channel:
                    raise ValueError(f"a request channel is named by a non-empty str, "
                                     f"not {channel!r}")
                if channel == "user":
                    raise ValueError("'user' is the user channel's name, which no request channel "
                                     "may take")
                if type(declared) is tuple and len(declared) == 2:
                    declared = _agents.Request(*declared)
                if type(declared) is not _agents.Request:
                    raise TypeError(f"channel {channel!r} is declared by outrig.Request or a "
                                    f"(request_type, reply_type) tuple, not {declared!r}")
                request, used = _agents.declared_with_classes(
                    declared.request_type, f"channel {channel!r}'s requests cannot be")
                reply, also = _agents.declared_with_classes(
                    declared.reply_type, f"channel {channel!r}'s replies cannot be")
                bound += used + also
                channels[channel] = _Channel(declared.request_type, declared.reply_type, request,
                                             reply, declared.doc)
                said.append({"name": channel, "doc": declared.doc,
                             "request_type": _schema._describe(declared.request_type),
                             "reply_type": _schema._describe(declared.reply_type),
                             "request": request.text(), "reply": reply.text()})
        bound = list(dict.fromkeys(bound))
        _agents.one_name_each(bound, copies)
        _agents.one_name_each(bound + sorted(classes, key=_type_qualname))
        kind = "request" if requests is not None else "work"
        spawn = next(_ids)
        line = {"t": "agent.spawn", "agent": kernel.agent, "id": spawn, "name": name,
                "prompt": prompt, "model": model, "kind": kind, "manifest": manifest,
                "channels": said,
                "invocations": [] if _skills is None else _skills._current()}
        _refuse_long(line, "spawn", "shorten the prompt, or give fewer inputs or channels")
        _spawning[spawn] = {"parent": kernel, "name": name, "kind": kind, "channels": channels,
                            "inputs": copies, "classes": bound}
        try:
            # Inside the `try`, so a line that could not be written leaves no entry behind.
            ask = kernel.ask(line)
            answer = await asyncio.shield(ask)
        except asyncio.CancelledError:
            # Given up on: a child the host launches anyway is released, since nothing holds it.
            ask.add_done_callback(functools.partial(_release_unheld, kernel))
            raise
        finally:
            _spawning.pop(spawn, None)
        error = answer.get("error")
        if error is not None:
            text = error.get("message", "the host refused the spawn")
            raise {
                "limit": _agents.AgentLimitReached,
                "closed": _agents.SessionClosing,
                "released": _agents.AgentReleased,
                "model": ValueError,
            }.get(error.get("kind"), RuntimeError)(text)
        return Child(_children[answer["child"]])

    async def release(self, children):
        """Release each of `children`, the `Child` objects of agents you spawned, all of them or
        none: a list naming one twice, or one you did not spawn or have released, is refused
        whole with `ValueError`.

        Releasing a child ends what it holds: every request still open on it ends
        `outrig.AgentReleased`, its own children are released the same way, its tasks are
        cancelled and its kernel removed. A child whose code never yields cannot be stopped, and
        stays, `wedged`, until the session ends. Returns once each has ended or been found wedged.
        """
        ids = []
        for child in children:
            if type(child) is not Child:
                raise TypeError(f"release() takes the Child objects runtime.spawn returned, "
                                f"not {_type_qualname(type(child))}")
            ids.append(child.id)
        if not ids:
            return
        line = {"t": "agent.release", "agent": self._kernel.agent, "id": next(_ids),
                "children": ids}
        _refuse_long(line, "release", "release fewer children at once")
        answer = await asyncio.shield(self._kernel.ask(line))
        if answer.get("error") is not None:
            raise ValueError(answer["error"])

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

    @property
    def bindings(self):
        """The session's bindings, by name, in the order the session gave them. Read-only.

        Each is a `Binding`: its `name`, the global it is bound as; its `description`, as the
        operator wrote it; and its `type`, the qualified name of the type of the object it holds on
        the host. The answer comes from what the session said when it started, so asking reaches
        no binding, runs no `repr()`, and reads no property.
        """
        return _BINDINGS

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
        # A handle, or a skill's invocation, is waited on through its view, and comes back as
        # itself.
        handles = {}
        for f in [f for f in fs if type(f) is Handle or type(f) is _skills.Invocation]:
            handles[f.future] = f
            fs.discard(f)
            fs.add(f.future)
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
                        pending = fs - done
                        return ({handles.get(f, f) for f in done},
                                {handles.get(f, f) for f in pending})
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


class WorkRuntime(Runtime):
    __doc__ = _runtime_doc(_WORK_CHANNELS)

    __slots__ = ()

    async def complete(self, value):
        """Complete the submission you were given with `value`, an instance of its result type --
        bound in your namespace under its name -- or that instance's JSON form.

        It returns once the value is accepted, and the submission is then done. One that does not
        check raises what is wrong, each problem with its path, and counts one of the
        submission's attempts: correct it and complete again. Past the last attempt the
        submission ends rejected; one that has ended takes no answer, so this raises.
        """
        work = self._kernel.child.last
        if work is None:
            raise RuntimeError("no submission has been given to you yet, so there is nothing to "
                               "complete")
        await work.answer(value)


class RequestRuntime(Runtime):
    __doc__ = _runtime_doc(_REQUEST_CHANNELS)

    __slots__ = ()


# ---------------------------------------------------------------------------- child agents
#
# Agent code launches a child with `runtime.spawn`: a kernel of this interpreter, with a thread and
# a loop of its own, which the host drives through rounds as it drives the primary. Its parent gives
# it work -- a submission, to a work child, or a request on one of a request child's channels --
# and each piece of work is a `_Work` both kernels hold: the sender's handle reads it, the child
# answers it, and its lock decides how it ends, whichever reaches it first of an answer, a cancel,
# the child's release, the session's close.
#
# What moves between the two is Python objects, never JSON: each crosses as a deep copy, so the two
# agents share no state, and is checked against its declared type by whoever sends it. The host
# hears of each step as a notice -- what its record of the session, and its rule for when a child
# gets a round, are made of -- and never carries the objects.

_ids = itertools.count(1)  # every id the interpreter gives: a request, an ask, a message
_works = {}  # each request not yet ended, by id
_launching = {}  # each typed call (`outrig.agent`) not yet ended, by its id, the submission's
_works_lock = threading.Lock()
# What a typed call's status reads when its launch -- its spawn, or its submission -- was refused.
_REFUSED = {"AgentLimitReached": "limit", "SessionClosing": "closed", "AgentReleased": "released"}
# Each child agent's record, by its agent id, for as long as anything holds it: its parent's `Child`,
# a request made of it, its kernel. The host says what each is doing for as long as its parent can
# ask, its release included.
_children = weakref.WeakValueDictionary()
_spawning = {}  # what a spawn waiting on the host holds for its child, by the spawn's id
_ENDED = ("accepted", "failed")
_agents = None  # `outrig._agents`, once the package is loaded


def _notify(message):
    """Tell the host one step of an agent's work with a child. Nothing here raises: the step has
    happened, and a record that lacks it is the cost of a host that could not be told."""
    try:
        _write_line(_encode(message))
    except BaseException as e:
        try:
            _diag(f"telling the host of {message.get('t')!r} failed: {e!r}")
        except BaseException:
            pass


# How long a request about a skill waits for the host's answer. The host gives up on a source
# well before this, so only an answer that was lost reaches it -- and a child's kernel, which no
# interrupt reaches, still never waits for good.
ASK_TIMEOUT = 60
# Where the reader thread puts the host's answer to each request about a skill still waited for,
# by the request's id.
_answers = {}
# This thread's request about a skill that an interrupt may end, as `left`: a list where an
# interrupt's signal that could not raise where it landed -- in the line's write, say -- leaves its
# exception, for the wait to raise.
_asking = threading.local()


def _ask_host(line):
    """Send `line`, a request about a skill, and return the host's answer to it, a line with `ok`
    or with `error`. Blocks only the calling thread, which may be any but the reader's: an
    import on a kernel's loop holds that loop meanwhile, as a hosted call does.

    An interrupt of the primary raises in the wait -- or, by `_on_sigint`'s rule, is left for it
    when it landed in this program's machinery, and raised rather than any answer, for a request
    agent code made: one this program makes for itself is not an interrupt's to end. The wait
    looks for one every `SIGNAL_CHECK` seconds, since the handler cannot wake it. An answer that
    comes after its asker gave up is dropped."""
    if threading.current_thread() is _reader_thread or _PROTO_OUT is None:
        where = "from a forked process" if _PROTO_OUT is None else "from the reader thread"
        return {"error": {"code": "source-error", "message": f"the host cannot be asked {where}"}}
    ask_id = line["id"] = next(_ids)
    execution = _CURRENT.get()
    interruptible = execution is not None and _agent_code_outward(
        sys._getframe(), execution.kernel
    )
    outer = getattr(_asking, "left", None)
    try:
        answers = _answers[ask_id] = queue.SimpleQueue()
        left = _asking.left = [] if interruptible else None
        _write_line(_encode(line))
        deadline = time.monotonic() + ASK_TIMEOUT
        answer = None
        while answer is None and not left:
            wait = deadline - time.monotonic()
            if wait <= 0:
                text = f"the host did not answer within {ASK_TIMEOUT} s"
                return {"error": {"code": "source-error", "message": text}}
            with contextlib.suppress(queue.Empty):
                answer = answers.get(timeout=min(wait, SIGNAL_CHECK))
        if left:
            raise left[0]
        return answer
    finally:
        _answers.pop(ask_id, None)
        _asking.left = outer


def _refuse_long(message, what, how):
    """Raise `ValueError`, saying `how` to shorten it, if `message` -- a `what` agent code sends
    the host -- would be a protocol line longer than `MESSAGE_MAX`, which bounds a channel message
    too. The host drops a line past its own bound unread, and answers nothing to it."""
    size = len(_encode(message))
    if size > MESSAGE_MAX:
        raise ValueError(f"this {what} is {size} bytes as sent, past the {MESSAGE_MAX} allowed; "
                         f"{how}")


def _loose_preview(value):
    """A preview of `value`, which may not be data: what has no JSON form is named by its type."""
    try:
        text = json.dumps(value, default=lambda o: f"<{_type_qualname(type(o))}>")
    except (TypeError, ValueError, RecursionError):
        text = json.dumps(f"<{_type_qualname(type(value))}>")
    return _agents.preview(json.loads(text))


def _answered(declared, value):
    """`(accepted, problems)`: `value`, an answer, as the value it is accepted as -- a copy of an
    instance of the declared type, or what its JSON form decodes into -- or what is wrong with it,
    each with its path."""
    _, problems = declared._encoding(value)
    if not problems:
        return copy.deepcopy(value), ()
    if not _holds_instances(value):
        try:
            return declared.decode(value), ()
        except _schema.DecodeError as e:
            return None, e.problems
    return None, tuple(problems.found)


def _holds_instances(value, depth=0):
    """Whether `value` holds a dataclass instance anywhere: it is then an instance, not a JSON
    form, and is checked as one."""
    if depth > 100:
        return False
    if dataclasses.is_dataclass(type(value)):
        return True
    if type(value) is list:
        return any(_holds_instances(item, depth + 1) for item in value)
    if type(value) is dict:
        return any(_holds_instances(item, depth + 1) for item in value.values())
    return False


def _copy_or_none(value):
    try:
        return copy.deepcopy(value)
    except Exception:
        return None


def _hosted_name(value):
    """What `value` is, when a binding holds it on the host: for the message refusing it."""
    kind = type(value)
    if kind is HostedRoot or (_rpyc is not None and issubclass(kind, _rpyc.core.netref.BaseNetref)):
        return (
            "an object a binding holds on the host; a child reaches the session's bindings itself, "
            "by name, so give it the name, or the data you read from the object"
        )
    return None


def _inputs(inputs, where):
    """`(manifest, copies, classes, encoded)` for `inputs`, the dict of names to values one agent
    gives another: each checked to be data, previewed, copied for the child, and as JSON."""
    if inputs is None:
        return [], {}, set(), {}
    if type(inputs) is not dict:
        raise TypeError(f"{where} takes its inputs as a dict of names to values, not "
                        f"{_type_qualname(type(inputs))}")
    manifest, copies, classes, encodings = [], {}, set(), {}
    for name, value in inputs.items():
        if (
            type(name) is not str
            or not name.isidentifier()
            or keyword.iskeyword(name)
            or name.startswith("__")
            or name in _agents.RESERVED
        ):
            raise ValueError(f"{where}: input {name!r} cannot name a variable in the child")
        encoded, shape, found = _agents.walk(value, name, _hosted_name)
        manifest.append({"name": name, "type": shape, "preview": _agents.preview(encoded)})
        copies[name] = copy.deepcopy(value)
        classes |= found
        encodings[name] = encoded
    return manifest, copies, classes, encodings


def _attempts(attempts):
    if type(attempts) is not int:
        raise TypeError(f"attempts is an int, not {_type_qualname(type(attempts))}")
    if attempts < 1:
        raise ValueError(f"attempts must be at least 1, not {attempts}")
    return attempts


def _refuse_closing():
    if _closing is not None:
        raise _agents.SessionClosing(f"the session is closing -- {_closing} -- so nothing was sent")


def _release_unheld(kernel, ask):
    """Release the child a spawn nobody waited for launched, if it launched one."""
    if ask.cancelled():
        return
    child = ask.result().get("child")
    if child is not None:
        _notify({"t": "agent.release", "agent": kernel.agent, "id": next(_ids), "children": [child]})


def _sending_kernel(what):
    """The kernel of the code sending `what`: the one whose loop its handle settles on."""
    kernel = _calling()
    if kernel is None:
        raise RuntimeError(f"{what} is sent from agent code, and this runs outside every execution")
    return kernel


@dataclasses.dataclass(frozen=True)
class _Channel:
    """One of a request child's channels, as its spawn declared it."""

    request_type: object
    reply_type: object
    request: object  # the request type's Schema
    reply: object  # the reply type's Schema
    doc: object


class _Child:
    """What a child agent's parent and its kernel share: its queue of submissions, its limit on
    requests, and the state the host last said it was in."""

    def __init__(self, agent, name, kind, parent, channels, requests_max):
        self.id = agent
        self.name = name
        self.kind = kind  # "work" or "request"
        self.parent = parent  # the parent's kernel
        self.channels = channels  # name -> _Channel, for a request child
        self.requests_max = requests_max
        self.lock = threading.Lock()
        self.state = "idle"
        self.unsettled = 0
        self.queue = collections.deque()  # submissions not yet given to it
        self.last = None  # the submission given to it last
        self.kernel = None
        self.released = False

    def __str__(self):
        return f"child {self.name!r} ({self.id})"

    def take(self, what):
        """Count one more request unsettled on it, or raise why it takes none."""
        with self.lock:
            if self.released:
                raise _agents.AgentReleased(f"{self} was released, so it takes no {what}")
            if self.unsettled >= self.requests_max:
                raise _agents.AgentLimitReached(
                    f"{self} already holds {self.requests_max} unsettled requests, its "
                    f"requests-max, so nothing was sent; cancel one, or wait for an answer"
                )
            self.unsettled += 1

    def settled(self, work):
        with self.lock:
            self.unsettled -= 1

    def withdraw(self, work):
        """Take `work`, not yet received, back out of the child's reach. Whether it was there:
        one that is not was received already."""
        if work.kind == "submission":
            with self.lock:
                try:
                    self.queue.remove(work)
                except ValueError:
                    return False
                return True
        endpoint = None if self.kernel is None else self.kernel.channels.get(work.channel)
        return type(endpoint) is RequestEndpoint and endpoint._withdraw(work)


class _Work:
    """One request an agent made of a child: a submission, or a request on one of its channels.

    Both kernels hold it. Its state changes under its lock, so it ends once, with whatever reached
    it first, and nothing after changes that.
    """

    def __init__(self, child, sender, kind, channel, body, declared, attempts_max, work_id=None):
        self.id = next(_ids) if work_id is None else work_id
        self.child = child
        self.sender = sender  # the sending kernel, whose loop its handle settles on
        self.kind = kind  # "submission" or "request"
        self.channel = channel  # None for a submission
        self.body = body  # what the child is given: the request, or the inputs and classes
        self.declared = declared  # the answer's Schema
        self.attempts_max = attempts_max
        self.attempts = 0
        self.lock = threading.Lock()
        self.state = "queued" if kind == "submission" else "waiting"
        self.reason = None
        self.value = None
        self.error = None
        self.final = None
        self.future = sender.loop.create_future()
        self.progress = None  # a submission's progress, as its sender reads it
        # A typed call's validator, and the call's arguments it is called with; `None` otherwise.
        self.validate = None
        self.arguments = None

    def __str__(self):
        if self.channel is None:
            return f"submission {self.id} to {self.child}"
        return f"request {self.id} on {self.child}'s channel {self.channel!r}"

    def status(self):
        with self.lock:
            if self.final is not None:
                return self.final
            return _agents.Status(self.state, None, self.child.state)

    def end(self, reason, *, value=None, error=None, **said):
        """End the request: accepted with `value` when `reason` is `None`, failed with `error` for
        `reason` otherwise. Whether this ended it: one that had ended is left as it was. Any
        thread."""
        with self.lock:
            if self.state in _ENDED:
                return False
            self.state = "accepted" if reason is None else "failed"
            self.reason = reason
            self.value, self.error = value, error
            self.final = _agents.Status(self.state, reason, self.child.state)
        with _works_lock:
            _works.pop(self.id, None)
        self.child.settled(self)
        _notify({"t": "agent.ended", "agent": self.child.id, "request": self.id,
                 "outcome": reason or "accepted", **said})
        try:
            self.sender.loop.call_soon_threadsafe(self._settle)
        except RuntimeError:
            pass  # the sender's loop has closed with its kernel: nothing waits on this
        return True

    def _settle(self):
        if self.future.done():
            return
        if self.reason is None:
            self.future.set_result(self.value)
        else:
            self.future.set_exception(self.error)
            # Read here: how it failed is the handle's to report, and a handle nobody awaits is not
            # a mistake in the code that dropped it.
            self.future.exception()

    def view(self):
        """A future of the sender's loop that settles as the request ends, and whose cancelling
        cancels nothing else."""
        view = self.sender.loop.create_future()

        def settle(future):
            if view.done():
                return
            error = future.exception()
            if error is None:
                view.set_result(future.result())
            else:
                view.set_exception(error)
                view.exception()

        if self.future.done():
            settle(self.future)
        else:
            self.future.add_done_callback(settle)
        return view

    def refused(self):
        """What an answer to this request, which has ended, raises; the host is told."""
        reason = self.reason
        _notify({"t": "agent.refused", "agent": self.child.id, "request": self.id,
                 "ended": reason or "accepted"})
        if reason == "cancelled":
            return _agents.RequestCancelled(f"{self} was cancelled by its sender; it takes no answer")
        return _agents.RequestSettled(
            f"{self} has already ended ({reason or 'answered'}), so it takes no further answer",
            reason or "accepted",
        )

    async def answer(self, value):
        """Answer the request with `value`: what `runtime.complete` and `delivery.reply` do."""
        with self.lock:
            ended = self.state in _ENDED
            attempt = self.attempts + 1
        if ended:
            raise self.refused()
        accepted, problems = _answered(self.declared, value)
        if not problems and self.validate is not None:
            # A validator that raises, whatever it raises, or returns what is not a list of str,
            # is a defect in the sender's code, which another attempt cannot fix: the request ends
            # failed with it, and no attempt is counted.
            try:
                found = self.validate(accepted, **self.arguments)
                if type(found) not in (list, tuple) or any(type(p) is not str for p in found):
                    raise TypeError(f"it returned {found!r}, not a list of str")
            except BaseException as e:
                try:
                    message = str(e)
                except BaseException:
                    message = "<a message that could not be read>"
                text = _agents.cut(f"the validator raised {_type_qualname(type(e))}: {message}")
                if not self.end("failed", error=e, message=text):
                    raise self.refused()
                raise RuntimeError(text)
            # Bounded as the decoder's problems are, so the notice and the error stay lines the
            # host reads: `PROBLEMS_MAX` of them, each cut, and the rest counted.
            kept = found[: _schema.PROBLEMS_MAX]
            problems = tuple(_schema.Problem("$", _agents.cut(text)) for text in kept)
            if len(found) > len(kept):
                problems += (_schema.Problem("$", f"... and {len(found) - len(kept)} more"),)
        said = None
        if _observing:
            said = (_agents.preview(self.declared.encode(accepted)) if not problems
                    else _loose_preview(value))
        _notify({"t": "agent.answered", "agent": self.child.id, "request": self.id,
                 "attempt": attempt, "problems": [str(p) for p in problems], "value": said})
        with self.lock:
            ended = self.state in _ENDED
            if not ended:
                self.attempts = attempt
        if ended:
            raise self.refused()
        if problems:
            listing = "\n".join(f"  {problem}" for problem in problems)
            if attempt >= self.attempts_max:
                text = (f"the answer to {self} was not accepted, and that was attempt {attempt} of "
                        f"{self.attempts_max}: it has ended rejected, and takes no answer now:\n"
                        f"{listing}")
                rejected = _agents.CompletionRejected(text, _copy_or_none(value), problems)
                if not self.end("rejected", error=rejected):
                    raise self.refused()
                raise _agents.CompletionRejected(text, value, problems)
            raise _schema.DecodeError(
                f"the answer to {self} was not accepted (attempt {attempt} of "
                f"{self.attempts_max}); correct what is listed and answer again:\n{listing}",
                problems,
            )
        if not self.end(None, value=accepted):
            raise self.refused()

    async def fail(self, message):
        if type(message) is not str:
            raise TypeError(f"fail() takes a str saying why, not {_type_qualname(type(message))}")
        if not self.end("failed", error=_agents.AgentRequestFailed(message),
                        message=_agents.cut(message)):
            raise self.refused()

    def cancel(self):
        with self.lock:
            if self.state in _ENDED:
                return False
        # Out of the child's reach first, so it never takes what was cancelled before it did.
        received = not self.child.withdraw(self)
        return self.end("cancelled", error=_agents.RequestCancelled(f"{self} was cancelled"),
                        received=received)


class Handle:
    """One submission or request an agent made of a child: what `child.submit(...)` and
    `child.channels[name].request(...)` return, at once, the work already sent.

    `await handle` gives the accepted answer, an instance of the declared type, or raises how the
    request ended -- an `outrig.AgentError` -- and awaiting again gives the same. Cancelling the
    code awaiting it cancels that wait and never the request; `handle.cancel()` cancels the
    request: one the child has not received never reaches it, and one it has ends so that its
    answer raises `outrig.RequestCancelled`. `done()` and `result()` answer without waiting, as a
    future's do, and `status` says what it is doing (see `help(outrig.Status)`). `future` is a
    view `asyncio.wait` takes; `runtime.wait` and `asyncio.gather` take the handle itself. A
    submission's `progress` is an endpoint of what the child reports while it works on it.
    """

    __slots__ = ("_work", "_future")

    def __init__(self, work):
        self._work = work
        self._future = None

    @property
    def id(self):
        """The request's id: what the child's delivery of it, and the session's events, name."""
        return self._work.id

    def __await__(self):
        return self._work.view().__await__()

    @property
    def future(self):
        """A future that settles as the request ends; cancelling it cancels nothing else."""
        future = self._future
        if future is None or future.cancelled():
            future = self._future = self._work.view()
        return future

    def done(self):
        """Whether the request has ended."""
        return self._work.state in _ENDED

    def result(self):
        """The accepted answer, or the exception the request ended with, raised; raises
        `asyncio.InvalidStateError` while it has not ended."""
        work = self._work
        with work.lock:
            state, value, error = work.state, work.value, work.error
        if state not in _ENDED:
            raise asyncio.InvalidStateError(f"{work} has not ended")
        if error is not None:
            raise error
        return value

    def cancel(self):
        """Cancel the request; whether this ended it. Not the child, which takes more work."""
        return self._work.cancel()

    @property
    def status(self):
        """What the request is doing, and its child: see `help(outrig.Status)`."""
        return self._work.status()

    @property
    def progress(self):
        """An endpoint of what the child reports on its `progress` channel while it works on this
        submission: `await receive()` takes the next, as a `Delivery`. Only a submission has one."""
        if self._work.progress is None:
            raise AttributeError("a request on a channel has no progress; a submission's handle does")
        return self._work.progress

    def __repr__(self):
        return f"<handle of {self._work}: {self._work.status()}>"


class ChildChannel:
    """A parent's end of one of a request child's channels: `request(body)` sends a request and
    returns its handle, and `pending()` counts those the child has not taken yet."""

    __slots__ = ("_child", "_name")

    def __init__(self, child, name):
        self._child = child
        self._name = name

    @property
    def request_type(self):
        """What a request on this channel carries."""
        return self._child.channels[self._name].request_type

    @property
    def reply_type(self):
        """What the child's reply to one is."""
        return self._child.channels[self._name].reply_type

    def request(self, body, *, attempts=3):
        """Send `body`, of the channel's request type, as a request, and return its handle at once.

        The child receives a copy. Its reply is checked against the reply type, and one that fails
        counts one of `attempts`: past them the request ends `outrig.CompletionRejected`. A child
        holding its `requests-max` unsettled requests takes none: this raises
        `outrig.AgentLimitReached`, sending nothing.
        """
        child, channel = self._child, self._child.channels[self._name]
        _refuse_closing()
        attempts = _attempts(attempts)
        encoded, problems = channel.request._encoding(body)
        if problems:
            raise TypeError(f"channel {self._name!r} takes {_describe_all((channel.request_type,))}, "
                            f"and this request is not one:\n{problems.listing()}")
        kernel = _sending_kernel("a request")
        body = copy.deepcopy(body)
        child.take("request")
        work = _Work(child, kernel, "request", self._name, body, channel.reply, attempts)
        with _works_lock:
            _works[work.id] = work
        _notify({"t": "agent.request", "agent": kernel.agent, "request": work.id, "child": child.id,
                 "channel": self._name, "attempts": attempts,
                 "body": _agents.preview(encoded) if _observing else None})
        endpoint = None if child.kernel is None else child.kernel.channels.get(self._name)
        if type(endpoint) is not RequestEndpoint or not endpoint._put(work):
            work.end("released", error=_agents.AgentReleased(f"{child} was released"))
        return Handle(work)

    def pending(self):
        """How many requests on this channel the child has not taken yet."""
        endpoint = None if self._child.kernel is None else self._child.kernel.channels.get(self._name)
        return endpoint.pending() if type(endpoint) is RequestEndpoint else 0

    def __repr__(self):
        return f"<{self._child}'s channel {self._name!r}: {self.pending()} pending>"


class Child:
    """A child agent `runtime.spawn` launched, from its parent's side.

    `id` names it in the session -- its events' subject is `agent/<id>` -- and `name` is what the
    spawn called it. A work child (`kind` "work") takes submissions: `submit(inputs, result=T)`
    gives it one and returns its handle. A request child ("request") answers requests on its
    channels: `channels[name].request(body)` sends one and returns its handle. `status` is what the
    host last said of it: `running`, `idle`, `wedged` or `released`. Release it with
    `await runtime.release([child])`.
    """

    __slots__ = ("_child", "channels")

    def __init__(self, child):
        self._child = child
        self.channels = types.MappingProxyType(
            {name: ChildChannel(child, name) for name in child.channels}
        )

    @property
    def id(self):
        return self._child.id

    @property
    def name(self):
        return self._child.name

    @property
    def kind(self):
        return self._child.kind

    @property
    def status(self):
        return self._child.state

    def submit(self, inputs, *, result, attempts=3):
        """Give this work child `inputs`, a dict of names to values, to work on, its answer a
        `result`; returns the submission's handle at once.

        Each input is bound in the child as a variable of its name, a copy: it must be data --
        `None`, a str, int, bool or float, a list or str-keyed dict of them, or a dataclass
        instance -- so an object a binding holds is refused, and the child reaches the session's
        bindings itself. `result` is declared with `outrig.schema`, and it and the dataclasses it
        uses are bound in the child under their names. The child completes with
        `await runtime.complete(value)`; an answer that fails its check counts one of `attempts`,
        and past them the submission ends `outrig.CompletionRejected`. The child works on one
        submission at a time, so a later one waits, `queued`. One whose inputs and result type
        take more than 1 MiB to describe raises `ValueError`, sending nothing.
        """
        child = self._child
        if child.kind != "work":
            raise TypeError(f"{child} answers requests on its channels and takes no submissions; "
                            f"send one with child.channels[name].request(body)")
        _refuse_closing()
        attempts = _attempts(attempts)
        kernel = _sending_kernel("a submission")
        return Handle(_submit(child, kernel, inputs, result, attempts))

    def __repr__(self):
        return f"<{self._child}, a {self._child.kind} child, {self._child.state}>"


def _submit(child, kernel, inputs, result, attempts, call=None):
    """Queue a submission of `inputs` on work child `child` from `kernel`, and return its `_Work`:
    what `Child.submit` does, and a typed call, whose `call` gives the submission its id, its
    progress endpoint and its validator."""
    manifest, copies, classes, encoded = _inputs(inputs, "submit")
    declared, used = _agents.declared_with_classes(result, "the result of a submission cannot be")
    _agents.one_name_each(used, copies)
    _agents.one_name_each(list(used) + sorted(classes, key=_type_qualname))
    work = _Work(child, kernel, "submission", None, {"inputs": copies, "classes": used}, declared,
                 attempts, None if call is None else call.id)
    notice = {"t": "agent.request", "agent": kernel.agent, "request": work.id, "child": child.id,
              "channel": None, "attempts": attempts, "manifest": manifest,
              "result": {"type": _schema._describe(result), "schema": declared.text()},
              "body": _agents.preview(encoded) if _observing else None}
    _refuse_long(notice, "submission", "submit fewer inputs, or a smaller result type")
    child.take("submission")
    if call is None:
        work.progress = _Progress(kernel.agent, child.id, work.id)
    else:
        work.progress = call.progress
        work.validate = call.declaration.validate
        work.arguments = call.arguments
    with _works_lock:
        _works[work.id] = work
    # Queued and told of in one step against the assign that gives the child its next
    # submission: the host hears of it once it is queued, and the child takes it only once the
    # host has heard, so what the child says of it names a request the host knows.
    with child.lock:
        queued = not child.released
        if queued:
            child.queue.append(work)
        _notify(notice)
    if not queued:
        work.end("released", error=_agents.AgentReleased(f"{child} was released"))
    return work


class _Progress(Endpoint):
    """A submission's progress, from its sender's side: what the child sends on its `progress`
    channel while it works on the submission, as `Delivery` envelopes. Reading it never ends the
    submission."""

    def __init__(self, agent, child, request):
        super().__init__(agent, "progress", receives=str, sends=())
        self._child = child
        self._request = request

    def _took(self, message):
        # Recorded as the child sent it, on its request; when its sender reads it is its own.
        return None


class _Call(_Work):
    """A typed call, `@outrig.agent`'s: the request its handle reads from the call on, before any
    child exists. Its id is its submission's; the submission is `work` once the child exists, and
    the call ends -- its future settles -- once the child has been released, so a caller awaiting
    it finds the child's place free."""

    def __init__(self, declaration, arguments, sender):
        super().__init__(None, sender, "submission", None, None, declaration.schema, 3)
        self.declaration = declaration
        self.arguments = arguments
        self.invocation = _invocation()
        self.progress = _Progress(sender.agent, None, self.id)
        self.work = None  # the submission, once made
        self.task = None

    def __str__(self):
        return f"call {self.id} of {self.declaration.name}"

    def status(self):
        with self.lock:
            if self.final is not None:
                return self.final
        if self.work is not None:
            return self.work.status()
        return _agents.Status("queued", None, "idle")

    def end(self, reason, *, value=None, error=None, **said):
        """Record how the call ended, which its handle reads at once; the future is the task's to
        settle, once the child is released. Whether this ended it. Any thread."""
        with self.lock:
            if self.state in _ENDED:
                return False
            self.state = "accepted" if reason is None else "failed"
            self.reason = reason
            self.value, self.error = value, error
            self.final = _agents.Status(
                self.state, reason, "released" if self.child is None else self.child.state
            )
        with _works_lock:
            _launching.pop(self.id, None)
        return True

    def settle(self):
        """Settle the future with how the call ended, from any thread: what the session's close
        does for a call still launching, whose task may never hear the spawn's answer."""
        try:
            self.sender.loop.call_soon_threadsafe(self._settle)
        except RuntimeError:
            pass  # the caller's loop has closed with its kernel: nothing waits on this

    def cancel(self):
        """Cancel the call: one not yet started sends nothing; one whose child is launching
        releases it once it exists; one whose submission exists cancels that. Whether this ended
        it: not once the submission has an outcome."""
        work = self.work
        if work is not None and work.state in _ENDED:
            return False
        ended = self.end("cancelled", error=_agents.RequestCancelled(f"{self} was cancelled"))
        if work is not None:
            work.cancel()
        return ended

    def start(self, context):
        """Run the call as a task of the sender's loop, in `context`, the caller's."""
        self.task = asyncio.Task(
            self.run(), loop=self.sender.loop, context=context, name=f"call-{self.id}"
        )

    async def run(self):
        """The whole call: launch the child, give it the submission, wait for that to end,
        release the child, and settle. A call ended by nothing here -- its task cancelled with
        its kernel -- ends cancelled, so its waiters hear either way."""
        kernel, d = self.sender, self.declaration
        try:
            if self.state in _ENDED:
                return  # cancelled before this started: nothing is sent
            try:
                child = await kernel.runtime.spawn(d.fn.__name__[:64], prompt=d.doc, model=d.model)
            except Exception as e:
                self.end(_REFUSED.get(type(e).__name__, "failed"), error=e)
                return
            self.child = child._child
            _notify({"t": "agent.call", "agent": kernel.agent, "request": self.id,
                     "child": child.id, "module": d.module, "qualname": d.qualname,
                     "digest": d.digest, "invocation": self.invocation})
            if self.state not in _ENDED:
                try:
                    _refuse_closing()
                    self.work = _submit(self.child, kernel, self.arguments, d.result, 3, self)
                except Exception as e:
                    self.end(_REFUSED.get(type(e).__name__, "failed"), error=e)
            work = self.work
            if work is not None:
                if self.state in _ENDED:
                    # Cancelled from another thread as the submission was made.
                    work.cancel()
                await asyncio.wait([work.future])
            if self.state in _ENDED:
                reason, value, error = self.reason, self.value, self.error
            else:
                reason, value, error = work.reason, work.value, work.error
            if reason not in ("released", "closed") and _closing is None:
                with contextlib.suppress(Exception):
                    await kernel.runtime.release([child])
            said = None
            if reason is None and _observing:
                encoded, problems = d.schema._encoding(value)
                said = None if problems else _agents.preview(encoded)
            _notify({"t": "agent.call.ended", "agent": kernel.agent, "request": self.id,
                     "child": child.id, "outcome": reason or "accepted",
                     "invocation": self.invocation, "value": said})
            self.end(reason, value=value, error=error)
        finally:
            self.end("cancelled", error=_agents.RequestCancelled(f"{self} was cancelled"))
            self._settle()


def _call(declaration, arguments):
    """Make a typed call -- `@outrig.agent`'s -- from the calling kernel, and return its handle at
    once: the call runs as a task on that kernel's loop, in the caller's context, so
    `outrig.runtime` is the caller's inside it."""
    kernel = _sending_kernel("a call")
    _refuse_closing()
    # Copied now, so the child and the validator get the arguments as they were passed, whatever
    # the caller does to them before the call's task sends them.
    call = _Call(declaration, copy.deepcopy(arguments), kernel)
    with _works_lock:
        _launching[call.id] = call
    try:
        kernel.loop.call_soon_threadsafe(call.start, contextvars.copy_context())
    except RuntimeError:
        # The kernel's loop has closed with it: nothing will run the call.
        with _works_lock:
            _launching.pop(call.id, None)
        raise
    return Handle(call)


class _ProgressOut(Endpoint):
    """A work child's `progress` channel: text sent on it goes to the handle of the submission it
    is working on, which its parent reads as `handle.progress`. It ends nothing, and nothing comes
    back on it."""

    def __init__(self, agent, child):
        super().__init__(agent, "progress", receives=(), sends=str)
        self._child = child

    async def send(self, message):
        """Report `message`, a str, on the submission you are working on."""
        if type(message) is not str:
            raise TypeError(f"channel 'progress' sends str, not {_type_name(type(message))}")
        work = self._child.last
        if work is None or work.state in _ENDED:
            raise RuntimeError("no submission is open, so progress has nowhere to go")
        number = next(_ids)
        _notify({"t": "agent.progress", "agent": self._agent, "request": work.id,
                 "body": _agents.cut(message) if _observing else None})
        try:
            work.progress._deliver(message, self._agent, number)
        except _Refused as e:
            raise RuntimeError(f"progress was not delivered: {e}") from None


class RequestEndpoint(Endpoint):
    """A request child's end of one of its channels: `await receive()` takes the next request, as
    a `RequestDelivery` to answer with `reply(value)` or end with `fail(message)`, and `pending()`
    counts those waiting. Nothing is sent on it -- a reply names its request -- so `send` raises.
    `request_type` is what a request carries, and `reply_type` what a reply must be."""

    def __init__(self, agent, name, channel):
        super().__init__(agent, name, receives=channel.request_type, sends=())
        self._channel = channel

    @property
    def request_type(self):
        return self._channel.request_type

    @property
    def reply_type(self):
        return self._channel.reply_type

    async def receive(self):
        """Take the next request on this channel, waiting until there is one, as a
        `RequestDelivery`; see `help(type(delivery))` for answering it."""
        work = await self._when(self._receivers, self._take)
        with work.lock:
            if work.state == "waiting":
                work.state = "received"
        _notify({"t": "agent.received", "agent": self._agent, "request": work.id})
        return RequestDelivery(work.id, work.body, work.sender.agent,
                               datetime.datetime.now(datetime.timezone.utc), work)

    async def send(self, message):
        raise TypeError(f"channel {self._name!r} carries requests to you, and nothing back: answer "
                        f"a request with `await delivery.reply(value)` on the delivery receive() "
                        f"gave you")

    def _put(self, work):
        """Queue `work` for a receive, from its sender's thread. Whether it was: not once this
        child's kernel has ended."""
        with self._lock:
            if _kernels.get(self._agent) is None or _kernels[self._agent].ended:
                return False
            self._queue.append(work)
            self._delivered += 1
            wake = iter(self._receivers.copy())
        _wake_all(wake, self._name)
        return True

    def _withdraw(self, work):
        with self._lock:
            try:
                self._queue.remove(work)
            except ValueError:
                return False
            return True


class Kernel:
    """One agent's execution environment: its namespace, its loop, its slot, its backlog."""

    def __init__(self, agent, child=None):
        self.agent = agent
        # A child's record, which its parent shares; `None` for an agent that is no child.
        self.child = child
        # Set once the host has ended this kernel: it takes no more work, and is going.
        self.ended = False
        # What this kernel's code waits on the host to answer, by the id it asked under.
        self.asks = {}
        self.loop = _Loop()
        self.loop.set_exception_handler(_attributed_exception)
        self.module = types.ModuleType(_module_name(agent))
        self.globals = self.module.__dict__
        self.globals["__builtins__"] = builtins
        self.globals["asyncio"] = asyncio
        # The channels this agent has, and the runtime class that describes them: each kind of
        # agent is told of its own, so the three are told apart here and nowhere else.
        if child is None:
            # The user's channel carries text both ways, and the host is at the other end of it.
            # A child has none: what it is given comes from its parent.
            self.channels = {"user": Endpoint(agent, "user", receives=str, sends=str)}
            runtime = Runtime
        elif child.kind == "request":
            self.channels = {
                name: RequestEndpoint(agent, name, channel)
                for name, channel in child.channels.items()
            }
            runtime = RequestRuntime
        else:
            self.channels = {"progress": _ProgressOut(agent, child)}
            runtime = WorkRuntime
        self.history = History()
        # Also `outrig.runtime`, from any code an execution of this kernel runs, whatever the
        # global comes to hold.
        self.runtime = self.globals["runtime"] = runtime(
            self.channels, self.held, self.history, Context(agent, self.history), self
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
        # signal: its own, or the main thread. Set before the loop runs (`_open`).
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
        # Hosted objects: a pool of connections per binding, each opened on the thread whose
        # call needs it, and numbered for the life of the kernel so a frame for a closed one is
        # never taken for a new one's.
        self._hosted = {}
        self._hosted_lock = threading.Lock()
        self._hosted_numbers = collections.defaultdict(lambda: itertools.count(1))
        # The stub of each of the session's bindings, the runtime's own reference to what is bound
        # under the binding's name.
        self.stubs = {}
        self.bind_stubs()

    def announce(self):
        _send({"t": "ready", "agent": self.agent, "version": _VERSION})

    def bind_stubs(self):
        """Bind each of the session's bindings under its name, as its stub, among the names bound
        at boot, which the inventory leaves out. Nothing, without RPyC."""
        if _Hosted is None:
            return
        for name in _BINDINGS:
            self.stubs[name] = self.globals[name] = self._boot[name] = self.stub(name)

    def stub(self, binding):
        """The root of `binding` -- an object in a process on the host -- as the `HostedRoot`
        agent code uses it. Making it sends nothing."""
        return HostedRoot(self._pool(binding))

    def hosted(self, binding):
        """The root of `binding` as RPyC's proxy over one of this kernel's connections to it:
        what `stub` stands for, for tests, bound to no name agent code sees.

        The first connection is opened on the calling thread at first use, and the root fetched
        then -- never on the reader thread, which is what delivers the reply this waits for. A
        proxy from a closed connection raises `EOFError` with the reason, and the next call here
        fetches the root again on a live connection.
        """
        return self._pool(binding).root()

    def _pool(self, binding):
        """This kernel's pool of connections to `binding`, made anew once the last has closed."""
        if _Hosted is None:
            raise RuntimeError(_rpyc_missing)
        if self.ended:
            raise RuntimeError(f"agent {self.agent!r} was released, and its connections to the "
                               f"session's bindings closed with it")
        if not isinstance(binding, str) or not binding:
            raise TypeError("a binding is named by a non-empty str")
        with self._hosted_lock:
            pool = self._hosted.get(binding)
            if pool is None or pool.closed is not None:
                pool = self._hosted[binding] = _Pool(self, binding)
        return pool

    def close_hosted(self, binding=None, reason="the kernel's connections are closing"):
        """Close this kernel's connections to `binding` -- to every binding, by default -- and
        wake every call waiting on them with `EOFError`: one in flight is left to the binding to
        finish, its outcome unknown here, and one waiting for a connection was never sent. What a
        release of the kernel (`0003-30`) and the session's close (`0003-22`) call; runs on any
        thread."""
        with self._hosted_lock:
            names = list(self._hosted) if binding is None else [binding]
            pools = [self._hosted.pop(name) for name in names if name in self._hosted]
        for pool in pools:
            pool.close(reason)

    def _open_hosted(self, binding, pool):
        """A new connection to `binding` for `pool`, which holds its condition variable. Called
        under it, and writes nothing: the first frame goes out with the first request."""
        agent = self.agent
        number = next(self._hosted_numbers[binding])

        def write(part, more, label=None):
            # Encoded without the reserve: running out of memory while sending the agent's own
            # frame is the agent's to see.
            _write_line(
                _encode(
                    {
                        "t": "rpc",
                        "agent": agent,
                        "binding": binding,
                        "id": number,
                        "data": base64.b64encode(part).decode("ascii"),
                        "more": more,
                        **(label or {}),
                    }
                )
            )

        channel = FrameChannel(write, cond=pool.cond)
        _rpc_channels[(agent, binding, number)] = channel
        return _Hosted(channel, agent=agent, binding=binding, number=number, pool=pool)

    def serve(self):
        """Run this kernel's loop on the calling thread for the life of the process, or until the
        host ends it."""
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
            if self.ended:
                self._gone()
                return
        _diag(f"agent {self.agent!r}: its event loop was closed")

    def ask(self, line):
        """Send `line`, which the host answers under its `id`, and return the future of this
        kernel's loop the answer settles. Called on that loop."""
        future = self.loop.create_future()
        self.asks[line["id"]] = future
        try:
            _write_line(_encode(line))
        except BaseException:
            self.asks.pop(line["id"], None)
            raise
        return future

    def end(self):
        """End this child's kernel, on the reader thread: every request it holds ends
        `AgentReleased`, its connections to the bindings close, and its loop cancels every task
        and stops once they finish, or after a second, after which its thread says so and is gone.
        A loop that never turns -- code that never yields -- never stops, and its thread runs on.
        """
        if self.ended:
            return
        self.ended = True
        child = self.child
        if child is not None:
            with child.lock:
                child.released = True
            with _works_lock:
                held = [work for work in _works.values() if work.child is child]
            for work in held:
                work.end("released", error=_agents.AgentReleased(f"{child} was released"))
        self.close_hosted(reason="its agent was released")
        try:
            self.loop.call_soon_threadsafe(self._stop)
        except RuntimeError:
            pass

    def _stop(self):
        """Cancel every task on this loop, and stop it once they have finished, or after a
        second."""
        tasks = [task for task in asyncio.all_tasks(self.loop) if not task.done()]
        if not tasks:
            self.loop.stop()
            return
        left = len(tasks)

        def finished(_):
            nonlocal left
            left -= 1
            if left == 0:
                self.loop.stop()

        for task in tasks:
            task.cancel()
            task.add_done_callback(finished)
        self.loop.call_later(1.0, self.loop.stop)

    def _gone(self):
        """This ended kernel's loop has stopped: close it, forget the kernel, and say so."""
        with contextlib.suppress(Exception):
            self.loop.close()
        _kernels.pop(self.agent, None)
        sys.modules.pop(self.module.__name__, None)
        for key in [key for key in list(_rpc_channels) if key[0] == self.agent]:
            _rpc_channels.pop(key, None)
        _notify({"t": "ended", "agent": self.agent})

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
        # The traceback, and the type of what was raised: the host records the one it can act on
        # from the type rather than from how the traceback reads.
        error = raised = None
        try:
            if execution.cancel_requested:
                raise asyncio.CancelledError("stopped before it started")
            self.globals["__outrig_echo__"] = _echo
            code = _with_room(_compile, source)
            _remember_source(code, source)
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
            error, raised = self._failure(e), _raised(e)
        except BaseException as e:
            # Everything else the body raised, `KeyboardInterrupt` included. The handler raises
            # one only inside agent code, so one arriving here is this execution's -- its body
            # was interrupted, or it awaited a task that was -- and never one meant to end the
            # interpreter. Letting any exception out would end the task with no reply, leaving
            # the host holding a slot this side has freed.
            error, raised = self._failure(e), _raised(e)
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
                raised=raised,
                background=self.take_background(),
            )
        except MemoryError:
            # Agent code still running -- a task, a thread -- took the room back. What the
            # execution wrote is lost; that it ended is not.
            self._reply(
                execution.id,
                "error",
                error=error or "MemoryError: memory ran out while this result was reported\n",
                raised=raised or "MemoryError",
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
        yielding, which is what `interrupt` is for. The one blocking call it does reach is a
        hosted call, whose wait this wakes with the `CancelledError` first; the loop's cancel
        then finds a finished task, or one that caught the error and carried on. A request for
        an execution that has already finished is the ordinary race of a Ctrl-C with a result,
        and is dropped without comment.
        """
        if not self._holds(exec_id):
            return
        self._wake(
            exec_id, False, lambda wait: asyncio.CancelledError(_woken_text(wait, "cancelled"))
        )
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
        """Interrupt `exec_id`'s agent code: wake the hosted call its thread is waiting in, on
        any kernel, or aim a SIGINT at the loop's thread, on the primary. Runs on the reader
        thread.

        Handled here rather than through the loop, whose queue is exactly what a wedged loop is
        not draining. A thread waiting for a binding to answer waits on a condition variable
        this thread can signal, so that interrupt needs no signal and reaches a child's kernel
        too; it raises in the caller, between frames. Otherwise the signal is sent to the main
        thread itself: `signal.raise_signal` would signal this one, and a main thread blocked in
        `waitpid` or `sleep` would never wake to run the handler. A thread inside a hosted call
        and not waiting is left to that signal, which decides where the interrupt lands, so one
        interrupt is raised once. `runaway` widens where it may land -- see `_on_sigint`.
        """
        signalled = self is _primary and signal.getsignal(signal.SIGINT) is _on_sigint
        woken = None
        if self._holds(exec_id):
            woken = self._wake(
                exec_id,
                runaway,
                lambda wait: KeyboardInterrupt(_woken_text(wait, "interrupted")),
                signalled,
            )
            if woken == "woken":
                return
        if self is not _primary:
            # Python runs signal handlers on the main thread alone, and only the primary is
            # there. A wedged sub-agent is contained, not recoverable -- unless it is inside a
            # hosted call, which raises at its next check.
            later = (
                "; it will raise when its hosted call next waits or ends" if woken == "busy" else ""
            )
            _diag(
                f"agent {self.agent!r} cannot be interrupted: it is not on the main thread{later}"
            )
            return
        if not self._holds(exec_id):
            return
        if not signalled:
            _diag(
                f"execution {exec_id!r} cannot be interrupted: SIGINT's handler has been "
                f"replaced, so only a cancel can reach it"
            )
            return
        self._interrupting, self._landed = (exec_id, runaway), None
        signal.pthread_kill(self.thread.ident, signal.SIGINT)

    def _wake(self, exec_id, runaway, make_exc, signalled=False):
        """Leave the exception `make_exc(wait)` builds for this kernel's thread to raise from
        the hosted call it is inside, if it is inside one that `exec_id`'s interrupt may end.
        Runs on the reader thread.

        Returns `woken` when the thread was waiting -- for a reply, or for a free connection --
        and raises at once; `busy` when it is inside a hosted call but not waiting -- running a
        callback's code, boxing, sending, reading a frame -- and raises at the call's next check,
        or, when `signalled`, is left to the signal that follows, which leaves the exception
        itself where it cannot raise it; `None` when there is nothing to wake, or when the rule
        `_on_sigint` applies says this interrupt may not end what the thread is doing: the call
        is this program's own, or it belongs to a task of another execution's and the host did
        not find the loop spinning.
        """
        thread = self.thread
        wait = _waits.get(thread.ident) if thread is not None else None
        if wait is None:
            return None
        with wait.pool.cond:
            if _waits.get(thread.ident) is not wait:
                return None
            running = self.running
            if (
                not runaway
                and running is not None
                and running.id == exec_id
                and running.started
                and wait.execution is not running
            ):
                return None
            if not wait.interruptible:
                return None
            if signalled and not wait.waiting:
                return "busy"
            said = None
            if wait.exc is None:
                wait.exc, wait.by = make_exc(wait), self
                said = wait.never_sent("cancelled", "interrupted")
            # Also when one was there already, which the signal left without waking the thread:
            # it raises now rather than at its wait's next look.
            wait.pool.cond.notify_all()
            woken = "woken" if wait.waiting else "busy"
        _say([said])
        return woken

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
        so it answers whatever the values are, and whoever asks, and asks no binding: a proxy's
        type is named after the host's.
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

    def tasks(self, request_id):
        """Name the tasks still running on this loop, other than the executions' own wrappers:
        what an agent's code left running, which a round ending does not stop. Always answered.

        Excluded by their code rather than their name, which agent code can give a task of its
        own. Read on the loop, the only thread `asyncio.all_tasks` may be asked on.
        """
        names, more = [], 0
        try:
            running = sorted(
                str.__str__(task.get_name())
                for task in asyncio.all_tasks(self.loop)
                if not task.done() and _code_of(task) is not _WRAPPER
            )
            more = max(len(running) - TASKS_MAX, 0)
            names = [name[:TASK_NAME_MAX] for name in running[:TASKS_MAX]]
        except Exception as e:
            _diag(f"agent {self.agent!r}: listing its tasks failed: {e!r}")
        _send({"t": "tasks", "agent": self.agent, "id": request_id, "names": names, "more": more})


# ---------------------------------------------------------------------------- interrupts

# The kernel on the main thread, the only thread Python runs a signal handler on: the primary
# agent's, once the host opens one there (`_open`), and `None` in a session that has none.
_primary = None

# Where `_open` hands the main thread its kernel, which `main` waits for.
_onto_main = queue.SimpleQueue()


def _on_sigint(signum, frame):
    """Turn an armed interrupt into a `KeyboardInterrupt` in the agent code holding the loop.

    Runs on the main thread, between two bytecodes or as a blocking call there returns early. It
    takes no lock and writes no line, because the main thread may be anywhere, a lock or a write
    of its own included. It raises only when all three of these hold, and otherwise returns and
    leaves the main thread where it was:

    - An interrupt is armed and the execution it names still holds the slot. A SIGINT nobody
      armed -- agent code running `pkill -INT python3` -- changes nothing.
    - Walking out from where the signal landed, code a submission wrote comes before any of the
      machinery below -- or the walk reaches the loop's dispatch from inside a task an agent
      started, which is agent code however it was compiled: a coroutine an imported module
      defines, scheduled with `create_task` or `gather` -- and no frame of a hosted call's own
      work comes first, short of where the call runs an object's own code: a callback's, or what
      boxing an argument or a result calls, such as `__fspath__` (`_in_hosted_work`). Otherwise
      the main thread is choosing the next callback, writing a protocol line, or reporting a
      result, where an exception would lose a callback or tear a line; or it is doing a hosted
      call's own work, where one could lose a frame read and not yet answered, leaving its
      connection waiting for a reply that has come, or leave the pool's lock held; or it is idle,
      and there is nothing to interrupt.
    - That code is the named execution's, including a task it started; or the execution has not
      started, so whatever holds the loop is what keeps it from starting; or the host found the
      loop spinning, which justifies ending whichever agent code is doing it. Without one of
      these, a user's interrupt for a suspended execution could end a healthy task belonging to
      another.

    When only the second fails and the main thread is in a hosted call an interrupt may end, the
    interrupt is left on the call, which raises it at its next check: before it waits, as its
    wait looks again -- every `SIGNAL_CHECK` seconds, since nothing here wakes it -- or as it
    ends. So it is on a request about a skill's files the main thread waits on (`_ask_host`),
    which an import from agent code makes. Outside both -- a call taking the pool's lock before
    it has begun, a proxy's finalizer -- it is dropped, and the host's next interrupt reaches the
    code.
    """
    kernel = _primary
    if kernel is None:
        # No kernel is on the main thread, so there is no agent code here to interrupt: the
        # thread is waiting for one (`main`).
        return
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
    if not _in_hosted_work(frame) and _agent_code_outward(frame, kernel):
        exc = KeyboardInterrupt("interrupted by outrig")
        kernel._landed = (exc, asyncio.current_task(kernel.loop), _CURRENT.get())
        raise exc
    wait = _waits.get(threading.get_ident())
    if wait is not None and wait.interruptible and wait.exc is None:
        wait.exc, wait.by = KeyboardInterrupt(_woken_text(wait, "interrupted")), kernel
        return
    left = getattr(_asking, "left", None)
    if left is not None and not left:
        exc = KeyboardInterrupt("interrupted by outrig")
        kernel._landed = (exc, asyncio.current_task(kernel.loop), _CURRENT.get())
        left.append(exc)


def _agent_code_outward(frame, kernel):
    """Whether, walking outward from `frame`, code a submission wrote -- or the loop's dispatch
    inside a task an agent started -- comes before any of this program's machinery. What
    `_on_sigint` asks of the frame a signal landed in, and a hosted call asks of its own frames
    when it starts, so the reader thread knows whether an interrupt may wake it."""
    while frame is not None:
        code = frame.f_code
        if code.co_filename == "<execution>":
            return True
        if code in _MACHINERY:
            return code is _HANDLE_RUN and _in_agent_task(kernel)
        frame = frame.f_back
    return False


def _in_agent_task(kernel):
    """Whether the loop is stepping a task some agent code started.

    The only tasks this program starts are the executions' own wrappers, so any other task is
    agent code's doing -- including one whose coroutine a module defines rather than a submission.
    """
    task = asyncio.current_task(kernel.loop)
    return task is not None and _code_of(task) is not _WRAPPER


def _code_of(task):
    """The code of `task`'s coroutine, read only from a coroutine of this process: a task can run
    any awaitable `asyncio.iscoroutine` accepts, a hosted proxy among them, whose attributes are
    requests -- and this is read inside the SIGINT handler."""
    coro = task.get_coro()
    return coro.cr_code if type(coro) is types.CoroutineType else None


# Machinery that can sit nearer the signal than agent code's frames; see `_on_sigint`. Every
# callback the loop runs, this program's own included, runs beneath `Handle._run`, so the loop's
# two dispatch frames cover them all -- including when agent code pumps the loop by hand. What else
# agent code triggers synchronously and must not be interrupted is a protocol line on its way out,
# which `Endpoint.send` writes, and the fork hooks; a hosted call's own work is told apart by
# `_in_hosted_work`. This program's output routing, by contrast, is deliberately absent: a runaway
# that prints spends most of its time there, and must still be interruptible.
_MACHINERY = frozenset(
    fn.__code__
    for fn in (
        asyncio.base_events.BaseEventLoop._run_once,
        asyncio.events.Handle._run,
        _write_line,
        FrameChannel.send,
        _before_fork,
        _after_fork_in_parent,
        _after_fork_in_child,
    )
)
_HANDLE_RUN = asyncio.events.Handle._run.__code__
_WRAPPER = Kernel._run.__code__


# ---------------------------------------------------------------------------- bindings


class Binding(collections.namedtuple("Binding", ("name", "description", "type"))):
    """One of the session's bindings: an object a process on the host holds, bound as `name`.

    `description` is what the operator says it is, and `type` the qualified name of the type of
    the host's object. A record of what the session said at start; reading it asks the host
    nothing.
    """

    __slots__ = ()


# The session's bindings, by name, as the host declared them (`_bindings`); empty until then, and
# for a session with none.
_BINDINGS = types.MappingProxyType({})


def _bindings(message):
    """Take the session's bindings, which the host sends once, before anything runs.

    Each binding's record goes in `runtime.bindings` for every kernel, and its package directory on
    `sys.path`: after the workspace and before the agent's own pip installs, so that the container
    imports the version of a library its binding runs rather than one the agent installed, in the
    order the host gave them, which decides between two bindings that hold different versions of
    one package. Each kernel open then binds a stub for each, as one opened later does.
    """
    global _BINDINGS
    entries, paths = message.get("bindings"), message.get("paths")
    if not isinstance(entries, list) or not isinstance(paths, list):
        raise ValueError("bindings without a list of bindings and a list of paths")
    records = {}
    for entry in entries:
        record = Binding(entry["name"], entry["description"], entry["type"])
        if not all(isinstance(field, str) for field in record):
            raise ValueError(f"a binding whose fields are not strings: {entry!r}")
        records[record.name] = record
    if not all(isinstance(path, str) for path in paths):
        raise ValueError("a binding package path that is not a string")
    _BINDINGS = types.MappingProxyType(records)
    at = sys.path.index(_SITE) if _SITE in sys.path else len(sys.path)
    for path in paths:
        if path not in sys.path:
            sys.path.insert(at, path)
            at += 1
    for kernel in list(_kernels.values()):
        kernel.bind_stubs()


def _close(message):
    """The session has closed to new work. A boundary call is refused here from now on, and each
    one still waiting for a free connection, so never sent, raises the closing error now; what was
    sent is the binding's to finish. Every request made of a child and not yet answered ends with
    the closing error, and no request or submission is sent from now on."""
    global _closing
    _closing = text = message["reason"]
    for kernel in list(_kernels.values()):
        for pool in list(kernel._hosted.values()):
            said = []
            with pool.cond:
                for wait in list(_waits.values()):
                    if wait.pool is pool and wait.asking is not None and wait.exc is None:
                        said.append(wait.never_sent("cancelled", "closed"))
                        wait.exc = _closed(text)
                pool.cond.notify_all()
            _say(said)
    with _works_lock:
        open_works = list(_works.values())
        launching = list(_launching.values())
    for work in open_works + launching:
        work.end("closed", error=_agents.SessionClosing(
            f"the session closed before {work} was answered: {text}"))
    for call in launching:
        call.settle()


def _unbind(message):
    """The process holding the bindings has ended: every kernel's connections to them close, a
    call in flight told its outcome on the host is unknown, and a later use raises with why."""
    for kernel in list(_kernels.values()):
        kernel.close_hosted(reason=message["reason"])


# ---------------------------------------------------------------------------- routing

def _open(agent, main, child=None):
    """Build `agent`'s kernel, which says it is ready. Every kernel is built here, on the reader
    thread, and none exists until the host asks for it.

    With `main`, the main thread serves it: the only thread Python runs a signal handler on, so the
    one kernel a signal can interrupt, which the host gives its primary agent. Every other kernel
    is served on a thread of its own.

    `child`, for a child agent's kernel, names the spawn waiting for it -- whose parent's inputs
    are bound in its namespace, and whose classes under their names -- and its `requests_max`.
    """
    global _primary
    if not _valid_agent(agent):
        raise ValueError(f"{agent!r} cannot name an agent")
    if agent in _kernels:
        raise ValueError(f"agent {agent!r} is already open")
    if main and _primary is not None:
        raise ValueError(f"agent {_primary.agent!r} already has the main thread")
    record = spec = None
    if child is not None:
        spec = _spawning.get(child.get("spawn")) if isinstance(child, dict) else None
        requests_max = child.get("requests_max") if isinstance(child, dict) else None
        if main or spec is None or type(requests_max) is not int or requests_max < 1:
            raise ValueError(f"no spawn is waiting for agent {agent!r} as {child!r} names it")
        record = _Child(agent, spec["name"], spec["kind"], spec["parent"], spec["channels"],
                        requests_max)
    kernel = Kernel(agent, child=record)
    if record is not None:
        record.kernel = kernel
        for name, value in spec["inputs"].items():
            kernel.globals[name] = value
        for cls in spec["classes"]:
            kernel.globals[cls.__name__] = cls
    if main:
        kernel.thread = threading.main_thread()
    else:
        kernel.thread = threading.Thread(target=kernel.serve, name=f"kernel-{agent}", daemon=True)
        try:
            kernel.thread.start()
        except BaseException:
            sys.modules.pop(kernel.module.__name__, None)
            kernel.loop.close()
            raise
    _kernels[agent] = kernel
    if record is not None:
        _children[agent] = record
    if main:
        _primary = kernel
        _onto_main.put(kernel)
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
    skill = message.get("skill")
    try:
        if endpoint is None:
            raise _Refused(f"agent {kernel.agent!r} has no channel {channel!r}")
        if skill is not None and type(skill) is not str:
            raise _Refused(f"a directive names its skill with a str, not {_type_name(type(skill))}")
        pending = endpoint._deliver(message.get("body"), "user", request_id, skill)
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
    incomplete = message.get("incomplete", False)
    if (
        type(number) is not int
        or type(incomplete) is not bool
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
        incomplete,
    )
    kernel.history._add(turn)


def _inventory(kernel, request_id, message):
    """List what the agent holds, on its loop, from past the name `after` when that is given."""
    after = message.get("after")
    kernel.loop.call_soon_threadsafe(
        kernel.inventory, request_id, after if isinstance(after, str) else None
    )


def _tasks(kernel, request_id, _message):
    """Name the tasks running on the agent's loop, on that loop."""
    kernel.loop.call_soon_threadsafe(kernel.tasks, request_id)


def _answer_ask(kernel, ask_id, message):
    """The host's answer to what `kernel`'s code asked under `ask_id`, handed to its loop."""
    future = kernel.asks.pop(ask_id, None)
    if future is not None:
        kernel.loop.call_soon_threadsafe(_resolve, future, message)


def _resolve(future, message):
    if not future.done():
        future.set_result(message)


def _assign(kernel, assign_id, _message):
    """Give the work child the next submission waiting for it, between its rounds: its inputs
    bound under their names, and its result's classes under theirs. Answered with the
    submission's id, or none when none is waiting."""
    child, work = kernel.child, None
    if child is not None:
        with child.lock:
            while child.queue and work is None:
                candidate = child.queue.popleft()
                with candidate.lock:
                    if candidate.state == "queued":
                        candidate.state = "running"
                        work = candidate
            if work is not None:
                child.last = work
    if work is not None:
        for name, value in work.body["inputs"].items():
            kernel.globals[name] = value
        for cls in work.body["classes"]:
            kernel.globals[cls.__name__] = cls
        _notify({"t": "agent.received", "agent": kernel.agent, "request": work.id})
    _notify({"t": "agent.assigned", "agent": kernel.agent, "id": assign_id,
             "request": None if work is None else work.id})


# What a request the host ends ends with: a work child's rounds ended without completing it, the
# token budget was spent first, or the child's model failed for good.
_HOST_ENDS = {
    "rejected": "CompletionRejected",
    "budget": "BudgetExhausted",
    "failed": "AgentRequestFailed",
}


def _settle_work(kernel, work_id, message):
    """End request `work_id` as the host decided."""
    with _works_lock:
        work = _works.get(work_id)
    outcome = message.get("outcome")
    if work is None or outcome not in _HOST_ENDS:
        return
    text = message.get("message") or f"{work} ended {outcome}"
    work.end(outcome, error=getattr(_agents, _HOST_ENDS[outcome])(text), message=text)


def _child_state(kernel, _, message):
    """What the host now says a child of `kernel`'s is doing."""
    child = _children.get(message.get("child"))
    state = message.get("state")
    if child is not None and state in ("running", "idle", "wedged", "released"):
        child.state = state


def _observe():
    """Say from now on when any agent's code takes a message from one of its channels.

    Nothing needs to know that to deliver one -- the host is told only because it records what the
    agents do -- so nothing says it until the host asks. Asked of the whole interpreter rather than
    of a kernel, since the host asks before it opens any.
    """
    global _observing
    _observing = True


def _pending(kernel, request_id, _):
    """Say how many messages wait on each of the agent's channels, and how many have ever been
    delivered to it, reading none of them."""
    channels = {name: endpoint._counts() for name, endpoint in kernel.channels.items()}
    _send({"t": "pending", "agent": kernel.agent, "id": request_id, "channels": channels})


def _file(kernel, request_id, message):
    """Start the answer to a file request, which a thread of its own gives. Reader thread only.

    A read of a file on a mount that has stopped answering never returns, and `O_NONBLOCK` does not
    bound it. Done here it would hold the reader, and with it every message the host sends next --
    an execution, its cancel, the end of the session. Held by a thread of its own, it holds that
    thread alone.
    """
    reply = {"t": "file", "agent": kernel.agent, "id": request_id}
    try:
        threading.Thread(
            target=_read_file, args=(reply, message), name=f"file-{request_id}", daemon=True
        ).start()
    except MemoryError:
        raise
    except Exception as e:
        reply["error"] = f"{type(e).__name__}: {e}"
        _send(reply)


def _read_file(reply, message):
    """Send `reply`, the answer to the file request `message`: the size of the regular file at its
    `path` and as much of its text as its `max` bytes hold.

    Anything that is not a regular file -- missing, a directory, a pipe -- is answered with no
    file, and the file is opened without blocking so that a pipe cannot hold this thread. A file
    that is not UTF-8 is an error, except that a cut which falls inside a character stops short of
    it. A read that fails otherwise is answered with the reason, so the host is never left waiting.
    """
    path, limit = message.get("path"), message.get("max")
    try:
        if not isinstance(path, str) or type(limit) is not int or limit < 0:
            raise ValueError("a file request needs a path and a byte limit")
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        try:
            info = os.fstat(fd)
            if stat.S_ISREG(info.st_mode):
                data = b""
                while len(data) < limit:
                    part = os.read(fd, limit - len(data))
                    if not part:
                        break
                    data += part
                decoder = codecs.getincrementaldecoder("utf-8")()
                text = decoder.decode(data, final=len(data) >= info.st_size)
                reply["file"] = {"size": info.st_size, "text": text}
        finally:
            os.close(fd)
    except (FileNotFoundError, NotADirectoryError):
        pass
    except Exception as e:
        reply["error"] = f"{type(e).__name__}: {e}"
    try:
        _send(reply)
    except Exception as e:
        # The host is gone, or there is no room to say; no one else is waiting on this thread.
        _diag(f"agent {reply['agent']!r}: the answer to a file request was not sent: {e!r}")


def _rpc(kernel, number, message):
    """Hand one part of an RPyC frame to the channel of connection `number`, or drop it. Reader
    thread only.

    A part for a connection that has closed, or that this kernel never opened, is answered with a
    close notice rather than buffered, so nothing waits on it. The part is decoded before anything
    changes, so `_handle`'s retry after running out of memory never applies one twice; the notice
    comes after, and is given up rather than retried. A request the host refused is answered
    here.
    """
    binding = message.get("binding")
    if not isinstance(binding, str):
        raise ValueError("an rpc message without a binding")
    channel = _rpc_channels.get((kernel.agent, binding, number))
    if "closed" in message:
        if channel is not None:
            channel.close(f"the host closed this connection: {message['closed']}")
        return
    if "refused" in message:
        pool = kernel._hosted.get(binding)
        for conn in list(pool.connections) if pool is not None else ():
            if conn.number == number:
                conn.refused(message["refused"], message.get("reason"), message.get("error"))
        return
    part = base64.b64decode(message.get("data", ""), validate=True)
    if channel is None or channel.closed:
        _rpc_notice(kernel.agent, binding, number, "this connection is closed")
        return
    more = message.get("more") is True
    try:
        reason = channel.put(part, more)
    except MemoryError:
        # Once more on the reserve, as `_handle` would retry; then the call waiting on this frame
        # is told rather than left waiting for a part that will never be held.
        _give_reserve()
        try:
            reason = channel.put(part, more)
        except MemoryError:
            reason = channel.close("no memory to hold a frame from the binding")
    if reason is not None:
        _rpc_notice(kernel.agent, binding, number, reason)


def _rpc_notice(agent, binding, number, reason):
    try:
        _send({"t": "rpc", "agent": agent, "binding": binding, "id": number, "closed": reason})
    except MemoryError:
        _diag(f"connection {number} to {binding!r} closed, but saying so failed: no memory")


# What each message addressed to an agent does, with the id it carries. `inv` and `tasks` are
# answered on the agent's loop and `file` by a thread of its own; the rest are handled here, on
# the reader thread.
_ROUTES = {
    "exec": _exec,
    "msg": _msg,
    "pending": _pending,
    "file": _file,
    "received": _received,
    "turn": _turn,
    "inv": _inventory,
    "tasks": _tasks,
    "rpc": _rpc,
    "cpu": lambda kernel, request_id, _: kernel.cpu(request_id),
    "cancel": lambda kernel, request_id, _: kernel.cancel(request_id),
    "interrupt": lambda kernel, request_id, message: kernel.interrupt(
        request_id, message.get("runaway") is True
    ),
    "spawned": _answer_ask,
    "released": _answer_ask,
    "assign": _assign,
    "settle": _settle_work,
    "child": _child_state,
    "end": lambda kernel, _, __: kernel.end(),
}


def _route(line):
    """Hand one protocol line to the kernel it names, or raise saying why it cannot be."""
    message = json.loads(line)
    if not isinstance(message, dict):
        raise ValueError(f"not an object: {type(message).__name__}")
    kind, agent = message.get("t"), message.get("agent")
    if kind == "open":
        try:
            _open(agent, message.get("main") is True, message.get("child"))
        except MemoryError:
            # `_with_room` tries once more, and answers nothing it might yet open.
            raise
        except Exception as e:
            # Answered, so a host waiting for the kernel hears why rather than waiting out its
            # bound; and reported, as every message this ignores is.
            if isinstance(agent, str):
                _send({"t": "ready", "agent": agent, "error": str(e)})
            raise
        return
    if kind == "observe":
        _observe()
        return
    if kind == "bindings":
        _bindings(message)
        return
    if kind == "close":
        _close(message)
        return
    if kind == "unbind":
        _unbind(message)
        return
    if kind == "skills":
        _skills._declare(message["skills"])
        return
    if kind == "skill":
        # The answer to a request about a skill, for the thread still waiting for it.
        answers = _answers.pop(message.get("id"), None)
        if answers is not None:
            answers.put(message)
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
    if len(sys.argv) not in (2, 3):
        _diag(f"usage: python3 -I -c <program> <outrig-dir> [<rpyc-dir>]; got {sys.argv[1:]!r}")
        os._exit(2)
    global _reader_thread
    # Before any agent exists, and every process it starts inherits it.
    _set_ceiling()
    # Straight after the standard library, and before anything agent code can write joins
    # `sys.path`, for the reason RPyC is: what `import outrig` finds is this process's own.
    try:
        _load_outrig(sys.argv[1])
    except Exception as e:
        _diag(f"OutRig's package could not be imported from {sys.argv[1]}: {e!r}")
        os._exit(2)
    # Before the workspace joins `sys.path`, so a module there named `rpyc` is not what this
    # process imports, and before any agent code runs, so `sys.modules` holds this copy for the
    # interpreter's life: a later `pip install rpyc` changes nothing in here.
    if len(sys.argv) == 3:
        _load_rpyc(sys.argv[2])
    _open_imports()
    # Before the reader starts, so no interrupt can arrive with nothing to handle it.
    signal.signal(signal.SIGINT, _on_sigint)
    # The greeting names no agent: there is none yet, and there may never be one on this thread.
    _send({"t": "ready", "version": _VERSION})
    _reader_thread = threading.Thread(target=_read, name="reader", daemon=True)
    _reader_thread.start()
    # The main thread serves the kernel the host opens onto it, and until then -- in a session
    # with no primary agent, for good -- waits, still taking signals, for the reader to end the
    # process.
    _onto_main.get().serve()


if __name__ == "__main__":
    main()
