"""What agent code uses to launch child agents and give them work, beside `runtime.spawn`.

`agent` is the decorator that declares a typed `async def` as work for a child. The errors a
child's work ends with are here, under `outrig.AgentError`, and so are `Request`, which declares
one of a request child's channels, and `Status`, what a handle says of its request without
waiting. They import on the host too, where a declaration is made as in a kernel and a call
raises.

The rest is what the interpreter uses to check what one agent gives another before it goes: a
value given as an input is data -- `messages.md`'s serializable subset -- and the classes a child
binds by name must not share one.
"""

import dataclasses
import functools
import hashlib
import inspect
import json
import keyword
import math
import os
import sys
import types
import typing

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
    "agent",
]

# The longest preview of a value the session's events carry, in characters.
PREVIEW_MAX = 1024

# What only the interpreter has, bound as it loads the package: `_call(declaration, arguments)`
# makes a typed call from the calling kernel and returns its handle, and `_submitted_source(code)`
# is the source of the submission that compiled `code`, or `None` for code none did. On the host
# both stay `None`.
_call = None
_submitted_source = None

# Names a parameter, or any input, cannot take: the child's own runtime and what every kernel
# binds at boot.
RESERVED = frozenset({"runtime", "asyncio"})

_CALL_NOTE = (
    "A call sends this work to a new child agent and returns its handle at once, before the "
    "child exists: await the handle for the result, or keep it for its status, progress and "
    "cancel(). The body is never run."
)


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
    `rejected`, `failed`, `cancelled`, `released`, `budget`, `closed`, `terminated`, or `limit`
    for a typed call whose child a limit refused -- and is `None` otherwise. `child` is its
    child's state: `running`, `idle`, `wedged` or `released`. Fixed once the request has ended.
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


# ---------------------------------------------------------------------------- declarations


@dataclasses.dataclass(frozen=True)
class Declaration:
    """What `@outrig.agent` read from a function, kept as the wrapper's `__outrig_agent__`."""

    fn: object
    module: str
    qualname: str
    doc: str  # the docstring, cleaned: the child's instructions
    parameters: dict  # each parameter's name to the Schema of its type
    result: object  # the return annotation
    schema: object  # the result's Schema
    validate: object
    model: object
    digest: object  # SHA-256 hex digest of the source that declared `fn`, or None

    @property
    def name(self):
        return f"{self.module}.{self.qualname}"


def agent(fn=None, *, validate=None, model=None):
    """Declare `fn`, an `async def`, as work for a child agent: `@outrig.agent`, or
    `@outrig.agent(validate=..., model=...)`.

    Its parameters are the child's inputs, its docstring the child's instructions, and its return
    annotation the result's type; each type is one `outrig.schema` takes. The body is never run.
    Calling the declaration checks each argument against its parameter's type and copies it,
    sends the work to a new child agent, and returns the handle at once: `await` it for the
    result, checked against the return type. The child is released once the call settles,
    awaited or not.

    `validate(result, **arguments)` runs on each answer the child gives, with the arguments as
    they were at the call, and returns a list of problems, empty to accept; the child corrects
    what it lists, within the attempt limit, reading the first 20, each cut to 1,024 characters.
    It must be pure -- no hosted calls, writes or model calls -- since it runs again on every
    attempt. `model` names a configured model or alias for the child; without it, the caller's.
    What a declaration cannot take -- a missing docstring, an unannotated parameter, a type
    outside the subset -- raises `TypeError` at decoration, naming the function.
    """
    if fn is None:
        return functools.partial(agent, validate=validate, model=model)
    declaration = _declare(fn, validate, model)
    signature = inspect.signature(fn)

    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        bound = signature.bind(*args, **kwargs)
        bound.apply_defaults()
        wrong = []
        for name, value in bound.arguments.items():
            _, problems = declaration.parameters[name]._encoding(value)
            wrong += [f"  {name}{p.path[1:]}: {p.message}" for p in problems.found]
        if wrong:
            raise TypeError(
                f"{declaration.name}() was given arguments outside their declared types:\n"
                + "\n".join(wrong)
            )
        if _call is None:
            raise RuntimeError(
                f"a call to {declaration.name} runs in an agent's kernel, where OutRig runs agent "
                "code, and this Python is not one"
            )
        return _call(declaration, bound.arguments)

    wrapper.__doc__ = f"{declaration.doc}\n\n{_CALL_NOTE}"
    wrapper.__outrig_agent__ = declaration
    return wrapper


