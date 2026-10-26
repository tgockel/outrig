"""The session's skills: know-how the project, the user or the application running the session
supplies, each instructions in a `SKILL.md` and, optionally, Python in a `skill.py`.

`read(name)` returns a skill's instructions and `catalog()` lists every skill. A skill imports as
a package: `review-diff` is `outrig_skills.review_diff`, and its `skill.py` is
`outrig_skills.review_diff.skill`, whose siblings it imports relatively. Its files are fetched from
the host as they are imported or opened, and none is on disk here, so a skill reads its data with
`importlib.resources.files(__package__) / "data" / "ids.txt"`; its modules have no `__file__`.

`invoke(name, text)` runs a skill's entry, its parameters read from `text` by a child agent, and
`invoke(name, params=params(name)(...))` with parameters you give: see `help(outrig.skills.invoke)`.
It returns an `Invocation`; the parser may ask instead, raising `SkillNeedsClarification`.

Importing a skill's package checks the `requires-python` and `dependencies` its `skill.py`
declares, and installs nothing: `pip install` what it lacks, then import it again. Each failure
is a `SkillError` whose `code` says why; what the skill's own code raises is raised as it was.
The package records where it came from as `__outrig_skill__`, an `Origin`; once the skill's files
change, fetching another is refused with `skill-changed`, and `reload(name)` imports it afresh.

`sys.modules` is the interpreter's, so every agent here shares one module per skill, and its
module-level state with it.
"""

import asyncio
import base64
import contextvars
import dataclasses
import functools
import hashlib
import importlib
import importlib.machinery
import importlib.resources.abc
import importlib.util
import inspect
import io
import linecache
import sys
import threading
import time
import types
import typing
import warnings

from . import _agents, _kernel, schema

__all__ = ["SkillError", "catalog", "invoke", "params", "read", "reload"]


class SkillError(ImportError):
    """A skill could not be read, checked, imported or invoked. `skill` names it, and `code` says
    why:

    - `unknown-skill` -- the session has no skill of that name;
    - `skill-changed` -- its files changed after its package was imported: `reload` it;
    - `skill-busy` -- an invocation of it is running, so it is not reloaded;
    - `source-error` -- what serves it could not list or read it;
    - `metadata-invalid`, `duplicate-metadata-block` -- its metadata is not valid;
    - `requires-python`, `unsatisfied-dependency` -- what it declares it needs is not here;
    - `entry-not-found`, `signature-unsupported` -- it has no entry function, or one whose
      parameters cannot be its parameter class;
    - `missing-binding` -- it takes a binding the session does not have.

    What a skill's own code raises while it is imported is raised as it was, never as this.
    """

    def __init__(self, code, skill, message):
        super().__init__(f"{code}: skill {skill!r}: {message}")
        self.code = code
        self.skill = skill
        self.message = message

    def __reduce__(self):
        return type(self), (self.code, self.skill, self.message)


class Origin:
    """Where a skill's imported package came from: the skill's `name`, the `source` serving it,
    and the `digest` of its files when the package was imported."""

    __slots__ = ("name", "source", "digest")

    def __init__(self, name, source, digest):
        self.name, self.source, self.digest = name, source, digest

    def __repr__(self):
        return f"Origin(name={self.name!r}, source={self.source!r}, digest={self.digest!r})"


class Skill:
    """One of the session's skills, as the session found it when it started: its `name` and
    `description`, the `source` serving it, the `package` it imports as, and its `entry` module,
    `None` for a skill with no `skill.py`. Reading it asks the host nothing."""

    __slots__ = ("name", "description", "source", "package", "entry")

    def __init__(self, name, description, source, package, entry):
        self.name, self.description, self.source = name, description, source
        self.package, self.entry = package, entry

    def __repr__(self):
        return f"Skill(name={self.name!r}, source={self.source!r}, package={self.package!r})"


def catalog():
    """Every skill of the session's, by name, each a `Skill`: the preamble lists only so many.
    Read-only."""
    if _ask is None:
        raise RuntimeError(_OUTSIDE)
    return _catalog


def read(name):
    """The instructions of skill `name`: its `SKILL.md` past the frontmatter. Once its package is
    imported, they are read as they were then, or not at all: `skill-changed` says they have
    changed since."""
    _skill(name)
    loader = _loaded.get(name)
    digest = None if loader is None else loader.digest
    return _request(name, "skill.instructions", digest=digest)["body"]


def reload(name):
    """Import skill `name`'s package afresh, and return it: first remove it and each of its
    modules from `sys.modules` -- exactly those, never another skill whose name begins the same.
    Refused with `skill-busy` while an invocation of it runs.

    Code that holds the old modules keeps the old code, and an object of an old class stays one:
    `isinstance` against the class reloaded is false of it."""
    package = _skill(name).package
    # Any kernel's, while that kernel is open: a released one's never ends.
    if any(i.skill == name and not i._kernel.ended for i in list(_running.values())):
        raise SkillError(
            "skill-busy", name, "an invocation of it is running; reload it once that has ended"
        )
    imported = sys.modules.pop(package, None) is not None
    _drop_below(package)
    _loaded.pop(name, None)
    if imported:
        _reloading.add(name)
    try:
        return importlib.import_module(package)
    finally:
        _reloading.discard(name)


