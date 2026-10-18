"""Declared types: strict decoding into them, and the JSON Schema that describes them.

`declare(T)` checks a type at once and returns its `Schema`. A JSON value then becomes an
instance of `T` only when it has exactly `T`'s shape:

    from dataclasses import dataclass
    from typing import Annotated, Literal, Optional
    from outrig.schema import alias, declare, doc

    @dataclass(frozen=True, kw_only=True)
    class Verdict:
        status: Annotated[Literal["approve", "reject"], doc("The outcome.")]
        next_step: Annotated[Optional[str], alias("next-step"), doc("What to do next, or null.")]

    verdict = declare(Verdict)
    verdict.decode({"status": "approve", "next-step": None})   # a Verdict
    verdict.text()                                             # its JSON Schema, as text

The types it takes: `str`, `bool`, `int`, finite `float`, `None`; `Literal` of str, int and bool
values; `list[T]`; `dict[str, T]`; unions of these, optionals included; dataclasses whose fields
are all of these; and `Annotated` around any of them. A type outside the set, or a field hint
that does not resolve in its module, fails at `declare`, naming the field.

`doc(text)` in a field's `Annotated` is its description, carried verbatim into the schema.
`alias(name)` is its JSON key; nothing else renames a field.

Decoding is strict. A key no field declares is rejected, a field's own name included when it has
an alias. A field without a default must be present, even when its type allows null. `null` is
accepted only where the type includes `None`. A field left out takes its dataclass default and
nothing else. A `Literal` matches by value and type, so `true` is not `1`, while an integer passes
as a float. A union takes a value exactly one member accepts, or that every member accepting it
builds alike. Every problem is reported with the JSON path where it is, up to `PROBLEMS_MAX`.
"""

import dataclasses
import inspect
import json
import math
import re
import sys
import types
import typing

__all__ = ["PROBLEMS_MAX", "DecodeError", "Problem", "Schema", "alias", "declare", "doc"]

PROBLEMS_MAX = 20  # problems one error lists; the rest are counted

_SUBSET = (
    "str, bool, int, float, None, Literal of str, int and bool values, list[T], dict[str, T], "
    "unions of these, Annotated around any of these, and dataclasses whose fields are all of these"
)

_UNIONS = (typing.Union, types.UnionType)
_SCALARS = (str, bool, int, float)

# A field's key as it reads after a dot in a path; any other key reads in brackets.
_PLAIN_KEY = re.compile(r"[A-Za-z_][A-Za-z0-9_-]*")

# What a node gives back for a value that has problems, so `None` can be a value.
_BAD = object()

_type_name = type.__dict__["__name__"].__get__
_type_qualname = type.__dict__["__qualname__"].__get__


# ---------------------------------------------------------------------------- markers


class _Doc:
    __slots__ = ("text",)

    def __init__(self, text):
        self.text = text

    def __repr__(self):
        return f"doc({self.text!r})"


class _Alias:
    __slots__ = ("name",)

    def __init__(self, name):
        self.name = name

    def __repr__(self):
        return f"alias({self.name!r})"


def doc(text):
    """A description for `Annotated`: `Annotated[str, doc("What it is.")]`.

    On a dataclass field it is the field's description; around any other type, that type's. The
    text reaches the schema verbatim.
    """
    if type(text) is not str:
        raise TypeError(f"doc() takes a str, not {_type_name(type(text))}")
    return _Doc(text)


def alias(name):
    """A field's JSON key, for the field's own `Annotated`: `Annotated[str, alias("next-step")]`.

    Without one, the key is the field's name. The field's name is then not a key at all: decoding
    rejects it as unknown.
    """
    if type(name) is not str or not name:
        raise TypeError(f"alias() takes a non-empty str, not {name!r}")
    return _Alias(name)


# ---------------------------------------------------------------------------- problems


class Problem:
    """One thing wrong with a value: `path`, where it is, as in `$.notes['retry-loop'].severity`,
    and `message`, what is wrong and what was expected."""

    __slots__ = ("path", "message")

    def __init__(self, path, message):
        self.path = path
        self.message = message

    def __repr__(self):
        return f"Problem(path={self.path!r}, message={self.message!r})"

    def __str__(self):
        return f"{self.path}: {self.message}"


