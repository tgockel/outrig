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
import collections
import collections.abc
import concurrent.futures.thread
import contextlib
import contextvars
import dataclasses
import datetime
import functools
import importlib
import inspect
import io
import itertools
import json
import math
import mmap
import multiprocessing.pool
import operator
import os
import resource
import select
import shlex
import shutil
import signal
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
# fits says why for a package whose compiled wheels are all there is of the versions asked for,
# where nothing could be built. The note for the rest is written after pip has exited with an
# error, as the last of its output.
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
import sys


def guard():
    from pip._internal import wheel_builder
    from pip._internal.exceptions import DistributionNotFound
    from pip._internal.distributions.sdist import SourceDistribution
    from pip._internal.index.package_finder import LinkEvaluator, LinkType
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

    def stuck(cause):
        """The project of a requirement whose versions that fit are all compiled, if it is one."""
        try:
            requirement = cause.requirement
            specifier = requirement.get_candidate_lookup()[1].req.specifier
            name = requirement.project_name
            if fits(specifier, compiled.get(name, ())):
                if not fits(specifier, usable.get(name, ())):
                    return name
        except Exception:
            pass

    def installation_error(self, e, *args, **kwargs):
        error = get_installation_error(self, e, *args, **kwargs)
        try:
            names = sorted(filter(None, map(stuck, e.causes)))
            if names and isinstance(error, DistributionNotFound):
                rest = (
                    "the versions asked for are published only as wheels for platforms this "
                    "interpreter does not match."
                )
                error = DistributionNotFound(f"{error}\n{why(names[0], rest)}")
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

# A binding is a library running in a process on the host, which agent code reaches through a
# `HostedObject`: a path of attribute reads, item reads and calls that sends nothing until it is
# awaited, and is then resolved through RPyC proxies on a worker thread while the kernel's loop
# turns. Its frames travel as `rpc` lines of this protocol, tagged by agent, binding and
# connection, and the host relays them to the binding's process: nothing here listens on a
# socket. The reader thread routes each part of a frame to its connection's channel, and the
# thread waiting for a reply waits on the condition variable the channel shares with its pool, so
# it never reads the protocol itself and releases the GIL while it waits. A kernel holds a pool
# of up to `POOL_MAX` connections per binding, one call in flight on each, and as many workers,
# so a call that blocks for an hour blocks its worker and no other; the reader thread can wake a
# waiting call with an interrupt through the same condition variable, on any kernel.
#
# Nothing on this side is enforcement. Agent code runs in this process and can write frames of its
# own, so every check that matters is the binding's, on the host. What this side does is make the
# ordinary case work: it copies a `list`, `dict`, `set` or path into a by-value form the host
# rebuilds as a builtin, sends a subclass of a by-value type as its base value, marks a callable so
# the host can call it back while the request runs, and refuses -- before anything is sent -- an
# object that could only cross as a reference into this process, or a proxy that belongs to another
# connection. The frame channel is a copy of the binding program's; keep them the same.

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
# The hosted call each thread is inside, by thread ident -- the innermost one, which is the one
# the reader thread wakes. Installed and removed under the pool's condition variable.
_waits = {}


class FrameChannel:
    """RPyC's channel, over `rpc` lines rather than a stream.

    Frames arrive in parts, each one line, from the thread reading the protocol; `put` joins them
    and `poll` waits for a whole one on a condition variable, so the thread serving the connection
    never reads the protocol itself. A frame is sent in parts the same way, each written whole
    under the process's send lock.

    Bounds are enforced here, in both directions: a part over `PART_MAX`, a part before a frame's
    last that is not full, a frame whose header claims more than `FRAME_MAX`, a compressed frame,
    or more than `BUFFER_MAX` bytes held unread -- each frame charged `FRAME_OVERHEAD` beyond its
    bytes -- closes the channel with the reason, and the waiting reader raises `EOFError` saying
    why. A frame is never half-delivered: `recv` returns whole frames or raises.
    """

    def __init__(self, write, cond=None):
        # `write(part, more)` writes one part as one protocol line. `cond` is the condition
        # variable to wait on, when the caller shares one between channels.
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

    def send(self, data):
        """Send one frame of `data`, in parts. Refuses a frame past the bound before writing any of
        it; a write that fails part way closes the channel, since the other side holds a torn
        frame."""
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
                self._write(bytes(part), more)
            except BaseException:
                self.close("a frame could not be written whole")
                raise
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
    code, boxing or sending; `exc` is what the reader thread wants raised there -- a
    `KeyboardInterrupt`, a `CancelledError`, the closing `EOFError` -- and `by` the kernel that
    set it, for the report when the call was a background task's. `interruptible` is decided
    once, from the calling thread's own frames, by the rule `_on_sigint` applies: a hosted call
    this program makes for itself is not an interrupt's to end. `within` is the awaited call this
    one ends with, on any binding: the one the thread was resolving when this one began, for a call
    a callback makes inside it, or when the host called back the coroutine callback this one was
    awaited inside. `nested` holds the calls, on any binding, awaited inside a coroutine callback
    the thread serves meanwhile, which run on it -- on the connection it holds to their binding,
    if it holds one -- while it serves that callback or one nested inside it.
    """

    __slots__ = (
        "pool", "execution", "interruptible", "waiting", "exc", "by", "conn", "within", "nested"
    )

    def __init__(self, pool, execution, interruptible):
        self.pool = pool
        self.execution = execution
        self.interruptible = interruptible
        self.waiting = False
        self.exc = None
        self.by = None
        self.conn = None
        self.within = None
        self.nested = None

    def block(self, ready, timeout=None):
        """Under the pool's condition variable: wait until `ready()`, or raise what the reader
        thread left for this call or one it is within, which comes first. False when `timeout`
        seconds pass with neither."""
        while True:
            if self.exc is not None:
                raise self.pool._pop_exc(self)
            within = self.within
            while within is not None:
                if within.exc is not None:
                    raise within.exc.with_traceback(None)
                within = within.within
            if ready():
                return True
            self.waiting = True
            try:
                signalled = self.pool.cond.wait(timeout)
            finally:
                self.waiting = False
            if not signalled:
                return ready()


class _Job(_Wait):
    """An awaited call: the one wait every request of it to its binding uses, on whichever thread
    resolves it, and what settles the code awaiting it -- `future`, on `loop`, in `task`. Its
    callbacks run in `context`, a copy of the awaiting task's.

    Its execution and `interruptible` are decided where it is awaited, since a worker's frames
    hold no agent code. A wake settles the future itself, at once, whatever the worker is doing
    -- waiting, running a callback, or still queued -- and leaves the worker an exception of its
    own to raise at every wait from then on, so it takes no connection again. Cancelling `task`
    from agent code wakes this call alone and leaves its connection for the next taker to tidy.
    """

    __slots__ = ("loop", "task", "future", "context")

    def wake(self, make):
        """Under the pool's condition variable: settle the awaiting code with what `make(self)`
        builds. An interrupt carries where it lands, for `Kernel._interrupted_task`."""
        self.exc, settled = make(self), make(self)
        if type(settled) is KeyboardInterrupt:
            settled._outrig_landed = (settled, self.task, self.execution)
        _settle_soon(self, None, settled)

    def abandoned(self, future):
        """The future's done callback: its awaiting task was cancelled, so the worker stops, on
        whichever binding a callback's call has it waiting."""
        if future.cancelled():
            with self.pool.cond:
                if self.exc is None:
                    self.exc = asyncio.CancelledError()
            self.pool.kernel._notify_hosted()


