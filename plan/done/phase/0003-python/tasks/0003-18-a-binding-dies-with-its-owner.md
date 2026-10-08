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

## Decisions

Every acceptance item was met, so the spike did not stop. The three forks were taken as
recommended: requirement specifiers only, with a path, a URL or a pip option refused before pip
runs (fork 1); the user's pip configuration read, with no `--isolated` (fork 2); and a grace of
5 s between `SIGTERM` and `SIGKILL`, exported as `STOP_GRACE`, which `stop` takes as a parameter
so that `0003-19` and `0003-21` can count it after the drain deadline (fork 3).

- **The supervisor is `crates/outrig/src/python/supervisor.rs`, the install
  `crates/outrig/src/python/install.rs`**, both crate-private and, until `0003-20` calls them
  from a session, allowed to be dead code outside tests. The binding program's bootstrap moved
  from the test relay into the supervisor as `PROGRAM`.
- **The parent-death signal is `SIGHUP`, and `SIGTERM` keeps its default disposition in the
  binding.** The binding answers `SIGHUP` by killing its own group -- `SIGTERM` to the group, one
  second, `SIGKILL` to the group, itself included -- once, however many times it is triggered,
  since the owner's death arrives as the signal and as the end of stdin together. With `SIGTERM`
  as the death signal the binding's escalation would have run at every orderly stop too, and its
  own grace, not the owner's, would have decided when the group ended. End of input on stdin runs
  the same group kill: a second detector of the owner's death, and the fix for `main` exiting on
  EOF with its group left running. `DEATH_GRACE` is 1 s rather than the owner's 5 s: nobody is
  waiting, and `sleep` and `git` leave on `SIGTERM` at once; it is what keeps the acceptance's
  5 s bound with margin.
- **The signal is set from Rust between fork and exec, blocked until Python has its handler.**
  `ctypes.CDLL(None)` fails in the static payload ("Dynamic loading not supported"), so `prctl`
  cannot be called from Python. The `pre_exec` hook blocks `SIGHUP`, sets `PR_SET_PDEATHSIG`,
  compares `getppid()` with the owner's pid and exits with status 3 if the parent changed; the
  binding's `main` installs the handler and unblocks. std's fork path runs `setpgid` before the
  hooks and inherits the signal mask into the exec'd program, which both depend on. Probed with
  the payload Python before building it: a `SIGHUP` blocked before exec and sent before the
  handler existed was pending after exec and handled the instant the unblock ran; a handler ran
  within milliseconds while the main thread was blocked in `readline()` with other threads busy.
- **One process-global spawning thread, `outrig-bind`**, started on first use as `supervise.rs`'s
  reaper is, with only a successful start remembered. It spawns under the requesting runtime's
  `Handle`, so the child's pipes and reaping belong to that runtime, and each request runs under
  `catch_unwind`, since the thread's end would signal every binding it started. A per-session
  thread was rejected: dropping a session object early would have killed every binding, and the
  global thread gives the guarantee the design asks for, a thread that outlives every session.
- **`tokio::process::Command` directly, not `Cmd::spawn_owned`**: the program is a path chosen at
  run time, the spawn needs a `pre_exec` hook and a process group, and the child is signaled as a
  group, which `process.rs` never does for the children sharing the owner's group. The second
  exception to that module's rule, beside `spawn_stdio`, with the same promise: dropping a
  `Binding` sends `SIGKILL` to its group before the drop returns.
- **Only `ESRCH` from `kill(-pgid, 0)` proves a group empty.** A killed process stays in its group
  as a zombie until its parent reaps it -- probed: a group whose one member was a zombie answered
  the probe as alive -- so the stop reaps the binding itself and leaves its children to init,
  which under systemd takes about two milliseconds; the report polls up to 2 s after `SIGKILL`
  and, when the proof fails, lists the survivors from `/proc/*/stat` with their state letters,
  so a `Z` tells the maintainer "unreaped" and a `D` "uninterruptible". On a host whose PID 1 does
  not reap orphans the proof cannot succeed, and the report says so rather than guess; making
  the owner a subreaper was rejected as the process-model change a spike reports rather than
  makes. The final `SIGKILL` goes to the group before the binding is reaped, while its unreaped
  process still anchors the group's id, so the number cannot have been handed to another group.