class DecodeError(ValueError):
    """A value that does not decode into its declared type.

    `problems` is a tuple of `Problem`, at most `PROBLEMS_MAX` of them, and `more` counts those
    past it. The message lists them.
    """

    def __init__(self, message, problems=(), more=0):
        super().__init__(message)
        self.problems = tuple(problems)
        self.more = more


class _Problems:
    """Problems found so far, bounded: past `PROBLEMS_MAX`, only counted."""

    __slots__ = ("found", "more")

    def __init__(self):
        self.found = []
        self.more = 0

    def __bool__(self):
        return bool(self.found or self.more)

    def add(self, path, message):
        if len(self.found) < PROBLEMS_MAX:
            self.found.append(Problem(path, message))
        else:
            self.more += 1

    def take(self, other, prefix=""):
        for problem in other.found:
            self.add(problem.path, prefix + problem.message)
        self.more += other.more

    def listing(self):
        """Each problem on a line of its own, indented, with the count of the rest."""
        lines = [f"  {problem}" for problem in self.found]
        if self.more:
            lines.append(f"  ... and {self.more} more")
        return "\n".join(lines)

    def count(self):
        n = len(self.found) + self.more
        return f"{n} problem" if n == 1 else f"{n} problems"


def _field_path(path, key):
    if type(key) is str and _PLAIN_KEY.fullmatch(key):
        return f"{path}.{key}"
    return f"{path}[{key!r}]"


def _what(value):
    """A JSON value as a problem names it, without quoting anything long."""
    if value is None:
        return "null"
    t = type(value)
    if t is bool:
        return "true" if value else "false"
    if t is str:
        return "a string"
    if t is int:
        return "an integer"
    if t is float:
        return "a number"
    if t is list:
        return "an array"
    if t is dict:
        return "an object"
    return f"a {_type_qualname(t)}"


# Each JSON value's kind, by its Python type.
_KINDS = {
    type(None): "null",
    bool: "boolean",
    str: "string",
    int: "number",
    float: "number",
    list: "array",
    dict: "object",
}


def _json_kind(value):
    return _KINDS.get(type(value))


def _shown(value):
    """A `Literal`'s value, or one compared against it, as JSON spells it."""
    if value is None or type(value) in _SCALARS:
        text = json.dumps(value, ensure_ascii=False)
        return text if len(text) <= 80 else f"{text[:77]}..."
    return _what(value)


# ---------------------------------------------------------------------------- nodes
#
# A declared type compiles to a graph of nodes, once. Each decodes a JSON value, encodes an
# instance, says what it expects, and renders its JSON Schema; a dataclass that refers to itself
# is one node its fields lead back to.


def _not_json(node, value, path, problems):
    """A JSON value of the wrong kind for `node`, in JSON's words."""
    problems.add(path, f"expected {node.expected()}, not {_what(value)}")
    return _BAD


def _not_type(node, value, path, problems):
    """An instance of the wrong type for `node`, in Python's words."""
    problems.add(path, f"expected {node.describe()}, not {_type_qualname(type(value))}")
    return _BAD


class _Scalar:
    __slots__ = ("t", "types", "phrase", "schema")

    # What each expects, in a problem and in JSON Schema.
    _FORMS = {
        str: ("a string", "string"),
        bool: ("true or false", "boolean"),
        int: ("an integer", "integer"),
        float: ("a number", "number"),
    }

    def __init__(self, t):
        self.t = t
        # An integer passes as a float, as in Python's own annotations.
        self.types = (int, float) if t is float else (t,)
        self.phrase, self.schema = self._FORMS[t]

    def describe(self):
        return _type_name(self.t)

    def expected(self):
        return self.phrase

    def kinds(self):
        return {_KINDS[self.t]}

    def fits(self, value):
        return type(value) in self.types

    def decode(self, value, path, problems):
        if not self.fits(value):
            return _not_json(self, value, path, problems)
        if type(value) is float and not math.isfinite(value):
            problems.add(path, "expected a finite number")
            return _BAD
        return value

    def encode(self, value, path, problems):
        if not self.fits(value):
            return _not_type(self, value, path, problems)
        if type(value) is float and not math.isfinite(value):
            problems.add(path, f"expected a finite float, not {value!r}")
            return _BAD
        return value

    def render(self, renderer):
        return {"type": self.schema}