def _job_of(wait):
    """The awaited call that `wait` is, or was made inside, if any."""
    return wait if type(wait) is _Job else getattr(wait, "within", None)


def _settle_soon(job, value, exc):
    """Settle `job`'s awaiting code with `value`, or with `exc`, through its loop."""
    with contextlib.suppress(RuntimeError):  # the loop has closed; nobody awaits the call
        _with_room(job.loop.call_soon_threadsafe, _settle, job.future, value, exc)


def _settle(future, value, exc):
    """On the loop: settle an awaited call's future, unless it was settled first. On the primary,
    an interrupt settled here disarms the signal on its way for the same interrupt, so the code
    that has it is not interrupted twice."""
    if future.done():
        return
    if exc is None:
        future.set_result(value)
        return
    if type(exc) is KeyboardInterrupt and future.get_loop() is getattr(_primary, "loop", None):
        _primary._interrupting = None
    future.set_exception(exc)


def _notify(cond):
    with cond:
        cond.notify_all()


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

    An awaited call is resolved on a worker of the pool's executor, `POOL_MAX` of them, so a
    call past that many waits in the executor's queue with no thread; the pool keeps every
    awaited call, queued or in progress, for the wake and the close to settle.

    Nothing dispatches, boxes or writes a frame while holding the condition variable.
    """

    def __init__(self, kernel, binding):
        self.kernel = kernel
        self.binding = binding
        self.cond = threading.Condition()
        self.connections = []
        self.closed = None  # the reason, once `close` has run
        self.jobs = set()  # the awaited calls, queued or in progress
        self.executor = None  # their workers, started at first use
        self._root = None
        self._clock = itertools.count()  # orders the connections by their last take

    @property
    def names(self):
        return f"binding {self.binding!r} on agent {self.kernel.agent!r}"

    def submit(self, job, proxy):
        """Queue `job`, resolving `proxy`, on the executor, unless the pool has closed, which
        settled it. A call that cannot be queued is settled with why, and sends nothing should
        its work run all the same."""
        try:
            with self.cond:
                if self.closed is not None:
                    return
                if self.executor is None:
                    self.executor = concurrent.futures.thread.ThreadPoolExecutor(
                        POOL_MAX, f"hosted-{self.kernel.agent}-{self.binding}"
                    )
                self.executor.submit(_run_job, job, proxy)
        except BaseException as e:
            with self.cond:
                self.jobs.discard(job)
                job.exc = e
            _settle_soon(job, None, e)

    def root(self):
        """The binding's root, fetched on a connection of this pool at first use, and again
        when the connection it came on has closed."""
        root = self._root
        if root is not None and not object.__getattribute__(root, "____conn__").closed_either():
            return root
        self._root = root = self.request(_rpyc.core.consts.HANDLE_GETROOT)
        return root

    def request(self, handler, *args):
        """One synchronous request on whichever connection of the pool this thread may use: a
        step of the awaited call the thread is resolving, which waits as that call, or a call of
        its own."""
        ident = threading.get_ident()
        outer = _waits.get(ident)
        if type(outer) is _Job and outer.pool is self:
            wait = outer
        else:
            execution = _CURRENT.get()
            kernel = self.kernel if execution is None else execution.kernel
            wait = _Wait(self, execution, _agent_code_outward(sys._getframe(), kernel))
            wait.within = _job_of(outer)
        with self.cond:
            _waits[ident] = wait
        try:
            conn = self.take(wait)
            try:
                return conn._request_here(handler, *args)
            finally:
                self.release(conn)
        finally:
            with self.cond:
                if outer is None:
                    _waits.pop(ident, None)
                else:
                    _waits[ident] = outer

    def take(self, wait):
        """A connection for this thread's call, as the class comment describes."""
        ident = threading.get_ident()
        # An awaited call keeps the connection its last step took, so a wake can tell it was sent.
        taken = wait.conn
        while True:
            with self.cond:
                wait.block(lambda: self._pick(ident, wait))
            conn = wait.conn
            # Its own, or free: the call's. Otherwise abandoned, with frames to drop -- outside
            # the condition variable, since refusing a request writes -- and then another look.
            if conn.depth > 1 or not conn._request_callbacks:
                return conn
            try:
                conn.tidy()
            finally:
                wait.conn = taken
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
        connection was never sent. Every awaited call is settled at once the same two ways, and
        the executor is shut down without waiting for its workers. A connection a thread holds is
        closed by that thread when its wait ends, since cleaning up releases locks only the holder
        may release."""
        unknown = f"{reason}: the call's outcome on the host is unknown"
        never = f"{reason}: the call was never sent"
        with self.cond:
            self.closed = reason
            for job in self.jobs:
                if job.exc is None:
                    job.wake(lambda job: EOFError(never if job.conn is None else unknown))
            conns = list(self.connections)
            for conn in conns:
                _rpc_channels.pop(conn.key, None)
                conn._channel.close(unknown)
                if conn.owner is None:
                    conn.close()
            for wait in list(_waits.values()):
                if wait.pool is self and wait.conn is None and wait.exc is None:
                    wait.exc = EOFError(never)
            self.cond.notify_all()
        if self.executor is not None:
            self.executor.shutdown(wait=False, cancel_futures=True)
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
        does, so an interrupt that ends a background task's call is reported to its owner. An
        awaited call's stays: its awaiting code has been told, and the worker only unwinds."""
        if type(wait) is _Job:
            return wait.exc
        exc, wait.exc = wait.exc, None
        if wait.by is not None:
            wait.by._landed = (exc, asyncio.current_task(wait.by.loop), wait.execution)
        return exc


