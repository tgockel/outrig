"""What agent code uses to launch child agents and give them work, beside `runtime.spawn`.

The errors a child's work ends with are here, under `outrig.AgentError`, and so are `Request`,
which declares one of a request child's channels, and `Status`, what a handle says of its
request without waiting. They import on the host too, where nothing launches a child yet.

The rest is what the interpreter uses to check what one agent gives another before it goes: a
value given as an input is data -- `messages.md`'s serializable subset -- and the classes a child
binds by name must not share one.
"""

import dataclasses
import json
import math

from . import schema

__all__ = [
    "AgentError",
    "AgentLimitReached",
    "AgentReleased",
    "AgentRequestFailed",
    "BudgetExhausted",
    "CompletionRejected",
    "Request",
    "RequestCancelled",
    "RequestSettled",
    "SessionClosing",
    "Status",
]

# The longest preview of a value the session's events carry, in characters.
PREVIEW_MAX = 1024


class AgentError(Exception):
    """The base of every error OutRig's child agents end work with.

    `except outrig.AgentError` around `await handle` catches the child's failure -- rejected,
    failed, cancelled, released, out of budget, the session closing, a limit -- and lets the
    caller's own bugs, a `TypeError` say, through.
    """


class CompletionRejected(AgentError):
    """The child's answers failed their check past the request's attempt limit, or a work child's
    rounds ended without completing.

    `value` is the last value it answered with and `problems` what was wrong with it, each a
    `outrig.schema.Problem`; both are empty when it never answered.
    """

    def __init__(self, message, value=None, problems=()):
        super().__init__(message)
        self.value = value
        self.problems = tuple(problems)


class AgentReleased(AgentError):
    """The child, or an agent above it, was released before it answered."""


class AgentRequestFailed(AgentError):
    """The child failed the request on purpose, with `await delivery.fail(message)`; `message` is
    why."""

    def __init__(self, message):
        super().__init__(message)
        self.message = message


class RequestCancelled(AgentError):
    """The request was cancelled with `handle.cancel()`. In the child, an answer to it raises
    this."""


class RequestSettled(AgentError):
    """An answer to a request that had already ended -- answered, rejected, failed, released, or
    closed -- refused. `ended` says how it had ended."""

    def __init__(self, message, ended):
        super().__init__(message)
        self.ended = ended


class AgentLimitReached(AgentError):
    """A limit the session holds every agent to refused at once, rather than waiting: the depth
    agents may nest to, the children a session holds, or the requests one child holds."""


class BudgetExhausted(AgentError):
    """The session's token budget was spent before the child answered: no model call is made
    from then on."""


class SessionClosing(AgentError):
    """The session closed to new work before the child answered, or before this could start."""


class Request:
    """One request channel of a child `runtime.spawn(..., requests={name: Request(...)})` makes.

    A request on it carries `request_type`, and the child's reply `reply_type`; both are declared
    with `outrig.schema`, so each must be one it takes. `doc` describes the channel in the child's
    instructions. A bare `(request_type, reply_type)` tuple is the same without a description.
    """

    __slots__ = ("request_type", "reply_type", "doc")

    def __init__(self, request_type, reply_type, *, doc=None):
        if doc is not None and type(doc) is not str:
            raise TypeError(f"a request channel's doc is a str, not {type(doc).__name__}")
        self.request_type = request_type
        self.reply_type = reply_type
        self.doc = doc

    def __repr__(self):
        return (
            f"Request({schema._describe(self.request_type)}, "
            f"{schema._describe(self.reply_type)}, doc={self.doc!r})"
        )


@dataclasses.dataclass(frozen=True)
class Status:
    """What a handle's request is doing, read without waiting.

    `state` is `queued` or `running` for a submission, `waiting` or `received` for a request on a
    channel, and `accepted` or `failed` once it has ended; `reason` says why a failed one failed --
    `rejected`, `failed`, `cancelled`, `released`, `budget`, `closed` or `terminated` -- and is
    `None` otherwise. `child` is its child's state: `running`, `idle`, `wedged` or `released`.
    Fixed once the request has ended.
    """

    state: str
    reason: object
    child: str

    def __str__(self):
        state = self.state if self.reason is None else f"{self.state}: {self.reason}"
        return f"{state} (child {self.child})"