class _Null:
    __slots__ = ()

    def describe(self):
        return "None"

    def expected(self):
        return "null"

    def kinds(self):
        return {"null"}

    def fits(self, value):
        return value is None

    def decode(self, value, path, problems):
        return None if value is None else _not_json(self, value, path, problems)

    def encode(self, value, path, problems):
        return None if value is None else _not_type(self, value, path, problems)

    def render(self, renderer):
        return {"type": "null"}


class _Literal:
    __slots__ = ("values",)

    def __init__(self, values):
        self.values = values

    def describe(self):
        return f"Literal[{', '.join(map(repr, self.values))}]"

    def expected(self):
        return f"one of {', '.join(map(_shown, self.values))}"

    def kinds(self):
        return {_json_kind(value) for value in self.values}

    def fits(self, value):
        return any(type(value) is type(v) for v in self.values)

    def _matches(self, value):
        return any(type(value) is type(v) and value == v for v in self.values)

    def decode(self, value, path, problems):
        if not self._matches(value):
            problems.add(path, f"expected {self.expected()}, not {_shown(value)}")
            return _BAD
        return value

    def encode(self, value, path, problems):
        if not self._matches(value):
            problems.add(path, f"expected {self.describe()}, not {value!r:.80}")
            return _BAD
        return value

    def render(self, renderer):
        return {"enum": list(self.values)}


class _List:
    __slots__ = ("item",)

    def __init__(self, item):
        self.item = item

    def describe(self):
        return f"list[{self.item.describe()}]"

    def expected(self):
        return "an array"

    def kinds(self):
        return {"array"}

    def fits(self, value):
        return type(value) is list

    def _each(self, value, path, problems, step):
        items, bad = [], False
        for i, item in enumerate(value):
            item = step(item, f"{path}[{i}]", problems)
            bad = bad or item is _BAD
            items.append(item)
        return _BAD if bad else items

    def decode(self, value, path, problems):
        if not self.fits(value):
            return _not_json(self, value, path, problems)
        return self._each(value, path, problems, self.item.decode)

    def encode(self, value, path, problems):
        if not self.fits(value):
            return _not_type(self, value, path, problems)
        return self._each(value, path, problems, self.item.encode)

    def render(self, renderer):
        return {"type": "array", "items": renderer.render(self.item)}


class _Dict:
    __slots__ = ("value",)

    def __init__(self, value):
        self.value = value

    def describe(self):
        return f"dict[str, {self.value.describe()}]"

    def expected(self):
        return "an object"

    def kinds(self):
        return {"object"}

    def fits(self, value):
        return type(value) is dict

    def _each(self, value, path, problems, step):
        out, bad = {}, False
        for key, item in value.items():
            at = f"{path}[{key!r}]"
            if type(key) is not str:
                problems.add(at, f"expected a string key, not {_type_qualname(type(key))}")
                bad = True
                continue
            item = step(item, at, problems)
            if item is _BAD:
                bad = True
            else:
                out[key] = item
        return _BAD if bad else out

    def decode(self, value, path, problems):
        if not self.fits(value):
            return _not_json(self, value, path, problems)
        return self._each(value, path, problems, self.value.decode)

    def encode(self, value, path, problems):
        if not self.fits(value):
            return _not_type(self, value, path, problems)
        return self._each(value, path, problems, self.value.encode)

    def render(self, renderer):
        return {"type": "object", "additionalProperties": renderer.render(self.value)}