def invoke(name, text=None, *, params=None, reload=False):
    """Call skill `name`'s entry, and return its `Invocation` at once: `await` it for what the
    entry returns, or raises.

    With `text`, what the user typed after `/name`, a child agent reads it into the skill's
    parameter class, `params(name)`, and the entry is called with what it read. The child may
    find that the text does not say: then awaiting raises `SkillNeedsClarification`, whose
    `question` is for the user, and nothing is called. With `params`, an instance of that class,
    no child is asked. Each parameter `bind` names gets its binding, whatever your variables hold.

    `reload=True` imports the skill afresh first; otherwise a skill whose files have changed
    since it was imported raises `skill-changed`. What the skill cannot be called with raises a
    `SkillError` here, before anything runs."""
    if _ask is None:
        raise RuntimeError(_OUTSIDE)
    kernel = _kernel._calling()
    if kernel is None:
        raise RuntimeError(
            "outrig.skills.invoke runs in an agent's code, and this runs outside every execution"
        )
    if (text is None) == (params is None):
        raise TypeError("invoke takes the text the user typed, or params=, and not both")
    if text is not None and type(text) is not str:
        raise TypeError(f"invoke's text is a str, not {_type_name(type(text))}")
    if params is not None and reload:
        raise ValueError(
            "params= is an instance of the class a reload replaces: reload with "
            "outrig.skills.reload(name), then construct one of outrig.skills.params(name)"
        )
    _refuse_closing()
    skill, loader, module = _entry_of(name, reload)
    derived = _derived(skill, loader, module)
    if params is not None:
        derived.check(params)
    bound = {}
    for parameter, binding in derived.bind.items():
        stub = kernel.stubs.get(binding)
        if stub is None:
            raise SkillError(
                "missing-binding",
                name,
                f"[tool.outrig.skill] bind gives parameter {parameter!r} the binding {binding!r}, "
                "which this session does not have; runtime.bindings lists the ones it does",
            )
        bound[parameter] = stub
    invocation = Invocation(kernel, derived, bound)
    context = contextvars.copy_context()
    context.run(_within.set, (invocation.id, *_within.get()))
    _running[invocation.id] = invocation
    try:
        kernel.loop.call_soon_threadsafe(invocation._start, context, text, params)
    except RuntimeError:
        # The kernel's loop has closed with it: nothing will run the invocation.
        _running.pop(invocation.id, None)
        raise
    return invocation


def params(name):
    """Skill `name`'s parameter class, `<Name>Params` -- `ReviewDiffParams` for `review-diff`:
    one field per parameter of its entry but those `bind` fills, each with its type, default and
    help. `invoke(name, params=...)` takes an instance; `help()` on it lists the fields."""
    skill, loader, module = _entry_of(name, False)
    return _derived(skill, loader, module).cls


@dataclasses.dataclass(frozen=True, kw_only=True)
class Clarification:
    """What a skill's parser completes with when the text does not say what to call the skill
    with: the `question` to put to the user."""

    question: typing.Annotated[str, schema.doc("The question for the user, in the user's terms.")]


class SkillNeedsClarification(_agents.AgentError):
    """`invoke`'s parser could not tell from the text what to call skill `skill` with, and asks
    `question` instead. Nothing was called: put the question to the user, then invoke again with
    `params=`, or with their new text."""

    def __init__(self, skill, question):
        super().__init__(f"skill {skill!r} asks: {question}")
        self.skill = skill
        self.question = question

    def __reduce__(self):
        return type(self), (self.skill, self.question)


