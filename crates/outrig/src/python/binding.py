"""The binding program: a session's hosted libraries, served over RPyC to the kernels of one
session.

One process holds every binding of a session. Started on the host by the Rust owner with the
payload's `python3 -I -c <this program>` and one argument, the vendored RPyC's directory, it reads
the session's bindings from its first line of input: each one's name, its factory as
`module:callable` with the arguments to call it with, and whether it serializes; and the package
directories, in the order they go on `sys.path`. It imports each factory and calls it once, in
order, and serves the object it returned as the root of every connection to that binding. The
arguments arrive on stdin rather than the command line, which any user on the host can read.

The bindings share what a process has: its environment, its `sys.path` -- where two bindings'
packages hold different versions of one library, the first binding's is the one imported, as in the
container -- and its life. A binding declared to serialize is a library that is not thread-safe: it
then runs one request at a time across all its connections, in the order they arrived, except that
a callback's nested request runs at once on the thread whose turn it is, and that `ping` and
`close` never wait. Each serializing binding has a turn of its own, so it never waits for another.

Its stdin and stdout carry NDJSON, one kind of line at a time: RPyC frames tagged by binding, agent
and connection, split into bounded parts (`rpc` lines), both ways; from the owner first, a `start`
line holding the bindings and the package directories; a `ready` line once every factory has
returned, naming the type of what each returned, or a `failed` line naming the binding whose factory
raised and what it raised; `event` lines -- a library's own, and each request's record: its
receipt, the policy's decision, its dispatch, which holds the request until the owner answers
whether the event is `published`, refused if it is not, and its end; `decision` lines, each a
request the policy holds on its serving thread for the owner's answer, which arrives under the same
id; `cancel` notices of requests whose callers were interrupted; and the `control` line that says
the session has closed. Everything else it writes goes to stderr, where the owner logs it.

The process is a supervised child of the owner, in a process group of its own, started with a
parent-death signal (`SIGHUP`) the owner set before exec and left blocked until `main` has
installed the handler for it. When that signal arrives, or stdin reaches its end -- the owner's
death closes the pipe -- the binding ends everything it started: SIGTERM to its group, a short
grace, SIGKILL to its group, itself included. SIGTERM keeps its default disposition, so while the
owner lives its stop alone -- SIGTERM to the group, a grace, SIGKILL -- decides when the group ends.

Each connection is served on a thread of its own. The objects one binding handed out to one agent
live in one table that agent's connections to the binding share, so a proxy resolves on whichever
of them it arrives on and on no other agent's or binding's; each connection counts the references
it handed out itself, gives back only those when it closes, and the table goes once the agent's last
connection to the binding has ended.

Enforcement lives here, on the host, because the container side cannot be trusted: agent code runs
in the same process as the other end of this transport and can write frames of its own. So every
one of RPyC's 20 request handlers is replaced, and each request is checked before anything acts on
it:

- A request's number, which the owner records it by, must be an integer below 2**64, not that of
  a request its connection is serving, and the number of the request the owner labeled its frame
  as, if it did; otherwise the connection closes, and that labeled request ends refused. A labeled
  frame dropped unread with its connection ends its request cancelled, so nothing the owner holds
  waits on a frame that will never be served. A boundary request the owner did not label is
  refused: it runs only under the owner's record of it.
- A request's target must be an object this binding handed out to this agent, on this connection
  or another of its. A by-value target would let `str.format` read attributes through its format
  string, past the attribute rule.
- A member name must be public -- no leading underscore -- or on RPyC's `safe_attrs` list, for
  every handler that names one, `cmp`'s operator included; `cmp` takes comparison operators only.
- Nothing arrives by reference from the container except a callable, which becomes a proxy that can
  be called and nothing else, until the request it was passed to returns.
- Frames and tracebacks never leave: a generator's frame reaches this process's globals through
  public names alone.
- No pickle, no import of anything the container names, no traceback text in an exception, and a
  release of more references than this connection handed out is refused.
- `oldslicing` and `buffiter`, which no proxy of the interpreter's sends, are refused: each runs
  more than its request names.

Then the session's policy decides each boundary request, from the rule table on the `start` line
(`Policy`): it runs, it is refused with `Denied`, or it waits on its serving thread -- holding no
turn -- for the owner's answer to a `decision` line. A rule matches the request's binding, the
host-side type of its target as this program names it (`type_name`, never what the object says of
itself), its member, and its operation; a call of a callable a `getattr` returned is matched as a
call of that member on that type (`Objects.originate`), and a request passing a host callable whose
own call the policy would not allow is refused, since the host could call it later with no request.

The frame channel below is a copy of the interpreter's, since the two programs share no module.
Keep them the same.
"""

import base64
import builtins
import collections
import contextlib
import importlib
import inspect
import itertools
import json
import os
import queue
import reprlib
import select
import signal
import struct
import sys
import threading
import time
import traceback
import types

# ---------------------------------------------------------------------------- limits

PART_MAX = 512 * 1024  # bytes of frame data in one `rpc` line
FRAME_MAX = 128 << 20  # bytes of data in one RPyC frame, either direction
BUFFER_MAX = 2 * FRAME_MAX  # bytes one connection holds unread, the frame in progress included
FRAME_OVERHEAD = 64  # bytes charged for each frame held, for the object that holds it
TEXT_MAX = 16 << 10  # characters of a refusal's text, and the last of a traceback's, in an event
CANCELS_MAX = 16  # requests one connection keeps cancelled before they are registered
CLOSED_MAX = 1024  # closed connections remembered, so a part still arriving for one is refused

# A request's receipt previews its arguments from what crossed by value: rendered as `repr`
# renders builtins, cut to 200 characters, 16 items of a container and 3 levels deep. Bytes are
# cut before they are rendered, as strings are.
PREVIEW_CHARS = 200
PREVIEW_ITEMS = 16
_PREVIEW = reprlib.Repr(
    maxlevel=3,
    maxtuple=PREVIEW_ITEMS,
    maxlist=PREVIEW_ITEMS,
    maxdict=PREVIEW_ITEMS,
    maxset=PREVIEW_ITEMS,
    maxfrozenset=PREVIEW_ITEMS,
    maxstring=PREVIEW_CHARS,
    maxlong=PREVIEW_CHARS,
    maxother=PREVIEW_CHARS,
)
_PREVIEW.repr_bytes = _PREVIEW.repr_str

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


# ---------------------------------------------------------------------------- events and decisions

_decision_ids = itertools.count(1)
# id -> the queue its serving thread waits on for the owner's answer, and the request a policy
# decision holds, if one does, which the reading thread marks `released` as it hands over the
# owner's allow: so the mark falls in the pipe's order against the control line. Serving threads
# add entries and the reading thread pops them with no lock: each is one dict operation, atomic
# under the GIL.
_decisions = {}


