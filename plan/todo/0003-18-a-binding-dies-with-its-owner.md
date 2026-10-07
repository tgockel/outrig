# 0003-18 -- A binding's process dies with its owner

## Context

Each binding runs in a process of its own on the host (`hosted-objects.md`): the payload's static
CPython, started by the Rust owner with `0003-16`'s binding program and the binding's packages on
its path. It acts with the host user's authority, and the library it hosts may start programs of
its own -- GitPython, the example library, runs `git` for nearly every call, and `git` may run
hooks. So the property that matters most is that the process stops, with everything it started: at
shutdown, and also when the owner is killed with no chance to clean up. Rust's `Drop` does not run
on `SIGKILL`. `process.rs` guarantees that dropping an `Owned` kills its child, and says nothing
about the child's children.

Linux has `PR_SET_PDEATHSIG`: the kernel sends a chosen signal to a process when its parent dies.
Three of its rules, from prctl(2), decide how it is used (`lifecycle.md`):

- **The parent is the thread that created the process**, not the process. A binding started from a
  tokio blocking-pool thread is signaled when that thread exits after its idle timeout, with the
  owner still running. So bindings are started from a thread that lives as long as the session.
- **It is not inherited.** A forked child starts with it cleared, so only the binding receives it;
  if the signal were `SIGKILL`, the binding's own children would survive. So it is a signal the
  binding catches, and the binding answers it by killing its own process group.
- **It is not sent if the parent is already gone when it is set**, which leaves a race between the
  owner's death and a binding's start.

A process group of its own also keeps a terminal's Ctrl-C from reaching the binding. Ctrl-C sends
`SIGINT` to the terminal's foreground group, and `process.rs` already starts the interpreter's
`podman exec` client in a group of its own for that reason (`Cmd::in_own_process_group`).

The task's second subject is packages. A binding's `requires` (`0003-20`) is installed with the
payload's own pip into a directory the binding imports from, and which the container later mounts
read-only. Only pure Python can load on either side: the payload is a static build with no way to
load an extension module. #465 records that this pip
offers only the bare `linux_<arch>` platform tag, so a package whose PyPI wheels are all compiled
falls back to its source distribution. `--only-binary=:all:` refuses that, and with it any build
code running on the host. It does not refuse a compiled wheel that pip finds compatible, such as a
local one tagged `linux_x86_64`, so each installed wheel's tags are checked as well.

## Goal

A binding process starts on the host from a cached, pure-Python package set, speaks a framed
protocol on its stdio, and is gone -- with everything it started -- when its owner shuts it down or
dies. This is a spike: if an acceptance item cannot be met, the task stops and reports to the
maintainer rather than changing the host process model on its own.

## Deliverables

- **Starting a binding**: the payload's `python3` run on the host with `-I` and `0003-16`'s binding
  program, in a process group of its own, with `PR_SET_PDEATHSIG` set to a signal the binding
  catches, from a thread that lives as long as the session. The binding answers that signal by
  killing its own process group. After setting it, the new process checks that its parent is still
  the owner, and exits if not, which closes the race. A test-only delay between the fork and the
  setting of the signal lets a test kill the owner inside that window.
- **Its environment**: the owner's, inherited, when none is given -- the CLI's case, so the user's
  `ssh-agent`, credential helpers and `git` config reach the library -- and exactly the given one
  otherwise, which an embedder supplies through `0003-20`'s `bind`. `-I` keeps `PYTHONPATH` and
  the other `PYTHON*` variables from changing the binding's own interpreter; the programs it starts
  receive the environment as it is.
- **Framing on its stdio**: NDJSON lines in both directions. RPyC frames tagged by agent go both
  ways; from the binding come event lines, and decision requests, each holding one request until the
  owner answers; from the owner come those answers and control lines. This task proves the
  multiplexing. `0003-21` sends events and control lines, and `0003-22` asks for decisions.
- **Installing `requires`** with the payload's pip into `bindings/<hash>/` under the cache root
  `payload.rs`'s `cache_root` chooses -- `~/.cache/outrig/bindings/<hash>/` by default -- with the
  hash taken over the requirement set. Wheels only (`--only-binary=:all:`), and every installed
  wheel checked to be pure Python: platform tag `any`, ABI tag `none`, and a Python tag that
  includes Python 3, as `py3-none-any` and `py2.py3-none-any` have. A failure names the requirement
  and the reason. The install goes to a staging directory beside its target, under a lock, and is
  renamed into place, as `payload.rs`'s `unpack_once` does, so a second session waiting on the lock
  uses the first one's install.
