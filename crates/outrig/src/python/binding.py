"""The binding program: one hosted library, served over RPyC to the kernels of one session.

Started on the host by the Rust owner with the payload's `python3 -I -c <this program>` and three
arguments -- the vendored RPyC's directory, the binding's package directory, and the factory as
`module:callable` -- it imports the factory, calls it once, and serves the object it returned as the
root of every connection. A fourth argument, `--serialize`, is for a library that is not
thread-safe: the binding then runs one request at a time across all its connections, in the order
they arrived, except that a callback's nested request runs at once on the thread whose turn it is,
and that `ping` and `close` never wait. Its stdin and stdout carry NDJSON: RPyC frames tagged by
agent and connection, split into bounded parts (`rpc` lines), and a `ready` line once the factory
has returned. Everything else it writes goes to stderr, where the owner logs it.

Each connection is served on a thread of its own. The objects handed out to one agent live in one
table its connections share, so a proxy resolves on whichever of them it arrives on and on no other
agent's; each connection counts the references it handed out itself, gives back only those when it
closes, and the table goes once the agent's last connection has ended.

Enforcement lives here, on the host, because the container side cannot be trusted: agent code runs
in the same process as the other end of this transport and can write frames of its own. So every
one of RPyC's 20 request handlers is replaced, and each request is checked before anything acts on
it:

- A request's target must be an object handed out to this agent, on this connection or another of
  its. A by-value target would let `str.format` read attributes through its format string, past
  the attribute rule.
- A member name must be public -- no leading underscore -- or on RPyC's `safe_attrs` list, for
  every handler that names one, `cmp`'s operator included; `cmp` takes comparison operators only.
- Nothing arrives by reference from the container except a callable, which becomes a proxy that can
  be called and nothing else, until the request it was passed to returns.
- Frames and tracebacks never leave: a generator's frame reaches this process's globals through
  public names alone.
- No pickle, no import of anything the container names, no traceback text in an exception, and a
  release of more references than this connection handed out is refused.

The frame channel below is a copy of the interpreter's, since the two programs share no module.
Keep them the same.
"""

import base64
import builtins
import collections
import contextlib
import importlib
import itertools
import json
import os
import select
import struct
import sys
import threading
import traceback
import types

# ---------------------------------------------------------------------------- limits

PART_MAX = 512 * 1024  # bytes of frame data in one `rpc` line
FRAME_MAX = 128 << 20  # bytes of data in one RPyC frame, either direction
BUFFER_MAX = 2 * FRAME_MAX  # bytes one connection holds unread, the frame in progress included
FRAME_OVERHEAD = 64  # bytes charged for each frame held, for the object that holds it

# RPyC's own frame header: a 4-byte length and a 1-byte compressed flag, then the data and a
# newline. Kept on the wire as RPyC's `Channel` writes it, so a compressed frame is refused by a
# direct check rather than inferred.
FRAME_HEADER = struct.Struct("!LB")
FLUSHER = b"\n"

# ---------------------------------------------------------------------------- descriptors

# The protocol descriptors, set by `main`: moved aside so nothing the library prints can forge a
# line, as the interpreter does with its own.
_PROTO_IN = None
_PROTO_OUT = None
_send_lock = threading.Lock()


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


def _send(message):
    line = (json.dumps(message) + "\n").encode("utf-8")
    with _send_lock:
        _write_all(_PROTO_OUT, line)


# ---------------------------------------------------------------------------- frames


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


# ---------------------------------------------------------------------------- the interception


class Refused(Exception):
    """A request the binding refused, saying what and why. It crosses to the container as the
    request's exception, so the agent reads the reason where the call was made.

    Named as a member of the `outrig` package the container will import, so a container that has
    that package resolves it to the class there, and one that does not gets RPyC's generic class
    under the same name.
    """

    __module__ = "outrig"


class ContainerError(Exception):
    """An exception the container raised inside a callback, rebuilt here as a plain `Exception`.

    Only a builtin exception type is rebuilt as itself. Anything else -- a class from a module the
    container names, or a `KeyboardInterrupt` and the other exceptions that are not `Exception`s
    -- arrives as this, carrying the type's name, its `args`, and its public attributes, so the
    library's `except Exception` cleanup runs and nothing is imported to find out more.
    """

    def __init__(self, name, args=(), attrs=()):
        super().__init__(name, *args)
        self.name = name
        self.attrs = dict(attrs)

    def __str__(self):
        args = ", ".join(repr(a) for a in self.args[1:])
        return f"{self.name}({args})"


