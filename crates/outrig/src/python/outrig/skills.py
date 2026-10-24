"""The session's skills: know-how the project, the user or the application running the session
supplies, each instructions in a `SKILL.md` and, optionally, Python in a `skill.py`.

`read(name)` returns a skill's instructions and `catalog()` lists every skill. A skill imports as
a package: `review-diff` is `outrig_skills.review_diff`, and its `skill.py` is
`outrig_skills.review_diff.skill`, whose siblings it imports relatively. Its files are fetched from
the host as they are imported or opened, and none is on disk here, so a skill reads its data with
`importlib.resources.files(__package__) / "data" / "ids.txt"`; its modules have no `__file__`.

Importing a skill's package checks the `requires-python` and `dependencies` its `skill.py`
declares, and installs nothing: `pip install` what it lacks, then import it again. Each failure
is a `SkillError` whose `code` says why; what the skill's own code raises is raised as it was.
The package records where it came from as
`__outrig_skill__`, an `Origin`; once the skill's files change, fetching another is refused with
`skill-changed`, and `reload(name)` imports it afresh.

`sys.modules` is the interpreter's, so every agent here shares one module per skill, and its
module-level state with it.
"""

import base64
import importlib
import importlib.machinery
import importlib.resources.abc
import importlib.util
import io
import linecache
import sys
import types

from . import _kernel

__all__ = ["Origin", "Skill", "SkillError", "catalog", "read", "reload"]


class SkillError(ImportError):
    """A skill could not be read, checked or imported. `skill` names it, and `code` says why:

    - `unknown-skill` -- the session has no skill of that name;
    - `skill-changed` -- its files changed after its package was imported: `reload` it;
    - `source-error` -- what serves it could not list or read it;
    - `metadata-invalid`, `duplicate-metadata-block` -- its metadata is not valid;
    - `requires-python`, `unsatisfied-dependency` -- what it declares it needs is not here.

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

    Code that holds the old modules keeps the old code, and an object of an old class stays one:
    `isinstance` against the class reloaded is false of it."""
    package = _skill(name).package
    imported = sys.modules.pop(package, None) is not None
    _drop_below(package)
    _loaded.pop(name, None)
    if imported:
        _reloading.add(name)
    try:
        return importlib.import_module(package)
    finally:
        _reloading.discard(name)


# ---------------------------------------------------------------------------- machinery

_ROOT = "outrig_skills"
# A compiled module's file, built for any interpreter, as the interpreter's own diagnostic says.
_COMPILED_SUFFIXES = (".so", ".pyd")
_OUTSIDE = (
    "outrig.skills reaches the session's skills, which only an agent's kernel has, and this "
    "Python is not one"
)

# The interpreter's, given by `_install`: send a request about a skill and return the host's
# answer; tell the host something, never raising; and the module a failed import could not load
# with the compiled file found where it looked, or `None`.
_ask = None
_notify = None
_compiled = None

# The session's skills, by name, from the host's `skills` line.
_catalog = types.MappingProxyType({})
# Each skill's package's last name, to the skill's name.
_modules = {}
# The loader of each skill's imported package, by the skill's name.
_loaded = {}
# The skills being reloaded, whose next import says so.
_reloading = set()

_cause_of = BaseException.__dict__["__cause__"].__get__
_context_of = BaseException.__dict__["__context__"].__get__
_type_name = type.__dict__["__name__"].__get__


def _install(ask, notify, compiled):
    """Take the interpreter's ways to ask and tell the host, and its evidence of compiled code,
    and serve `outrig_skills`: before `PathFinder`, so no directory on `sys.path` named so can
    stand in for it. Called once, as the interpreter starts."""
    global _ask, _notify, _compiled
    _ask, _notify, _compiled = ask, notify, compiled
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
        self.digest = None
        self.files = frozenset()
        self.sources = {}

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
        _check(self.name, answer.get("requires_python"), answer.get("dependencies") or [])
        module.__outrig_skill__ = Origin(self.name, answer["source"], self.digest)
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