# What a hosted object answers itself. Its API is private, since the host refuses every private
# name, so it shadows no hosted attribute; every other name is a step, and a dunder one only where
# the host could answer it -- RPyC's safe list, `__dir__` and `__call__`, set when RPyC loads.
_OWN = frozenset(("_set", "_delete", "_resolve", "_sync", "_isinstance", "_netref", "_binding"))
_HOST_DUNDERS = frozenset()
# The wait of the thread serving the coroutine callback a task runs in: what that task awaits, on
# any binding, is resolved there, on the connection that thread holds to the binding if it holds
# one, whose host thread holds the binding's `serialize` turn.
_SERVING = contextvars.ContextVar("outrig_serving", default=None)
# Per thread, as `waits`: the waits whose queued calls the thread serves, the innermost first. A
# call it runs for one coroutine callback can call back another, which it serves inside that call
# while calls the outer one's other tasks await are still queued for it.
_served = threading.local()
_END = object()  # what `next` gives back for an exhausted iterator
_entered = {}  # by a hosted object's id and a task: the managers its `async with` entered
# Types Python's machinery treats specially, which a hosted object never claims to be: it would be
# taken for a class, a module, a function, a descriptor or a coroutine, and handled as one.
_SPECIAL = (
    type, types.ModuleType, types.FunctionType, types.MethodType, types.BuiltinFunctionType,
    types.CodeType, types.FrameType, types.TracebackType, types.GeneratorType,
    types.CoroutineType, types.AsyncGeneratorType, types.WrapperDescriptorType,
    types.MethodWrapperType, types.MethodDescriptorType, types.ClassMethodDescriptorType,
    types.GetSetDescriptorType, types.MemberDescriptorType, property, classmethod, staticmethod,
    collections.abc.Coroutine, collections.abc.Generator, collections.abc.AsyncGenerator,
)


class _Doc:
    """`HostedObject.__doc__`: the class's text for the class, and for one hosted object that text
    with what is known of it here, so `help(x)` describes `x` rather than the class."""

    def __init__(self, text):
        self.text = inspect.cleandoc(text)

    def __get__(self, obj, cls=None):
        if obj is None:
            return self.text
        _, ref, path = _parts(obj)
        if ref is None or path:
            return f"{self.text}\n\nThis is a path: nothing has been asked of the host yet."
        names = sorted(n for n, v in vars(type(ref)).items() if callable(v) and n[0] != "_")
        return (
            f"{self.text}\n\nThe methods the host listed for {_host_type(ref)}: "
            f"{', '.join(names) or 'none'}."
        )


class HostedObject:
    __doc__ = _Doc(
        """An object on the host, reached through a binding, and used by awaiting it.

        `x.name`, `x[key]` and `x(...)` build a path and send nothing. `await x` resolves the path
        on the host, a request for each step, while the event loop turns, and gives back a value,
        or a hosted object bound to what the host holds, which builds and awaits the same way; what
        the host raises is raised there. `async for` iterates one, and `async with` enters one.

        Nothing else asks the host anything. `repr`, `dir`, `help`, `type` and `hash` answer here,
        and `len`, truthiness, `for`, `in`, `==`, operators, `with` and assignment raise
        `TypeError` naming what to await instead, such as `await x.__len__()`. `hasattr(x, name)`
        is always true: a missing attribute raises `AttributeError` where it is awaited.

        `await x._set(name, value)` and `await x._delete(name)` set and delete an attribute, and
        `await x._isinstance(obj)` asks the host whether `obj` is an instance of `x`.
        `x._resolve()` is a coroutine that awaits `x`, for `asyncio.create_task(x._resolve(),
        name=...)`. `x._sync()` resolves `x` on the calling thread, for a plain function the host
        calls back; it is refused on an event loop's thread. `x._netref` is a resolved object's
        RPyC proxy, and `x._binding` the binding's name."""
    )
    __module__ = "outrig"
    __slots__ = ("_parts", "__weakref__")  # the pool, the proxy resolved to or `None`, the steps
    __hash__ = object.__hash__

    def __init__(self, pool, ref=None, path=()):
        object.__setattr__(self, "_parts", (pool, ref, path))

    def __getattribute__(self, name):
        if name in _OWN or (name[:2] == name[-2:] == "__" and name not in _HOST_DUNDERS):
            return object.__getattribute__(self, name)
        return _then(self, f".{name}", getattr, name)

    def __getitem__(self, key):
        return _then(self, f"[{key!r}]", operator.getitem, key)

    def __call__(self, *args, **kwargs):
        return _then(self, "(...)", _call, args, kwargs)

    def __setattr__(self, name, value):
        raise TypeError(
            f"an assignment cannot be awaited: `await x._set({name!r}, value)` assigns on the host"
        )

    def __delattr__(self, name):
        raise TypeError(
            f"a deletion cannot be awaited: `await x._delete({name!r})` deletes on the host"
        )

    def __await__(self):
        """Queue the path on the pool's executor -- or, awaited in a coroutine callback, on the
        thread serving its call -- with what a worker cannot read from its own frames, and settle
        once it is resolved. In `_MACHINERY`, so an interrupt cannot land while it is queued."""
        pool, ref, path = _parts(self)
        if ref is not None and not path:
            return self
        execution = _awaiting(pool)
        loop = asyncio.get_running_loop()
        job = _Job(pool, execution, _agent_code_outward(sys._getframe(1), pool.kernel))
        job.loop, job.task, job.future = loop, asyncio.current_task(loop), loop.create_future()
        job.future.add_done_callback(job.abandoned)
        # Without `_SERVING`: a plain callback may hold its thread until a coroutine it started
        # ends, so that coroutine's calls go to the executor.
        job.context = contextvars.copy_context()
        job.context.run(_SERVING.set, None)
        serving = _SERVING.get()
        with pool.cond:
            if pool.closed is not None:
                raise EOFError(f"{pool.closed}: the call was never sent")
            pool.jobs.add(job)
        nested = False
        if serving is not None:
            # A queue holds calls to any binding, under its serving wait's condition variable.
            with serving.pool.cond:
                nested = serving.nested is not None
                if nested:
                    serving.nested.append((job, self))
        if nested:
            # The serving thread may be waiting on another binding, in a callback nested inside.
            pool.kernel._notify_hosted()
        else:
            pool.submit(job, self)
        return (yield from job.future)

    def __aiter__(self):
        return _items(self)

    async def __anext__(self):
        item = await _then(self, ".__next__()", next, _END)
        if item is _END:
            raise StopAsyncIteration
        return item

    async def __aenter__(self):
        manager, value = await _then(self, ".__enter__()", _enter)
        _entered.setdefault((id(self), asyncio.current_task()), []).append(manager)
        return value

    async def __aexit__(self, typ, exc, tb):
        key = (id(self), asyncio.current_task())
        managers = _entered[key]
        manager = managers.pop()
        if not managers:
            del _entered[key]
        return await _then(manager, ".__exit__(...)", _exit, exc)

    def _set(self, name, value):
        return _then(self, f"._set({name!r}, ...)", setattr, name, value)

    def _delete(self, name):
        return _then(self, f"._delete({name!r})", delattr, name)

    async def _isinstance(self, obj):
        if type(obj) is HostedObject:
            obj = await obj  # on its own binding, as any await
        return await _then(self, "._isinstance(...)", _instance_of, obj)

    async def _resolve(self):
        return await self

    def _sync(self):
        current = threading.current_thread()
        if current is _reader_thread or any(k.thread is current for k in list(_kernels.values())):
            raise TypeError(
                f"{self!r}._sync() would hold this kernel's event loop until the host answered: "
                f"`await` it instead. `_sync()` is for a plain-function callback, which runs on "
                f"a worker and cannot await"
            )
        _awaiting(_parts(self)[0])
        return _resolved(self)

    @property
    def _netref(self):
        _, ref, path = _parts(self)
        return None if path else ref

    @property
    def _binding(self):
        return _parts(self)[0].binding

    @property
    def __class__(self):
        return _local_class(self)

    def __instancecheck__(self, obj):
        """`isinstance(obj, x)` for a hosted class `x`: answered here for an instance of exactly
        that class, and for an object of this process, against the class's library here; any
        other answer is the host's, which `await x._isinstance(obj)` asks for."""
        pool, cls, path = _parts(self)
        if cls is None or path or object.__getattribute__(cls, "____id_pack__")[2]:
            raise TypeError(f"isinstance() arg 2 must be a class, and {self!r} is not one")
        if type(obj) is not HostedObject:
            return _instance_of(cls, obj)
        other, ref, rest = _parts(obj)
        if other is pool and ref is not None and not rest:
            a, b = (object.__getattribute__(p, "____id_pack__") for p in (cls, ref))
            if a[1] == b[1]:
                return b[2] != 0
        raise TypeError(
            f"whether {obj!r} is an instance of {self!r} is the host's to say: "
            f"`await x._isinstance(obj)` asks it"
        )

    def __repr__(self):
        pool, ref, path = _parts(self)
        if ref is not None and not path:
            return f"<hosted {_host_type(ref)} from {pool.binding}>"
        start = pool.binding if ref is None else f"({_host_type(ref)} from {pool.binding})"
        return f"<hosted {start}{''.join(step[0] for step in path)}: unresolved, await it>"

    def __dir__(self):
        _, ref, path = _parts(self)
        names = set(_OWN)
        if ref is not None and not path:
            names.update(name for name, value in vars(type(ref)).items() if callable(value))
        return sorted(names)