- **Stopping a binding**: `SIGTERM` to the group, a grace period (fork 3), `SIGKILL` to the group,
  the binding reaped, and a report of whether the group is proven empty -- `kill(-pgid, 0)` finds
  no process. A descendant that left the group with `setsid` is outside what that proves, and the
  report's documentation says so (`lifecycle.md`).
- A crate-private supervisor in `crates/outrig/src/python/` that does all of the above for a
  session's bindings.
- **The spike's record**, in this task's `## Decisions`: the versions involved -- RPyC, the example
  library, Python, git, podman, the kernel and OS, as far as this task used them; where each
  process, mount and credential sits; what ran for real and what was mocked; what went untested;
  and which limits were reached and what happened.

## Acceptance

- **`kill -9` of the owner leaves nothing.** The owner is killed while its binding is in a call
  that started `sleep 1000` as a grandchild, and within 5 s no process remains in the binding's
  group.
- **The start race is closed.** With the test-only delay set, the owner is killed after the fork
  and before the signal is set. The binding finds that its parent is no longer the owner and exits,
  and within 5 s no process remains in its group.
- **A pool thread's exit is not a parent's death.** A binding the supervisor started survives a
  tokio blocking-pool thread exiting after its idle timeout.
- **Ctrl-C does not reach the binding.** `SIGINT` sent to the owner's process group leaves the
  binding running.
- **The framing multiplexes.** Frames for two agents, an event line and a decision request,
  interleaved on one binding's stdio, each reach their reader. The request that asked for the
  decision is held until the test owner answers, and frames for the other agent keep reaching
  their reader meanwhile.
- **A requirement set installs once.** Two sessions started together with the same requirements
  install once, and both import the package.
- **Only pure Python installs.** A local compiled wheel for this platform, which pip would install,
  is refused by the tag check, and a requirement that ships only an sdist is refused by
  `--only-binary`. Each failure names the requirement and says that only pure-Python wheels are
  installed, and neither leaves a directory in the cache.
- **Shutdown escalates.** A binding that ignores `SIGTERM` is killed with its group after the
  grace, and the report says the group is empty.
- e2e: the cache directory, mounted read-only into a container, imports there with the payload's
  interpreter.
- e2e, with network: GitPython 3.2.0 installs, as the realistic case, and through `0003-16`'s test
  relay the interpreter reads `head.commit.hexsha` from a `Repo` the binding process built over a
  test repository.
- `crates/outrig/public-api.txt` is unchanged.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **What a requirement may name -- Recommended: requirement specifiers resolved from an index -- a
   name, extras, a version specifier, markers -- with a path or a URL refused.** The cache is keyed
   by the requirement text, so a path whose file changes, or a URL whose target moves, would keep
   serving the first install. And `0003-20` approves a repository's binding by a digest of its
   declaration, which would not cover what a path names. Tests reach their local wheels through
   pip's own `--find-links`, set for the install. The cost: a library on no index has to be put on
   one, which a local directory index satisfies.
2. **Whether the install reads the user's pip configuration -- Recommended: yes.** `PIP_*`
   variables and `pip.conf` carry the index URL, proxy and certificate settings a network may
   require, and without them an install behind a proxy fails. The cache key stays the requirement
   set, so changing the configured index reinstalls nothing; removing the cache directory does.
3. **The grace between `SIGTERM` and `SIGKILL` -- Recommended: 5 s, the time `host.rs` gives the
   interpreter's exec client to exit (`EXIT_GRACE`), counted after the drain deadline rather than
   against it.** `lifecycle.md` leaves both open. Counting it after the deadline keeps a draining
   call from losing time to the stop. The cost is that `shutdown` can return up to the grace past
   its deadline, before the container stop's own time is added.

## Dependencies

- **Hard: 0003-16.** The binding program this task starts and supervises.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- where a binding runs, its packages, and its
  environment.
- `plan/phase/0003-python/lifecycle.md` -- descendants, the owner's abrupt death, and what a
  process group does not cover.
- `crates/outrig/src/process.rs` -- what owning a child guarantees, and
  `Cmd::in_own_process_group`.
- `crates/outrig/src/python/payload.rs` -- `unpack_once` and `cache_root`, whose lock, rename and
  cache rule the install repeats.
- #465 -- the agent's own pip, which should share the tag
  check.
- #295 and `plan/next/hosted-effect-confinement.md` -- why binding hosts are Linux-only, and
  confining what a binding's programs do, deferred.