# How deep a payload may nest. The owner's JSON reader stops at 128 levels, the envelope's
# included, and drops the line unread.
DEPTH_MAX = 32


def _check(value, depth=0):
    """Refuse what the owner's JSON reader would not take -- a type JSON has no form for, a key
    that is not a string, an integer past 64 bits, a float that is not finite, nesting past
    `DEPTH_MAX` -- since a line the owner drops unread would leave a decision waiting for ever."""
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
    bound, and of what the owner reads. `payload` may name neither `t` nor a field: a payload's
    `id` would otherwise take a decision's place, and its `t` the line's kind."""
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


def _record(key, kind, seq, **fields):
    """Publish an event of request `seq` on connection `key`, `(binding, agent, id)`. Written as
    UTF-8 with a lone surrogate -- which a library's text can hold -- replaced: the owner's reader
    would drop the line."""
    binding, agent, cid = key
    line = dict(t="event", kind=kind, binding=binding, agent=agent, conn=cid, seq=seq)
    text = json.dumps({**line, **fields}, ensure_ascii=False)
    _write(f"{text}\n".encode("utf-8", "replace"))


def _dropped(key, label, reason):
    """End request `label` on connection `key`, whose frame was dropped unread, the connection
    closed for `reason`. Nothing for a frame the owner did not label."""
    if label is not None:
        with contextlib.suppress(Exception):
            ended = dict(outcome="cancelled", reason="disconnected", detail=reason[:TEXT_MAX])
            _record(key, "ended", label, **ended)


def _decide(payload, request=None):
    """Ask the owner to decide `payload`, a dict, holding the calling thread -- and `request`,
    the request it is serving -- until the answer arrives; returns the answer. A payload the
    owner would not take -- one past the line bound, or not JSON -- raises here, before anything
    waits."""
    did = next(_decision_ids)
    line = _line("decision", payload, id=did)
    waiter = queue.SimpleQueue()
    _decisions[did] = (waiter, request)
    try:
        _write(line)
    except BaseException:
        _decisions.pop(did, None)
        raise
    return waiter.get()


def _go_on(answer):
    """Return if the owner's `answer` to a held request allows it; else raise what ends it -- the
    approver's deny, the cancel, or the close's refusal."""
    answer = answer if isinstance(answer, dict) else {}
    action, reason = answer.get("action"), answer.get("reason")
    detail = str(answer.get("detail") or "")
    if action == "allow":
        return
    if action == "refuse" or (action == "cancel" and reason == "closed"):
        raise Closed(detail)
    if action == "cancel":
        raise EOFError(detail or "the call was cancelled before it ran")
    code = reason if reason in ("approver", "no_approver") else "approver"
    raise Denied(detail or f"the owner's answer could not be read: {answer!r}"[:TEXT_MAX], code)


# The session's closing error once the owner's control line has said it closed -- the closed flag,
# set by the reading thread -- else `None`.
_closing = None
# Every request being served, by `(binding, agent, connection, seq)`, from its receipt until it
# ends: what a cancel and the close find it by. Serving threads add and pop entries and the
# reading thread reads them, each one dict operation, atomic under the GIL.
_requests = {}
# Held while a cancel looks for its requests and while one is registered, with what each
# connection's `Channel` keeps for cancels, so neither misses the other.
_cancel_lock = threading.Lock()


def _control(message):
    """The owner's control line: the session has closed to new work, `closed` its closing error.
    It comes behind every frame the owner forwarded before the close, so a boundary request read
    from now on is refused; one waiting for its binding's turn is cancelled now. Bookkeeping, and
    what is running, go on."""
    global _closing
    text = message.get("closed")
    if not isinstance(text, str):
        _diag(f"a control line nothing handles: {json.dumps(message)}")
        return
    _closing = text
    for request in list(_requests.values()):
        # One the owner's gate allowed before the close is the drain's, as a forwarded one is.
        if request.boundary and not request.released:
            request.turn.drop(request, Closed(text))


# ---------------------------------------------------------------------------- frames


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


class Channel(FrameChannel):
    """The frame channel of a connection here, which tells the thread receiving a frame whether
    the frame arrived after the close: `late`, set by `recv`. The reading thread puts frames in
    the order the owner wrote them, and the owner writes the close's control line behind every
    frame it forwarded before the close, so a frame put once the flag is set is late.

    It keeps, too, what a cancel named ahead of its request. The owner sends a cancel behind the
    frames of the requests it names, so a request the cancel finds unregistered is in a frame not
    yet claimed -- registered as a request, or found to hold none -- or has ended, or never was:
    `cancelled` keeps it while a frame is unclaimed, for its registration to take.

    And it keeps each frame's label: the number of the request the owner holds it as, which the
    owner sends on the frame's first part, or `None`. `recv` sets `label` beside `late`; a labeled
    frame the channel drops unread -- held, or in progress, when it closes, or arriving after --
    ends its request."""

    early = 0  # frames held that arrived before the close
    late = False  # whether the frame `recv` last returned arrived after it
    label = None  # the label of the frame `recv` last returned
    unclaimed = 0  # frames put and not yet claimed, under `_cancel_lock`

    def __init__(self, write, key):
        super().__init__(write)
        self.key = key  # (binding, agent, connection id)
        self.cancelled = set()  # seqs a cancel named while a frame was unclaimed
        self.labels = collections.deque()  # those of the frames held, and of the one in progress

    def put(self, part, more, label=None):
        with self._cond:
            if self._reason is not None:
                _dropped(self.key, label, self._reason)
            elif not self._parts:
                self.labels.append(label)
            held = len(self._frames)
            reason = super().put(part, more)
            if _closing is None:
                self.early += len(self._frames) - held
            with _cancel_lock:
                self.unclaimed += len(self._frames) - held
            return reason

    def recv(self):
        with self._cond:
            data = super().recv()
            self.label = self.labels.popleft()
            self.late = not self.early
            self.early = max(self.early - 1, 0)
            return data

    def close(self, reason="closed"):
        with self._cond:
            reason = super().close(reason)
            while self.labels:
                _dropped(self.key, self.labels.popleft(), reason)
            return reason

    def claim(self, seq=None):
        """Claim a frame `recv` returned, registered as request `seq` or holding none; whether a
        cancel named `seq` first. Once none is unclaimed, what was kept names nothing to come."""
        with _cancel_lock:
            self.unclaimed -= 1
            early = seq in self.cancelled
            self.cancelled.discard(seq)
            if not self.unclaimed:
                self.cancelled.clear()
            return early


# ---------------------------------------------------------------------------- the interception


class Refused(Exception):
    """A request the binding refused, saying what and why. It crosses to the container as the
    request's exception, so the agent reads the reason where the call was made.

    Named as a member of the `outrig` package the container will import, so a container that has
    that package resolves it to the class there, and one that does not gets RPyC's generic class
    under the same name.
    """

    __module__ = "outrig"


class Closed(Exception):
    """A request the session's close kept from running: read after the closed flag, or still
    waiting for its binding's turn when the flag was set. It carries the session's closing error,
    and crosses as `outrig.Closed`, the class the interpreter's own refusals after the close
    raise."""

    __module__ = "outrig"


class Denied(Exception):
    """A request the session's policy refused before its target was invoked: by a rule, a default,
    or the approver. `reason` says which, and crosses to the container, where the interpreter
    raises it as `runtime.Denied`; `_code` is the reason its record gives, and stays here."""

    __module__ = "outrig"

    def __init__(self, reason, code):
        super().__init__(reason)
        self.reason = reason
        self._code = code


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
# The operation a `callattr` of each of these dunders stands for, which a rule naming that
# operation matches too.
DUNDER_OPERATIONS = {
    "__repr__": "repr",
    "__str__": "str",
    "__hash__": "hash",
    "__exit__": "ctxexit",
    **{name: "cmp" for name in COMPARISONS},
}


class _Context(threading.local):
    """Per thread: the request being served, which the callables it was passed belong to, or
    `None` while a reply to this side's own request is being unboxed, where a callable is
    refused; the references handed out while one message is boxed, taken back if that message is
    never sent; the seq of the request whose callback this thread is calling, the parent of a
    request the container makes inside it; and the host objects a request's arguments name."""

    request = None
    boxed = None
    parent = None
    # The host objects a request's arguments named, as `(id_pack, object)` in the order they were
    # unboxed, while the request is unboxed.
    passed = None