class Invocation:
    """One call of a skill's entry, which `invoke` returns at once, the call under way.

    `await` it for what the entry returns, or raises -- `outrig.RequestCancelled` once
    cancelled -- and awaiting again gives the same. Cancelling the code awaiting it cancels the
    wait, never the invocation; `cancel()` cancels the invocation: its parser released, or the
    entry cancelled at its next `await`. `done()` and `result()` answer without waiting, and
    `future` is a view `asyncio.wait` takes; `runtime.wait` and `asyncio.gather` take this
    itself. `id` is what the session's events name it by, and `skill` the skill's name."""

    __slots__ = ("id", "skill", "_kernel", "_derived", "_bound", "_future", "_view", "_task")

    def __init__(self, kernel, derived, bound):
        self.id = _next_id()
        self.skill = derived.name
        self._kernel, self._derived, self._bound = kernel, derived, bound
        # Of the kernel's loop, and settled once, by `_end`, as the invocation ends.
        self._future = kernel.loop.create_future()
        self._view = None
        self._task = None

    def __await__(self):
        return self._chained().__await__()

    @property
    def future(self):
        if self._view is None or self._view.cancelled():
            self._view = self._chained()
        return self._view

    def done(self):
        return self._future.done()

    def result(self):
        return self._future.result()

    def cancel(self):
        """Cancel the invocation, from any thread; whether it had not ended."""
        if self._future.done():
            return False
        try:
            self._kernel.loop.call_soon_threadsafe(self._cancel)
        except RuntimeError:
            return False  # its kernel's loop has closed
        return True

    def __repr__(self):
        state = "ended" if self.done() else "running"
        return f"<invocation {self.id} of skill {self.skill!r}: {state}>"

    def _cancel(self):
        if self._task is not None and not self._task.done():
            self._task.cancel()

    def _chained(self):
        """A future of the kernel's loop that settles as the invocation ends, and whose
        cancelling cancels nothing else."""
        view = self._kernel.loop.create_future()

        def settle(future):
            if view.done():
                return
            if future.exception() is None:
                view.set_result(future.result())
            else:
                view.set_exception(future.exception())
                view.exception()

        if self._future.done():
            settle(self._future)
        else:
            self._future.add_done_callback(settle)
        return view

    def _start(self, context, text, params):
        """Run it on the kernel's loop, in `context`: the caller's, with this invocation set."""
        self._task = asyncio.Task(
            self._run(text, params), loop=self._kernel.loop, context=context,
            name=f"invocation-{self.id}",
        )
        # A task cancelled before its first step never runs `_run`, whose `finally` ends it.
        self._task.add_done_callback(lambda _: self._end("cancelled", None, None, None))

    async def _run(self, text, params):
        """Parse `text` into the skill's parameters, unless `params` is given, then call the
        entry, telling the host as it is called and as it ends."""
        derived, parse, entered, value, error = self._derived, None, None, None, None
        outcome = "returned"
        try:
            if params is None:
                parse = _agents._call(derived.declaration, {"text": text})
                params = await parse
                if type(params) is Clarification:
                    raise SkillNeedsClarification(self.skill, params.question)
            args, kwargs = derived.arguments(params, self._bound)
            preview = None
            if _observing():
                preview = _agents.preview(derived.declaration.schema.encode(params))
            source, digest = derived.origin
            _notify({"t": "agent.invocation", "agent": self._kernel.agent, "invocation": self.id,
                     "skill": self.skill, "source": source, "digest": digest,
                     "params": preview, "call": None if parse is None else parse.id,
                     "bindings": derived.bindings()})
            entered = time.monotonic()
            value = derived.entry(*args, **kwargs)
            if asyncio.iscoroutine(value):
                value = await value
        except asyncio.CancelledError as e:
            outcome, error = "cancelled", e
        except BaseException as e:
            outcome, error = "raised", e
            if not isinstance(e, Exception):
                # An interrupt or an exit, which the kernel's loop reports as for any task.
                raise
        finally:
            if parse is not None and not parse.done():
                parse.cancel()
            self._end(outcome, value, error, entered)

    def _end(self, outcome, value, error, entered):
        """Record how the invocation ended, once: it holds its skill busy no longer and its id
        names nothing from here on, the host is told when its entry was called, and its future
        settles. On the kernel's loop, and waiting for nothing: a released kernel's loop gives
        its tasks a second."""
        if _running.pop(self.id, None) is None:
            return
        if entered is not None:
            _notify({"t": "agent.invocation.ended", "agent": self._kernel.agent,
                     "invocation": self.id, "outcome": outcome,
                     "duration": time.monotonic() - entered,
                     "result": _qualified(type(value)) if outcome == "returned" else None,
                     "exception": _qualified(type(error)) if outcome == "raised" else None})
        if outcome == "returned":
            self._future.set_result(value)
            return
        if outcome == "cancelled":
            error = _agents.RequestCancelled(
                f"invocation {self.id} of skill {self.skill!r} was cancelled"
            )
        self._future.set_exception(error)
        # Read here: a handle nobody awaits is not a mistake in the code that dropped it.
        self._future.exception()


# ---------------------------------------------------------------------------- machinery

_ROOT = "outrig_skills"
# A compiled module's file, built for any interpreter, as the interpreter's own diagnostic says.
_COMPILED_SUFFIXES = (".so", ".pyd")
_OUTSIDE = (
    "outrig.skills reaches the session's skills, which only an agent's kernel has, and this "
    "Python is not one"
)

# The interpreter's, given by `_install`: send a request about a skill and return the host's
# answer; tell the host something, never raising; the module a failed import could not load
# with the compiled file found where it looked, or `None`; the next id of the interpreter's
# counter; raise `outrig.SessionClosing` once the session is closing; and whether the host
# records what agents give each other.
_ask = None
_notify = None
_compiled = None
_next_id = None
_refuse_closing = None
_observing = None