def _declare(fn, validate, model):
    """`fn` read as a `Declaration`, or `TypeError` naming it and what is wrong."""
    if type(fn) is not types.FunctionType or not inspect.iscoroutinefunction(fn):
        raise TypeError(f"@outrig.agent declares an `async def` function, not {fn!r}")
    name = f"{fn.__module__}.{fn.__qualname__}"
    if not fn.__doc__ or not fn.__doc__.strip():
        raise TypeError(f"{name} has no docstring, which is what a declaration tells its child")
    signature = inspect.signature(fn)
    for p in signature.parameters.values():
        if p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            raise TypeError(
                f"{name} takes {'*' if p.kind is p.VAR_POSITIONAL else '**'}{p.name}; a "
                "declaration's parameters are named, each one input of the child"
            )
        if p.annotation is p.empty:
            raise TypeError(f"{name}: parameter {p.name!r} has no type annotation")
        if p.name in RESERVED or keyword.iskeyword(p.name) or p.name.startswith("__"):
            raise TypeError(f"{name}: parameter {p.name!r} cannot name a variable in the child")
    if signature.return_annotation is signature.empty:
        raise TypeError(f"{name} has no return annotation, which is the result's type")
    try:
        hints = typing.get_type_hints(fn, include_extras=True)
    except Exception as e:
        raise TypeError(
            f"{name}: its annotations cannot be resolved: {schema._type_name(type(e))}: {e}"
        ) from None
    parameters, classes = {}, []
    for p in signature.parameters.values():
        parameters[p.name], used = declared_with_classes(
            hints[p.name], f"{name}: parameter {p.name!r} cannot be"
        )
        classes += used
        if p.default is not p.empty:
            _, problems = parameters[p.name]._encoding(p.default)
            if problems:
                raise TypeError(
                    f"{name}: the default of parameter {p.name!r} is not of its type:\n"
                    f"{problems.listing()}"
                )
    result = hints["return"]
    declared, used = declared_with_classes(result, f"{name}: the result cannot be")
    try:
        one_name_each(list(dict.fromkeys(classes + used)), list(signature.parameters))
    except TypeError as e:
        raise TypeError(f"{name}: {e}") from None
    if validate is not None:
        if not callable(validate):
            raise TypeError(f"{name}: validate is not callable: {validate!r}")
        try:
            taking = inspect.signature(validate)
        except (TypeError, ValueError):
            taking = None
        if taking is not None:
            try:
                taking.bind(None, **dict.fromkeys(signature.parameters))
            except TypeError as e:
                raise TypeError(
                    f"{name}: validate cannot be called as validate(result, "
                    f"{', '.join(f'{p}=...' for p in signature.parameters)}): {e}"
                ) from None
    if model is not None and type(model) is not str:
        raise TypeError(
            f"{name}: model names a configured model, a str, not "
            f"{schema._type_qualname(type(model))}"
        )
    return Declaration(
        fn, fn.__module__, fn.__qualname__, inspect.cleandoc(fn.__doc__), parameters, result,
        declared, validate, model, _digest(fn),
    )


def _digest(fn):
    """The SHA-256 hex digest of the source that declared `fn`: the submission's for one compiled
    in a submission, whichever execution runs the declaring; its module's file otherwise, or what
    the module's loader gives; `None` when none can be read."""
    filename = fn.__code__.co_filename
    try:
        if filename == "<execution>":
            source = None if _submitted_source is None else _submitted_source(fn.__code__)
        elif os.path.isabs(filename):
            with open(filename, "rb") as f:
                source = f.read()
        else:
            loader = getattr(sys.modules.get(fn.__module__), "__loader__", None)
            source = loader.get_source(fn.__module__) if hasattr(loader, "get_source") else None
    except Exception:
        source = None
    if source is None:
        return None
    return hashlib.sha256(source.encode() if type(source) is str else source).hexdigest()