# ---------------------------------------------------------------------------- the policy

# Read from `type` itself, so a metaclass's descriptor of the same name never runs.
_QUALNAME = type.__dict__["__qualname__"].__get__
_MODULE = type.__dict__["__module__"].__get__


def type_name(obj):
    """The qualified name of `obj`'s type on the host -- or of `obj` itself when it is a class, as
    a member read from a class is the member of its instances -- read from the class's own slots.
    Never from the object: RPyC's id_pack asks the object for `____id_pack__` and `__class__`,
    which an object whose attributes come from its items lets the container set."""
    cls = obj if issubclass(type(obj), type) else type(obj)
    try:
        module = _MODULE(cls)
    except AttributeError:
        module = None
    if type(module) is not str:
        module = "?"
    return f"{module}.{_QUALNAME(cls)}"


def method_identity(obj):
    """A bound method's own `(type, name)` -- its `__self__`'s type and its function's name -- read
    from the method's slots, so no library code runs; `None` for anything else. It holds however
    the method was reached: by a read, from a container, or from a call."""
    if type(obj) is types.MethodType:
        func = obj.__func__
        if type(func) is types.FunctionType:
            return type_name(obj.__self__), func.__name__
    elif type(obj) is types.BuiltinMethodType:
        owner = obj.__self__
        if owner is not None and not issubclass(type(owner), types.ModuleType):
            return type_name(owner), obj.__name__
    return None


def static_identity(obj, name):
    """The method a read of `name` on `obj` would give, as its own `(type, name)`, found without
    running the read: `inspect.getattr_static` looks in the instance's and the classes'
    dictionaries and calls no descriptor. So a member that is another name for a method -- `alias =
    push` in a class body -- is known as the method before anything runs. `None` when what it finds
    is not a method this can name, or nothing is found, as for a name `__getattr__` makes."""
    try:
        found = inspect.getattr_static(obj, name)
    except Exception:
        return None
    if type(found) in (staticmethod, classmethod):
        found = found.__func__
    if type(found) is types.FunctionType or type(found) in (
        types.MethodDescriptorType,
        types.WrapperDescriptorType,
        types.ClassMethodDescriptorType,
    ):
        return type_name(obj), found.__name__
    return method_identity(found)


def calls_of(known):
    """What a call of a callable known as each `(type, member)` of `known` is matched as: a call of
    that member on that type, and, for a dunder with an operation of its own, that operation too
    -- so a `__repr__` read and then called is still a `repr`."""
    described = []
    for type_, member in known:
        described.append(("call", member, type_))
        if member in DUNDER_OPERATIONS:
            described.append((DUNDER_OPERATIONS[member], member, type_))
    return described


# How strict each action is: when layers, or a request's descriptions, disagree, the strictest
# decides. An evaluator may allow without a person, and a person may allow where a deny never does.
STRICTNESS = {"allow": 0, "evaluate": 1, "escalate": 2, "deny": 3}


def _pattern(pattern):
    """A rule's field as a test of a name: anything when it is absent, else the name exactly,
    with `*` standing for any run of characters. A request that names no member matches only a
    rule that names none.

    Matched without backtracking, since a name is the container's to make long: the text before
    the first `*` must begin the name and the text after the last end it, and each piece between
    is found, leftmost, after the one before -- which, with `*` the only wildcard, is a match
    whenever one exists."""
    if pattern is None:
        return lambda value: True
    if "*" not in pattern:
        return lambda value: value == pattern
    first, *middle, last = pattern.split("*")

    def matches(value):
        if value is None or len(value) < len(first) + len(last):
            return False
        if not (value.startswith(first) and value.endswith(last)):
            return False
        at, end = len(first), len(value) - len(last)
        for piece in middle:
            found = value.find(piece, at, end)
            if found < 0:
                return False
            at = found + len(piece)
        return True

    return matches


class Verdict:
    """What the policy decided of a request, and what decided it: `rule`, counted from 1, of
    `layer`'s table, or its default when `rule` is `None`."""

    def __init__(self, action, layer, rule):
        self.action, self.layer, self.rule = action, layer, rule

    @property
    def code(self):
        """The reason a refusal by it is recorded with."""
        return "default" if self.rule is None else "rule"

    def __str__(self):
        if self.rule is None:
            return f"the {self.layer} policy's default"
        return f"{self.layer} rule {self.rule}"

    def fields(self):
        """The fields its `decided` record and its decision line carry."""
        return dict(action=self.action, by=self.code, layer=self.layer, rule=self.rule)