class _Union:
    __slots__ = ("members",)

    def __init__(self, members):
        self.members = members

    def describe(self):
        return " | ".join(member.describe() for member in self.members)

    def expected(self):
        return " or ".join(member.expected() for member in self.members)

    def kinds(self):
        return set().union(*(member.kinds() for member in self.members))

    def fits(self, value):
        return any(member.fits(value) for member in self.members)

    def decode(self, value, path, problems):
        accepted, refused = [], []
        for member in self.members:
            trial = _Problems()
            result = member.decode(value, path, trial)
            if result is _BAD:
                refused.append((member, trial))
            else:
                accepted.append((member, result))
        if len(accepted) == 1:
            # Taken as built: a dataclass's own `__eq__` is the agent's code, and nothing here
            # needs it.
            return accepted[0][1]
        if accepted:
            first = accepted[0][1]
            # Members that build the same value -- `3` for `int | float` -- leave nothing to choose.
            if all(type(result) is type(first) and result == first for _, result in accepted[1:]):
                return first
            names = ", ".join(member.describe() for member, _ in accepted)
            problems.add(path, f"is ambiguous: it decodes as each of {names}")
            return _BAD
        kind = _json_kind(value)
        near = [(m, t) for m, t in refused if kind in m.kinds()]
        self._refused(path, problems, near, f"expected {self.expected()}, not {_what(value)}")
        return _BAD

    def encode(self, value, path, problems):
        refused = []
        for member in self.members:
            trial = _Problems()
            result = member.encode(value, path, trial)
            if result is not _BAD:
                return result
            refused.append((member, trial))
        near = [(m, t) for m, t in refused if m.fits(value)]
        found = _type_qualname(type(value))
        self._refused(path, problems, near, f"expected {self.describe()}, not {found}")
        return _BAD

    def _refused(self, path, problems, near, otherwise):
        """Say why no member took a value: in the words of the one member of its kind, if one
        is, since that is the member it was meant for; else `otherwise`."""
        if len(near) == 1:
            problems.take(near[0][1])
            return
        if near:
            problems.add(path, f"matches none of {self.describe()}")
            for member, trial in near:
                problems.take(trial, prefix=f"as {member.describe()}: ")
            return
        problems.add(path, otherwise)

    def render(self, renderer):
        return {"anyOf": [renderer.render(member) for member in self.members]}


class _Described:
    """A type in `Annotated` with a `doc()`, anywhere but a field's own annotation."""

    __slots__ = ("inner", "text")

    def __init__(self, inner, text):
        self.inner = inner
        self.text = text

    def describe(self):
        return self.inner.describe()

    def expected(self):
        return self.inner.expected()

    def kinds(self):
        return self.inner.kinds()

    def fits(self, value):
        return self.inner.fits(value)

    def decode(self, value, path, problems):
        return self.inner.decode(value, path, problems)

    def encode(self, value, path, problems):
        return self.inner.encode(value, path, problems)

    def render(self, renderer):
        return {"description": self.text, **renderer.render(self.inner)}


class _Field:
    __slots__ = ("name", "key", "node", "text", "required", "step")

    def __init__(self, name, key, node, text, required):
        self.name = name
        self.key = key
        self.node = node
        self.text = text
        self.required = required
        # What the field adds to a path, worked out once rather than for every value.
        self.step = _field_path("", key)


class _Class:
    """A dataclass: an object whose keys are its fields', built by calling the class."""

    __slots__ = ("cls", "fields", "by_key", "renamed")

    def __init__(self, cls):
        self.cls = cls
        self.fields = ()
        self.by_key = {}
        # An aliased field's own name, which a value might use by mistake, to the key it has.
        self.renamed = {}

    def describe(self):
        return _type_qualname(self.cls)

    def expected(self):
        return f"an object ({self.describe()})"

    def kinds(self):
        return {"object"}

    def fits(self, value):
        return type(value) is self.cls

    def decode(self, value, path, problems):
        if type(value) is not dict:
            return _not_json(self, value, path, problems)
        values, bad = {}, False
        for key, item in value.items():
            field = self.by_key.get(key) if type(key) is str else None
            if field is None:
                renamed = self.renamed.get(key) if type(key) is str else None
                hint = f"; that field's key is {_shown(renamed)}" if renamed else ""
                problems.add(_field_path(path, key), f"unknown key{hint}")
                bad = True
                continue
            item = field.node.decode(item, path + field.step, problems)
            if item is _BAD:
                bad = True
            else:
                values[field.name] = item
        for field in self.fields:
            if field.required and field.key not in value:
                problems.add(path + field.step, "missing required key")
                bad = True
        if bad:
            return _BAD
        # The class's own constructor, so a field left out takes the default it declares, and a
        # `default_factory` makes each instance its own.
        try:
            return self.cls(**values)
        except Exception as e:
            problems.add(path, f"{self.describe()} could not be built: {_type_name(type(e))}: {e}")
            return _BAD

    def encode(self, value, path, problems):
        if not self.fits(value):
            return _not_type(self, value, path, problems)
        out, bad = {}, False
        for field in self.fields:
            item = field.node.encode(getattr(value, field.name), path + field.step, problems)
            if item is _BAD:
                bad = True
            else:
                out[field.key] = item
        return _BAD if bad else out

    def render(self, renderer):
        return renderer.reference(self)

    def body(self, renderer):
        properties = {}
        for field in self.fields:
            schema = renderer.render(field.node)
            properties[field.key] = {"description": field.text, **schema} if field.text else schema
        schema = {"type": "object", "properties": properties}
        required = [field.key for field in self.fields if field.required]
        if required:
            schema["required"] = required
        schema["additionalProperties"] = False
        return schema