# Boxing labels of this transport's own, beside RPyC's four. A container-side shim copies a
# `list`, `dict`, `set` or `frozenset` into these, nested ones included, and marks a callable; the
# binding rebuilds the first four as builtins and the last as a callback proxy.
LABEL_LIST = 101
LABEL_DICT = 102
LABEL_SET = 103
LABEL_FROZENSET = 104
LABEL_CALLABLE = 105

# The operators `cmp` may name: what the comparison dunders of a proxy send.
COMPARISONS = frozenset(["__eq__", "__ne__", "__lt__", "__gt__", "__le__", "__ge__", "__cmp__"])


class _Context(threading.local):
    """Per thread: the callbacks the request being served created, or `None` while a reply to
    this side's own request is being unboxed, where a callable is refused; and the references
    handed out while one message is boxed, taken back if that message is never sent."""

    marks = None
    boxed = None


# ---------------------------------------------------------------------------- the agent's objects


class Objects:
    """The objects handed out to one agent, by id_pack, shared by the agent's connections: a proxy
    resolves on whichever of them it arrives on. Each connection's references are counted apart,
    so a release is checked against what that connection handed out, and the object stays until no
    connection holds it.

    An object's last reference may run library code as it dies -- a destructor -- so nothing dies
    under the lock: what a method takes out of the table, it leaves in a local that dies with its
    frame, after the lock.
    """

    def __init__(self):
        self._lock = threading.Lock()
        self._slots = {}  # id_pack -> [obj, references held across the agent's connections]
        self._held = {}  # connection -> {id_pack -> references it handed out}

    def __len__(self):
        with self._lock:
            return len(self._slots)

    def __getitem__(self, id_pack):
        with self._lock:
            return self._slots[id_pack][0]

    def add(self, conn, id_pack, obj):
        """One more reference to `obj`, handed out by `conn`."""
        with self._lock:
            slot = self._slots.setdefault(id_pack, [obj, 0])
            slot[1] += 1
            held = self._held.setdefault(conn, {})
            held[id_pack] = held.get(id_pack, 0) + 1

    def release(self, conn, id_pack, count):
        """`count` fewer references from `conn` to the object `id_pack` names, unless that is more
        than `conn` handed out."""
        with self._lock:
            held = self._held.get(conn, {})
            handed = held.get(id_pack, 0)
            if not handed:
                raise Refused("a release of an object this connection never handed out")
            if count > handed:
                raise Refused(
                    f"a release of {count} references to an object this connection handed out "
                    f"{handed} times"
                )
            if count == handed:
                del held[id_pack]
                if not held:
                    del self._held[conn]  # so a closed connection is not kept as a key
            else:
                held[id_pack] = handed - count
            slot = self._take(id_pack, count)
        del slot  # outside the lock: this may have been the object's last reference

    def forget(self, conn):
        """Every reference `conn` handed out, taken back: it has closed."""
        with self._lock:
            slots = [
                self._take(id_pack, count) for id_pack, count in self._held.pop(conn, {}).items()
            ]
        del slots  # outside the lock, as above

    def _take(self, id_pack, count):
        """Under the lock: `count` fewer references to the object `id_pack` names. Its slot leaves
        the table once none are left, and is returned so that it outlives the lock."""
        slot = self._slots[id_pack]
        slot[1] -= count
        if not slot[1]:
            del self._slots[id_pack]
        return slot


class Turn:
    """One request at a time across every connection of this binding, under `--serialize`: taken
    with `with`, granted to waiters in the order they arrived -- which `threading.RLock` does not
    promise -- and re-entrant for the thread whose turn it is, so a callback's nested request,
    served on that thread, runs at once.

    Each waiter is a record a later task can mark `dropped` with a reason; it then stops waiting
    and raises `EOFError(reason)` instead of taking its turn.
    """

    def __init__(self):
        self._cond = threading.Condition()
        self._owner = None  # the ident of the thread whose turn it is
        self._depth = 0  # how many times that thread has taken it
        self._waiters = collections.deque()  # in arrival order

    def __enter__(self):
        me = threading.get_ident()
        with self._cond:
            if self._owner == me:
                self._depth += 1
                return
            waiter = types.SimpleNamespace(dropped=None)
            self._waiters.append(waiter)
            try:
                self._cond.wait_for(
                    lambda: waiter.dropped is not None
                    or (self._owner is None and self._waiters[0] is waiter)
                )
            finally:
                # However the wait ends, nobody waits behind a waiter that is gone.
                self._waiters.remove(waiter)
            if waiter.dropped is not None:
                self._cond.notify_all()  # the next in line may be first now
                raise EOFError(waiter.dropped)
            self._owner, self._depth = me, 1

    def __exit__(self, *exc):
        with self._cond:
            self._depth -= 1
            if not self._depth:
                self._owner = None
                self._cond.notify_all()