- **The stop's shape**: `SIGTERM` to the group; until the grace passes, reap the binding once it
  exits and watch for `ESRCH`, returning with `killed: false` as soon as the group is empty; at
  the deadline `SIGKILL` the group, reap, wait for `ESRCH` up to `EMPTY_WAIT`. A well-behaved
  binding stops in milliseconds; one that ignores `SIGTERM` costs the grace.
- **The install.** The lock, stage and rename are `payload.rs`'s, factored into `locked_once`;
  the directory is the first 16 hex digits of blake3 over the trimmed, sorted, deduplicated set;
  a requirement is checked against a PEP 508 subset before pip runs, which refuses paths, URLs,
  direct references and options in one rule; pip runs with `-I`, `--only-binary=:all:`,
  `--target`, `--no-input` and `--progress-bar off`, in the owner's environment plus
  `PIP_REQUIRE_VIRTUALENV=0` -- a `require-virtualenv` in a developer's configuration would
  otherwise refuse every install into a cache directory -- and the caller's variables, which
  tests use for `PIP_NO_INDEX`, `PIP_FIND_LINKS` and `PIP_CONFIG_FILE=/dev/null`. The tag check
  reads each installed `WHEEL`'s `Tag:` lines together -- `bdist_wheel` writes a compressed
  `py2.py3-none-any` as one line per Python tag, so judged one by one the `py2-none-any` line
  would refuse every universal wheel, `six` included -- requiring every line to be `-none-any`
  and one of them Python 3, and names the distribution, the requirement it came from by
  canonical name or "a dependency of the set", and the offending tag; it is joined by a scan for
  `.so` and `.pyd` files, since a tag is the publisher's claim and the payload cannot load one
  either way. pip's own `No matching distribution found for X` line is lifted into the
  error, which is how an sdist-only requirement is named. An empty set installs nothing and
  yields an empty directory. Probed: pip 26.2.1 installs a hand-built `cp313-cp313-linux_x86_64`
  wheel into `--target`, so the tag check is what refuses one.
- **The wire, beside `0003-16`'s `rpc` lines.** From the binding: `{"t":"event", ...}` with the
  payload's keys at the top level, and `{"t":"decision","id":N, ...}`, each holding its serving
  thread -- and the request it serves -- until `{"t":"decision","id":N,"answer":...}` arrives
  from the owner. From the owner, `{"t":"control", ...}` is read and routed to a function that
  notes it and nothing more, until `0003-21` gives it the closed flag. The binding's primitives
  are `_event` and `_decide`; the test fixture calls them through `__main__`, as the interception
  will from inside the program.
- **The supervisor's handle**: `start(Spec) -> Binding`, with the `rpc`, `event` and `decision`
  receivers taken once each, bounded at 64 lines until `0003-21` chooses the bound a session
  relies on; a line a dropped receiver cannot take is discarded, so a binding never blocks on a
  pipe nobody reads; a line over 1 MiB is discarded with a warning, as `host.rs` discards one
  from the interpreter; and stderr is logged line by line with the last twenty kept for the
  error a failed start reports, which names the factory and carries the traceback.
- **The test relay runs on the supervisor.** Every binding `relay.rs` starts goes through
  `start`, its `rpc` lines through the supervisor's receiver and its stdin through the
  supervisor's sender, so all 55 tests of `0003-16` and `0003-17` exercise the spawn, the
  routing and the stop. A binding inherits the test's environment unless extras are given, which
  compose an exact one, or an exact environment is given outright. The relay owns a small
  multi-thread runtime, stops its bindings on it before it drops, and keeps its std pump threads.
- **The owner as a process of its own.** The tests of the owner's death re-execute the test
  binary on one `#[ignore]`d test, `owner_process`, with a role in `OUTRIG_TEST_OWNER_ROLE` and a
  directory in `OUTRIG_TEST_OWNER_DIR` where it reports -- files, not stdout, since libtest owns
  stdout -- in a process group of its own, so `SIGINT` to that group reaches nothing else. The
  race test's child appends `forked <pid>` to a probe file before its two-second delay and the
  verdict after the check, with `open`, `write` and `close` alone; the test kills the owner
  between the two lines. The Ctrl-C owner installs a no-op `SIGINT` handler rather than
  `SIG_IGN`, which exec would have carried into the binding and made the test vacuous.