# ---------------------------------------------------------------------------- inputs


def preview(value):
    """`value`, a JSON value, as text cut to `PREVIEW_MAX` characters: what an event shows."""
    return cut(json.dumps(value, ensure_ascii=False))


def cut(text):
    """`text` cut to `PREVIEW_MAX` characters, saying how many more it had."""
    if len(text) <= PREVIEW_MAX:
        return text
    return text[:PREVIEW_MAX] + f"... [{len(text) - PREVIEW_MAX} more characters]"


def walk(value, path, foreign=None):
    """`(json, shape, classes)` for `value`, an input one agent gives another.

    It must be data: `None`, a `str`, `int`, `bool` or finite `float`, a `list` or a `dict` with
    `str` keys of the same, or an instance of a dataclass `outrig.schema` takes, checked against
    its fields' types. `json` is its JSON form, `shape` its type as text -- `list[Row]` -- and
    `classes` the dataclasses in it. Anything else raises `TypeError` naming where it is;
    `foreign(value)` may say what the value is, for the message.
    """
    kind = type(value)
    if value is None:
        return None, "None", set()
    if kind in (str, int, bool):
        return value, kind.__name__, set()
    if kind is float:
        if not math.isfinite(value):
            raise TypeError(f"{path} is {value!r}, and a float that is not finite has no JSON form")
        return value, "float", set()
    if kind is list or kind is dict:
        out = [] if kind is list else {}
        shapes, classes = [], set()
        items = enumerate(value) if kind is list else value.items()
        for key, item in items:
            if kind is dict and type(key) is not str:
                raise TypeError(f"{path} has the key {key!r}; a dict given as data is keyed by str")
            step = f"{path}[{key!r}]" if kind is dict else f"{path}[{key}]"
            encoded, shape, found = walk(item, step, foreign)
            if kind is list:
                out.append(encoded)
            else:
                out[key] = encoded
            if shape not in shapes:
                shapes.append(shape)
            classes |= found
        inner = " | ".join(shapes)
        if kind is list:
            return out, f"list[{inner}]" if inner else "list", classes
        return out, f"dict[str, {inner}]" if inner else "dict", classes
    if isinstance(kind, type) and dataclasses.is_dataclass(kind):
        declared, used = declared_with_classes(kind, f"{path} is a")
        try:
            encoded = declared.encode(value)
        except TypeError as e:
            raise TypeError(f"{path} is not data its class declares: {e}") from None
        return encoded, kind.__name__, set(used)
    what = foreign(value) if foreign is not None else None
    if what is None:
        what = f"a {schema._type_qualname(kind)}"
    raise TypeError(
        f"{path} is {what}, outside the serializable subset an input must be in: "
        f"{schema._SUBSET}"
    )


def declared_with_classes(t, where):
    """`(Schema, classes)`: `t` declared, as `outrig.schema.declare` declares it with `where`
    beginning any error, and every dataclass it uses."""
    compiler = schema._Compiler(where)
    node = compiler.node(t, "")
    return schema.Schema(node), list(compiler.classes)


def one_name_each(classes, names=()):
    """Raise `TypeError` if two of `classes` share a `__name__`, or one shares a name in `names`:
    each is bound under its name in a child's namespace, where one name holds one thing."""
    seen = {name: None for name in names}
    for cls in classes:
        name = cls.__name__
        if name in seen:
            other = seen[name]
            if other is cls:
                continue
            if other is None:
                raise TypeError(
                    f"class {schema._type_qualname(cls)} would be bound in the child under "
                    f"{name!r}, which an input is named"
                )
            raise TypeError(
                f"classes {schema._type_qualname(other)} and {schema._type_qualname(cls)} would "
                f"both be bound in the child under {name!r}; one name holds one class"
            )
        seen[name] = cls