# The session's skills, by name, from the host's `skills` line.
_catalog = types.MappingProxyType({})
# Each skill's package's last name, to the skill's name.
_modules = {}
# The loader of each skill's imported package, by the skill's name.
_loaded = {}
# The skills being reloaded, whose next import says so.
_reloading = set()
# Each invocation not yet ended, in any kernel, by its id.
_running = {}
# The ids of the invocations the code reading it runs inside, innermost first: set in the context
# each one runs in, which the tasks and `to_thread` workers it starts carry.
_within = contextvars.ContextVar("outrig_invocations", default=())
# Taken to keep the first of two parameter classes derived at once.
_derive_lock = threading.Lock()

_cause_of = BaseException.__dict__["__cause__"].__get__
_context_of = BaseException.__dict__["__context__"].__get__
_type_name = type.__dict__["__name__"].__get__
_type_qualname = type.__dict__["__qualname__"].__get__
_type_module = type.__dict__["__module__"].__get__


def _install(ask, notify, compiled, next_id, refuse_closing, observing):
    """Take the interpreter's ways to ask and tell the host, its evidence of compiled code, its
    ids, its refusal once the session closes, and whether the host records previews; and serve
    `outrig_skills`: before `PathFinder`, so no directory on `sys.path` named so can stand in for
    it. Called once, as the interpreter starts."""
    global _ask, _notify, _compiled, _next_id, _refuse_closing, _observing
    _ask, _notify, _compiled = ask, notify, compiled
    _next_id, _refuse_closing, _observing = next_id, refuse_closing, observing
    sys.meta_path.insert(sys.meta_path.index(importlib.machinery.PathFinder), _Finder())


def _declare(entries):
    """Take the session's skills, which the host sends once, before any kernel opens."""
    global _catalog, _modules
    found = {}
    for entry in entries:
        name = entry["name"]
        package = f"{_ROOT}.{name.replace('-', '_')}"
        entry_module = f"{package}.skill" if entry["executable"] else None
        found[name] = Skill(name, entry["description"], entry["source"], package, entry_module)
    _catalog = types.MappingProxyType(found)
    _modules = {skill.package.rpartition(".")[2]: name for name, skill in found.items()}


def _skill(name):
    """The session's skill `name`, or the `SkillError` saying it has none."""
    if _ask is None:
        raise RuntimeError(_OUTSIDE)
    skill = _catalog.get(name) if type(name) is str else None
    if skill is None:
        raise SkillError(
            "unknown-skill",
            name,
            "the session has no skill of that name; outrig.skills.catalog() lists those it has",
        )
    return skill


def _from_agent(line):
    """`line`, naming the agent whose code sends it, when an agent's does."""
    kernel = _kernel._calling()
    if kernel is not None:
        line["agent"] = kernel.agent
    return line


def _request(name, kind, **fields):
    """What the host answers to a request of `kind` about skill `name`, or the `SkillError` its
    answer gives."""
    try:
        answer = _ask(_from_agent({"t": kind, "skill": name, **fields}))
    except OSError as e:
        raise SkillError("source-error", name, f"the host could not be asked: {e}") from None
    error = answer.get("error")
    if error is not None:
        code, message = error.get("code"), error.get("message")
        raise SkillError(str(code or "source-error"), name, str(message or "no reason given"))
    return answer.get("ok") or {}


def _check(name, requires_python, dependencies):
    """Raise `SkillError` unless this interpreter has what skill `name` declares it needs.
    Nothing is installed, and pip's own code never runs."""
    if requires_python is None and not dependencies:
        return
    import importlib.metadata
    import platform

    # pip's own copy of `packaging`, pinned with this interpreter: what an agent installs cannot
    # stand in for it, since pip's directory comes first on `sys.path`.
    from pip._vendor.packaging.requirements import Requirement
    from pip._vendor.packaging.specifiers import SpecifierSet

    version = platform.python_version()
    unmet = []
    # Each of `packaging`'s errors is a `ValueError`: a specifier, a requirement or a marker that
    # does not parse, or a marker that names what no environment has.
    try:
        if not SpecifierSet(requires_python or "").contains(version, prereleases=True):
            raise SkillError(
                "requires-python",
                name,
                f"it requires Python {requires_python}, and this interpreter is {version}",
            )
        for text in dependencies:
            requirement = Requirement(text)
            if requirement.marker is not None and not requirement.marker.evaluate():
                continue
            try:
                installed = importlib.metadata.version(requirement.name)
            except importlib.metadata.PackageNotFoundError:
                unmet.append(f"{text} (not installed)")
                continue
            if not requirement.specifier.contains(installed, prereleases=True):
                unmet.append(f"{text} ({installed} is installed)")
    except ValueError as e:
        raise SkillError(
            "metadata-invalid", name, f"what its skill.py declares it needs does not parse: {e}"
        ) from None
    if unmet:
        raise SkillError(
            "unsatisfied-dependency",
            name,
            f"it requires {'; '.join(unmet)}. Nothing was installed: `pip install` what it "
            "lacks, then import it again",
        )