_parts = HostedObject._parts.__get__


def _refusing(name, advice):
    """A special method of a hosted object, which would need the host's answer without an
    await."""

    def refused(self, *args):
        raise TypeError(f"{name} of a hosted object needs the host's answer: {advice}")

    return refused


for _name, _advice in {
    "__iter__": "iterate it with `async for item in x`",
    "__len__": "`await x.__len__()`",
    "__bool__": "`await x.__bool__()`, or `await x.__len__()` for a container",
    "__contains__": "`await x.__contains__(item)`",
    "__enter__": "enter it with `async with x:`",
    "__exit__": "enter it with `async with x:`",
    "__setitem__": "`await x.__setitem__(key, value)`",
    "__delitem__": "`await x.__delitem__(key)`",
    **{
        f"__{op}__": f"`await x.__{op}__(other)`; `is` compares the proxies here"
        for op in "eq ne lt le gt ge".split()
    },
    **{
        f"__{op}__": f"`await x.__{op}__()`"
        for op in "int float complex index neg pos abs invert round trunc floor ceil".split()
    },
    **{
        f"__{form}{op}__": f"`await x.__{form}{op}__(other)`"
        for op in (
            "add sub mul matmul truediv floordiv mod divmod pow lshift rshift and xor or"
        ).split()
        for form in ("", "r")
    },
    "__reduce_ex__": "it names an object in another process, and cannot be copied or pickled",
}.items():
    setattr(HostedObject, _name, _refusing(_name, _advice))
del _name, _advice


def _then(proxy, label, fn, *args):
    """`proxy` with one more step, shown as `label`: `fn(what the path gave, *args)`."""
    pool, ref, path = _parts(proxy)
    return HostedObject(pool, ref, (*path, (label, fn, *args)))


def _call(fn, args, kwargs):
    return fn(*args, **kwargs)


def _enter(manager):
    return manager, type(manager).__enter__(manager)


def _exit(manager, exc):
    # RPyC's proxy sends the type it is given first; `_ctxexit_args` takes the exception itself.
    # Truth-tested here, which asks the host for a proxy: `async with` would test a hosted object,
    # which refuses. Only for an exception, as `with` tests it.
    suppress = type(manager).__exit__(manager, exc, None, None)
    return exc is not None and bool(suppress)


def _instance_of(cls, obj):
    """`isinstance(obj, cls)` for the class a hosted object resolved to: RPyC's answer for a
    hosted object -- a request, unless the class ids settle it -- and for an object of this
    process the answer of the class's library here, `False` when it does not import."""
    if type(obj) is HostedObject:
        return isinstance(_parts(obj)[1], cls)
    local = _named_class(cls)
    return local is not None and isinstance(obj, local)


async def _items(proxy):
    """`async for` over a hosted object: a request for its iterator, then one for each item."""
    it = await _then(proxy, ".__iter__()", iter)
    if type(it) is not HostedObject:
        it = HostedObject(_parts(proxy)[0], it)  # a value's own iterator, stepped the same way
    while (item := await _then(it, ".__next__()", next, _END)) is not _END:
        yield item


def _replay(proxy):
    """What `proxy` names, found on this thread: the binding's root, or the proxy it was resolved
    to, then each step as RPyC's proxies do it -- a request for each, two for a method's call.
    The host's refusal to read a name is raised as an `AttributeError`, as a missing one is."""
    pool, obj, path = _parts(proxy)
    if obj is None:
        obj = pool.root()
    for _, fn, *args in path:
        try:
            obj = fn(obj, *args)
        except Exception as e:
            if fn is not getattr or _type_name(type(e)) != "outrig.Refused":
                raise
            raise _attribute_error(e) from None
    return obj


