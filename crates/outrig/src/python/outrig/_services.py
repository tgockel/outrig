"""Services: what the session's owner serves by name from Rust, each reached in an agent's kernel as
an object of a class generated from the service's description.

A service describes its methods, each by an exact name and a JSON Schema for its arguments. The
interpreter makes a class of each description -- a subclass of `outrig.Service`, one method per
described one, its signature and docstring rendered from the schema -- and gives each kernel an
object of it. A call is made by value: the arguments cross to the host as JSON, the host checks
them against the schema, and the result comes back as JSON data. Nothing of the service is in this
process.
"""

import collections
import inspect
import itertools
import json
import keyword
import types
import typing

# The interpreter's, bound as it loads the package: `_call(obj, method, arguments, timeout,
# generation)` calls `method`, by its exact name, of the service `obj` stands for, from the calling
# kernel, and returns its result or raises `ServiceError`. On the host it stays `None`.
_call = None

# How much of a schema a docstring shows of a method whose signature it cannot render, in
# characters.
SCHEMA_SHOWN_MAX = 2000

_SELF = inspect.Parameter("self", inspect.Parameter.POSITIONAL_OR_KEYWORD)
# The receiver of a method whose arguments are taken whole, which may hold one named `self`.
_SELF_ONLY = inspect.Parameter("self", inspect.Parameter.POSITIONAL_ONLY)
# How deep the renderer follows a schema -- `$ref`s, unions, `items` -- before it leaves the type
# unspelled: deep enough for any signature a model reads, and far from Python's recursion limit.
_DEPTH_MAX = 32
# How many schemas the renderer visits for one type, a `$ref` counted each time it is met, before
# it leaves the type unspelled: shared definitions branching to one another are as many paths as
# their product, and the renderer runs on the reader of the host's lines.
_VISITS_MAX = 256


class ServiceError(Exception):
    """A call of a service's method that returned no result, and why.

    `outcome` says how it ended, as the session's events do:

    - `refused`: the service was not called, or its result was not handed over. `category` says
      why: `admission_closed`, the session is closing; `unknown_method`, the service's description
      has no method of that name, which a method held from an earlier description meets once the
      service has changed; `invalid_arguments`, the arguments do not match the method's schema,
      `path` the first JSON path that failed; `result_too_large`, the service returned more than a
      result may carry, so what it did was done.
    - `raised`: the service was called and said it failed. `category` is its: `invalid_arguments`,
      `not_found`, `refused`, `unavailable`, `failed`, `cancelled`, `other`.
    - `cancelled`: the service was not called; `reason` says what stopped it.
    - `unknown`: the service was called and abandoned -- told to stop, given a grace, and left --
      so what it did is not known; `reason` is `deadline`, `interrupted` or `closed`. Nothing
      calls it again for you.

    `service` and `method` name what was called, `call` is the id the session's events give the
    call, and `message` says what went wrong in words.
    """

    __module__ = "outrig"

    def __init__(self, message, *, outcome, service, method, call=None, category=None,
                 reason=None, path=None):
        super().__init__(message)
        self.message = message
        self.outcome = outcome
        self.service = service
        self.method = method
        self.call = call
        self.category = category
        self.reason = reason
        self.path = path

    def __str__(self):
        why = self.category or self.reason
        how = f"{self.outcome}, {why}" if why else self.outcome
        return f"{self.service}.{self.method} ({how}): {self.message}"

    def __reduce__(self):
        return _rebuilt, (type(self), self.message, self._fields())

    def _fields(self):
        return {name: getattr(self, name) for name in
                ("outcome", "service", "method", "call", "category", "reason", "path")}


def _rebuilt(cls, message, fields):
    return cls(message, **fields)


class ServiceMethod(collections.namedtuple(
        "ServiceMethod", ("name", "description", "params", "result", "attribute"))):
    """One method a service describes: its exact `name`, its `description`, the JSON Schema of its
    arguments (`params`) and of its result (`result`, or `None`), and the attribute it is reached
    by on the service's object, or `None` where no attribute stands for it."""

    __module__ = "outrig"
    __slots__ = ()


