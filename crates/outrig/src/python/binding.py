"""The binding program: a session's hosted libraries, served over RPyC to the kernels of one
session by `outrig._serve`, the serving module a program on the host serves its live objects with.

One process holds every binding of a session. Started on the host by the Rust owner with the
payload's `python3 -I -c <this program>` and two arguments, the directory holding OutRig's own
package and the vendored RPyC's directory, it reads the session's bindings from its first line of
input: each one's name, its factory as `module:callable` with the arguments to call it with, and
whether it serializes; and the package directories, in the order they go on `sys.path`. It imports
each factory and calls it once, in order, and serves the object it returned as the root of every
connection to that binding. The arguments arrive on stdin rather than the command line, which any
user on the host can read.

The bindings share what a process has: its environment, its `sys.path` -- where two bindings'
packages hold different versions of one library, the first binding's is the one imported, as in the
container -- and its life.

Its stdin and stdout carry NDJSON, one kind of line at a time: RPyC frames tagged by binding, agent
and connection, split into bounded parts (`rpc` lines), both ways; from the owner first, a `start`
line holding the bindings and the package directories; a `ready` line once every factory has
returned, naming the type of what each returned, or a `failed` line naming the binding whose factory
raised and what it raised; `event` lines -- a library's own, and each request's record: its
receipt, its dispatch, which holds the request until the owner answers whether the event is
`published`, refused if it is not, and its end; `cancel` notices of requests whose callers were
interrupted; and the `control` line that says the session has closed. Everything else it writes
goes to stderr, where the owner logs it.

The process is a supervised child of the owner, in a process group of its own, started with a
parent-death signal (`SIGHUP`) the owner set before exec and left blocked until `main` has
installed the handler for it. When that signal arrives, or stdin reaches its end -- the owner's
death closes the pipe -- the binding ends everything it started: SIGTERM to its group, a short
grace, SIGKILL to its group, itself included. SIGTERM keeps its default disposition, so while the
owner lives its stop alone -- SIGTERM to the group, a grace, SIGKILL -- decides when the group ends.
"""

import contextlib
import importlib
import json
import os
import select
import signal
import sys
import threading
import time
import traceback

# ---------------------------------------------------------------------------- descriptors

# The protocol descriptors, set by `main`: moved aside so nothing the library prints can forge a
# line, as the interpreter does with its own.
_PROTO_IN = None
_PROTO_OUT = None
_send_lock = threading.Lock()
# Bytes in one protocol line toward the owner, which drops a longer one unread.
LINE_MAX = 1 << 20


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


def _diag(text):
    """One line on stderr, where the owner logs it. Never a protocol message."""
    if len(text) > 1000:
        text = f"{text[:1000]}..."
    with contextlib.suppress(OSError):
        _write_all(2, f"outrig-binding: {text}\n".encode("utf-8", "backslashreplace"))


def _encode(message):
    return (json.dumps(message) + "\n").encode("utf-8")


def _write(line):
    with _send_lock:
        _write_all(_PROTO_OUT, line)


def _send(message):
    _write(_encode(message))


# ---------------------------------------------------------------------------- the owner

# The signal the owner's death delivers: `PR_SET_PDEATHSIG`, set by the owner between fork and
# exec and blocked until `main` has installed the handler, so a death in between is pending
# rather than lost.
DEATH_SIGNAL = signal.SIGHUP
# How long the programs this process started have to leave on SIGTERM before SIGKILL, once the
# owner is gone and nobody else will send it.
DEATH_GRACE = 1.0
_dying = threading.Lock()  # taken by the one call to `_die` that runs


def _die(reason):
    """End this process and everything it started: SIGTERM to the process group, a grace, SIGKILL
    to the group, itself included. Once: the owner's death arrives as the signal and as the end
    of stdin together, and the signal can interrupt the sleep. Main thread only, where
    `signal.signal` is allowed; the signal handler and `_read` both run there. `killpg` cannot
    fail here: this process is a member of its own group, and may always signal itself."""
    if not _dying.acquire(blocking=False):
        return
    _diag(f"{reason}; stopping the process group")
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    os.killpg(os.getpgrp(), signal.SIGTERM)
    time.sleep(DEATH_GRACE)
    os.killpg(os.getpgrp(), signal.SIGKILL)
    os._exit(1)


# ---------------------------------------------------------------------------- a library's events

# How deep a payload may nest. The owner's JSON reader stops at 128 levels, the envelope's
# included, and drops the line unread.
DEPTH_MAX = 32


def _check(value, depth=0):
    """Refuse what the owner's JSON reader would not take -- a type JSON has no form for, a key
    that is not a string, an integer past 64 bits, a float that is not finite, nesting past
    `DEPTH_MAX` -- since the owner drops such a line unread."""
    if depth > DEPTH_MAX:
        raise ValueError(f"the payload nests more than {DEPTH_MAX} levels deep")
    if value is None or isinstance(value, (bool, str)):
        return
    if isinstance(value, int):
        if not -(1 << 63) <= value < (1 << 64):
            raise ValueError(f"the integer {value} is past what the owner reads")
    elif isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            raise ValueError("a float in the payload must be finite")
    elif isinstance(value, dict):
        for key, item in value.items():
            if not isinstance(key, str):
                raise ValueError(f"a key in the payload must be a string, not {type(key).__name__}")
            _check(item, depth + 1)
    elif isinstance(value, (list, tuple)):
        for item in value:
            _check(item, depth + 1)
    else:
        raise ValueError(f"the payload may not hold a {type(value).__name__}")