def _attribute_error(e):
    """The host's refusal `e`, as an `AttributeError` of the same name and text."""
    cls = type(e)
    error = type(_type_name(cls), (cls, AttributeError), {"__module__": cls.__module__})(*e.args)
    error.__dict__.update(e.__dict__)
    return error


def _resolved(proxy):
    """`proxy` resolved on this thread, as agent code receives it."""
    return _wrapped(_replay(proxy))


def _wrapped(obj):
    """A host's answer as agent code sees it: a proxy as a resolved hosted object -- the same one
    for the same proxy, so one host object returned twice on a connection is one object here --
    inside a tuple too, and a value as itself."""
    if issubclass(type(obj), _rpyc.core.netref.BaseNetref):
        conn = object.__getattribute__(obj, "____conn__")
        pack = object.__getattribute__(obj, "____id_pack__")
        proxy = conn._facades.get(pack)
        if proxy is None:
            proxy = conn._facades[pack] = HostedObject(conn._pool, obj)
        return proxy
    if type(obj) is tuple:
        return tuple(_wrapped(item) for item in obj)
    return obj


def _host_type(ref):
    """The name of a proxy's host type, or of the class it is, from its id pack."""
    name, _, instance = object.__getattribute__(ref, "____id_pack__")
    return str(name) if instance else f"class {name}"


_type_module = type.__dict__["__module__"].__get__


def _named_class(ref):
    """The local class RPyC's proxy class names for the host's type -- for a proxy of a class,
    that class -- if it has the host's own module and name; else `None`."""
    cls = getattr(vars(type(ref)).get("__class__"), "instance", None)
    if not isinstance(cls, type):
        return None
    name = object.__getattribute__(ref, "____id_pack__")[0]
    return cls if f"{_type_module(cls)}.{_type_name(cls)}" == name else None


def _local_class(proxy):
    """What a hosted object's `__class__` says, asking nothing: for a resolved instance, its host
    type's class here -- so `isinstance(c, Commit)` holds where the library imports -- unless that
    is one of `_SPECIAL`; else `HostedObject`."""
    _, ref, path = _parts(proxy)
    if ref is not None and not path and object.__getattribute__(ref, "____id_pack__")[2]:
        cls = _named_class(ref)
        if cls is not None and not issubclass(cls, _SPECIAL):
            return cls
    return HostedObject


def _kind(value):
    """The type `Kernel.held` reports for `value`, asking nothing: a resolved hosted object's
    names the host's type."""
    if type(value) is HostedObject:
        _, ref, path = _parts(value)
        if ref is not None and not path:
            return f"HostedObject[{_host_type(ref)}]"
    return _type_name(type(value))


def _awaiting(pool):
    """The execution using `pool`, refusing one of another kernel: an interrupt of a kernel
    reaches only its own pools' calls."""
    execution = _CURRENT.get()
    if execution is not None and execution.kernel is not pool.kernel:
        raise TypeError(
            f"a hosted object of agent {pool.kernel.agent!r} cannot be used by agent "
            f"{execution.kernel.agent!r}: each agent reaches a binding through its own"
        )
    return execution


def _run_job(job, proxy, within=None):
    """Resolve `proxy` for `job` on this thread, in its context -- a worker of the pool's executor,
    or the thread serving the coroutine callback it was awaited in, where it ends with `within`,
    the call that callback was served for -- and settle the awaiting code through its loop."""
    pool, ident = job.pool, threading.get_ident()
    with pool.cond:
        outer = _waits.get(ident)
        job.within = within
        _waits[ident] = job
    try:
        value, exc = job.context.run(_resolved, proxy), None
    except BaseException as e:
        # Raised where the call is awaited, without this thread's frames.
        value, exc = None, e.with_traceback(None)
    finally:
        with pool.cond:
            if outer is None:
                _waits.pop(ident, None)
            else:
                _waits[ident] = outer
            pool.jobs.discard(job)
    _settle_soon(job, value, exc)


def _load_rpyc(directory):
    """Import RPyC from `directory` and build the connection class over it."""
    global _rpyc, _Hosted, _rpyc_missing, _MACHINERY, _HOST_DUNDERS
    sys.path.append(directory)
    try:
        import rpyc
    except Exception as e:
        _rpyc_missing = f"RPyC could not be imported from {directory}: {e!r}"
        _diag(_rpyc_missing)
        return
    _rpyc = rpyc
    _Hosted = _connection_class(rpyc)
    safe = rpyc.core.protocol.DEFAULT_CONFIG["safe_attrs"]
    _HOST_DUNDERS = frozenset(safe | {"__dir__", "__call__"}) - {"__doc__", "__new__"}
    # A signal landing while a request is encoded or written is declined, as one landing in
    # `_write_line` is: RPyC registers the request's callback before `_send` runs, so the
    # request is outstanding from before its first byte goes out.
    _MACHINERY = _MACHINERY | frozenset([_Hosted._send.__code__])