class Service:
    """One of the session's services: an object whose methods are calls to the host, made by value
    -- the arguments and the result are JSON data -- and attributed to the agent whose kernel the
    object is.

    Each method it describes is a method of this object, with a signature and a docstring from
    its schema, where its name can be an attribute: `-` becomes `_`, and a keyword or a name this
    class already uses gains a trailing `_`. Two names that would meet one attribute leave it to
    neither, and reading it raises, naming both. Every method is reachable by its exact name:
    `call(name, arguments)` calls it, `method(name)` gives it, and `methods` lists them all.

    A call blocks its thread until the host answers, and holds the agent's event loop when made
    on it: `await asyncio.to_thread(lambda: svc.method(x))` keeps the loop turning. A method that
    starts long work returns a ticket at once, and its completion arrives on a channel. A call
    that does not return raises `outrig.ServiceError`, saying how it ended.

    `outrig.bindings[name]` is the calling agent's own object of the service `name`.
    """

    __module__ = "outrig"
    __slots__ = ("_outrig",)

    # What each generated class says of its service: set as it is made.
    _service = None
    _description = ""
    _generation = 0
    _methods = types.MappingProxyType({})
    _functions = types.MappingProxyType({})

    def __new__(cls, *args, **kwargs):
        raise TypeError("a service's object is OutRig's to make, one for each agent: reach it as "
                        "outrig.bindings[name]")

    def call(self, name, arguments=None, *, timeout=None):
        """Call the method named exactly `name` with `arguments`, a dict of JSON data, and return
        its result. `timeout` -- seconds -- shortens the call's deadline; it never lengthens it,
        and one past the deadline raises `ValueError` before anything is sent."""
        return _call(self, name, arguments, timeout, type(self)._generation)

    def method(self, name):
        """The method named exactly `name`, bound to this object: what `call(name, ...)` calls,
        with its signature and its docstring."""
        function = type(self)._functions.get(name)
        if function is None:
            raise AttributeError(f"service {type(self)._service!r} has no method {name!r}: "
                                 f"its `methods` lists those it has")
        return types.MethodType(function, self)

    @property
    def methods(self):
        """Each method the service describes, by its exact name, as a `ServiceMethod`."""
        return type(self)._methods

    def __dir__(self):
        attributes = (method.attribute for method in type(self)._methods.values())
        return sorted({"call", "method", "methods", *filter(None, attributes)})

    def __repr__(self):
        cls = type(self)
        count = len(cls._methods)
        return (f"<outrig service {cls._service!r}: {_line(cls._description)} "
                f"({count} method{'' if count == 1 else 's'})>")


# Each presenter -- the class a service's object is made from -- by the kind its description
# names. A kind OutRig does not know is presented by the generic one.
_PRESENTERS = {"service": Service}


def _made(cls, name, kernel):
    """An object of `cls`, the class of service `name`, for `kernel`."""
    obj = object.__new__(cls)
    Service._outrig.__set__(obj, (name, kernel))
    return obj


def _state(obj):
    """`(name, kernel)` of `obj`, a service's object, read without running its class's code."""
    return Service._outrig.__get__(obj)


class _Ambiguous:
    """An attribute two methods' names would both take: reading it raises, naming them."""

    __slots__ = ("_text",)

    def __init__(self, service, attribute, names):
        listed = " and ".join(repr(name) for name in names)
        self._text = (f"{attribute!r} of service {service!r} could be its method {listed}: reach "
                      f"one by its exact name, as .method({names[0]!r})")

    def __get__(self, obj, owner=None):
        if obj is None:
            return self
        raise AttributeError(self._text)

    def __repr__(self):
        return f"<ambiguous: {self._text}>"


def generated(entry):
    """The class of the service `entry` describes, as the host said it: `name`, `description`,
    `presenter`, `generation` and `methods`, each a method's `name`, `description`, `params` and
    `result`."""
    name = entry["name"]
    generation = entry["generation"]
    base = _PRESENTERS.get(entry.get("presenter"), Service)
    taken = set(dir(base))
    described = {}  # each method by its exact name, the first described under it
    for method in entry["methods"]:
        if isinstance(method["name"], str):
            described.setdefault(method["name"], method)
    by_attribute = collections.defaultdict(list)
    for exact in described:
        attribute = _attribute(exact, taken)
        if attribute is not None:
            by_attribute[attribute].append(exact)
    # The attribute standing for each method that one names alone: the convenience, where nothing
    # is ambiguous. An exact name another method's name also became is ambiguous too.
    attribute_of = {names[0]: attr for attr, names in by_attribute.items() if len(names) == 1}
    namespace = {
        "__module__": "outrig.services",
        "__qualname__": name,
        "__slots__": (),
        "__doc__": _class_doc(entry, described, attribute_of),
        "_service": name,
        "_description": entry.get("description") or "",
        "_generation": generation,
    }
    methods = {}
    functions = {}
    for exact, method in described.items():
        attribute = attribute_of.get(exact)
        function = _function(name, method, generation, attribute)
        functions[exact] = function
        methods[exact] = ServiceMethod(exact, method.get("description") or "", method.get("params"),
                                       method.get("result"), attribute)
    for attribute, names in by_attribute.items():
        if len(names) == 1:
            namespace[attribute] = functions[names[0]]
        else:
            namespace[attribute] = _Ambiguous(name, attribute, names)
    namespace["_methods"] = types.MappingProxyType(methods)
    namespace["_functions"] = types.MappingProxyType(functions)
    return type(name, (base,), namespace)


