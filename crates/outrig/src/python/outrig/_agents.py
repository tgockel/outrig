"""What agent code uses to launch child agents and give them work, beside `runtime.spawn`.

`agent` is the decorator that declares a typed `async def` as work for a child, and `Agent` the
base of an agent class, whose instance is one long-lived child answering requests on the methods
`agent` marks. The errors a child's work ends with are here, under `outrig.AgentError`, and so
are `Request`, which declares one of a request child's channels, and `Status`, what a handle says
of its request without waiting. They import on the host too, where a declaration is made as in a
kernel and a call, or a construction, raises.

The rest is what the interpreter uses to check what one agent gives another before it goes: a
value given as an input is data -- `messages.md`'s serializable subset -- and the classes a child
binds by name must not share one.
"""

import copy
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
    "Agent",
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
# makes a typed call from the calling kernel and returns its handle, `_launch(agent, inputs,
# model)` launches an agent class instance's child from the calling kernel and returns what the
# runtime keeps of the instance, and `_submitted_source(code)` is the source of the submission
# that compiled `code`, or `None` for code none did. On the host all three stay `None`.
_call = None
_launch = None
_submitted_source = None

# Names a parameter, or any input, cannot take: the child's own runtime and what every kernel
# binds at boot.
RESERVED = frozenset({"runtime", "asyncio"})

_CALL_NOTE = (
    "A call sends this work to a new child agent and returns its handle at once, before the "
    "child exists: await the handle for the result, or keep it for its status, progress and "
    "cancel(). The body is never run."
)