def _connection_class(rpyc):
    """The connection class a kernel opens to a binding, built over the imported `rpyc`."""
    consts = rpyc.core.consts
    brine = rpyc.core.brine
    vinegar = rpyc.core.vinegar
    netref = rpyc.core.netref
    Connection = rpyc.core.protocol.Connection
    get_id_pack = rpyc.lib.get_id_pack

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
            # The hosted object of each proxy this connection made, by id pack, while it lives.
            self._facades = weakref.WeakValueDictionary()

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

        def _send(self, msg, seq, args):
            """RPyC's, without its shared send queue -- a frame past the bound raises in the thread
            that asked, and a finalizer's release is queued by `_async_request` rather than sent --
            and when a frame could not be written whole, which closed the channel, the binding is
            told, so it releases the connection's objects and thread."""
            data = brine.I1.pack(msg) + brine.dump((seq, args))
            try:
                with self._sendlock:
                    self._channel.send(data)
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
            saved = self._context.marks
            self._context.marks = marks = []
            try:
                seq = self._get_seq_id()
                self._request_callbacks[seq] = callback
                self._marks[seq] = marks
                try:
                    self._send(consts.MSG_REQUEST, seq, (handler, self._box(args)))
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
                self._send(consts.MSG_REQUEST, seq, (consts.HANDLE_DEL, boxed))

        def _ctxexit_args(self, args):
            """A proxy's `__exit__` sends only the exception's type. The exception itself is
            `sys.exception()` while a `with` block unwinds -- or, from a hosted object's
            `__aexit__`, sent in the type's place -- and goes by value when its type is the one
            sent; otherwise the type alone, and `None` stays `None`."""
            proxy, exc = args
            if exc is None:
                return (proxy, None)
            current = sys.exception()
            if isinstance(exc, BaseException):
                current, exc = exc, type(exc)
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
            # Before anything else: a hosted object, or a proxy of a host list, answers
            # `isinstance(x, list)`, and a proxy of a host method is callable.
            if type(obj) is HostedObject:
                pool, ref, path = _parts(obj)
                if pool is not self._pool:
                    raise self._foreign(pool.names)
                if ref is None or path:
                    # Resolved here, on the thread that sends this request, nested on its
                    # connection; a dict's key is never rebuilt, so no proxy is hashed.
                    return self._box(_replay(obj), seen)
                obj = ref
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
                return super()._unbox_exc(raw)
            except Exception as e:
                return vinegar.GenericException(
                    f"an exception from {self.names} could not be rebuilt: {e!r}"
                )

        # ------------------------------------------------------------------ requests

        def sync_request(self, handler, *args):
            """A proxy's request, on whichever of its kernel's connections to this binding the
            pool gives: `netref.syncreq` calls this on the connection that produced the proxy,
            and the binding resolves the proxy on any of them. A proxy of a closed connection
            raises the reason without sending."""
            if self.closed_either():
                raise EOFError(self._channel.reason or f"the connection to {self.names} is closed")
            return self._pool.request(handler, *args)

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
            between frames, and nowhere else.
            """
            wait = _waits.get(threading.get_ident()) or _Wait(self._pool, None, False)
            channel = self._channel
            with self._pool.cond:
                if not wait.block(lambda: channel.ready, rpyc.lib.Timeout(timeout).timeleft()):
                    return False
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
            """Serve the host's call of a callback: a plain function runs here, and a coroutine
            function's coroutine on the kernel's loop, served from here (`_awaited`). Inside an
            awaited call -- its own request, or one a callback of it makes -- the proxies the host
            passes arrive as hosted objects. What it returns goes back by the argument rules with
            callables refused too; an interrupt, a cancel or an exit it raised is answered and
            then raised here, so the execution ends with it. A request for a call this side gave
            up on is refused, and the callback does not run."""
            if not serving:
                exc = RuntimeError(
                    "the call this callback belongs to was interrupted in the container; the "
                    "callback did not run"
                )
                self._send(consts.MSG_EXCEPTION, seq, self._box_exc(RuntimeError, exc, None))
                return
            saved = self._context.marks
            self._context.marks = None
            raised = None
            try:
                try:
                    handler, args = raw_args
                    args = self._unbox(args)
                    if _job_of(_waits.get(threading.get_ident())) is not None:
                        args = _wrapped(args)
                    res = self._HANDLERS[handler](self, *args)
                    # By its type: a proxy's class has every name its host type has, and its
                    # `__class__` would be a request.
                    coroutine = issubclass(type(res), collections.abc.Coroutine)
                    if coroutine and not issubclass(type(res), netref.BaseNetref):
                        res = self._awaited(res)
                    reply = (consts.MSG_REPLY, self._box(res))
                except BaseException as e:
                    t, v, tb = sys.exc_info()
                    self._last_traceback = tb
                    reply = (consts.MSG_EXCEPTION, self._box_exc(t, v, tb))
                    if not isinstance(e, Exception):
                        raised = e
            finally:
                self._context.marks = saved
            try:
                self._send(reply[0], seq, reply[1])
            except Exception:
                # The interrupt is the result; a channel that closed meanwhile is not.
                if raised is None:
                    raise
            if raised is not None:
                raise raised

        def _awaited(self, coro):
            """Run the coroutine a callback returned on the kernel's loop and serve, meanwhile, on
            this thread, the hosted calls it awaits, on any binding, and the frames that arrive
            here. A call goes on the connection this thread holds to its binding, if any -- this
            one, or that of a call this thread is inside -- whose host thread holds the `serialize`
            turn the call needs. The calls queued for the coroutine callbacks this thread serves
            further out are run meanwhile too: one of their tasks may be what this coroutine waits
            for. Refused on the loop's own thread, which is waiting for the host."""
            kernel, cond = self._pool.kernel, self._pool.cond
            if threading.current_thread() is kernel.thread:
                coro.close()
                raise TypeError(
                    "a coroutine function cannot be called back while its kernel's thread waits "
                    "for the host: await the call it is passed to"
                )
            wait = _waits[threading.get_ident()]
            created = wait.nested is None
            if created:
                wait.nested = collections.deque()
            # Set while the task is made, which takes a copy of this thread's context.
            token = _SERVING.set(wait)
            try:
                done = asyncio.run_coroutine_threadsafe(coro, kernel.loop)
            finally:
                _SERVING.reset(token)
            done.add_done_callback(lambda _: _notify(cond))
            outer = getattr(_served, "waits", ())
            _served.waits = served = (wait, *outer)
            try:
                while not done.done():
                    with cond:
                        wait.block(
                            lambda: done.done()
                            or any(w.nested for w in served)
                            or self._channel.ready
                        )
                    # Taken outside the queue's condition variable, which may be another pool's:
                    # only this thread takes from a queue it serves. A call ends with the call its
                    # own callback was served for.
                    serving = next((w for w in served if w.nested), None)
                    if serving is not None:
                        _run_job(*serving.nested.popleft(), _job_of(serving))
                    elif not done.done():
                        self.serve()
                return done.result()
            finally:
                _served.waits = outer
                done.cancel()
                if created:
                    # What was queued too late to be served here goes to its binding's executor.
                    with cond:
                        leftover, wait.nested = wait.nested, None
                    for job, proxy in leftover:
                        job.pool.submit(job, proxy)

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