class _Renderer:
    """One rendering's `$defs`: every dataclass but the root, once each, under its name."""

    def __init__(self, root):
        self.root = root
        self.defs = {}
        self.names = {}

    def render(self, node):
        return node.render(self)

    def reference(self, node):
        if node is self.root:
            return {"$ref": "#"}
        name = self.names.get(node)
        if name is None:
            base = _type_name(node.cls)
            name, n = base, 1
            while name in self.defs:
                n += 1
                name = f"{base}-{n}"
            self.names[node] = name
            # Claimed before its body renders, so a class that refers to itself finds it.
            self.defs[name] = None
            self.defs[name] = node.body(self)
        pointer = name.replace("~", "~0").replace("/", "~1")
        return {"$ref": f"#/$defs/{pointer}"}


# ---------------------------------------------------------------------------- declaring


def _describe(t):
    """A type as a declaration error names it: `bytes`, `set[int]`, `Row`."""
    if t is type(None):
        return "None"
    if typing.get_origin(t) is None and isinstance(t, type):
        return _type_qualname(t)
    return repr(t).removeprefix("typing.")


class _Compiler:
    """Checks a type and builds its nodes. `where` begins every error, which names the type
    outside the set and the field it is in."""

    def __init__(self, where):
        self.where = where
        self.classes = {}

    def outside(self, t, inside, why=f"which is outside the serializable subset: {_SUBSET}"):
        return TypeError(f"{self.where} {_describe(t)}{inside}, {why}")

    def node(self, t, inside):
        if t is None or t is type(None):
            return _Null()
        if isinstance(t, type) and t in _SCALARS:
            return _Scalar(t)
        origin, args = typing.get_origin(t), typing.get_args(t)
        if origin is typing.Annotated:
            base, texts, aliases = _markers(t)
            if aliases:
                raise self.outside(t, inside, "but alias() names a field's key, and is only "
                                   "allowed in the annotation of a dataclass field itself")
            if len(texts) > 1:
                raise self.outside(t, inside, "which has more than one doc()")
            node = self.node(base, inside)
            return _Described(node, texts[0]) if texts else node
        if origin is typing.Literal:
            if not all(type(value) in (str, int, bool) for value in args):
                note = "; for null, write Literal[...] | None" if None in args else ""
                raise self.outside(t, inside, f"but a Literal takes only str, int and bool "
                                   f"values{note}")
            return _Literal(args)
        if origin in _UNIONS:
            return _Union([self.node(member, inside) for member in args])
        if origin is list and len(args) == 1:
            return _List(self.node(args[0], inside))
        if origin is dict and len(args) == 2 and args[0] is str:
            return _Dict(self.node(args[1], inside))
        if origin is None and isinstance(t, type) and dataclasses.is_dataclass(t):
            return self.dataclass(t)
        raise self.outside(t, inside)

    def dataclass(self, cls):
        node = self.classes.get(cls)
        if node is not None:
            return node
        # Recorded before its fields compile, so a class that refers to itself is checked once.
        node = self.classes[cls] = _Class(cls)
        hints = self.hints(cls)
        fields, by_key = [], {}
        for f in dataclasses.fields(cls):
            inside = f" (field {f.name!r} of {_type_qualname(cls)})"
            if not f.init:
                raise self.outside(cls, inside, "which is init=False, so decoding cannot set it")
            hint = hints.get(f.name, f.type)
            base, texts, aliases = _markers(hint)
            if len(texts) > 1 or len(aliases) > 1:
                marker = "doc()" if len(texts) > 1 else "alias()"
                raise self.outside(hint, inside, f"which has more than one {marker}")
            key = aliases[0].name if aliases else f.name
            if key in by_key:
                why = f"whose JSON key {key!r} is already field {by_key[key].name!r}'s"
                raise self.outside(cls, inside, why)
            required = f.default is dataclasses.MISSING and f.default_factory is dataclasses.MISSING
            field = _Field(f.name, key, self.node(base, inside), texts[0] if texts else None,
                           required)
            fields.append(field)
            by_key[key] = field
        node.fields = tuple(fields)
        node.by_key = by_key
        node.renamed = {
            field.name: field.key
            for field in fields
            if field.key != field.name and field.name not in by_key
        }
        return node

    def hints(self, cls):
        """`cls`'s field hints, resolved in the module each was declared in, markers kept."""
        try:
            return typing.get_type_hints(cls, include_extras=True)
        except Exception as e:
            raise TypeError(
                f"{self.where} {_type_qualname(cls)}, whose field types cannot be resolved: "
                f"{_type_name(type(e))}: {e}{_unresolved(cls)}"
            ) from None