def _attribute(exact, taken):
    """The attribute that stands for the method named `exact`, or `None` where none can: `-` as
    `_`, and a trailing `_` for a keyword or a name `taken` already. A dunder-shaped name is
    Python's, and stands for no method."""
    attribute = exact.replace("-", "_")
    if not attribute.isidentifier() or (attribute.startswith("__") and attribute.endswith("__")):
        return None
    if keyword.iskeyword(attribute) or attribute in taken:
        attribute += "_"
    return attribute


def _function(service, method, generation, attribute):
    """The function that calls `method`, of `service`'s description at `generation`: its
    signature and docstring rendered from its schema where they can be, and `(**arguments)` with
    the schema shown where they cannot."""
    exact = method["name"]
    rendered = _rendered(method.get("params"))
    if rendered is None:
        def function(self, /, **arguments):
            return _call(self, exact, arguments, None, generation)

        signature = inspect.Signature(
            [_SELF_ONLY, inspect.Parameter("arguments", inspect.Parameter.VAR_KEYWORD)])
        notes = None
    else:
        signature, notes = rendered

        def function(self, *args, **kwargs):
            # What the caller passed, and nothing else: a parameter left out is left out.
            bound = signature.bind(self, *args, **kwargs).arguments
            del bound["self"]
            return _call(self, exact, dict(bound), None, generation)

    shown = attribute or exact
    function.__name__ = shown
    function.__qualname__ = f"{service}.{shown}"
    function.__module__ = "outrig.services"
    function.__signature__ = signature
    function.__doc__ = _method_doc(method, notes, attribute)
    return function


def _method_doc(method, notes, attribute):
    """`method`'s docstring: its description, then each argument's note -- or, for one whose
    signature could not be rendered, its schema -- then how its exact name reaches it."""
    exact = method["name"]
    parts = [(method.get("description") or "").strip() or f"The service's method {exact!r}."]
    if notes is None:
        schema = json.dumps(method.get("params"), indent=2, ensure_ascii=False, default=str)
        if len(schema) > SCHEMA_SHOWN_MAX:
            schema = (schema[:SCHEMA_SHOWN_MAX]
                      + f"... [{len(schema) - SCHEMA_SHOWN_MAX} more characters]")
        parts.append("Called with keyword arguments, which the host checks against this "
                     f"schema:\n{schema}")
    elif notes:
        parts.append("Arguments:\n" + "\n".join(f"  {note}" for note in notes))
    result = method.get("result")
    if isinstance(result, dict):
        said = result.get("description") or result.get("title")
        kind = _shown(_annotation(result, result))
        parts.append(f"Returns {kind}" + (f": {said}" if said else "."))
    if attribute != exact:
        parts.append(f"Its exact name is {exact!r}: .call({exact!r}, arguments) and "
                     f".method({exact!r}) reach it.")
    return "\n\n".join(parts)


def _class_doc(entry, described, attribute_of):
    """The docstring of `entry`'s class: what the service is, and how its methods are reached."""
    lines = [(entry.get("description") or "").strip() or f"The service {entry['name']!r}.", ""]
    lines.append("A service the host runs: each method is a call to the host, by value, which "
                 "blocks until it answers. `call(name, arguments)` calls any method by its exact "
                 "name, `method(name)` gives one, and `methods` lists them. A call that returns "
                 "no result raises outrig.ServiceError.")
    unreached = [exact for exact in described if exact not in attribute_of]
    if unreached:
        lines.append("")
        lines.append("Reached by exact name only: " + ", ".join(repr(name) for name in unreached))
    return "\n".join(lines)


# What the top of a schema may hold for its signature to be rendered: an object, its properties
# and what describes it.
_TOP = frozenset({"type", "properties", "required", "description", "title", "$defs", "definitions",
                  "additionalProperties", "$schema", "$id", "$comment", "default", "examples"})


class _Unset:
    """The default of an optional argument whose schema gives none: left out of the call."""

    __slots__ = ()

    def __repr__(self):
        return "..."


_UNSET = _Unset()