def _line(kind, payload, **fields):
    """A `kind` line carrying `payload`'s keys beside `fields`, encoded, within the owner's
    bound, and of what the owner reads. `payload` may name neither `t` nor a field, which it
    would otherwise replace: its `t` the line's kind."""
    reserved = {"t", *fields}.intersection(payload)
    if reserved:
        raise ValueError(f"the {kind} payload may not carry {sorted(reserved)}")
    _check(payload)
    try:
        # Encoded as UTF-8 rather than escaped: a lone surrogate, which `os.fsdecode` makes of a
        # byte that is not UTF-8, would otherwise cross as an escape the owner's reader refuses.
        text = json.dumps({"t": kind, **fields, **payload}, ensure_ascii=False, allow_nan=False)
        line = (text + "\n").encode("utf-8")
    except (UnicodeEncodeError, ValueError) as e:
        raise ValueError(f"the {kind} payload holds text that is not UTF-8: {e}") from None
    if len(line) > LINE_MAX:
        raise ValueError(f"the {kind} line is {len(line)} bytes, past the {LINE_MAX}-byte bound")
    return line


def _event(payload):
    """Publish `payload`, a dict, to the owner as an `event` line."""
    _write(_line("event", payload))


def _read(handle, stream):
    """Hand each line the owner writes to `handle`, decoded, until stdin ends."""
    with stream:
        while True:
            line = stream.readline()
            if not line:
                return
            if not line.strip():
                continue
            try:
                handle(json.loads(line))
            except Exception as e:
                _diag(f"ignored a line: {e}")


def main():
    if len(sys.argv) != 3:
        _diag(f"usage: python3 -I -c <program> <package-dir> <rpyc-dir>; got {sys.argv[1:]!r}")
        os._exit(2)
    package, rpyc_dir = sys.argv[1:]
    global _PROTO_IN, _PROTO_OUT
    # The protocol never shares a descriptor with the library: fd 0 reads nothing, and what the
    # library prints goes to stderr with the diagnostics.
    _PROTO_IN = os.dup(0)
    _PROTO_OUT = os.dup(1)
    devnull = os.open(os.devnull, os.O_RDONLY)
    os.dup2(devnull, 0)
    os.close(devnull)
    os.dup2(2, 1)
    sys.__stdout__.reconfigure(line_buffering=True)
    # The owner set the parent-death signal before exec and left it blocked; with the handler in
    # place, a death since then is delivered now. Before the factories, which may start programs.
    signal.signal(DEATH_SIGNAL, lambda *_: _die("the owner is gone"))
    signal.pthread_sigmask(signal.SIG_UNBLOCK, {DEATH_SIGNAL})
    # After the standard library, so no directory can replace a module this program imports.
    sys.path += [package, rpyc_dir]
    import rpyc
    from outrig import _serve

    server = _serve.Server(_serve.connection_class(rpyc), _write, _diag)
    stream = os.fdopen(_PROTO_IN, "rb")
    paths, bindings = _arguments(stream.readline())
    # In the session's order, which decides between two bindings that hold different versions of
    # one package, as the container's `sys.path` does.
    sys.path.extend(paths)
    types = {}
    for name, factory, args, kwargs, serialize in bindings:
        module, _, attr = factory.partition(":")
        try:
            root = getattr(importlib.import_module(module), attr)(*args, **kwargs)
        except BaseException as e:
            _diag(
                f"binding {name!r}: the factory {factory!r} failed: "
                f"{''.join(traceback.format_exception(e))}"
            )
            error = "".join(traceback.format_exception_only(e)).strip()
            _send({"t": "failed", "binding": name, "error": error[:FAILED_MAX]})
            os._exit(1)
        server.hosted[name] = _serve.Hosted(root, serialize)
        types[name] = f"{type(root).__module__}.{type(root).__qualname__}"
    _send({"t": "ready", "types": types})
    _read(lambda message: _serve.handle(server, message), stream)
    _die("the owner closed the protocol")


# Characters of a raising factory's one-line rendering sent to the owner; the whole traceback is
# on stderr.
FAILED_MAX = 4096


def _arguments(line):
    """The package directories, and each binding as `(name, factory, args, kwargs, serialize)`,
    from the owner's first line; or end the process saying why there are none: no factory runs on
    a line this program did not understand."""
    if not line:
        _die("the owner closed the protocol before sending the bindings")
    try:
        message = json.loads(line)
        bindings = [
            (b["name"], b["factory"], b["args"], b["kwargs"], b["serialize"])
            for b in message["bindings"]
        ]
        paths = message["paths"]
        if (
            message.get("t") != "start"
            or not isinstance(paths, list)
            or not all(isinstance(b[2], list) and isinstance(b[3], dict) for b in bindings)
        ):
            raise ValueError("not a start line")
    except (ValueError, KeyError, TypeError) as e:
        _diag(f"the first line was not the bindings to start ({e}): {line[:200]!r}")
        os._exit(2)
    return paths, bindings


if __name__ == "__main__":
    main()