_REQUEST_NOTE = (
    "A call sends its arguments as a request to the instance's child agent and returns the "
    "request's handle at once: await the handle for the reply, or keep it for its status and "
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

    When a model call of the child's failed and ended one of those rounds -- the provider
    refused a reply, say -- the child did not stop of its own accord, and `__cause__` is an
    `AgentRequestFailed` whose message says how the call failed. It is `None` otherwise.
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

    On a method of an `outrig.Agent` subclass it marks a request channel instead, and takes
    neither keyword: see `help(outrig.Agent)`.
    """
    if fn is None:
        return functools.partial(agent, validate=validate, model=model)
    owner = fn.__qualname__.rpartition(".")[0] if type(fn) is types.FunctionType else ""
    if owner and not owner.endswith("<locals>"):
        # Defined in a class body: the class reads the mark as it is made.
        return _Marked(fn, validate, model)
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
    """The SHA-256 hex digest of the source that declared `fn`, a function or a class: the
    submission's for a function compiled in a submission, whichever execution runs the declaring;
    its module's file otherwise, or what the module's loader gives; `None` when none can be
    read."""
    module = sys.modules.get(fn.__module__)
    code = None if isinstance(fn, type) else fn.__code__
    filename = code.co_filename if code else getattr(module, "__file__", None) or ""
    try:
        if filename == "<execution>":
            source = None if _submitted_source is None else _submitted_source(code)
        elif os.path.isabs(filename):
            with open(filename, "rb") as f:
                source = f.read()
        else:
            loader = getattr(module, "__loader__", None)
            source = loader.get_source(fn.__module__) if hasattr(loader, "get_source") else None
    except Exception:
        source = None
    if source is None:
        return None
    return hashlib.sha256(source.encode() if type(source) is str else source).hexdigest()


# ---------------------------------------------------------------------------- agent classes


class _Marked:
    """A method `@outrig.agent` marked in a class body: a request channel, which the
    `outrig.Agent` subclass it is in reads as it is made. Any other class refuses it."""

    def __init__(self, fn, validate, model):
        self.fn, self.validate, self.model = fn, validate, model

    def __set_name__(self, owner, name):
        if not issubclass(owner, Agent):
            raise TypeError(
                f"{owner.__module__}.{owner.__qualname__}.{name}: @outrig.agent on a method marks "
                f"a request channel of an outrig.Agent subclass, and {owner.__qualname__} is not "
                "one; declare typed work for a child as a function instead"
            )


@dataclasses.dataclass(frozen=True)
class RequestMethod:
    """What a method of an `outrig.Agent` subclass marked `@outrig.agent` declares: one request
    channel, kept as its sender's `__outrig_channel__`."""

    doc: str  # the docstring, cleaned: the channel's description
    parameters: dict  # each parameter's name, after `self`, to the Schema of its type
    request_type: object  # None, the one parameter's type, or the generated message dataclass
    reply_type: object  # the return annotation
    request: object  # the request type's Schema
    reply: object  # the reply type's Schema
    classes: tuple  # every dataclass the two use
    key: object  # what a subclass restating the channel must declare alike


class Agent:
    """The base of an agent class: each instance is one long-lived child agent, and each method
    marked `@outrig.agent` is one of its request channels.

        class Reviewer(outrig.Agent):
            \"\"\"You review the repository in /workspace.\"\"\"

            @outrig.agent
            async def review(self, path: str) -> Verdict:
                \"\"\"Review the file at `path`.\"\"\"

        reviewer = Reviewer()
        verdict = await reviewer.review("src/main.py")

    The class's docstring is the first part of the child's instructions, as written. A marked
    method is an `async def` whose docstring describes its channel, whose parameters after `self`
    are each annotated, and which has a return annotation, each a type `outrig.schema` takes. A
    call checks its arguments and sends them as one request -- none as `None`, one as itself,
    several as a frozen dataclass the class holds, `<Class><Method>Message` -- and returns the
    request's handle at once: await it for the reply, of the return type. The body is never run.
    Methods not marked are ordinary ones, which run here. What a class cannot declare raises
    `TypeError` as the class is made, naming the member.

    `Reviewer(**inputs, model=None)` binds each input, which must be data, in the child as a
    variable, launches the child in the background -- on `model`, a configured model or alias,
    or on yours -- and returns at once; a request made before the child exists waits for it. The
    child's first round is its own, to get ready in: `await reviewer.ready()` returns once it has
    ended, and raises what the launch met instead. `await reviewer.release()` ends the child and
    every request still open on it; `async with Reviewer() as reviewer:` waits for `ready()` on
    entry and releases on leaving. An instance collected unreleased is released then.
    """

    # What the runtime keeps of the instance, once it is made; and the request channels its class
    # declares, each by name its sender.
    _outrig = None
    _outrig_channels = {}

    def __init_subclass__(cls, **kwargs):
        super().__init_subclass__(**kwargs)
        _declare_class(cls)

    def __init__(self, *, model=None, **inputs):
        name = f"{type(self).__module__}.{type(self).__qualname__}"
        if not type(self)._outrig_channels:
            raise TypeError(f"{name} declares no request channel -- no method marked "
                            "@outrig.agent -- so an instance of it would answer nothing")
        if model is not None and type(model) is not str:
            raise TypeError(f"model names a configured model, a str, not "
                            f"{schema._type_qualname(type(model))}")
        if _launch is None:
            raise RuntimeError(f"an instance of {name} runs in an agent's kernel, where OutRig "
                               "runs agent code, and this Python is not one")
        self._outrig = _launch(self, inputs, model)

    async def ready(self):
        """Return once the child has ended its first round, which is its own, to get ready in.

        Raises what the launch met instead -- `ValueError` for an unknown model,
        `outrig.AgentLimitReached`, `outrig.SessionClosing` -- or `outrig.BudgetExhausted` or
        `outrig.AgentRequestFailed` when the child cannot have that round, or
        `outrig.AgentReleased` when the instance was released first. It settles once: awaiting
        again answers the same at once.
        """
        await _runs(self).ready()

    async def release(self):
        """End the child, and every request still open on it with `outrig.AgentReleased`, which
        each call raises from now on. Returns once it has ended; a second call, once the first
        has."""
        await _runs(self).release("explicit")

    async def __aenter__(self):
        try:
            await self.ready()
        except BaseException:
            await _runs(self).release("async_with")
            raise
        return self

    async def __aexit__(self, *exc_info):
        await _runs(self).release("async_with")


def _runs(agent):
    """What the runtime keeps of `agent`, an agent class's instance."""
    if agent._outrig is None:
        raise TypeError(f"this {type(agent).__qualname__} was not made by outrig.Agent.__init__: "
                        "a subclass's __init__ calls super().__init__(**inputs, model=...)")
    return agent._outrig


def _declare_class(cls):
    """Check `cls`, an `Agent` subclass being made, and make each method it marks
    `@outrig.agent` the sender of its request channel; `TypeError` naming what is wrong."""
    where = f"{cls.__module__}.{cls.__qualname__}"
    if not cls.__doc__ or not cls.__doc__.strip():
        raise TypeError(f"{where} has no docstring, which is the first part of its child's "
                        "instructions")
    senders, own = {}, []
    for base in reversed(cls.__mro__[1:]):
        senders.update(vars(base).get("_outrig_channels", {}))
    for member, value in list(vars(cls).items()):
        if type(value) in (staticmethod, classmethod) and type(value.__func__) is _Marked:
            wrap = f"@{type(value).__name__}"
            raise TypeError(f"{where}.{member}: a request channel is a method of the instance, "
                            f"which {wrap} would make it not; remove {wrap}")
        if type(value) is _Marked:
            name = f"{where}.{member}"
            if member == "user" or hasattr(Agent, member):
                taken = "the user channel's" if member == "user" else "a member of outrig.Agent"
                raise TypeError(f"{name}: a request channel cannot be named {member!r}, which is "
                                f"{taken}")
            base = senders.get(member)
            if base is None and any(member in vars(b) for b in cls.__mro__[1:]):
                raise TypeError(f"{name} would make a base class's ordinary member a request "
                                "channel; name the channel otherwise")
            method = _method(cls, member, value, base and base.__outrig_channel__)
            senders[member] = _sender(member, method, value.fn)
            setattr(cls, member, senders[member])
            value = value.fn
        if type(value) is types.FunctionType:
            own.append(value)
    for member, sender in senders.items():
        if inspect.getattr_static(cls, member, None) is not sender:
            raise TypeError(f"{where}.{member} replaces request channel {member!r} with an "
                            "ordinary member; restate it marked @outrig.agent with the same "
                            "signature, or name the member otherwise")
    seen = {}
    for member, sender in senders.items():
        for c in sender.__outrig_channel__.classes:
            other, by = seen.setdefault(c.__name__, (c, member))
            if other is not c:
                raise TypeError(
                    f"{where}.{by} and {where}.{member} use two classes named {c.__name__!r}, "
                    f"{schema._type_qualname(other)} and {schema._type_qualname(c)}; each is "
                    "bound in the child under its name, where one name holds one class"
                )
    cls._outrig_channels = senders
    # Its module's file; or, for a class a submission declared, that submission's source, which
    # any function its body defined -- not one bound there from elsewhere -- was compiled from.
    own = [fn for fn in own if fn.__code__.co_qualname.startswith(f"{cls.__qualname__}.")]
    cls._outrig_digest = _digest(own[0] if own else cls)


def _method(cls, member, marked, base):
    """What `marked`, `cls`'s method `member`, declares, or `TypeError` naming it and what is
    wrong. Restating `base`, a base class's channel, it must declare the same, and keeps that
    channel's types."""
    fn, name = marked.fn, f"{cls.__module__}.{cls.__qualname__}.{member}"
    if marked.model is not None:
        raise TypeError(f"{name}: a request method takes no model=; an instance's child runs on "
                        f"one model, which the constructor's model= names: "
                        f"{cls.__name__}(model=...)")
    if marked.validate is not None:
        raise TypeError(f"{name}: a request method takes no validate=; its reply is checked "
                        "against its type")
    if not inspect.iscoroutinefunction(fn):
        raise TypeError(f"{name}: @outrig.agent marks an `async def` method, not {fn!r}")
    if not fn.__doc__ or not fn.__doc__.strip():
        raise TypeError(f"{name} has no docstring, which describes its channel to the child")
    signature = inspect.signature(fn)
    parameters = list(signature.parameters.values())
    if not parameters or parameters[0].name != "self":
        raise TypeError(f"{name}: a request method's first parameter is self")
    parameters = parameters[1:]
    for p in parameters:
        if p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            raise TypeError(
                f"{name} takes {'*' if p.kind is p.VAR_POSITIONAL else '**'}{p.name}; a request "
                "has one fixed shape, each parameter one part of it"
            )
        if p.annotation is p.empty:
            raise TypeError(f"{name}: parameter {p.name!r} has no type annotation")
    if signature.return_annotation is signature.empty:
        raise TypeError(f"{name} has no return annotation, which is the reply's type")
    try:
        hints = typing.get_type_hints(fn, include_extras=True)
    except Exception as e:
        raise TypeError(
            f"{name}: its annotations cannot be resolved: {schema._type_name(type(e))}: {e}"
        ) from None
    schemas = {}
    for p in parameters:
        schemas[p.name], _ = declared_with_classes(
            hints[p.name], f"{name}: parameter {p.name!r} cannot be"
        )
        if p.default is not p.empty:
            _, problems = schemas[p.name]._encoding(p.default)
            if problems:
                raise TypeError(
                    f"{name}: the default of parameter {p.name!r} is not of its type:\n"
                    f"{problems.listing()}"
                )
    reply, used = declared_with_classes(hints["return"], f"{name}: the reply cannot be")
    key = ([(p.name, p.kind, p.default, schemas[p.name].text()) for p in parameters], reply.text())
    doc = inspect.cleandoc(fn.__doc__)
    if base is not None:
        if key != base.key:
            raise TypeError(f"{name} restates a base class's request channel with another "
                            "signature, which code written against the base would not send; "
                            "restate it alike, or name it otherwise")
        return dataclasses.replace(base, doc=doc)
    if len(parameters) > 1:
        title = cls.__name__ + "".join(part[:1].upper() + part[1:] for part in member.split("_"))
        if title + "Message" in vars(cls):
            raise TypeError(f"{name}: its request is the dataclass {title}Message, a name "
                            f"{cls.__qualname__} already gives a member of its own")
        fields = []
        for p in parameters:
            field = {"kw_only": p.kind is p.KEYWORD_ONLY}
            # A default the dataclass could share between messages is made afresh for each.
            if p.default is not p.empty and type(p.default).__hash__ is None:
                field["default_factory"] = functools.partial(copy.deepcopy, p.default)
            elif p.default is not p.empty:
                field["default"] = p.default
            fields.append((p.name, hints[p.name], dataclasses.field(**field)))
        request_type = dataclasses.make_dataclass(
            title + "Message", fields, frozen=True, module=cls.__module__
        )
        setattr(cls, request_type.__name__, request_type)
    else:
        request_type = hints[parameters[0].name] if parameters else None
    request, also = declared_with_classes(request_type, f"{name}: the request cannot be")
    return RequestMethod(doc, schemas, request_type, hints["return"], request, reply,
                         tuple(dict.fromkeys(also + used)), key)


def _sender(member, method, fn):
    """The method a channel's mark becomes: a call binds its arguments to `fn`'s signature,
    checks each against its type, and sends the request they make, returning its handle."""
    signature = inspect.signature(fn)

    @functools.wraps(fn)
    def send(self, *args, **kwargs):
        bound = signature.bind(self, *args, **kwargs)
        bound.apply_defaults()
        arguments = dict(list(bound.arguments.items())[1:])
        wrong = []
        for name, value in arguments.items():
            _, problems = method.parameters[name]._encoding(value)
            wrong += [f"  {name}{p.path[1:]}: {p.message}" for p in problems.found]
        if wrong:
            raise TypeError(
                f"{type(self).__qualname__}.{member}() was given arguments outside their "
                "declared types:\n" + "\n".join(wrong)
            )
        if len(arguments) > 1:
            message = method.request_type(**arguments)
        else:
            message = next(iter(arguments.values()), None)
        return _runs(self).request(self, member, message)

    send.__doc__ = f"{method.doc}\n\n{_REQUEST_NOTE}"
    send.__outrig_channel__ = method
    return send