def _drop_below(package):
    """Remove each module below `package` from `sys.modules`: exactly those, never another
    skill's whose name begins the same."""
    prefix = f"{package}."
    for key in [key for key in list(sys.modules) if type(key) is str and key.startswith(prefix)]:
        sys.modules.pop(key, None)


def _native(exc):
    """The module and compiled file an `ImportError` among `exc`, its causes and its contexts
    shows compiled code to be the reason for, or `None`. Read through `BaseException`'s own
    descriptors, so no agent code runs."""
    seen, todo = set(), [exc]
    while todo and len(seen) < 100:
        e = todo.pop()
        if e is None or id(e) in seen:
            continue
        seen.add(id(e))
        if issubclass(type(e), ImportError):
            found = _compiled(e) or _compiled_in_skill(e)
            if found is not None:
                return found
        todo += (_cause_of(e), _context_of(e))
    return None


def _compiled_in_skill(e):
    """The module a failed import of a skill's own module looked for, and the compiled file in
    the skill's listing where it looked, or `None`. A package whose import failed is gone from
    `_loaded`, so its listing is found through the frames `e` passed: each module's `__spec__`
    holds its loader."""
    kind = type(e)
    if kind is not ImportError and kind is not ModuleNotFoundError:
        return None
    name = e.name
    if type(name) is not str:
        return None
    if kind is ImportError:
        if type(e.name_from) is not str:
            return None
        name = f"{name}.{e.name_from}"
    parts = name.split(".")
    skill = _modules.get(parts[1]) if len(parts) > 2 and parts[0] == _ROOT else None
    loader = _loaded.get(skill)
    tb = e.__traceback__
    while loader is None and skill is not None and tb is not None:
        # `dict`'s own `get`: a frame's globals can be an agent's subclass of `dict`.
        spec = dict.get(tb.tb_frame.f_globals, "__spec__")
        if type(spec) is importlib.machinery.ModuleSpec and type(spec.loader) is _Loader:
            if spec.loader.name == skill:
                loader = spec.loader
        tb = tb.tb_next
    if loader is None:
        return None
    prefix = "/".join(parts[2:]) + "."
    for path in sorted(loader.files):
        if path.startswith(prefix) and "/" not in path[len(prefix) :]:
            if path.endswith(_COMPILED_SUFFIXES):
                return name, f"{path} in skill {skill!r}"
    return None


# ---------------------------------------------------------------------------- invoking

# What the child that reads the user's text into a skill's parameters is told first; the entry's
# docstring follows. Formatted with the skill's name and its parameter class's, never with the
# docstring or the text, which reach the child as they are.
_PARSE = (
    "`text` is what the user typed after `/{name}`: exact options, such as `--name value`, or "
    "prose. Complete with the {params} it asks for when the text determines one: give an exact "
    "option to its field, and make a plain reading that stays within what the words say "
    "without asking -- a number written in words as that number, and a text that names nothing "
    "for a field leaving that field's default. Complete with a Clarification instead, its "
    "`question` in the user's own terms, when the text does not determine a required field, or "
    "when the reading you would make covers more than the words name: a default that takes in "
    "everything where the user named a part. Read `text` from the variable; it is not repeated "
    "here.\n\nWhat `/{name}` does, as its entry's docstring says:"
)


def _current():
    """The ids of the invocations the calling code runs inside and that have not ended, innermost
    first: what the interpreter names on what that code asks of the host."""
    return [id for id in _within.get() if id in _running]


def _qualified(t):
    """Class `t`'s module and qualified name, read without running any of its code."""
    return f"{_type_module(t)}.{_type_qualname(t)}"


def _entry_of(name, reload_first):
    """Skill `name`, the loader its package was imported with, and its entry module, imported now
    if it is not: reloaded first with `reload_first`, and otherwise, when imported before, checked
    against the skill's files."""
    skill = _skill(name)
    if skill.entry is None:
        raise SkillError(
            "entry-not-found",
            name,
            "it has no skill.py, so nothing to call: its instructions are all of it, and "
            "outrig.skills.read(name) returns them",
        )
    if reload_first:
        reload(name)
    elif name in _loaded:
        _request(name, "skill.check", digest=_loaded[name].digest)
    module = importlib.import_module(skill.entry)
    return skill, module.__spec__.loader, module


def _derived(skill, loader, module):
    """What `invoke` calls `skill`'s entry with, worked out once per import of its package and
    kept on `loader`: outside the lock, since working it out can import, and the first kept when
    two kernels work it out at once."""
    derived = loader.derived
    if derived is None:
        derived = _derive(skill, loader, module)
        with _derive_lock:
            if loader.derived is None:
                loader.derived = derived
            derived = loader.derived
    return derived