# The turn every request takes once it is checked: a `Turn` under `--serialize`, set by `main`;
# otherwise nothing, and requests on different connections run at the same time.
_turn = contextlib.nullcontext()


def connection_class(rpyc):
    """The connection class that intercepts every request, built over the imported `rpyc`.

    A function rather than a module-level class because RPyC is imported from a directory `main`
    is given, and so that a test can build the class without starting a binding.
    """
    consts = rpyc.core.consts
    brine = rpyc.core.brine
    vinegar = rpyc.core.vinegar
    netref = rpyc.core.netref
    Connection = rpyc.core.protocol.Connection
    get_id_pack = rpyc.lib.get_id_pack
    get_methods = rpyc.lib.get_methods
    safe_attrs = frozenset(rpyc.core.protocol.DEFAULT_CONFIG["safe_attrs"])

    # RPyC's defaults, with every switch that would trust the container turned off, and nothing
    # waiting on a timer: a hosted call takes as long as the library takes. The attribute
    # switches are left alone: only RPyC's own handlers read them, and every one is replaced.
    config = dict(
        allow_pickle=False,
        include_local_traceback=False,
        include_local_version=True,
        import_custom_exceptions=False,
        instantiate_custom_exceptions=False,
        propagate_SystemExit_locally=False,
        propagate_KeyboardInterrupt_locally=False,
        sync_request_timeout=None,
        bind_threads=False,
    )

    # Requests with no target object: everything else names one as its first argument.
    targetless = frozenset(
        [consts.HANDLE_PING, consts.HANDLE_CLOSE, consts.HANDLE_GETROOT, consts.HANDLE_INSPECT]
    )
    # Requests that run no library code, so they wait for no turn under `--serialize`.
    unserialized = frozenset([consts.HANDLE_PING, consts.HANDLE_CLOSE])

    def allowed(name):
        """The attribute rule: a public name, or one on RPyC's safe list."""
        return type(name) is str and (
            (name and not name.startswith("_")) or name in safe_attrs
        )

    def check_attr(obj, name):
        if not allowed(name):
            raise Refused(f"{name!r} is not a public name, and it is not on RPyC's safe list")
        if name in ("format", "format_map") and (
            isinstance(obj, str) or (isinstance(obj, type) and issubclass(obj, str))
        ):
            raise Refused(f"str.{name} reads attributes through its format string; it is refused")
        return name

    class Callback:
        """A callable the container passed as an argument: callable and nothing else, for as long
        as the request it was passed to is running.

        Not a netref. It sends no `inspect`, answers no attribute, has no `repr`, and refuses
        to be pickled or copied, so a library that holds it can do one thing with it: call it,
        which runs the container's callable on the thread waiting for this request. The lock keeps
        a call from being queued after the request has returned: `revoke` takes it before the
        reply is sent, so a call that got through precedes the reply on the wire.
        """

        __slots__ = ("_conn", "_id_pack", "_live", "_lock")

        def __init__(self, conn, id_pack):
            self._conn = conn
            self._id_pack = id_pack
            self._live = True
            self._lock = threading.Lock()

        def __call__(self, *args, **kwargs):
            with self._lock:
                if not self._live:
                    raise Refused(
                        "this callback cannot be called: the request it was passed to has returned"
                    )
                result = self._conn.async_request(
                    consts.HANDLE_CALL, self, args, tuple(kwargs.items())
                )
            return result.value

        def revoke(self):
            with self._lock:
                self._live = False

        def __getattr__(self, name):
            raise AttributeError(
                f"a callback has no attribute {name!r}: it can be called and nothing else"
            )

        def __repr__(self):
            raise TypeError("a callback has no repr: it can be called and nothing else")

        def __reduce__(self):
            raise TypeError("a callback cannot be pickled or copied: it can be called and nothing else")

        def __reduce_ex__(self, protocol):
            return self.__reduce__()

    class Binding(Connection):
        """One connection to one kernel, every request checked; what it hands out goes in the
        agent's table."""

        def __init__(self, channel, *, root_object, objects, on_close):
            # The connection's RPyC root is a service of nothing, whose `on_disconnect` RPyC
            # calls at cleanup; `getroot` answers with the object the factory returned.
            super().__init__(rpyc.core.service.VoidService(), channel, config)
            self._root_object = root_object
            # The agent's `Objects`; RPyC's own per-connection `_local_objects` stays empty.
            self._objects = objects
            self._on_close = on_close
            self._context = _Context()
            # The last exception's whole traceback text, for the event `0003-21` records. The
            # container gets one line of it.
            self.last_traceback_text = None

        # ------------------------------------------------------------------ dispatch

        @classmethod
        def _request_handlers(cls):
            return {
                consts.HANDLE_PING: cls._handle_ping,
                consts.HANDLE_CLOSE: cls._handle_close,
                consts.HANDLE_GETROOT: cls._handle_getroot,
                consts.HANDLE_GETATTR: cls._handle_getattr,
                consts.HANDLE_DELATTR: cls._handle_delattr,
                consts.HANDLE_SETATTR: cls._handle_setattr,
                consts.HANDLE_CALL: cls._handle_call,
                consts.HANDLE_CALLATTR: cls._handle_callattr,
                consts.HANDLE_REPR: cls._handle_repr,
                consts.HANDLE_STR: cls._handle_str,
                consts.HANDLE_CMP: cls._handle_cmp,
                consts.HANDLE_HASH: cls._handle_hash,
                consts.HANDLE_INSTANCECHECK: cls._handle_instancecheck,
                consts.HANDLE_DIR: cls._handle_dir,
                consts.HANDLE_PICKLE: cls._handle_pickle,
                consts.HANDLE_DEL: cls._handle_del,
                consts.HANDLE_INSPECT: cls._handle_inspect,
                consts.HANDLE_BUFFITER: cls._handle_buffiter,
                consts.HANDLE_OLDSLICING: cls._handle_oldslicing,
                consts.HANDLE_CTXEXIT: cls._handle_ctxexit,
            }

        def _dispatch(self, data):
            """Route one frame. Every path releases the receive lock exactly once, and a frame
            that cannot be decoded closes the connection rather than killing its thread."""
            released = False
            try:
                msg = data[0]
                if msg == consts.MSG_REQUEST:
                    self._recvlock.release()
                    released = True
                    seq, args = brine.load(data[1:])
                    self._dispatch_request(seq, args)
                elif msg == consts.MSG_REPLY:
                    seq, args = brine.load(data[1:])
                    try:
                        obj = self._unbox_reply(args)
                    except Refused as e:
                        is_exc, obj = True, e
                    else:
                        is_exc = False
                    self._seq_request_callback(msg, seq, is_exc, obj)
                    self._recvlock.release()
                    released = True
                elif msg == consts.MSG_EXCEPTION:
                    self._recvlock.release()
                    released = True
                    seq, args = brine.load(data[1:])
                    self._seq_request_callback(msg, seq, True, self._unbox_exc(args))
                else:
                    raise ValueError(f"message type {msg!r}")
            except EOFError:
                raise
            except Exception as e:
                if not released:
                    self._recvlock.release()
                reason = f"an undecodable frame: {type(e).__name__}: {e}"
                self.close_with(reason)
                raise EOFError(reason) from None

        def _dispatch_request(self, seq, raw_args):
            saved = self._context.marks
            self._context.marks = marks = []
            handed = []
            try:
                with contextlib.ExitStack() as turn:
                    try:
                        handler, boxed = raw_args
                        method = self._HANDLERS.get(handler) if type(handler) is int else None
                        if method is None:
                            raise Refused(
                                f"request type {handler!r} is not one this binding answers"
                            )
                        target = self._target(handler, boxed)
                        if handler == consts.HANDLE_DEL:
                            # The target stays an id_pack: unboxed, this frame would hold the
                            # object past its release.
                            args = (target, *map(self._unbox, boxed[1][1:]))
                        else:
                            args = self._unbox(boxed)
                        if handler not in unserialized:
                            # Once the request is checked, so a refused one waits for nobody; held
                            # until its exception, if any, is rendered below.
                            turn.enter_context(_turn)
                        res = method(self, *args)
                        reply = (consts.MSG_REPLY, self._box_tracked(res, handed))
                    except BaseException:
                        # The reply never goes out, so nothing it would have referenced is held.
                        self._release(handed)
                        t, v, tb = sys.exc_info()
                        self._last_traceback = tb
                        reply = (consts.MSG_EXCEPTION, self._box_exc(t, v, tb))
            finally:
                # Before the reply goes out, so no call can be queued after it.
                for mark in marks:
                    mark.revoke()
                self._context.marks = saved
            try:
                self._send(reply[0], seq, reply[1])
            except ValueError as e:
                # The reply itself -- a tuple of many values -- is past the frame bound.
                self._release(handed)
                refused = Refused(str(e))
                self._send(consts.MSG_EXCEPTION, seq, self._box_exc(Refused, refused, None))
            except BaseException:
                # The reply could not be written: the channel closed under the request -- under
                # a callback, say -- and this connection's cleanup has already run, so what the
                # reply would have referenced goes back now or never.
                self._release(handed)
                raise

        def _send(self, msg, seq, args):
            """RPyC's, without its shared send queue: a frame past the bound raises in the thread
            that asked, and no thread sends another's frame. The queue exists for a send from a
            finalizer, and this side makes none -- it holds no proxies."""
            data = brine.I1.pack(msg) + brine.dump((seq, args))
            with self._sendlock:
                self._channel.send(data)

        def _async_request(self, handler, args=(), callback=(lambda a, b: None)):
            """RPyC's, with the references a request's arguments hand out taken back when the
            request cannot be sent."""
            boxed = []
            seq = self._get_seq_id()
            self._request_callbacks[seq] = callback
            try:
                self._send(consts.MSG_REQUEST, seq, (handler, self._box_tracked(args, boxed)))
            except BaseException:
                self._request_callbacks.pop(seq, None)
                self._release(boxed)
                raise

        def _box_tracked(self, obj, boxed):
            """`_box(obj)`, recording in `boxed` every reference it hands out."""
            saved = self._context.boxed
            self._context.boxed = boxed
            try:
                return self._box(obj)
            finally:
                self._context.boxed = saved

        def _release(self, boxed):
            """Take back one reference to each object in `boxed`, handed out for a message that
            was never sent."""
            for id_pack in boxed:
                with contextlib.suppress(Refused):
                    self._objects.release(self, id_pack, 1)

        def _target(self, handler, boxed):
            """The id_pack of the request's target, which must be an object this connection handed
            out; `None` for a request without one."""
            if handler in targetless:
                return None
            if type(boxed) is not tuple or boxed[0] != consts.LABEL_TUPLE or not boxed[1]:
                raise Refused("the request names no target")
            label, value = boxed[1][0]
            if label != consts.LABEL_LOCAL_REF:
                raise Refused(
                    "the request's target is not an object this binding handed out; a value or a "
                    "reference into the container cannot be one"
                )
            return value

        # ------------------------------------------------------------------ boxing

        def _unbox(self, package):
            """Rebuild a request's arguments. Only values, this connection's own objects, the
            shim's copies, and callables arrive; anything by reference is refused."""
            label, value = package
            if label == consts.LABEL_VALUE:
                return value
            if label == consts.LABEL_TUPLE:
                return tuple(self._unbox(item) for item in value)
            if label == consts.LABEL_LOCAL_REF:
                return self._lookup(value)
            if label == consts.LABEL_REMOTE_REF:
                raise Refused(
                    f"an object of type {self._claimed(value)} was sent by reference, and only "
                    f"values, this binding's own objects, and callables cross; `list()` a "
                    f"generator, or pass what the object holds"
                )
            if label == LABEL_LIST:
                return [self._unbox(item) for item in value]
            if label == LABEL_DICT:
                return {self._unbox(key): self._unbox(item) for key, item in value}
            if label == LABEL_SET:
                return {self._unbox(item) for item in value}
            if label == LABEL_FROZENSET:
                return frozenset(self._unbox(item) for item in value)
            if label == LABEL_CALLABLE:
                marks = self._context.marks
                if marks is None:
                    raise Refused(
                        f"a callable of type {self._claimed(value)} was returned from a callback; "
                        f"only values come back"
                    )
                proxy = Callback(self, value)
                marks.append(proxy)
                return proxy
            raise Refused(f"unknown label {label!r}")

        def _unbox_reply(self, package):
            """A reply to this side's own request -- a callback's return value -- where a callable
            is refused too."""
            saved = self._context.marks
            self._context.marks = None
            try:
                return self._unbox(package)
            finally:
                self._context.marks = saved

        @staticmethod
        def _claimed(id_pack):
            """The type name an id_pack claims, as text."""
            try:
                return str(id_pack[0])
            except Exception:
                return repr(id_pack)

        def _lookup(self, id_pack, required=True):
            try:
                return self._objects[id_pack]
            except (KeyError, TypeError):
                if required:
                    raise Refused(
                        "the request names an object this binding never handed out to this agent"
                    ) from None
                return None

        def _box(self, obj):
            """Box a result. Frames and tracebacks never cross, and a value past the frame bound
            is refused before anything builds it."""
            if brine.dumpable(obj):
                if type(obj) in (bytes, str) and len(obj) > FRAME_MAX:
                    raise Refused(
                        f"a result of {len(obj)} bytes is past the {FRAME_MAX}-byte frame bound"
                    )
                return consts.LABEL_VALUE, obj
            if type(obj) is tuple:
                return consts.LABEL_TUPLE, tuple(self._box(item) for item in obj)
            if type(obj) is Callback:
                if not obj._live:
                    # The container released it when the call it was passed to returned, so a
                    # reference would name nothing there.
                    raise Refused(
                        "a callback cannot be read back once the call it was passed to has "
                        "returned"
                    )
                if obj._conn is not self:
                    # Its id_pack names a callable on the connection it came in on, not here.
                    raise Refused(
                        "a callback cannot be read back on a connection other than the one it "
                        "came in on"
                    )
                return consts.LABEL_LOCAL_REF, obj._id_pack
            if isinstance(obj, (types.FrameType, types.TracebackType)):
                raise Refused(
                    f"a {type(obj).__name__} object does not cross: it reaches this process's "
                    f"globals"
                )
            id_pack = get_id_pack(obj)
            self._objects.add(self, id_pack, obj)
            if self._context.boxed is not None:
                self._context.boxed.append(id_pack)
            return consts.LABEL_REMOTE_REF, id_pack

        def _box_exc(self, typ, val, tb):
            """An exception for the container: its type, args and public attributes, with one line
            of text -- how this side renders it -- where the traceback would go. The whole
            traceback stays here."""
            self.last_traceback_text = "".join(traceback.format_exception(typ, val, tb))
            try:
                dumped = vinegar.dump(
                    typ, val, tb, include_local_traceback=False, include_local_version=True
                )
                line = "".join(traceback.format_exception_only(typ, val))
            except Exception as e:
                # The exception's own attributes would not read; the container is told that much.
                return (
                    ("builtins", "RuntimeError"),
                    (f"an exception of type {typ.__name__} could not be sent: {e!r}",),
                    (),
                    "",
                )
            if type(dumped) is tuple:
                names, args, attrs, _ = dumped
                # `vinegar` sends every public attribute, a method's repr included; set on the
                # rebuilt exception, that text would shadow `add_note`.
                attrs = tuple(
                    (name, value) for name, value in attrs if not callable(getattr(typ, name, None))
                )
                dumped = (names, args, attrs, line)
            return dumped

        def _unbox_exc(self, raw):
            """An exception the container raised inside a callback. Never raises, and never
            imports."""
            try:
                exc = self._container_exception(raw)
                if exc is None:
                    exc = ContainerError("None")
                return exc
            except Exception as e:
                return ContainerError(f"<an exception that could not be read: {e!r}>")

        def _container_exception(self, raw):
            """Rebuild a dumped exception, as `vinegar.load` would with every switch off and no
            cache of classes named by the container; `None` stays `None`."""
            if raw is None:
                return None
            if raw == consts.EXC_STOP_ITERATION:
                return StopIteration()
            (modname, clsname), args, attrs, text = raw
            cls = getattr(builtins, clsname, None) if modname == "builtins" else None
            if not (isinstance(cls, type) and issubclass(cls, Exception)):
                cls = None
            # Data only: a method's repr would shadow the method, `add_note` among them.
            public = [
                (name, value)
                for name, value in attrs
                if type(name) is str
                and name
                and not name.startswith("_")
                and brine.dumpable(value)
                and not callable(getattr(cls or BaseException, name, None))
            ]
            try:
                exc = cls.__new__(cls)
                exc.args = tuple(args)
            except Exception:
                # No builtin, or one whose `__new__` needs arguments -- `ExceptionGroup`,
                # `UnicodeDecodeError`: the library still gets an exception to handle.
                return ContainerError(f"{modname}.{clsname}", tuple(args), public)
            for name, value in public:
                with contextlib.suppress(Exception):
                    setattr(exc, name, value)
            if type(text) is str:
                exc._remote_tb = text
            return exc

        # ------------------------------------------------------------------ closing

        def close(self):
            """Mark closed and clean up, writing nothing: RPyC's `close` makes a synchronous
            request of the other side, which may be gone, and `__del__` reaches here."""
            if self._closed:
                return
            self._closed = True
            self._cleanup(_anyway=True)

        def close_with(self, reason):
            """Close for `reason`, telling the other side first."""
            with contextlib.suppress(Exception):
                self._on_close(reason)
            self._channel.close(reason)
            self.close()

        def _cleanup(self, _anyway=True):
            """RPyC's, and then this connection's references go back to the agent's table -- under
            the turn when the binding serializes, since the last of them may run a destructor."""
            with _turn:
                super()._cleanup(_anyway)
                self._objects.forget(self)

        # ------------------------------------------------------------------ handlers

        def _handle_ping(self, data):
            return data

        def _handle_close(self):
            self._cleanup()

        def _handle_getroot(self):
            return self._root_object

        def _handle_getattr(self, obj, name):
            return getattr(obj, check_attr(obj, name))

        def _handle_delattr(self, obj, name):
            delattr(obj, check_attr(obj, name))

        def _handle_setattr(self, obj, name, value):
            setattr(obj, check_attr(obj, name), value)

        def _handle_call(self, obj, args, kwargs=()):
            return obj(*args, **dict(kwargs))

        def _handle_callattr(self, obj, name, args, kwargs=()):
            return self._handle_call(self._handle_getattr(obj, name), args, kwargs)

        def _handle_repr(self, obj):
            return repr(obj)

        def _handle_str(self, obj):
            return str(obj)

        def _handle_cmp(self, obj, other, op="__cmp__"):
            if op not in COMPARISONS:
                raise Refused(f"{op!r} is not a comparison operator")
            return getattr(type(obj), op)(obj, other)

        def _handle_hash(self, obj):
            return hash(obj)

        def _handle_dir(self, obj):
            return tuple(name for name in dir(obj) if allowed(name))

        def _handle_inspect(self, id_pack):
            """The methods a proxy's class is built from: the public ones, those on the safe list,
            and `__call__` for an object this side can call -- without it no hosted method could
            be called, and a call is checked by `call` in any case."""
            obj = self._lookup(id_pack)
            methods = [(name, doc) for name, doc in get_methods(netref.LOCAL_ATTRS, obj) if allowed(name)]
            if callable(obj):
                methods.append(("__call__", getattr(type(obj).__call__, "__doc__", None)))
            return tuple(methods)

        def _handle_instancecheck(self, obj, other_id_pack):
            other = self._lookup(other_id_pack, required=False)
            return other is not None and isinstance(other, obj)

        def _handle_pickle(self, obj, proto):
            raise Refused("pickling is refused: no request is answered with pickled bytes")

        def _handle_del(self, id_pack, count=1):
            """Release `count` references to the object `id_pack` names -- taken from the request
            rather than from the object, whose attributes would run library code -- unless that is
            more than this connection handed out. The object was not unboxed for this request, so
            once no connection holds it, it dies here, before the reply."""
            if type(count) is not int or count < 1:
                raise Refused(f"a release of {count!r} references")
            self._objects.release(self, id_pack, count)

        def _handle_buffiter(self, obj, count):
            if type(count) is not int or count < 0:
                raise Refused(f"a batch of {count!r} items")
            return tuple(itertools.islice(obj, count))

        def _handle_oldslicing(self, obj, attempt, fallback, start, stop, args):
            try:
                getitem = self._handle_getattr(obj, attempt)
                return getitem(slice(start, stop), *args)
            except Exception:
                if stop is None:
                    stop = sys.maxsize
                getslice = self._handle_getattr(obj, fallback)
                return getslice(start, stop, *args)

        def _handle_ctxexit(self, obj, exc):
            """`__exit__` with the exception the container's `with` block raised, rebuilt by value
            from what the shim sent: a builtin as itself, anything else as `ContainerError`."""
            try:
                exc = self._container_exception(exc)
            except Exception as e:
                raise Refused(f"the exception for __exit__ could not be read: {e!r}") from None
            typ = None if exc is None else type(exc)
            return self._handle_getattr(obj, "__exit__")(typ, exc, None)

    return Binding