class Policy:
    """The rules in force, as the owner sent them on the `start` line: the operator's table, its
    `default` resolved, and each repository table that may only restrict it. Each layer decides on
    its own -- its first matching rule, else its `default`; a repository table with neither takes
    no part -- and the strictest action wins."""

    def __init__(self, wire):
        self.configured = wire["configured"] is True
        tables = [("operator", wire["operator"]), *(("repo", table) for table in wire["repo"])]
        self.layers = [(layer, *self._table(table)) for layer, table in tables]

    @staticmethod
    def _table(table):
        rules = [
            (
                _pattern(rule.get("binding")),
                _pattern(rule.get("type")),
                _pattern(rule.get("member")),
                rule.get("operation"),
                rule["action"],
            )
            for rule in table["rules"]
        ]
        return rules, table.get("default")

    @staticmethod
    def _layer(layer, rules, default, binding, operation, member, type_):
        """`layer`'s verdict on one description of a request: its first matching rule, else its
        default; `None` when it has neither."""
        for i, (matches_binding, matches_type, matches_member, op, action) in enumerate(rules, 1):
            if (
                op in (None, operation)
                and matches_binding(binding)
                and matches_type(type_)
                and matches_member(member)
            ):
                return Verdict(action, layer, i)
        return None if default is None else Verdict(default, layer, None)

    def decide(self, binding, descriptions):
        """The verdict on a request of `binding` described as each of `descriptions`,
        `(operation, member, type)`: the strictest any layer gives any of them, the first of
        equals."""
        best = None
        for operation, member, type_ in descriptions:
            for layer in self.layers:
                found = self._layer(*layer, binding, operation, member, type_)
                if found is not None and (
                    best is None or STRICTNESS[found.action] > STRICTNESS[best.action]
                ):
                    best = found
                    if best.action == "deny":
                        return best
        return best or Verdict("allow", "operator", None)


# Until the `start` line says otherwise: nothing configured, every request allowed.
_policy = Policy({"configured": False, "operator": {"default": "allow", "rules": []}, "repo": []})


def id_pack_of(obj):
    """The id_pack `obj` is handed out under, laid out as RPyC's `get_id_pack` lays one out -- a
    class as `(name, its id, 0)`, anything else as `(its type's name, the type's id, its id)` -- but
    named by `type_name`. RPyC asks the object, for `____id_pack__` and `__class__`, which an object
    whose attributes come from its items lets the container set; a name the binding gives is one a
    receipt, a preview and a rule can take as the host's."""
    if issubclass(type(obj), type):
        return (type_name(obj), id(obj), 0)
    return (type_name(obj), id(type(obj)), id(obj))


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
        # id_pack -> [obj, references held across the agent's connections, the `(type, member)`
        # of each `getattr` that returned it]
        self._slots = {}
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
            slot = self._slots.setdefault(id_pack, [obj, 0, []])
            slot[1] += 1
            held = self._held.setdefault(conn, {})
            held[id_pack] = held.get(id_pack, 0) + 1

    def originate(self, id_pack, origin):
        """Record that a `getattr` returned the object `id_pack` names as `origin`, its target's
        type and the member read. Origins only accumulate while the object is held: a callable
        reached under two members is matched as both, so neither route loosens the other."""
        with self._lock:
            slot = self._slots.get(id_pack)
            if slot is not None and origin not in slot[2]:
                slot[2].append(origin)

    def origins(self, id_pack):
        """Every `(type, member)` a `getattr` returned the object `id_pack` names as, in the order
        they were recorded."""
        with self._lock:
            slot = self._slots.get(id_pack)
            return tuple(slot[2]) if slot is not None else ()

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
    """One request at a time across every connection of a binding declared to serialize: taken
    with `with turn.taking()`, granted to waiters in the order they arrived -- which
    `threading.RLock` does not promise -- and re-entrant for the thread whose turn it is, so a
    callback's nested request, served on that thread, runs at once. A binding that does not
    serialize has a turn nothing waits for.

    A waiter is the record `taking` is given, or one of its own; `drop` takes it out of the line,
    and it raises the exception `drop` gave instead of taking its turn.
    """

    def __init__(self, serialize=True):
        self._serialize = serialize
        self._cond = threading.Condition()
        self._owner = None  # the ident of the thread whose turn it is
        self._depth = 0  # how many times that thread has taken it
        self._waiters = collections.deque()  # in arrival order

    @contextlib.contextmanager
    def taking(self, waiter=None):
        if not self._serialize:
            yield
            return
        me = threading.get_ident()
        with self._cond:
            if self._owner == me:
                self._depth += 1
            else:
                waiter = waiter or types.SimpleNamespace(dropped=None)
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
                    raise waiter.dropped
                self._owner, self._depth = me, 1
        try:
            yield
        finally:
            with self._cond:
                self._depth -= 1
                if not self._depth:
                    self._owner = None
                    self._cond.notify_all()

    def drop(self, waiter, exc):
        """Take `waiter` out of the line, to raise `exc`, if it still waits there; whether it
        did."""
        with self._cond:
            if waiter not in self._waiters:
                return False
            waiter.dropped = exc
            self._cond.notify_all()
            return True


class Hosted:
    """One binding this process holds: the object its factory returned, and the turn every
    request to it takes once checked, which only a binding that serializes makes it wait for."""

    def __init__(self, root, serialize):
        self.root = root
        self.turn = Turn(serialize)