def _derive(skill, loader, module):
    """`skill`'s parameter class and parser, from its entry's signature and what its package was
    imported with, or the `SkillError` saying why its entry cannot have them."""
    name = skill.name
    entry_name = loader.entry or "main"
    entry = getattr(module, entry_name, None)
    if not inspect.isfunction(entry):
        how = "[tool.outrig.skill] entry names" if loader.entry else "is the entry by default"
        raise SkillError(
            "entry-not-found", name, f"its skill.py defines no function {entry_name!r}, which {how}"
        )
    where = f"{module.__name__}.{entry_name}"

    def unsupported(message):
        return SkillError("signature-unsupported", name, f"{where}: {message}")

    try:
        parameters = list(inspect.signature(entry).parameters.values())
        # Where a parameter's annotation is resolved, as `typing.get_type_hints` resolves one: the
        # globals of the function the signature comes from, through what decorators wrap and a
        # `functools.partial` holds -- a bounded walk, as a chain can loop.
        target = inspect.unwrap(entry)
        for _ in range(100):
            if not isinstance(target, functools.partial):
                break
            target = inspect.unwrap(target.func)
        namespace = getattr(target, "__globals__", {})
    except Exception as e:
        raise unsupported(
            f"its signature cannot be read: {_type_name(type(e))}: {e}"
        ) from None
    for parameter in loader.bind:
        if parameter not in {p.name for p in parameters}:
            raise unsupported(f"[tool.outrig.skill] bind names {parameter!r}, no parameter of it")
    helps = dict(loader.parameters)
    fields = []
    for p in parameters:
        if p.kind is p.VAR_KEYWORD:
            raise unsupported(f"it takes **{p.name}, which names no parameter to make a field of")
        if p.name in loader.bind:
            if p.kind is p.VAR_POSITIONAL:
                raise unsupported(
                    f"[tool.outrig.skill] bind names *{p.name}, which is no one value"
                )
            continue
        # Only a field's annotation is resolved: the return's and a bound parameter's may name
        # what the skill imports only under `TYPE_CHECKING`, and are no field's.
        hint = str
        if p.annotation is not p.empty:
            probe = types.SimpleNamespace(__annotations__={p.name: p.annotation})
            try:
                hint = typing.get_type_hints(probe, namespace, include_extras=True)[p.name]
            except Exception as e:
                raise unsupported(
                    f"the annotation of parameter {p.name!r} cannot be resolved: "
                    f"{_type_name(type(e))}: {e}"
                ) from None
        if p.kind is p.VAR_POSITIONAL:
            hint = list[hint]
        base, _, aliases = schema._markers(hint)
        help = helps.pop(p.name, None)
        if help is not None:
            hint = typing.Annotated[(base, *aliases, schema.doc(help))]
        if (aliases[0].name if aliases else p.name) == "question":
            raise unsupported(
                f"parameter {p.name!r} would be the key `question`, which is a Clarification's, "
                "so what its parser completes with could not be told apart"
            )
        try:
            declared, _ = _agents.declared_with_classes(
                base, f"{where}: parameter {p.name!r} cannot be"
            )
        except TypeError as e:
            raise SkillError("signature-unsupported", name, str(e)) from None
        if p.kind is p.VAR_POSITIONAL:
            spec = dataclasses.field(default_factory=list)
        elif p.default is p.empty:
            spec = dataclasses.field()
        else:
            _, problems = declared._encoding(p.default)
            if problems:
                raise unsupported(
                    f"the default of parameter {p.name!r} is not of its type:\n"
                    f"{problems.listing()}"
                )
            default = p.default
            if type(default).__hash__ is None:
                # Shared between calls, as a mutable default is in any Python.
                spec = dataclasses.field(default_factory=lambda default=default: default)
            else:
                spec = dataclasses.field(default=default)
        fields.append((p.name, hint, spec))
    for key in helps:
        warnings.warn(
            f"skill {name!r}: [tool.outrig.skill.parameters] has {key!r}, which is no field of its "
            "parameter class, so its help is not used",
            stacklevel=4,
        )
    camel = "".join(part[:1].upper() + part[1:] for part in name.split("-"))
    cls = dataclasses.make_dataclass(
        f"{camel}Params", fields, frozen=True, kw_only=True, module=module.__name__
    )
    cls.__doc__ = (
        f"The parameters of skill {name!r}'s entry, {where}, but those `bind` fills: what "
        f"outrig.skills.invoke({name!r}, params=...) takes."
    )

    # The parse, declared as `@outrig.agent` declares a function: in the entry's module, so its
    # call's events name the skill, with the digest of that module's source.
    async def parse_params(text: str) -> cls | Clarification:
        ...

    told = entry.__doc__
    told = inspect.cleandoc(told) if told and told.strip() else skill.description
    parse_params.__doc__ = _PARSE.format(name=name, params=cls.__name__) + "\n\n" + told
    parse_params.__module__, parse_params.__qualname__ = module.__name__, "parse_params"
    try:
        declaration = _agents._declare(parse_params, None, None)
    except TypeError as e:
        raise SkillError("signature-unsupported", name, str(e)) from None
    source = loader.sources.get(module.__name__)
    digest = None if source is None else hashlib.sha256(source.encode()).hexdigest()
    declaration = dataclasses.replace(declaration, digest=digest)
    origin = (loader.source, loader.digest)
    return _Derived(name, cls, declaration, entry, parameters, dict(loader.bind), origin)


