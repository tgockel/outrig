# Same paths do not hold under a symlinked host directory

## Context

With a binding, `0003-20` mounts the workspace in the container at its host path, as written and
resolved -- `/home/alice/project` -- so a path the agent writes and a path the library reads name
one file. When a directory on that path is a symlink on the host -- `/home` linking to
`/var/home`, as on Fedora Silverblue -- the binding process, started in that directory, reports
`os.getcwd()` as `/var/home/alice/project`, and a library that resolves paths (`os.path.realpath`,
`Path.resolve()`) hands the agent paths under `/var/home`, which do not exist in the container.
The reverse holds too: the container sees only `/home/alice/project`.

Read from the kernel's behavior and GitPython's use of `os.path.abspath`, not reproduced.

## Shape

One of:

- Mount the workspace at its canonical path as well, when that differs, so both spellings name
  the same directory in the container.
- Or start each binding with `PWD` set and its working directory given as written, and document
  that a library resolving symlinks returns paths only the host has.

## Acceptance

- With the workspace reached through a symlinked parent, a path a binding's library returns for a
  file in the workspace opens in the container, and the agent's `os.getcwd()` is a path the
  binding understands.