# Agents whose takes the host has asked to be told of, by id.
_observed = set()


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
        message, delivery = await self._when(self._receivers, self._take)
        if self._agent in _observed:
            # Taken already, so nothing here may raise and lose it: what the host is told is a
            # record of the take, and a take it never hears of is one the record lacks.
            try:
                _write_line(
                    _encode(
                        {"t": "took", "agent": self._agent, "channel": self._name, "message": message}
                    )
                )
            except BaseException as e:
                try:
                    _diag(f"took a message on {self._name!r}, but saying so failed: {e!r}")
                except BaseException:
                    pass
        return delivery

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

    def _deliver(self, body, sender, message):
        """Queue `body` from `sender`, posted as `message`, and return how many messages then wait.
        Reader thread only.

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
            # Beside the id it was posted under, which a take is reported by.
            self._queue.append((message, delivery))
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
    context: see `help(runtime.history)`. Not all of it is sent to the model on each call:
    `runtime.context.promote(turn)` sends a turn again until `runtime.context.demote(turn)`, and
    every call is held to the model's context window: see `help(runtime.context)`.

    `runtime.python` says what this interpreter can import -- `pip install` adds pure-Python
    packages, and nothing compiled loads -- and where code that needs more can run.

    `runtime.bindings` lists the objects the session hosts for you on the host, by name, with what
    each is; asking makes no request to any of them.
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
        for f in fs:
            if type(f) is HostedObject:
                raise TypeError(
                    f"{f!r} is awaitable, not a future: wait on a task, as "
                    f"`asyncio.create_task(x._resolve(), name=...)`"
                )
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
        # Hosted objects: a pool of connections per binding, each opened on the thread whose
        # call needs it, and numbered for the life of the kernel so a frame for a closed one is
        # never taken for a new one's.
        self._hosted = {}
        self._hosted_lock = threading.Lock()
        self._hosted_numbers = collections.defaultdict(lambda: itertools.count(1))

    def announce(self):
        _send({"t": "ready", "agent": self.agent, "version": _VERSION})

    def stub(self, binding):
        """The root of `binding` -- an object in a process on the host -- as the `HostedObject`
        agent code awaits. Making it resolves nothing."""
        return HostedObject(self._pool(binding))

    def hosted(self, binding):
        """The root of `binding` as a proxy over one of this kernel's connections to it: the
        synchronous engine under `stub`, for tests, bound to no name agent code sees.

        The first connection is opened on the calling thread at first use, and the root fetched
        then -- never on the reader thread, which is what delivers the reply this waits for. A
        proxy from a closed connection raises `EOFError` with the reason, and the next call here
        fetches the root again on a live connection.
        """
        if threading.current_thread() is _reader_thread:
            raise RuntimeError("a hosted object cannot be resolved on the reader thread")
        return self._pool(binding).root()

    def _pool(self, binding):
        """This kernel's pool of connections to `binding`, made anew once the last has closed."""
        if _Hosted is None:
            raise RuntimeError(_rpyc_missing)
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
        finish, its outcome unknown here, and one waiting for a connection, or queued, was never
        sent. What a release of the kernel (`0003-26`) and the session's close (`0003-22`) call;
        runs on any thread."""
        with self._hosted_lock:
            names = list(self._hosted) if binding is None else [binding]
            pools = [self._hosted.pop(name) for name in names if name in self._hosted]
        for pool in pools:
            pool.close(reason)
        self._notify_hosted()  # a callback of a call it settled may wait on another binding

    def _open_hosted(self, binding, pool):
        """A new connection to `binding` for `pool`, which holds its condition variable. Called
        under it, and writes nothing: the first frame goes out with the first request."""
        agent = self.agent
        number = next(self._hosted_numbers[binding])

        def write(part, more):
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
                    }
                )
            )

        channel = FrameChannel(write, cond=pool.cond)
        _rpc_channels[(agent, binding, number)] = channel
        return _Hosted(channel, agent=agent, binding=binding, number=number, pool=pool)

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

        The task was holding the loop -- which is why the interrupt went where it did -- or
        awaiting a hosted call the interrupt woke, and it belongs to the execution that started
        it, so that is who is told, through the same route the task's own output takes.
        Retrieving the exception keeps asyncio from reporting the same death a second time when
        the task is collected; letting go of what the handler kept is what allows it to be
        collected at all.
        """
        landed, self._landed = self._landed, None
        what = "a task that was holding the event loop"
        if (landed is None or landed[0] is not exc) and type(exc) is KeyboardInterrupt:
            # An awaited call's: several can escape at once, so each says where it landed.
            landed = exc.__dict__.pop("_outrig_landed", None)
            what = "a task awaiting a hosted call"
        if landed is None or landed[0] is not exc:
            return False
        _, task, owner = landed
        if task is not None:
            with contextlib.suppress(BaseException):
                task.exception()
        text = f"[outrig interrupted {what}]\n{_format_error(exc)}"
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
        hosted call, whose wait this wakes with the `CancelledError` first, as it settles every
        awaited call of the execution; the loop's cancel then finds a finished task, or one that
        caught the error and carried on. A request for an execution that has already finished is
        the ordinary race of a Ctrl-C with a result, and is dropped without comment.
        """
        if not self._holds(exec_id):
            return

        def cancelled(wait):
            return asyncio.CancelledError(_woken_text(wait, "cancelled"))

        self._wake(exec_id, False, cancelled)
        # On the loop's queue ahead of its own cancel, so the code sees this one's message.
        self._wake_jobs(exec_id, cancelled)
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
        """Interrupt `exec_id`'s agent code: wake the hosted call its thread is waiting in and
        every call it awaits, on any kernel, and aim a SIGINT at the loop's thread, on the
        primary, unless that thread's call was woken. Runs on the reader thread.

        Handled here rather than through the loop, whose queue is exactly what a wedged loop is
        not draining. A thread waiting for a binding to answer waits on a condition variable
        this thread can signal, so that interrupt needs no signal and reaches a child's kernel
        too; it raises in the caller, between frames. Otherwise the signal is sent to the main
        thread itself: `signal.raise_signal` would signal this one, and a main thread blocked in
        `waitpid` or `sleep` would never wake to run the handler. `runaway` widens where it may
        land -- see `_on_sigint`.
        """
        woken = jobs = None
        if self._holds(exec_id):
            if self is _primary:
                # Armed before an awaited call is woken, whose delivery disarms it: the code that
                # receives the interrupt is not given a second by the signal.
                self._interrupting, self._landed = (exec_id, runaway), None

            def interrupted(wait):
                return KeyboardInterrupt(_woken_text(wait, "interrupted"))

            woken = self._wake(exec_id, runaway, interrupted)
            jobs = self._wake_jobs(exec_id, interrupted)
            if woken == "woken":
                self._interrupting = None
                return
        if self is not _primary:
            # Python runs signal handlers on the main thread alone, and only the primary is
            # there. A wedged sub-agent is contained, not recoverable -- unless it is inside a
            # hosted call, which raises when it next waits.
            if not jobs:
                later = "; it will raise when its hosted call next waits" if woken == "busy" else ""
                _diag(
                    f"agent {self.agent!r} cannot be interrupted: it is not on the main "
                    f"thread{later}"
                )
            return
        if not self._holds(exec_id):
            return
        if signal.getsignal(signal.SIGINT) is not _on_sigint:
            _diag(
                f"execution {exec_id!r} cannot be interrupted: SIGINT's handler has been "
                f"replaced, so only a cancel can reach it"
            )
            return
        signal.pthread_kill(self.thread.ident, signal.SIGINT)

    def _wake(self, exec_id, runaway, make_exc):
        """Leave the exception `make_exc(wait)` builds for this kernel's thread to raise from
        the hosted call it is inside, if it is inside one that `exec_id`'s interrupt may end.
        Runs on the reader thread.

        Returns `woken` when the thread was waiting -- for a reply, or for a free connection --
        and raises at once; `busy` when it is inside a hosted call but not waiting -- running a
        callback's code, boxing, sending -- and raises when it next waits, unless the call ends
        first; `None` when there is nothing to wake, or when the rule `_on_sigint` applies says
        this interrupt may not end what the thread is doing: the call is this program's own, or
        it belongs to a task of another execution's and the host did not find the loop spinning.
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
            if wait.exc is None:
                wait.exc, wait.by = make_exc(wait), self
                wait.pool.cond.notify_all()
            return "woken" if wait.waiting else "busy"

    def _wake_jobs(self, exec_id, make_exc):
        """Settle every call `exec_id`'s execution awaits from agent code -- queued, waiting, or
        running a callback -- at once, with what `make_exc(call)` builds, and leave its worker the
        same to raise. `runaway` does not widen this: a wedge is on the loop's thread, never in a
        suspended await. Runs on the reader thread; returns whether any call was woken."""
        running = self.running
        if running is None or running.id != exec_id:
            return False
        woken = False
        for pool in list(self._hosted.values()):
            with pool.cond:
                for job in pool.jobs:
                    if job.execution is running and job.interruptible and job.exc is None:
                        job.wake(make_exc)
                        woken = True
        self._notify_hosted()
        return woken

    def _notify_hosted(self):
        """Wake every thread waiting on this kernel's connections to look again: a callback's
        call can wait on one binding for an awaited call of another."""
        for pool in list(self._hosted.values()):
            with pool.cond:
                pool.cond.notify_all()

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
        so it answers whatever the values are, and whoever asks. A resolved hosted object's type
        names the host's, from what its proxy already holds.
        """
        return sorted(
            (name, _kind(value))
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
                if not task.done() and getattr(task.get_coro(), "cr_code", None) is not _WRAPPER
            )
            more = max(len(running) - TASKS_MAX, 0)
            names = [name[:TASK_NAME_MAX] for name in running[:TASKS_MAX]]
        except Exception as e:
            _diag(f"agent {self.agent!r}: listing its tasks failed: {e!r}")
        _send({"t": "tasks", "agent": self.agent, "id": request_id, "names": names, "more": more})


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
    if not _agent_code_outward(frame, kernel):
        return
    exc = KeyboardInterrupt("interrupted by outrig")
    kernel._landed = (exc, asyncio.current_task(kernel.loop), _CURRENT.get())
    raise exc


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
    return task is not None and getattr(task.get_coro(), "cr_code", None) is not _WRAPPER


# Machinery that can sit nearer the signal than agent code's frames; see `_on_sigint`. Every
# callback the loop runs, this program's own included, runs beneath `Handle._run`, so the loop's
# two dispatch frames cover them all -- including when agent code pumps the loop by hand. What else
# agent code triggers synchronously and must not be interrupted is a protocol line on its way out,
# which `Endpoint.send` writes, a hosted call being queued, which would run with nobody awaiting
# it, and the fork hooks. This program's output routing, by contrast, is deliberately absent: a
# runaway that prints spends most of its time there, and must still be interruptible.
_MACHINERY = frozenset(
    fn.__code__
    for fn in (
        asyncio.base_events.BaseEventLoop._run_once,
        asyncio.events.Handle._run,
        _write_line,
        FrameChannel.send,
        HostedObject.__await__,
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
    one package.
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
        pending = endpoint._deliver(message.get("body"), "user", request_id)
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


def _observe(kernel, _request_id, _message):
    """Say from now on when the agent's code takes a message from one of its channels.

    Nothing needs to know that to deliver one -- the host is told only because it records what the
    agent does -- so nothing says it until the host asks.
    """
    _observed.add(kernel.agent)


def _pending(kernel, request_id, _):
    """Say how many messages wait on each of the agent's channels, and how many have ever been
    delivered to it, reading none of them."""
    channels = {name: endpoint._counts() for name, endpoint in kernel.channels.items()}
    _send({"t": "pending", "agent": kernel.agent, "id": request_id, "channels": channels})


def _rpc(kernel, number, message):
    """Hand one part of an RPyC frame to the channel of connection `number`, or drop it. Reader
    thread only.

    A part for a connection that has closed, or that this kernel never opened, is answered with a
    close notice rather than buffered, so nothing waits on it. The part is decoded before anything
    changes, so `_handle`'s retry after running out of memory never applies one twice; the notice
    comes after, and is given up rather than retried.
    """
    binding = message.get("binding")
    if not isinstance(binding, str):
        raise ValueError("an rpc message without a binding")
    channel = _rpc_channels.get((kernel.agent, binding, number))
    if "closed" in message:
        if channel is not None:
            channel.close(f"the host closed this connection: {message['closed']}")
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
# answered on the agent's loop; the rest are handled here, on the reader thread.
_ROUTES = {
    "exec": _exec,
    "msg": _msg,
    "pending": _pending,
    "received": _received,
    "turn": _turn,
    "observe": _observe,
    "inv": _inventory,
    "tasks": _tasks,
    "rpc": _rpc,
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
    if kind == "bindings":
        _bindings(message)
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
    if len(sys.argv) not in (2, 3) or not _valid_agent(sys.argv[1]):
        _diag(f"usage: python3 -I -c <program> <agent-id> [<rpyc-dir>]; got {sys.argv[1:]!r}")
        os._exit(2)
    global _primary, _reader_thread
    # Before any agent exists, and every process it starts inherits it.
    _set_ceiling()
    # Before the workspace joins `sys.path`, so a module there named `rpyc` is not what this
    # process imports, and before any agent code runs, so `sys.modules` holds this copy for the
    # interpreter's life: a later `pip install rpyc` changes nothing in here.
    if len(sys.argv) == 3:
        _load_rpyc(sys.argv[2])
    _open_imports()
    primary = Kernel(sys.argv[1])
    primary.thread = threading.current_thread()
    _kernels[primary.agent] = primary
    _primary = primary
    # Before the reader starts, so no interrupt can arrive with nothing to handle it.
    signal.signal(signal.SIGINT, _on_sigint)
    primary.announce()
    _reader_thread = threading.Thread(target=_read, name="reader", daemon=True)
    _reader_thread.start()
    primary.serve()


if __name__ == "__main__":
    main()