def _markers(hint):
    """`hint` without `Annotated`, and the `doc()` texts and `alias()`es it carried."""
    if typing.get_origin(hint) is not typing.Annotated:
        return hint, [], []
    meta = hint.__metadata__
    texts = [m.text for m in meta if isinstance(m, _Doc)]
    aliases = [m for m in meta if isinstance(m, _Alias)]
    return hint.__origin__, texts, aliases


def _unresolved(cls):
    """` (field 'x')` for the first of `cls`'s fields whose hint does not resolve on its own, so
    the error names it; empty if each does."""
    for f in dataclasses.fields(cls):
        owner = next(
            (base for base in cls.__mro__ if f.name in inspect.get_annotations(base)), None
        )
        if owner is None:
            continue
        module = sys.modules.get(owner.__module__)
        probe = types.SimpleNamespace(__annotations__={f.name: inspect.get_annotations(owner)[f.name]})
        try:
            typing.get_type_hints(
                probe,
                globalns=getattr(module, "__dict__", {}),
                localns=dict(vars(owner)),
                include_extras=True,
            )
        except Exception:
            return f" (field {f.name!r})"
    return ""


def _contract(members, where):
    """One `Schema` over every type in `members`, as their union, which takes nothing when there
    are none: what one direction of a channel admits. Each is checked as `declare` checks it, with
    `where` beginning any error."""
    compiler = _Compiler(where)
    nodes = [compiler.node(member, "") for member in members]
    return Schema(nodes[0] if len(nodes) == 1 else _Union(nodes))


def declare(t):
    """`t`'s `Schema`, checking `t` now.

    Raises `TypeError` for a type outside the set this module takes -- naming the field it is in
    -- and for a field hint that does not resolve in its module.
    """
    return Schema(_Compiler("outrig.schema cannot take").node(t, ""))


class Schema:
    """A declared type: `decode` a JSON value into it, `encode` an instance into JSON, and
    `json_schema()` or `text()` for its JSON Schema. Made by `declare`."""

    __slots__ = ("_node",)

    def __init__(self, node):
        self._node = node

    def __repr__(self):
        return f"<schema of {self._node.describe()}>"

    def decode(self, value):
        """`value`, a parsed JSON value, as a new instance of the declared type.

        Raises `DecodeError` listing every problem, each with its JSON path. Nothing in `value` is
        kept: containers are new, and a dataclass is built by calling it.
        """
        problems = _Problems()
        result = self._node.decode(value, "$", problems)
        if problems:
            raise DecodeError(
                f"{problems.count()} decoding {self._node.describe()}:\n{problems.listing()}",
                problems.found,
                problems.more,
            )
        return result

    def encode(self, value):
        """`value`, an instance of the declared type, as JSON: what `decode` takes back.

        A dataclass becomes an object under its fields' keys. Raises `TypeError` listing every
        place `value` is not of the declared type.
        """
        result, problems = self._encoding(value)
        if problems:
            raise TypeError(
                f"{problems.count()} encoding {self._node.describe()}:\n{problems.listing()}"
            )
        return result

    def _encoding(self, value):
        """`(json, problems)`: what `encode` gives and finds, without raising."""
        problems = _Problems()
        return self._node.encode(value, "$", problems), problems

    def json_schema(self):
        """The JSON Schema of the declared type, as a new dict.

        A dataclass is an object listing its fields under their keys, each with its `doc()` text
        as `description`, requiring each without a default, and taking no other key. Dataclasses
        other than the declared one are in `$defs`.
        """
        renderer = _Renderer(self._node if isinstance(self._node, _Class) else None)
        root = self._node.body(renderer) if renderer.root else renderer.render(self._node)
        if renderer.defs:
            root["$defs"] = renderer.defs
        return root

    def text(self):
        """`json_schema()` as indented JSON text, every `doc()` text in it verbatim."""
        return json.dumps(self.json_schema(), indent=2, ensure_ascii=False)
