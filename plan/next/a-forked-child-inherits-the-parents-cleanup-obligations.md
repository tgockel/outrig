# A forked child's panic hook would sweep the parent's containers

`crates/outrig/src/nsfork.rs`'s `fork_collect` does a bare `fork()` and runs Rust in the
child before `_exit`. The child inherits the parent's memory, which includes
`container::TRACKED` -- every cleanup obligation the parent is currently carrying -- and
the process-wide panic hook installed by `outrig::container::install_panic_hook`.

If anything in that child panics, its hook sweeps obligations that belong to the *parent*
and force-removes containers the parent is still using.

Unchanged by the `#147` fix, and not made worse by it: the sweep is now label-scoped, so
what the child would remove is exactly what the parent created rather than whatever held
a name. That is a smaller blast radius, not an absent one.

The lock side of this is already handled -- `pending_removals` uses `try_lock`, so a child
that inherited a held lock gives up rather than deadlocking on a thread that does not
exist in it.

## Sketch

The child should not be carrying the parent's obligations at all. Either clear `TRACKED`
in the child immediately after `fork()`, or install a hook in the child that does not
sweep. `pthread_atfork` is the usual place, but the child here runs a very small amount of
code before `_exit`, so doing it explicitly at the top of the child branch is simpler and
easier to prove.

Note the async-signal-safety constraint the module already documents: what runs between
`fork` and `_exit` must be safe in a forked child, which locking a mutex is not. Clearing
the map is a write under a lock, so the honest fix may be to drop the hook rather than to
edit the registry.
