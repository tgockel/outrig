"""The kernel's half of `outrig`: `outrig.runtime` and `outrig.bindings`, each the calling
execution's, and what a facility reference (`outrig.Facility`) resolves to. The interpreter
installs it as it starts, and nothing else imports it.

`sys.modules` is the whole interpreter's, so one `outrig` serves every kernel in it, and a module
imported once is called from each. What either name gives is therefore decided when it is read,
from the execution reading it: a task, a `threading.Thread`, and an item given to a thread pool
carry the context of the execution that made them, `asyncio.to_thread`'s included.
"""

import collections.abc

import outrig
import outrig._agents

# Gives the kernel of the execution calling it, or `None` outside every execution.
_calling = None

_OUTSIDE = (
    "outrig.{name} is the calling execution's, and this code runs outside every execution: in a "
    "thread started with _thread.start_new_thread, say, or a pool's initializer, or a context of "
    "its own. Carry the execution's context to it -- start the thread with threading.Thread, "
    "hand the work to asyncio.to_thread, or run it through contextvars.copy_context().run(...) "
    "called inside the execution"
)


def _kernel(name):
    kernel = _calling()
    if kernel is None:
        raise RuntimeError(_OUTSIDE.format(name=name))
    return kernel


class _Runtime:
    """The runtime of the execution reading it: see `help(runtime)` for what it offers.

    Every attribute read goes to that runtime, decided at the read, so a module that ran
    `from outrig import runtime` once still reaches each caller's own. `type(outrig.runtime)` is
    therefore this class, not the runtime's. Read outside every execution -- in a thread started
    with `_thread.start_new_thread`, or a pool's initializer -- it raises `RuntimeError`.
    """

    __slots__ = ()

    def __getattr__(self, name):
        return getattr(_kernel("runtime").runtime, name)

    def __dir__(self):
        return dir(_kernel("runtime").runtime)

    def __repr__(self):
        kernel = _calling()
        return "<outrig.runtime, outside every execution>" if kernel is None else repr(kernel.runtime)


class _Bindings(collections.abc.Mapping):
    """What the calling agent was given, by name, each as its own kernel's proxy. Read-only.

    For an agent given facilities -- `facilities=` to its constructor, or its maker's, inherited
    -- that is its mapping as it stands, read now: a binding as the calling kernel's own proxy of
    it, any other object as itself. For any other agent it is the session's bindings, a live
    object a program on the host injects joining as it is injected. A name not among them raises
    `KeyError`, a binding the session holds included: nothing falls back to the session's.

    The authoritative lookup: `outrig.bindings["repo"]` is the binding whatever this kernel's
    global `repo` now holds -- a global is bound only where its name was free, so an input or a
    builtin of the same name keeps it -- and from a module's code, which has no such global, the
    way to reach one at all. `runtime.bindings` says what each is without asking it.
    """

    __slots__ = ()

    def __getitem__(self, name):
        return _kernel("bindings").facility(name)

    def __iter__(self):
        return iter(list(_kernel("bindings").facility_names()))

    def __len__(self):
        return len(_kernel("bindings").facility_names())

    def __contains__(self, name):
        return name in _kernel("bindings").facility_names()

    def __repr__(self):
        kernel = _calling()
        if kernel is None:
            return "<outrig.bindings, outside every execution>"
        return f"<outrig.bindings: {', '.join(list(kernel.facility_names())) or 'none'}>"


def _resolve(kind, name):
    """What the calling execution's kernel holds for the facility of `kind` named `name`."""
    kernel = _calling()
    if kernel is None:
        raise RuntimeError(_OUTSIDE.format(name=f"Facility {kind} {name!r}"))
    return kernel.resolve(kind, name)


def install(calling):
    """Bind `outrig.runtime` and `outrig.bindings`, and resolve each facility reference, through
    `calling()`: the kernel of the execution calling it, or `None` outside every execution."""
    global _calling
    _calling = calling
    outrig.runtime = _Runtime()
    outrig.bindings = _Bindings()
    outrig._agents._resolve = _resolve
