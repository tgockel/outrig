# A binding's process and everything it starts act with the user's full authority

## Context

Phase 0003 runs each binding as a host process of the embedded CPython, as the user, with the
user's environment unless the embedder supplies another; the CLI supplies none
(`plan/phase/0003-python/hosted-objects.md`). Whatever the hosted library does on the host is
part of what the binding grants. That is documented -- in `plan/phase/0003-python/security.md`
and the SECURITY.md entry `0003-20` adds -- and not mitigated.

The phase 0003 design brief (now in `plan/phase/0003-python/hosted-objects.md`) showed what that
includes, with GitPython as the example. GitPython runs `git`, and `git` runs programs the
repository names: hooks, and config keys such as `core.fsmonitor`, `core.sshCommand`,
`credential.helper` and filter drivers, also reachable through `include.path`. The workspace `.git/`
is writable from the container, and the primary runs with `--userns=keep-id`, so files the agent
writes there are owned by the host user and `git`'s `safe.directory` check accepts them. A hook the
agent's code writes runs on the host, as the user, at the next hosted commit. It inherits the
binding's environment: `SSH_AUTH_SOCK` lets it sign with the user's keys, and `git credential fill`
returns tokens from the user's credential helpers.

## Shape

Confine the binding process and every process it starts, the same way for any library. Measures
that only `git` honors -- protected `-c` overrides, `GIT_CONFIG_NOSYSTEM`, a hooks path of
`/dev/null` -- are library-specific treatment, which the phase rules out. Mechanisms from 02 §6,
none chosen:

- A separate UID, or a user namespace, for the binding process.
- Landlock rules on what it and its descendants may read and write: the bound directories, the
  package cache, and what the embedder lists.
- A sanitized environment by default, with credentials only as the embedder grants them per
  binding. A program in the same process tree as a granted credential helper can still query it.
- A cgroup per binding, for resource limits and for finding every descendant at shutdown.
- Network access stays, since a push needs it; limiting where it goes overlaps the network policy.

Each has to be shown to work on the hosts OutRig supports, and none of them alone is claimed to be
enough.

## Acceptance

- A hook and a config-named program written by agent code, then run by a hosted call, cannot read
  a canary file outside the allowed set, and a canary credential is absent from their environment.
- A commit and a push to a test remote through the example library still work.
- The mechanism contains no library-specific code.