class Request:
    """A request a connection serves, from its receipt until it ends: whether it is recorded every
    time -- a boundary request -- or is bookkeeping, its binding's turn, and the callables it was
    passed. A cancel or the close finds it in `_requests`: `dropped` is what it raises instead of
    running, once one of them took it out of the turn's line or a cancel came before its target
    was invoked; `interrupted` says a cancel came while it ran. `lock` orders a cancel against the
    invocation, so a request is either dropped or noted, never run unnoted. `released` once the
    owner's gate allowed it, which the close then leaves to run."""

    dropped = None
    interrupted = False
    invoked = False
    released = False

    def __init__(self, seq, boundary, turn):
        self.seq, self.boundary, self.turn = seq, boundary, turn
        self.marks = []
        self.lock = threading.Lock()

    def cancel(self):
        """Its caller was interrupted: dropped if it has not run, else noted."""
        dropped = EOFError("the call was cancelled before it ran: its caller was interrupted")
        if self.turn.drop(self, dropped):
            return
        with self.lock:
            if self.invoked:
                self.interrupted = True
            else:
                self.dropped = dropped


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
    # Each request type by the name its record gives it: the `HANDLE_` constant's suffix.
    operations = {
        value: name[len("HANDLE_") :].lower()
        for name, value in vars(consts).items()
        if name.startswith("HANDLE_")
    }
    # The operations recorded only when refused; every other one is a boundary request.
    bookkeeping = frozenset(["ping", "close", "getroot", "inspect", "pickle", "del"])
    # Where in a request's arguments the member it names is: an attribute, or `cmp`'s operator.
    member_at = {
        consts.HANDLE_GETATTR: 1,
        consts.HANDLE_SETATTR: 1,
        consts.HANDLE_DELATTR: 1,
        consts.HANDLE_CALLATTR: 1,
        consts.HANDLE_OLDSLICING: 1,
        consts.HANDLE_CMP: 2,
    }
    containers = {
        consts.LABEL_TUPLE: tuple,
        LABEL_LIST: list,
        LABEL_SET: set,
        LABEL_FROZENSET: frozenset,
    }

    def allowed(name):
        """The attribute rule: a public name, or one on RPyC's safe list."""
        return type(name) is str and (
            (name and not name.startswith("_")) or name in safe_attrs
        )

    def check_attr(obj, name):
        """The rule for the attribute `name` of `obj`. A refusal by it carries the name as its
        `name`, as no other refusal does, so the container can raise a refused read as the
        `AttributeError` a missing attribute is."""
        if not allowed(name):
            reason = f"{name!r} is not a public name, and it is not on RPyC's safe list"
        elif name in ("format", "format_map") and (
            isinstance(obj, str) or (isinstance(obj, type) and issubclass(obj, str))
        ):
            reason = f"str.{name} reads attributes through its format string; it is refused"
        else:
            return name
        refused = Refused(reason)
        refused.name = name
        raise refused

    def check_member(handler, args):
        """The checks on the member a request names, made before its dispatch: the attribute
        rule, and only comparison operators for `cmp`. `oldslicing` and `buffiter` are refused
        whole: no proxy of the interpreter's sends either, and each runs more than its request
        names -- a second member, or each step of an iteration -- with no rule asked about it."""
        if handler == consts.HANDLE_OLDSLICING:
            raise Refused("oldslicing is Python 2's slicing, which this binding does not answer")
        if handler == consts.HANDLE_BUFFITER:
            raise Refused("buffiter is not answered: iterate with __iter__ and __next__")
        if handler == consts.HANDLE_CMP:
            if args[2] not in COMPARISONS:
                raise Refused(f"{args[2]!r} is not a comparison operator")
        elif handler in member_at:
            check_attr(args[0], args[member_at[handler]])

    class Named:
        """An object in a preview, by reference: its claimed type and number, never its own
        `repr`, which would run its code. One this binding handed out is named as `id_pack_of`
        named it."""

        def __init__(self, id_pack):
            self.text = f"<{id_pack[0]} #{id_pack[2]}>"

        def __repr__(self):
            return self.text

    def shown(package):
        """An argument as it crossed, rebuilt for a preview: values as they came, in the
        containers they came in, and anything by reference as `Named`."""
        label, value = package
        if label == consts.LABEL_VALUE:
            return value
        if label == LABEL_DICT:
            return {shown(key): shown(item) for key, item in value}
        if label in containers:
            return containers[label](map(shown, value))
        return Named(value)

    def preview(value):
        return _PREVIEW.repr(value)[:PREVIEW_CHARS]

    class Callback:
        """A callable the container passed as an argument: callable and nothing else, for as long
        as the request it was passed to is running.

        Not a netref. It sends no `inspect`, answers no attribute, has no `repr`, and refuses
        to be pickled or copied, so a library that holds it can do one thing with it: call it,
        which runs the container's callable on the thread waiting for this request. The lock keeps
        a call from being queued after the request has returned: `revoke` takes it before the
        reply is sent, so a call that got through precedes the reply on the wire.
        """

        __slots__ = ("_conn", "_id_pack", "_live", "_lock", "_seq")

        def __init__(self, conn, id_pack, seq):
            self._conn = conn
            self._id_pack = id_pack
            self._live = True
            self._lock = threading.Lock()
            self._seq = seq  # the request it was passed to

        def __call__(self, *args, **kwargs):
            with self._lock:
                if not self._live:
                    raise Refused(
                        "this callback cannot be called: the request it was passed to has returned"
                    )
                with contextlib.suppress(Exception):
                    _record(self._conn._key, "callback", self._seq)
                result = self._conn.async_request(
                    consts.HANDLE_CALL, self, args, tuple(kwargs.items())
                )
            # This thread serves the connection until the reply comes, so a request the
            # container makes inside the callback is received here, whichever thread called it.
            context = self._conn._context
            saved, context.parent = context.parent, self._seq
            try:
                return result.value
            finally:
                context.parent = saved

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
        """One connection of one kernel to one binding, every request checked; what it hands out
        goes in the table the agent's connections to the binding share."""

        def __init__(self, channel, *, key, hosted, objects, on_close):
            # First: cleanup takes it, and a connection that fails to initialize is cleaned up.
            self._turn = hosted.turn
            self._key = key  # (binding, agent, connection id), as its records name it
            # The connection's RPyC root is a service of nothing, whose `on_disconnect` RPyC
            # calls at cleanup; `getroot` answers with the object the factory returned.
            super().__init__(rpyc.core.service.VoidService(), channel, config)
            self._root_object = hosted.root
            # The agent's `Objects` for this binding; RPyC's own per-connection `_local_objects`
            # stays empty.
            self._objects = objects
            self._on_close = on_close
            self._context = _Context()

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
            that cannot be decoded closes the connection rather than killing its thread, as does
            a reply the owner labeled as a request; that request ends refused."""
            released = False
            # Read under the receive lock, as the frame was.
            late, label = self._channel.late, self._channel.label
            try:
                msg = data[0]
                if msg == consts.MSG_REQUEST:
                    self._recvlock.release()
                    released = True
                    seq, args = brine.load(data[1:])
                    labeled, label = label, None  # the request ends under it from here
                    self._dispatch_request(seq, args, late, labeled)
                elif label is not None:
                    raise ValueError(f"a reply labeled as request {label}")
                elif msg == consts.MSG_REPLY:
                    self._channel.claim()
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
                    self._channel.claim()
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
                self.close_with(reason, label)
                raise EOFError(reason) from None

        def _dispatch_request(self, seq, raw_args, late=False, label=None):
            """Serve one request, `late` when it arrived after the closed flag, and record it:
            a boundary request's receipt before anything is decided, its dispatch before its
            target runs -- held until the owner answers that the event is published, and
            refused if it is not -- and how it ended; bookkeeping only when it is refused. A
            request the owner labeled, as `label`, is a boundary request whatever it asks, and a
            boundary request it did not label is refused. A request numbered as the owner cannot
            record, as one this connection is serving, or other than its label closes the
            connection instead, recorded nowhere but as its label's refusal."""
            receipt = self._receipt(raw_args)
            boundary = label is not None or receipt["operation"] not in bookkeeping
            request = Request(seq, boundary, self._turn)
            key = (*self._key, seq)
            with _cancel_lock:
                if type(seq) is not int or not 0 <= seq < 1 << 64:
                    shown = _PREVIEW.repr(seq)[:PREVIEW_CHARS]
                    reason = f"a request numbered {shown}, not an integer below 2**64"
                elif key in _requests:
                    reason = (
                        f"request {seq} arrived while one of that number is served on this "
                        f"connection"
                    )
                elif label is not None and seq != label:
                    reason = f"request {seq} arrived labeled as request {label}"
                else:
                    reason = None
                    _requests[key] = request
            if reason is not None:
                self.close_with(reason, label)
                raise EOFError(reason)
            if self._channel.claim(seq):
                request.cancel()
            saved, self._context.request = self._context.request, request
            handed = []
            try:
                with contextlib.ExitStack() as turn:
                    try:
                        if request.boundary:
                            _record(self._key, "received", seq, **receipt)
                            if late:
                                raise Closed(_closing)
                            if label is None:
                                # Its events would land on any request the owner holds under
                                # its number: a boundary request runs only under its label.
                                raise Refused(f"request {seq} came without the owner's label")
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
                            decides = (
                                _policy.configured
                                and request.boundary
                                and receipt["operation"] not in bookkeeping
                            )
                            self._context.passed = [] if decides else None
                            try:
                                args = self._unbox(boxed)
                                passed = self._context.passed
                            finally:
                                self._context.passed = None
                            check_member(handler, args)
                            if decides:
                                # The target is the first object unboxed; the rest are arguments.
                                arguments = passed if target is None else passed[1:]
                                self._decide_policy(
                                    request, seq, handler, target, args, arguments, receipt
                                )
                        if handler not in unserialized:
                            # Once the request is checked, so a refused one waits for nobody; held
                            # until its exception, if any, is rendered below.
                            turn.enter_context(self._turn.taking(request))
                        with request.lock:
                            if request.dropped is not None:
                                raise request.dropped
                            request.invoked = True
                        if request.boundary:
                            ack = next(_decision_ids)
                            published = queue.SimpleQueue()
                            _decisions[ack] = (published, None)
                            try:
                                _record(self._key, "dispatched", seq, ack=ack)
                            except BaseException:
                                _decisions.pop(ack, None)
                                raise
                            if published.get() is not True:
                                raise Refused("the owner could not record this request")
                        res = method(self, *args)
                        reply = (consts.MSG_REPLY, self._box_tracked(res, handed))
                        if (
                            handler == consts.HANDLE_GETATTR
                            and reply[1][0] == consts.LABEL_REMOTE_REF
                            and callable(res)
                        ):
                            # Before the reply, so no call of it can come first.
                            self._objects.originate(reply[1][1], (type_name(args[0]), args[1]))
                        ended = {"outcome": "returned"}
                    except BaseException as e:
                        # The reply never goes out, so nothing it would have referenced is held.
                        self._release(handed)
                        t, v, tb = sys.exc_info()
                        self._last_traceback = tb
                        reply = (consts.MSG_EXCEPTION, self._box_exc(t, v, tb))
                        if e is request.dropped:
                            reason = "closed" if t is Closed else "interrupted"
                            ended = {"outcome": "cancelled", "reason": reason}
                        elif t is Denied:
                            ended = {
                                "outcome": "refused",
                                "reason": e._code,
                                "detail": str(e)[:TEXT_MAX],
                            }
                        elif request.invoked and (request.boundary or t is not Refused):
                            ended = {
                                "outcome": "raised",
                                "exception": f"{t.__module__}.{t.__qualname__}",
                                "traceback": "".join(traceback.format_exception(e))[-TEXT_MAX:],
                            }
                        else:
                            ended = {
                                "outcome": "refused",
                                "reason": "admission_closed" if t is Closed else "interception",
                                "detail": str(e)[:TEXT_MAX],
                            }
            finally:
                # Before the reply goes out, so no call can be queued after it.
                for mark in request.marks:
                    mark.revoke()
                self._context.request = saved
                _requests.pop(key, None)
            if request.boundary or ended["outcome"] == "refused":
                if not request.boundary:
                    _record(self._key, "received", seq, **receipt)
                _record(self._key, "ended", seq, interrupted=request.interrupted, **ended)
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

        def _receipt(self, raw_args):
            """What a request's receipt says of it: its operation, the member it names, its
            target's type when that is an object this agent was handed, a preview of each
            argument built from what crossed, and the request a callback made it inside. Decides
            nothing and never raises: a request too malformed to read is received all the
            same."""
            receipt = dict(
                operation="unknown",
                member=None,
                target=None,
                arguments=[],
                keywords={},
                parent=self._context.parent,
            )
            with contextlib.suppress(Exception):
                handler, boxed = raw_args
                receipt["operation"] = operations[handler] if type(handler) is int else "unknown"
                args = shown(boxed)
                at = member_at.get(handler)
                if at is not None and type(args[at]) is str:
                    receipt["member"] = args[at][:PREVIEW_CHARS]
                rest = [a for i, a in enumerate(args) if i != at and (i or handler in targetless)]
                if handler in (consts.HANDLE_CALL, consts.HANDLE_CALLATTR):
                    # A call's own arguments: its positional ones, and its keywords as pairs.
                    rest, keywords = args[-2:]
                    receipt["keywords"] = {
                        str(name)[:PREVIEW_CHARS]: preview(value)
                        for name, value in keywords[:PREVIEW_ITEMS]
                    }
                receipt["arguments"] = [preview(arg) for arg in rest[:PREVIEW_ITEMS]]
                target = self._target(handler, boxed)
                obj = None if target is None else self._lookup(target, required=False)
                if obj is not None:
                    if handler == consts.HANDLE_CALL:
                        # Shown as the call of the member it was read from, when it was.
                        _, receipt["member"], receipt["target"] = self._calls(obj, target)[0]
                    else:
                        receipt["target"] = type_name(obj)
            return receipt

        # ------------------------------------------------------------------ policy

        def _calls(self, obj, id_pack):
            """What a call of `obj` is matched as, `(operation, member, type)`: the call of each
            member a `getattr` returned it as, and of a bound method's own; else a call of its own
            type, naming no member. The first is how the call is shown."""
            found = list(self._objects.origins(id_pack))
            own = method_identity(obj)
            if own is not None and own not in found:
                found.append(own)
            return calls_of(found) or [("call", None, type_name(obj))]

        def _descriptions(self, handler, target, args):
            """What a request is matched as, each description a way the request could be named
            and each of which the policy must allow. `callattr` reads a member and calls what it
            read, so it is all three, and a call of the method the read would give, by the
            method's own name; one of a dunder that has an operation of its own is that operation
            too. A `callattr` whose method cannot be known before the read runs -- a property's,
            or one `__getattr__` makes -- is refused: no rule could decide what it would call, and
            the interpreter's proxies send `callattr` only for a method its type defines, so a
            read and then a call of what it gave is the way to it."""
            operation = operations[handler]
            if handler == consts.HANDLE_CALL:
                return self._calls(args[0], target)
            type_ = type_name(args[0])
            at = member_at.get(handler)
            member = args[at] if at is not None else None
            described = [(operation, member, type_)]
            if handler == consts.HANDLE_CALLATTR:
                known = [(type_, member)]
                canonical = static_identity(args[0], member)
                if canonical is None:
                    raise Refused(
                        f"callattr of {member!r} on {type_} is not answered: what it would call "
                        f"cannot be known before it runs; read {member!r} and call what it gives"
                    )
                if canonical not in known:
                    known.append(canonical)
                described += [("getattr", member, type_), *calls_of(known)]
            return described

        def _decide_policy(self, request, seq, handler, target, args, passed, receipt):
            """The policy's decision on a boundary request, before it takes its turn, recorded as
            `decided`: return to run it; raise `Denied` to refuse it; or hold it for the owner's
            answer, and raise what ends it unless that is an allow."""
            self._check_passed(seq, passed)
            described = self._descriptions(handler, target, args)
            verdict = _policy.decide(self._key[0], described)
            _record(self._key, "decided", seq, **verdict.fields(), reason=None)
            if verdict.action == "allow":
                return
            operation, member, type_ = described[0]
            if verdict.action == "deny":
                what = f"{operation} {member} on {type_}" if member else f"{operation} on {type_}"
                raise Denied(f"{verdict} denies {what}", verdict.code)
            with request.lock:
                if request.dropped is not None:
                    raise request.dropped  # its caller gave up before anyone was asked
            binding, agent, conn = self._key
            ask = dict(binding=binding, agent=agent, conn=conn, seq=seq, action=verdict.action)
            ask.update(operation=operation, member=member, target=type_)
            ask.update(arguments=receipt["arguments"], keywords=receipt["keywords"])
            _go_on(_decide(dict(ask, reason=f"{verdict} says {verdict.action}"), request))

        def _check_passed(self, seq, passed):
            """Refuse a request that passes a host callable whose own call the policy would not
            allow -- as an argument, nested in one, or as a value to assign: the host could call
            it later with no request, so nobody could be asked about that call."""
            for i, (id_pack, obj) in enumerate(passed, 1):
                if not callable(obj):
                    continue
                calls = self._calls(obj, id_pack)
                verdict = _policy.decide(self._key[0], calls)
                if verdict.action == "allow":
                    continue
                _, member, type_ = calls[0]
                what = type_ if member is None else f"{member} of {type_}"
                reason = (
                    f"argument {i} passes {what}, whose call {verdict} would "
                    f"{verdict.action}; the host could call it later with no request"
                )
                refused = {**verdict.fields(), "action": "deny"}
                _record(self._key, "decided", seq, **refused, reason=reason)
                raise Denied(reason, verdict.code)

        def _send(self, msg, seq, args):
            """RPyC's, without its shared send queue: a frame past the bound raises in the thread
            that asked, and no thread sends another's frame. The queue exists for a send from a
            finalizer, and this side makes none -- it holds no proxies. A reply's first line
            names the request it answers."""
            data = brine.I1.pack(msg) + brine.dump((seq, args))
            with self._sendlock:
                self._channel.send(data, None if msg == consts.MSG_REQUEST else {"reply": seq})

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
                obj = self._lookup(value)
                if self._context.passed is not None:
                    self._context.passed.append((value, obj))
                return obj
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
                request = self._context.request
                if request is None:
                    raise Refused(
                        f"a callable of type {self._claimed(value)} was returned from a callback; "
                        f"only values come back"
                    )
                proxy = Callback(self, value, request.seq)
                request.marks.append(proxy)
                return proxy
            raise Refused(f"unknown label {label!r}")

        def _unbox_reply(self, package):
            """A reply to this side's own request -- a callback's return value -- where a callable
            is refused too."""
            saved, self._context.request = self._context.request, None
            try:
                return self._unbox(package)
            finally:
                self._context.request = saved

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
            id_pack = id_pack_of(obj)
            self._objects.add(self, id_pack, obj)
            if self._context.boxed is not None:
                self._context.boxed.append(id_pack)
            return consts.LABEL_REMOTE_REF, id_pack

        def _box_exc(self, typ, val, tb):
            """An exception for the container: its type, args and public attributes, with one line
            of text -- how this side renders it -- where the traceback would go. The whole
            traceback stays here, in the request's record."""
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

        def close_with(self, reason, label=None):
            """Close for `reason`, telling the other side first. `label`, the request the owner
            holds the frame that caused it as, ends refused."""
            if label is not None:
                refused = dict(outcome="refused", reason="interception", detail=reason[:TEXT_MAX])
                with contextlib.suppress(Exception):
                    _record(self._key, "ended", label, **refused)
            with contextlib.suppress(Exception):
                self._on_close(reason)
            self._channel.close(reason)
            self.close()

        def _cleanup(self, _anyway=True):
            """RPyC's, and then this connection's references go back to the agent's table -- under
            the turn when the binding serializes, since the last of them may run a destructor."""
            with self._turn.taking():
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
    """The connections to this process's bindings, by binding, agent and connection id, shared by
    the reading thread and the serving threads; and each agent's object table for each binding,
    which the agent's connections to that binding share for as long as one of them is serving."""

    def __init__(self, Binding, hosted):
        self._Binding = Binding
        self._hosted = hosted  # binding -> its `Hosted`
        self._lock = threading.Lock()
        self._open = {}  # (binding, agent, id) -> channel
        # (binding, agent, id) of the last `CLOSED_MAX` connections that have closed, oldest first
        self._closed = collections.OrderedDict()
        self._objects = {}  # (binding, agent) -> its Objects
        self._serving = collections.Counter()  # (binding, agent) -> its connections still serving

    def holds(self, binding):
        return binding in self._hosted

    def channel(self, binding, agent, cid):
        """The channel for `(binding, agent, cid)`, started if new; `None` once it has closed."""
        key = (binding, agent, cid)
        table = (binding, agent)
        with self._lock:
            if key in self._closed:
                return None
            channel = self._open.get(key)
            if channel is not None:
                return channel

            def write(part, more, label=None):
                data = base64.b64encode(part).decode("ascii")
                line = {"t": "rpc", "binding": binding, "agent": agent, "id": cid, "data": data}
                _send({**line, "more": more, **(label or {})})

            channel = Channel(write, key)
            self._serving[table] += 1
            if self._serving[table] == 1:
                self._objects[table] = Objects()
            conn = self._Binding(
                channel,
                key=key,
                hosted=self._hosted[binding],
                objects=self._objects[table],
                on_close=lambda reason: self.notice(binding, agent, cid, reason),
            )
            self._open[key] = channel
        threading.Thread(
            target=self._serve,
            args=(key, conn),
            name=f"serve-{binding}-{agent}-{cid}",
            daemon=True,
        ).start()
        return channel

    def _serve(self, key, conn):
        try:
            conn.serve_all()
        except Exception as e:
            _diag(f"connection {key[0]}/{key[1]}#{key[2]}: serving it failed: {e!r}")
        finally:
            self.forget(*key)
            self._ended(key[:2])

    def _ended(self, table):
        """One of an agent's connections to a binding has finished serving, its cleanup run;
        after the last of them, the agent's table for the binding goes."""
        with self._lock:
            self._serving[table] -= 1
            if self._serving[table]:
                return
            del self._serving[table]
            objects = self._objects.pop(table)
        binding, agent = table
        _diag(
            f"binding {binding!r}, agent {agent!r}: its last connection has ended, and its object "
            f"table is dropped with {len(objects)} objects"
        )

    def forget(self, binding, agent, cid):
        with self._lock:
            self._open.pop((binding, agent, cid), None)
            self._closed[(binding, agent, cid)] = None
            if len(self._closed) > CLOSED_MAX:
                self._closed.popitem(last=False)

    def notice(self, binding, agent, cid, reason):
        """Tell the other side that `(binding, agent, cid)` has closed, and why."""
        self.forget(binding, agent, cid)
        _send({"t": "rpc", "binding": binding, "agent": agent, "id": cid, "closed": reason})

    def close(self, binding, agent, cid, reason):
        """The other side closed `(binding, agent, cid)`: wake its reader, which ends its
        thread."""
        with self._lock:
            channel = self._open.get((binding, agent, cid))
        if channel is not None:
            channel.close(reason)

    def cancel(self, binding, agent, cid, seqs):
        """The callers of requests `seqs` on `(binding, agent, cid)` were interrupted: one still
        waiting for its turn is dropped, never to run, and one running is noted. One not yet
        registered is kept for its registration while a frame of the connection is unclaimed,
        and a connection that would keep more than `CANCELS_MAX` closes."""
        with self._lock:
            channel = self._open.get((binding, agent, cid))
        if channel is None:
            return
        for seq in seqs:
            with _cancel_lock:
                request = _requests.get((binding, agent, cid, seq))
                if request is None and channel.unclaimed:
                    channel.cancelled.add(seq)
                over = len(channel.cancelled) > CANCELS_MAX
            if request is not None:
                request.cancel()
            if over:
                reason = f"more than {CANCELS_MAX} requests cancelled before they arrived"
                channel.close(reason)
                self.notice(binding, agent, cid, reason)
                return