# ---------------------------------------------------------------------------- connections


class Connections:
    """The connections of this binding, by agent and connection id, shared by the reading thread
    and the serving threads; and each agent's object table, which its connections share for as
    long as one of them is serving."""

    def __init__(self, Binding, root_object):
        self._Binding = Binding
        self._root_object = root_object
        self._lock = threading.Lock()
        self._open = {}  # (agent, id) -> channel
        self._closed = set()  # (agent, id) of connections that have closed
        self._objects = {}  # agent -> its Objects
        self._serving = collections.Counter()  # agent -> its connections still serving

    def channel(self, agent, cid):
        """The channel for `(agent, cid)`, started if new; `None` once it has closed."""
        key = (agent, cid)
        with self._lock:
            if key in self._closed:
                return None
            channel = self._open.get(key)
            if channel is not None:
                return channel
            channel = FrameChannel(
                lambda part, more: _send(
                    {
                        "t": "rpc",
                        "agent": agent,
                        "id": cid,
                        "data": base64.b64encode(part).decode("ascii"),
                        "more": more,
                    }
                )
            )
            self._serving[agent] += 1
            if self._serving[agent] == 1:
                self._objects[agent] = Objects()
            conn = self._Binding(
                channel,
                root_object=self._root_object,
                objects=self._objects[agent],
                on_close=lambda reason: self.notice(agent, cid, reason),
            )
            self._open[key] = channel
        threading.Thread(
            target=self._serve, args=(key, conn), name=f"serve-{agent}-{cid}", daemon=True
        ).start()
        return channel

    def _serve(self, key, conn):
        try:
            conn.serve_all()
        except Exception as e:
            _diag(f"connection {key[0]}#{key[1]}: serving it failed: {e!r}")
        finally:
            self.forget(*key)
            self._ended(key[0])

    def _ended(self, agent):
        """One of `agent`'s connections has finished serving, its cleanup run; after the last of
        them, the agent's table goes."""
        with self._lock:
            self._serving[agent] -= 1
            if self._serving[agent]:
                return
            del self._serving[agent]
            objects = self._objects.pop(agent)
        _diag(
            f"agent {agent!r}: its last connection has ended, and its object table is dropped "
            f"with {len(objects)} objects"
        )

    def forget(self, agent, cid):
        with self._lock:
            self._open.pop((agent, cid), None)
            self._closed.add((agent, cid))

    def notice(self, agent, cid, reason):
        """Tell the other side that `(agent, cid)` has closed, and why."""
        self.forget(agent, cid)
        _send({"t": "rpc", "agent": agent, "id": cid, "closed": reason})

    def close(self, agent, cid, reason):
        """The other side closed `(agent, cid)`: wake its reader, which ends its thread."""
        with self._lock:
            channel = self._open.get((agent, cid))
        if channel is not None:
            channel.close(reason)


