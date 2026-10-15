# A hosted proxy's class is fixed when it arrives

## Context

RPyC builds a proxy's class as it unboxes the proxy, and resolves the class's `__class__` then:
the local class of the host type's name, if its module is already in `sys.modules`, and nothing
otherwise. A proxy that arrived before agent code imported the library keeps no local class, so
its `__class__` asks the host for `__class__`, which the binding refuses as a private name. Seen
through `run-new` with GitPython 3.2.0 (`0003-23`'s `## Decisions`):

- `isinstance(repo, git.Repo)` is false for the whole session when `repo` was used before
  `import git`: the binding's root is fetched at its first use and kept, with the class it got
  then.
- `isinstance(c, git.Commit)`, for a commit fetched before the import, raises `outrig.Refused`,
  since `git.Commit` is an ABC whose instance check lets the error through; a plain class's check
  answers false.
- Each such check is a refused request in the record.

A proxy fetched after the import gets its class and answers as the local object does.

## Options

- **Resolve `__class__` when it is read.** The interpreter already wraps the proxy's
  `__getattribute__` (`_one_request_per_read`); answering `__class__` from `sys.modules` at the
  time of the read, and with the proxy's own class when no local class exists, would make
  `isinstance` hold whenever the library is imported, never raise, and never ask the host. It
  departs from RPyC's proxy, which is what `0003-23` binds.
- **Say it in the orientation**: import a binding's library before using the binding if
  `isinstance` matters.
- **Leave it.** `isinstance` on a hosted object is rare in agent code, and a fresh read of the
  object after the import answers it.

## Evaluation

Whether agents run into it: transcripts of sessions with a binding, counting `isinstance` checks
on hosted objects and refused `__class__` reads in the record.