def _handle(connections, line):
    """Route one protocol line by its kind: an `rpc` line to its connection, a decision's answer
    or a dispatch's publication to the thread holding the request, a control line to `_control`;
    or say on stderr why it could not be."""
    message = json.loads(line)
    kind = message.get("t") if isinstance(message, dict) else None
    if kind in ("decision", "published"):
        held = _decisions.pop(message.get("id"), None)
        answer = message.get("answer")
        if held is None:
            _diag(f"an answer to decision {message.get('id')!r}, which nothing awaits")
            return
        waiter, request = held
        if request is not None and isinstance(answer, dict) and answer.get("action") == "allow":
            request.released = True
        waiter.put(answer)
        return
    if kind == "control":
        _control(message)
        return
    if kind != "rpc":
        raise ValueError(f"not a protocol message: {line[:200]!r}")
    binding, agent, cid = message.get("binding"), message.get("agent"), message.get("id")
    if not isinstance(binding, str) or not isinstance(agent, str) or type(cid) is not int:
        raise ValueError("an rpc message without a binding, an agent and an integer id")
    if "closed" in message:
        connections.close(
            binding, agent, cid, f"the other side closed this connection: {message['closed']}"
        )
        return
    if "cancel" in message:
        connections.cancel(binding, agent, cid, message["cancel"])
        return
    more = message.get("more") is True
    # On a frame's first part: the number of the request the owner holds the frame as.
    label = message.get("request")
    if not connections.holds(binding):
        connections.notice(binding, agent, cid, f"this process holds no binding {binding!r}")
        return
    channel = connections.channel(binding, agent, cid)
    if channel is None:
        _dropped((binding, agent, cid), label, "this connection has closed")
        connections.notice(binding, agent, cid, "this connection has closed")
        return
    try:
        reason = channel.put(base64.b64decode(message.get("data", ""), validate=True), more, label)
    except MemoryError:
        # Rather than leave the kernel waiting on a frame that will never finish. A part that
        # failed before it was put leaves its label here.
        reason = channel.close("no memory to hold a frame from the interpreter")
        _dropped((binding, agent, cid), label, reason)
    except ValueError as e:
        # Never put, so the label it carries is dropped here.
        reason = channel.close(f"a part that is not base64: {e}")
        _dropped((binding, agent, cid), label, reason)
    if reason is not None:
        connections.notice(binding, agent, cid, reason)