- **The spike's record.**
  - Versions: RPyC 6.0.2 from the pinned wheel; GitPython 3.2.0 from PyPI, with gitdb 4.0.12 and
    smmap 5.0.3, all `py3-none-any`; the payload's CPython 3.13.15 (`python-build-standalone`
    20260901, x86_64, `+static`) and its pip 26.2.1; git 2.43.0; podman 4.9.3 with
    `docker.io/library/alpine:latest`; Linux 7.0.0-38-generic, Ubuntu 24.04.5, systemd as PID 1;
    Rust 1.99.0, tokio 1.53.1, nix 0.31.3.
  - Where things sit: every binding on the host as a child of the test binary -- or of the owner
    helper -- in a process group of its own, started from the `outrig-bind` thread; the
    interpreter on the host as before, in its own group; the install cache under a temporary
    root for the tests and `~/.cache/outrig/bindings/<hash>/` in production; GitPython's wheels
    through the user's pip configuration from PyPI; the container test mounts the install
    directory read-only at its host path beside the payload in an alpine container. No credential
    of any kind is involved; the GitPython binding reads a repository the test made.
  - Real: the supervisor's spawn, routing and stop, the parent-death signal and the race check,
    the binding's group kill on the signal and on EOF, the install from local wheels and from the
    index, the tag check, the mount, and a `Repo` over a real repository answering through the
    relay. Stood in for: the Rust relay in `host.rs`, by the test relay; the session's config,
    builder and shutdown, by the tests calling `start`, `install` and `stop` themselves
    (`0003-19`, `0003-20`); hosted-call events, by the fixture's `event` method.
  - Untested: aarch64; a binding dying mid-session (`0003-21`'s fork 3); a descendant that leaves
    the group with `setsid`; a host whose PID 1 does not reap orphans; the pending-signal path
    inside the Rust hook -- the race test exercises the parent-changed branch, and the pending
    delivery was probed with the payload Python by hand; a stop while a `--serialize` lock is
    held, which changes nothing about signals; the bounded queues under a binding that stops
    reading, which `0003-21` owns.
  - Limits reached: the race window held at a two-second delay with the owner killed inside it;
    the stubborn binding and its `trap "" TERM` child survived `SIGTERM` and died at the 500 ms
    grace with the group proven empty; a grandchild `sleep 1000` died with its owner's death
    within the 5 s bound, the binding's own second of grace included; a 64-line queue each way
    carried every test's traffic, the 64 MiB result included; and pip refused the sdist with
    `No matching distribution found`, as read from its output.
- **Simplified after review.** `/simplify` generated independent versions of the supervisor,
  the install and the binding program's additions and compared each with the original. The
  supervisor took the alternative's shape -- the greeting read inline before the router starts,
  a boxed job in place of a request struct, a closure in place of a hook type, the child kept
  with a `reaped` flag rather than taken out -- which also fixed a defect the review found: the
  original took the child out of the handle at the start of `stop`, so a `stop` future dropped
  mid-way would have killed the binding alone and left its group running; kept from the
  original were the retried thread start, the `catch_unwind` around each spawn, the warnings on
  a failed read or write, the truncated quotes in diagnostics and the field documentation. The
  binding program took the alternative's: one `queue.SimpleQueue` per decision, popped by the
  reading thread, which also closes a window in which a second answer could have overwritten
  the first; a lambda for the handler; and no guard on `killpg`, which cannot fail for a process
  signaling its own group. The install kept the original, with the alternative's slice-pattern
  tag parser, its stricter version-specifier grammar (so `foo==` and `foo==1.0 --hash=...` are
  refused before pip runs), one scan of the `.dist-info` directories, and `split_once` on the
  stem, which is how `importlib.metadata` reads it; the alternative's author found the two-line
  tag fact above by installing `six`.
- **Six findings of an external review of the landed commit, each fixed with a test:**
  - A `start` dropped while the factory ran -- under a timeout, say -- dropped a bare `Child`
    before any `Binding` existed, and `kill_on_drop` ends the leader alone, so a program the
    factory had started lived on. The handle is now built, with its group kill armed, as soon
    as the child exists, and the spawning thread kills the group itself when the requester is
    gone by the time the child is made.
  - A decision payload's own `id` or `t` took the envelope's place, so an answer to the
    advertised id freed nobody, or another caller. A payload may name neither.
  - A decision line past the owner's 1 MiB line bound was written and dropped unread, leaving
    its caller waiting for ever. The line is encoded and checked against the bound before the
    waiter is registered, and a write that fails takes the registration back.
  - A bare archive name -- `pure-1.0-py3-none-any.whl`, markers or not -- passed the specifier
    check, and pip reads such an argument as a file in its working directory, which the
    text-keyed cache would then have served after the file changed. pip's archive suffixes are
    refused as names.
  - `PIP_USER=1`, or `user = true` in the user's pip configuration, makes pip refuse
    `--target`; `PIP_USER=0` is set beside `PIP_REQUIRE_VIRTUALENV=0`.
  - Under `nohup` the pool-thread test's control process inherited an ignored `SIGHUP` and
    outlived the thread, failing the test with the supervisor right; the control now resets
    the disposition before setting the signal. The binding program is unaffected: it installs
    a handler, and a blocked signal is queued whatever its disposition.
- **Five more from the review's second pass, each fixed with a test:**
  - A decision payload the owner's JSON reader would not take -- nested past its 128 levels, an
    integer past 64 bits -- passed the byte bound, was dropped unread, and left its caller
    waiting. The payload is walked before the waiter is registered: JSON's types only, string
    keys, 64-bit integers, finite floats, and at most 32 levels.
  - The archive check read the name alone, and pip reads the whole argument before its markers
    as a filename when it ends in an archive suffix, so `probe===1.0.tar.gz` named a file in
    pip's working directory. The whole argument is checked, a trailing `[extras]` stripped as
    pip strips it.
  - A child whose reply had been queued, with its requester's `start` dropped before reading
    it, was dropped alone by the oneshot. The child now crosses from the spawning thread inside
    a value whose drop kills the group, and leaves it only into the handle.
  - A `root` or a `dry-run` in the user's pip configuration gave a clean exit and an empty
    stage, which would have been published and served from then on. `PIP_ROOT=/`,
    `PIP_DRY_RUN=0` and `PIP_NO_DEPS=0` are set beside the earlier overrides -- `--prefix` fails
    loudly on its own -- and pip's `--report` is read afterwards: a distribution it says it
    installed must be in the stage, while a report listing nothing, every marker false, is a
    complete empty install.
  - The cancelled-start test built its `Spec` inside the runtime, where the fixture paths'
    first unpack starts a runtime of its own and panics; run alone, cold, it failed. Both
    tests that build a `Spec` under a runtime now build it first.
- **Two from the review's third pass, each fixed with a test:**
  - A string the host made from bytes that are not UTF-8 -- `os.fsdecode` of such a file name
    -- holds a lone surrogate, which `json.dumps` escaped and the owner's reader refused,
    dropping the line with its caller waiting. The envelope is encoded as UTF-8 before the
    waiter is registered, so such text raises in the caller instead.
  - `--only-binary=:all:` constrains what pip resolves from an index. A wheel on an index of
    one's own, or in a `find-links` directory, may declare a dependency by URL on a source
    archive, which pip fetches and whose build backend it runs to read its metadata: measured
    with the payload's pip 26.2.1, a wheel depending on an archive whose in-tree backend wrote
    a mark left the mark under `--only-binary=:all:`. pip now runs as a program of its own that
    disables its source-distribution step for the run, so any such candidate fails with the
    reason before anything of it runs. The seam is pip's internal `SourceDistribution` class;
    the payload pins pip, and the test with the marked backend holds the seam to its job. PyPI
    itself refuses a distribution that depends by URL, so an index of one's own is where this
    would arise.
- **One from the review's fourth pass, test-only:** the test wheels' directory reached pip as
  `PIP_FIND_LINKS`, which pip splits on whitespace, and as an unescaped path in `needs_url`'s
  dependency URL, so a `TMPDIR` with a space in it failed the install tests. Both are
  percent-encoded `file://` URIs now, and the suites were run under such a `TMPDIR`.
- **Filed in `plan/next/`:** `binding-diagnostics-are-not-evented.md` -- a binding's own
  diagnostics and its library's output reach the log and no event, where the interpreter's
  reach both.