def _handle(connections, line):
    """Route one protocol line to its connection, or say on stderr why it could not be."""
    message = json.loads(line)
    if not isinstance(message, dict) or message.get("t") != "rpc":
        raise ValueError(f"not an rpc message: {line[:200]!r}")
    agent, cid = message.get("agent"), message.get("id")
    if not isinstance(agent, str) or type(cid) is not int:
        raise ValueError("an rpc message without an agent and an integer id")
    if "closed" in message:
        connections.close(agent, cid, f"the other side closed this connection: {message['closed']}")
        return
    part = base64.b64decode(message.get("data", ""), validate=True)
    more = message.get("more") is True
    channel = connections.channel(agent, cid)
    if channel is None:
        connections.notice(agent, cid, "this connection has closed")
        return
    try:
        reason = channel.put(part, more)
    except MemoryError:
        # Rather than leave the kernel waiting on a frame that will never finish.
        reason = channel.close("no memory to hold a frame from the interpreter")
    if reason is not None:
        connections.notice(agent, cid, reason)


def _read(connections):
    with os.fdopen(_PROTO_IN, "rb") as stream:
        while True:
            line = stream.readline()
            if not line:
                return
            if not line.strip():
                continue
            try:
                _handle(connections, line)
            except Exception as e:
                _diag(f"ignored a line: {e}")