def _read(connections, stream):
    with stream:
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
    if len(sys.argv) != 2:
        _diag(f"usage: python3 -I -c <program> <rpyc-dir>; got {sys.argv[1:]!r}")
        os._exit(2)
    rpyc_dir = sys.argv[1]
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
    sys.path.append(rpyc_dir)
    import rpyc

    Binding = connection_class(rpyc)
    stream = os.fdopen(_PROTO_IN, "rb")
    global _policy
    paths, bindings, policy = _arguments(stream.readline())
    _policy = Policy(policy)
    # In the session's order, which decides between two bindings that hold different versions of
    # one package, as the container's `sys.path` does.
    sys.path.extend(paths)
    hosted, types = {}, {}
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
        hosted[name] = Hosted(root, serialize)
        types[name] = f"{type(root).__module__}.{type(root).__qualname__}"
    _send({"t": "ready", "types": types})
    _read(Connections(Binding, hosted), stream)
    _die("the owner closed the protocol")


# Characters of a raising factory's one-line rendering sent to the owner; the whole traceback is
# on stderr.
FAILED_MAX = 4096


def _arguments(line):
    """The package directories, each binding as `(name, factory, args, kwargs, serialize)`, and the
    policy's tables, from the owner's first line; or end the process saying why there are none: no
    factory runs on a line this program did not understand, and no request on rules it could not
    read."""
    if not line:
        _die("the owner closed the protocol before sending the bindings")
    try:
        message = json.loads(line)
        bindings = [
            (b["name"], b["factory"], b["args"], b["kwargs"], b["serialize"])
            for b in message["bindings"]
        ]
        paths = message["paths"]
        policy = message["policy"]
        Policy(policy)  # every rule readable, or nothing starts
        if (
            message.get("t") != "start"
            or not isinstance(paths, list)
            or not all(isinstance(b[2], list) and isinstance(b[3], dict) for b in bindings)
        ):
            raise ValueError("not a start line")
    except (ValueError, KeyError, TypeError, AttributeError) as e:
        _diag(f"the first line was not the bindings to start ({e}): {line[:200]!r}")
        os._exit(2)
    return paths, bindings, policy


if __name__ == "__main__":
    main()
