"""The kernel's half of `outrig`: `outrig.runtime` and `outrig.bindings`, each the calling
execution's. The interpreter installs it as it starts, and nothing else imports it.

`sys.modules` is the whole interpreter's, so one `outrig` serves every kernel in it, and a module
imported once is called from each. What either name gives is therefore decided when it is read,
from the execution reading it: a task, a `threading.Thread`, and an item given to a thread pool
carry the context of the execution that made them, `asyncio.to_thread`'s included.
"""

import collections.abc

import outrig

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
    """The session's bindings, by name, each as the calling kernel's own proxy of it. Read-only.

    The authoritative lookup: `outrig.bindings["repo"]` is the binding whatever this kernel's
    global `repo` now holds, and from a module's code, which has no such global, the way to reach
    one at all. A name the session has not bound raises `KeyError`. `runtime.bindings` says what
    each is without asking it.
    """

    __slots__ = ()

    def __getitem__(self, name):
        return _kernel("bindings").stubs[name]

    def __iter__(self):
        return iter(list(_kernel("bindings").stubs))

    def __len__(self):
        return len(_kernel("bindings").stubs)

    def __repr__(self):
        kernel = _calling()
        if kernel is None:
            return "<outrig.bindings, outside every execution>"
        return f"<outrig.bindings: {', '.join(kernel.stubs) or 'none'}>"


def install(calling):
    """Bind `outrig.runtime` and `outrig.bindings`, resolved through `calling()`: the kernel of the
    execution calling it, or `None` outside every execution."""
    global _calling
    _calling = calling
    outrig.runtime = _Runtime()
    outrig.bindings = _Bindings()