def _rendered(params):
    """`(signature, notes)` for a method whose arguments match `params`, or `None` where the
    schema is not an object of named properties a signature can hold.

    Required properties are parameters in the schema's order, positional or by keyword; the rest
    are keyword-only, each with its schema's default, or with none and left out of the call when
    not passed. A property whose type the renderer cannot spell is a parameter with no
    annotation."""
    if params is True or params == {}:
        return inspect.Signature([_SELF]), []
    if not isinstance(params, dict) or set(params) - _TOP:
        return None
    if params.get("type", "object") != "object":
        return None
    properties = params.get("properties", {})
    required = params.get("required", [])
    if not isinstance(properties, dict) or not isinstance(required, list):
        return None
    names = list(properties) + [name for name in required if name not in properties]
    if not all(isinstance(name, str) and name.isidentifier() and not keyword.iskeyword(name)
               and name != "self" for name in names):
        return None
    needed, optional, notes = [], [], []
    for name in names:
        schema = properties.get(name, True)
        annotation = _annotation(schema, params)
        described = schema if isinstance(schema, dict) else {}
        if name in required:
            needed.append(inspect.Parameter(name, inspect.Parameter.POSITIONAL_OR_KEYWORD,
                                            annotation=annotation))
        else:
            default = described.get("default", _UNSET)
            optional.append(inspect.Parameter(name, inspect.Parameter.KEYWORD_ONLY,
                                              default=default, annotation=annotation))
        said = described.get("description")
        told = [said.strip()] if isinstance(said, str) and said.strip() else []
        if "default" in described:
            told.append(f"default {described['default']!r}")
        note = f"{name} ({_shown(annotation)}{', required' if name in required else ''})"
        notes.append(f"{note}: {'; '.join(told)}" if told else note)
    return inspect.Signature([_SELF, *needed, *optional]), notes


def _shown(annotation):
    """`annotation` as a signature shows it, or `any` for none."""
    if annotation is inspect.Parameter.empty:
        return "any"
    return inspect.formatannotation(annotation)


_SCALARS = {"string": str, "integer": int, "number": float, "boolean": bool, "null": type(None)}


def _annotation(schema, root, seen=(), depth=0, visits=None):
    """The Python type `schema` describes, for a signature: a scalar, a list or a dict, a
    `Literal` of an enum or a const, a union of `anyOf`, `oneOf` or a list of types, and a `$ref`
    into `root`'s `$defs` or `definitions` followed. `inspect.Parameter.empty` for anything
    else, for what lies more than `_DEPTH_MAX` steps in, and for a type that takes more than
    `_VISITS_MAX` schemas to spell."""
    empty = inspect.Parameter.empty
    visits = itertools.count() if visits is None else visits
    if depth > _DEPTH_MAX or next(visits) >= _VISITS_MAX:
        return empty
    depth += 1
    if schema is True or schema == {}:
        return typing.Any
    if not isinstance(schema, dict):
        return empty
    if "$ref" in schema:
        ref = schema["$ref"]
        if not isinstance(ref, str) or ref in seen:
            return empty
        target = _referenced(ref, root)
        return (empty if target is None
                else _annotation(target, root, (*seen, ref), depth, visits))
    for word in ("enum", "const"):
        if word in schema:
            values = schema[word] if word == "enum" else [schema[word]]
            if not isinstance(values, list) or not values or not all(
                    v is None or isinstance(v, (str, int, float, bool)) for v in values):
                return empty
            return typing.Literal[tuple(values)]
    for word in ("anyOf", "oneOf"):
        if word in schema:
            members = schema[word]
            if not isinstance(members, list) or not members:
                return empty
            return _union([_annotation(member, root, seen, depth, visits) for member in members])
    kind = schema.get("type")
    if isinstance(kind, list):
        return _union([_typed(each, schema, root, seen, depth, visits) for each in kind])
    if isinstance(kind, str):
        return _typed(kind, schema, root, seen, depth, visits)
    return empty


def _typed(kind, schema, root, seen, depth, visits):
    if kind in _SCALARS:
        return _SCALARS[kind]
    if kind == "array":
        items = schema.get("items")
        item = (inspect.Parameter.empty if items is None
                else _annotation(items, root, seen, depth, visits))
        return list if item is inspect.Parameter.empty else list[item]
    if kind == "object":
        return dict
    return inspect.Parameter.empty


def _union(members):
    if not members or any(member is inspect.Parameter.empty for member in members):
        return inspect.Parameter.empty
    return typing.Union[tuple(members)]


def _referenced(ref, root):
    """The schema `ref` points at within `root`, or `None`: only `#/$defs/<name>` and
    `#/definitions/<name>` are followed."""
    for prefix in ("#/$defs/", "#/definitions/"):
        if ref.startswith(prefix):
            defs = root.get(prefix[2:-1])
            key = ref[len(prefix):].replace("~1", "/").replace("~0", "~")
            if isinstance(defs, dict):
                return defs.get(key)
    return None


def _line(text):
    """`text`'s first line, cut to 120 characters, for a `repr`."""
    first = (text or "").strip().splitlines()[0] if (text or "").strip() else ""
    return first if len(first) <= 120 else first[:117] + "..."