class _Derived:
    """What `invoke` calls one import of a skill's entry with: its parameter class, the typed
    call that parses into it -- whose schema, of the class or a `Clarification`, checks an
    instance too -- the entry and its parameters, `bind`, and the source serving the skill with
    the digest of its files then."""

    __slots__ = ("name", "cls", "declaration", "entry", "parameters", "bind", "origin")

    def __init__(self, name, cls, declaration, entry, parameters, bind, origin):
        self.name, self.cls, self.declaration = name, cls, declaration
        self.entry, self.parameters, self.bind, self.origin = entry, parameters, bind, origin

    def check(self, params):
        """Raise `TypeError` unless `params` is an instance of this class, of its field types."""
        if type(params) is not self.cls:
            raise TypeError(
                f"params is a {_type_qualname(type(params))}, not a {self.cls.__name__}, skill "
                f"{self.name!r}'s parameter class as now imported: "
                f"outrig.skills.params({self.name!r}) returns it"
            )
        _, problems = self.declaration.schema._encoding(params)
        if problems:
            raise TypeError(
                f"params holds what {self.cls.__name__}'s field types do not take:\n"
                f"{problems.listing()}"
            )

    def arguments(self, params, bound):
        """`(args, kwargs)` to call the entry with: its positional parameters in order, a bound
        one from `bound`, `*args` unpacked, and its keyword-only parameters by name."""
        args, kwargs = [], {}
        for p in self.parameters:
            value = bound[p.name] if p.name in bound else getattr(params, p.name)
            if p.kind is p.VAR_POSITIONAL:
                args.extend(value)
            elif p.kind is p.KEYWORD_ONLY:
                kwargs[p.name] = value
            else:
                args.append(value)
        return args, kwargs

    def bindings(self):
        """The bindings the entry is given, in the order of its parameters."""
        bound = (self.bind[p.name] for p in self.parameters if p.name in self.bind)
        return list(dict.fromkeys(bound))


class _Finder:
    """What serves `outrig_skills` and each skill of the catalog, and nothing else.

    It never asks the host: the import system holds its lock across every `find_spec` of every
    import, so a module is found from the catalog and the listing its skill's package was
    imported with, and fetched only once its own import runs."""

    def find_spec(self, fullname, path=None, target=None):
        if fullname == _ROOT:
            # A package that holds the skills and nothing else, as a namespace package would.
            return importlib.machinery.ModuleSpec(_ROOT, None, is_package=True)
        parts = fullname.split(".")
        name = _modules.get(parts[1]) if len(parts) > 1 and parts[0] == _ROOT else None
        if name is None:
            return None
        origin = f"outrig skill {name}"
        if len(parts) == 2:
            return importlib.machinery.ModuleSpec(
                fullname, _Loader(name), origin=origin, loader_state="__init__.py", is_package=True
            )
        loader = _loaded.get(name)
        if loader is None:
            return None
        base = "/".join(parts[2:])
        for file, package in ((f"{base}/__init__.py", True), (f"{base}.py", False)):
            if file in loader.files:
                return importlib.machinery.ModuleSpec(
                    fullname, loader, origin=origin, loader_state=file, is_package=package
                )
        return None


