"""OutRig's own package, imported by agent code in a kernel and by a program on the host.

`outrig.schema` declares types, decodes JSON into them strictly, and renders their JSON Schema:
see `help(outrig.schema)`.

In an agent's kernel, `outrig.runtime` is the runtime of the execution reading it -- what the
`runtime` global is, reachable from a module's code too -- and `outrig.bindings[name]` is that
execution's own proxy of a binding, whatever the kernel's globals now hold under its name.

In a program `outrig run <script.py>` runs on the host, `outrig.config()` is a read-only view of
the configuration it loaded, and `outrig.Session` enters a session the engine starts for it: see
`help(outrig.config)` and `help(outrig.Session)`.
"""

from . import schema

__all__ = ["schema"]

# What only a kernel has. `outrig._kernel` binds each as the interpreter starts; anywhere else,
# asking for one says why there is none.
_KERNEL_ONLY = frozenset({"runtime", "bindings"})
# What only a program on the host has, bound by `outrig._host`. Absent anywhere else, so that
# `hasattr` answers rather than raises.
_HOST_ONLY = frozenset(
    {
        "config",
        "Session",
        "Mount",
        "Event",
        "Missed",
        "ShutdownReport",
        "ShutdownException",
        "current_session",
    }
)


def __getattr__(name):
    if name in _KERNEL_ONLY:
        raise RuntimeError(
            f"outrig.{name} exists only in an agent's kernel, where OutRig runs agent code, and "
            "this Python is not one"
        )
    if name in _HOST_ONLY:
        raise AttributeError(
            f"outrig.{name} exists only in a program `outrig run` runs on the host, and this "
            "Python is not one"
        )
    raise AttributeError(f"module 'outrig' has no attribute {name!r}")