def main():
    serialize = sys.argv[4:] == ["--serialize"]
    if len(sys.argv) != 4 and not serialize:
        _diag(
            "usage: python3 -I -c <program> <rpyc-dir> <package-dir> <module:callable> "
            f"[--serialize]; got {sys.argv[1:]!r}"
        )
        os._exit(2)
    rpyc_dir, packages, factory = sys.argv[1:4]
    global _PROTO_IN, _PROTO_OUT, _turn
    if serialize:
        _turn = Turn()
    # The protocol never shares a descriptor with the library: fd 0 reads nothing, and what the
    # library prints goes to stderr with the diagnostics.
    _PROTO_IN = os.dup(0)
    _PROTO_OUT = os.dup(1)
    devnull = os.open(os.devnull, os.O_RDONLY)
    os.dup2(devnull, 0)
    os.close(devnull)
    os.dup2(2, 1)
    sys.__stdout__.reconfigure(line_buffering=True)
    # After the standard library, so neither directory can replace a module this program imports.
    sys.path.append(rpyc_dir)
    sys.path.append(packages)
    import rpyc

    Binding = connection_class(rpyc)
    module, _, name = factory.partition(":")
    try:
        root_object = getattr(importlib.import_module(module), name)()
    except BaseException as e:
        _diag(f"the factory {factory!r} failed: {''.join(traceback.format_exception(e))}")
        os._exit(1)
    connections = Connections(Binding, root_object)
    _send({"t": "ready"})
    _read(connections)
    os._exit(0)


if __name__ == "__main__":
    main()