class _Loader:
    """One import of a skill's package, and the loader of each of its modules after it: the
    files the host listed for the skill then, the digest each fetch names, and the source of
    each module it ran."""

    def __init__(self, name):
        self.name = name
        self.source = None
        self.digest = None
        self.files = frozenset()
        self.sources = {}
        # What `[tool.outrig.skill]` said when the package was imported -- `entry`, `bind` and
        # `parameters` -- and what `invoke` derived from it, once it has.
        self.entry = None
        self.bind = {}
        self.parameters = {}
        self.derived = None

    def create_module(self, spec):
        return None

    def exec_module(self, module):
        """Run `module`'s file, when the listing has one: for the skill's package, once the host
        has listed the skill and what it declares it needs is here, telling the host how the
        import went.

        What the file raises is raised as it was. A package that fails takes every module below
        it out of `sys.modules`, so its next import fetches each again, at the files' digest
        then."""
        name, spec = self.name, module.__spec__
        package = spec.name == _catalog[name].package
        try:
            if package:
                self._load(module)
            if spec.loader_state in self.files:
                self._run(module, spec.loader_state)
        except BaseException as e:
            if package:
                # Told before the package goes, since its listing is evidence of compiled code.
                self._tell(e)
                if _loaded.get(name) is self:
                    del _loaded[name]
                _drop_below(spec.name)
            raise
        if package:
            self._tell(None)

    def get_source(self, fullname):
        return self.sources.get(fullname)

    def get_resource_reader(self, fullname):
        """The files of module `fullname`'s directory -- its own, for a package."""
        base = fullname[len(_catalog[self.name].package) + 1 :].replace(".", "/")
        if f"{base}/__init__.py" not in self.files:
            base = base.rpartition("/")[0]
        return _Node(self, base)

    def _load(self, module):
        """Ask the host for the skill's listing, check what the skill declares it needs, and
        record on its package where it came from -- before `__init__.py` runs, so its relative
        imports find its siblings."""
        answer = _request(self.name, "skill.load")
        self.digest, self.files = answer["digest"], frozenset(answer["files"])
        self.source, self.entry = answer["source"], answer.get("entry")
        self.bind = dict(answer.get("bind") or {})
        self.parameters = dict(answer.get("parameters") or {})
        _check(self.name, answer.get("requires_python"), answer.get("dependencies") or [])
        module.__outrig_skill__ = Origin(self.name, self.source, self.digest)
        _loaded[self.name] = self

    def _fetch(self, path):
        """What the skill's `path` holds, as of the digest its package was imported at."""
        answer = _request(self.name, "skill.read", path=path, digest=self.digest)
        return base64.b64decode(answer["data"])

    def _run(self, module, path):
        """Fetch `path` and run it as `module`, under a filename that names the skill --
        registered in `linecache`, so a traceback or `inspect.getsource` never looks for it on
        disk. The source is kept first: what the module runs, `@outrig.agent`'s digest among it,
        can ask for it."""
        source = importlib.util.decode_source(self._fetch(path))
        self.sources[module.__name__] = source
        filename = f"<skill {self.name}>/{path}"
        linecache.cache[filename] = (len(source), None, source.splitlines(True), filename)
        exec(compile(source, filename, "exec", dont_inherit=True), module.__dict__)

    def _tell(self, error):
        """Tell the host the skill's package was imported, or why its import failed: a
        `SkillError`'s code, `native-extension-unavailable` when compiled code is the reason,
        `import-failed` otherwise."""
        said = None
        if type(error) is SkillError:
            said = {"code": error.code, "message": error.message}
        elif error is not None:
            found = _native(error)
            said = {"code": "import-failed", "message": f"{_type_name(type(error))} was raised"}
            if found is not None:
                said = {
                    "code": "native-extension-unavailable",
                    "message": f"importing it reached {found[0]!r}, whose {found[1]} is compiled "
                    "code, which this interpreter cannot load",
                }
        line = {"t": "skill.imported", "skill": self.name, "digest": self.digest}
        line.update(reload=self.name in _reloading, error=said)
        _notify(_from_agent(line))


class _Node(importlib.resources.abc.Traversable, importlib.resources.abc.TraversableResources):
    """A file or directory of a skill's, as the listing its package was imported with has it,
    and what `importlib.resources` reads it through. Opening a file fetches it from the host."""

    def __init__(self, loader, path):
        self._loader = loader
        self._path = path

    def files(self):
        return self

    @property
    def name(self):
        return self._path.rpartition("/")[2]

    def _below(self):
        prefix = f"{self._path}/" if self._path else ""
        return [path[len(prefix) :] for path in self._loader.files if path.startswith(prefix)]

    def is_file(self):
        return self._path in self._loader.files

    def is_dir(self):
        return bool(self._below())

    def iterdir(self):
        names = sorted({path.partition("/")[0] for path in self._below()})
        return iter([self.joinpath(name) for name in names])

    def joinpath(self, *descendants):
        whole = "/".join([self._path, *map(str, descendants)])
        return _Node(self._loader, "/".join(p for p in whole.split("/") if p not in ("", ".")))

    def open(self, mode="r", *args, **kwargs):
        if not self.is_file():
            raise FileNotFoundError(f"skill {self._loader.name!r} has no file {self._path!r}")
        data = io.BytesIO(self._loader._fetch(self._path))
        return data if "b" in mode else io.TextIOWrapper(data, *args, **kwargs)

    def read_text(self, encoding=None, errors=None):
        # `importlib.resources.read_text` passes `errors`, which `Traversable`'s own lacks.
        with self.open(encoding=encoding, errors=errors) as text:
            return text.read()

    def __repr__(self):
        return f"<file {self._path!r} of skill {self._loader.name!r}>"
