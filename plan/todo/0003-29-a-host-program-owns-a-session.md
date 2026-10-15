# 0003-29 -- A host program owns a session

## Context

`0003-27` runs a program on the host and `0003-28` gives the engine a session with no primary
agent. This task joins them: the program writes `async with outrig.Session(...) as session:`, the
engine starts a no-primary session for it, the program observes the session's events, closes
admission, and reads `session.shutdown_report` after exit -- or catches
`outrig.ShutdownException` when the engine could not prove the session stopped. The Python package
is a client of the engine (R2): every resource is the engine's, every control is applied by it
and acknowledged after it applied, and the program owns only its own control flow.

Three facts of the Rust side shape it. Subscriptions exist only on the builder, before start, so
the program's subscription is made when its session is built and observation begins at
`starting`. `shutdown(deadline)` bounds the drain, and the steps after it -- interrupting kernels,
stopping the bindings group, stopping the container, waiting for the interpreter's exit -- have
fixed bounds of their own, so a deadline is not the whole shutdown's wall-clock bound. And the
report (`crates/outrig/src/harness/report.rs`) is what the program reads, as a Python mirror; it
never depends on the program having consumed the event stream, which is lossy by design.

The program can also end without cleaning up: it returns with a session open, raises, is killed,
or ignores a signal (R6). The engine survives it and owns the cleanup: close admission, shut down,
print the report. The approved direction for a program that does not respond is cooperative
interruption, a bounded and configurable grace, then SIGTERM to the owned process group, a
termination grace, then SIGKILL; containers stop through podman and are force-removed when
needed. Five seconds is the proposed default for the cooperative grace, not a fixed figure.
Cleanup after the engine itself is killed stays outside this task, as it is outside the phase.

## Goal

A program enters a session, watches it, closes it and reads its report, with `ShutdownException`
raised in the program's task when stop was not proven; and the engine shuts down every session
the program leaves open, however the program ended.

## Deliverables

- **The owner protocol.** NDJSON over the two inherited descriptors (`0003-27`): a versioned
  greeting that fails the launch before the program runs on a mismatch; requests answered by id;
  events pushed; and every control acknowledged only after the engine applied it, never on
  queueing. The kinds this task needs are the configuration view, `session.start`,
  `admission.close`, `session.shutdown` and the event push; `0003-35` and `0003-36` add theirs.
  The Rust side is a module beside `harness` that owns the program's process and its sessions.
- **`outrig.Session(*, workspace=None, mounts=(), grace=None)`** records and type-checks its
  inputs and starts nothing. `__aenter__` starts a no-primary session: an explicit `workspace`
  overrides the configured one; omitted, the configured `[workspace] host-path` applies when
  declared; with neither, no workspace is mounted. `mounts` are added to `[[workspace.mounts]]`,
  and a repeated host or container path is an error. Factory bindings come from the configuration
  under the same-paths rule; a repo-declared one is refused with `binding.refused`, since the
  program supplies no approver (`plan/next/host-program-approves-repo-bindings.md`). Events are
  recorded under `[events] mode = "record"` into the session directory under the session root, as
  `run-new` records them; policy is the configuration's; the engine ensures the image as `run-new`
  does. Entry returns the session and sets the current-session `ContextVar`; exit restores the
  token, on exceptions too. A failed entry stops what it started and raises, and `__aexit__` is
  not called, as Python has it.
- **`session.events()`**: one async iterator per session (fork 3), yielding records with `type`,
  `subject`, `time` and `data` as decoded JSON, and `Missed(count)` records where the subscription
  fell behind. The subscription is registered when the session is built, so the first record is
  `session.state` `starting`; iteration ends after `reported`, once the retained records are
  drained. The stream's loss semantics are the program's to read in `help()`: a slow consumer
  loses counted records and never slows the session.
- **`await session.close_admission()`**, acknowledged after the gate closed.
- **`__aexit__`** closes admission and shuts down with `grace` as the drain deadline,
  `DEFAULT_DRAIN` when `None` (fork 5); sets `session.shutdown_report`, a frozen dataclass mirror
  of `ShutdownReport` with its `verdict` (fork 4); and raises `outrig.ShutdownException` -- an
  `Exception`, not an `AgentError` -- when the verdict is not proven stopped. The report is set
  before it raises, and a body exception is the new exception's `__context__`, so both survive.
- **`outrig.current_session()`** returns the current session or raises the documented error;
  `0003-35`'s constructor reads it.
- **The program exits or dies with a session open.** The engine observes the connection's end or
  the process's exit, closes admission on each open session, shuts each down with the default
  grace, prints each report to stderr, and exits per fork 1. A death during `__aenter__` takes the
  start's own failure path, which stops what it started.
- **Escalation (R6).** A forwarded SIGINT or SIGTERM starts the cooperative grace per fork 2,
  configurable through a key under a new `[host]` table and a `--grace` option before the script,
  default five seconds. Past it the engine sends SIGTERM to the program's group, waits a
  termination grace, sends SIGKILL, and then cleans up the program's sessions as above. A second
  signal escalates at once. Containers stop through the engine's podman stop and force removal as
  today. The grace is documented as a bound on the program's cooperation, not on the whole
  shutdown.
- **Sequential and concurrent sessions** in one program: each has its own container, interpreter,
  bindings process and report, and the engine's process-wide state -- the container panic hook,
  the reaper thread, the supervisor's spawning thread -- serves several at once.
- `lifecycle.md`'s rows for the host program and `embedding.md`'s "A Python program as owner"
  are what this task makes true.

## Acceptance

- Construction starts nothing: no podman call until entry. Entry returns a session with no
  primary; normal exit sets `shutdown_report` with a clean verdict; a body that raises
  `ValueError` still shuts down, the `ValueError` propagates, and the report is set.
- **A stop the engine cannot prove raises in the program.** With the test hook `0003-19` used to
  force a not-proven report, `__aexit__` raises `ShutdownException` in the program's task, the
  report is readable in the handler, and with a body exception too, the exception's `__context__`
  is that body exception.
- A bad image fails entry, leaves no container, and `__aexit__` is not called.
- **The `ContextVar`.** Set on entry and restored after exit, on exceptions too; nested sessions
  restore the outer; a task created inside the context sees the session; `current_session()`
  outside every session raises the documented error.
- **Events.** `events()` yields `session.state` `starting` first, then `idle`; a consumer that
  stops reading while more events are published than the queue holds resumes to `Missed(n)` with
  `n > 0`; after exit the iterator yields what it retained and ends; a second `events()` raises.
- `close_admission()` returns after the gate closed: a fixture binding's call made afterward from
  a kernel the test opens is `refused`, admission closed.
- `shutdown_report` lists a fixture binding's group empty and the hosted calls that ended after
  the close, whether or not `events()` was consumed.
- **The engine cleans up after the program.** A program that returns with a session open: the
  engine shuts the session down, prints the report, leaves no container and exits with fork 1's
  status. The same for an unhandled exception and for `kill -9` of the program, each within a
  stated bound; `kill -9` during entry leaves no container.
- **Signals.** The R6 example exits 0 after SIGINT, printing the report itself. A program that
  ignores SIGTERM and holds a session is killed after the grace, and its session is cleaned;
  `--grace` changes the wait; a second SIGINT ends the wait at once.
- Two sessions entered concurrently in one program each start, each runs a kernel the test opens
  over the protocol, and each stops with its own report; two sequential sessions work.
- `[events] mode = "record"` writes `events.jsonl` under the session root with `0003-13`'s
  content and permissions; `off` writes nothing.
- **Workspace rules.** `[workspace] host-path` is mounted when `workspace` is omitted; with neither
  declared, no workspace is mounted and a kernel's cwd is the container's default; an explicit
  path is mounted at its host path when a factory binding is present; a repeated mount path is an
  error at entry.
- `crates/outrig/public-api.txt` regenerated for whatever the library exposes;
  `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The exit status after the engine cleaned up -- Recommended: the program's own status, unless
   a report is not proven stopped, where 3 outranks it**, as `run-new`'s interpreter-death rule
   has it. The alternative always exits 1 when the engine had to clean up, which hides the
   program's status.
2. **What starts the cooperative grace -- Recommended: the first forwarded signal, as R6 reads**,
   with the grace configurable and a second signal escalating at once. The alternative starts it
   only on a second signal, which never kills a program that handles the first slowly, and never
   ends one that ignores it either.
3. **One subscription per session -- Recommended: yes**, registered when the session is built; a
   second `events()` raises. Several would each need registering before start.
4. **The Python report -- Recommended: a frozen dataclass mirror with `verdict`**, documented by
   `help()`. The alternative is the report's JSON as a dict.
5. **`grace` is the drain deadline -- Recommended: yes**, with the fixed termination bounds after
   it as today, and documented as such.

## Dependencies

- **Hard: 0003-27.** The program, its supervision, the forwarded signals and the owner
  descriptors.
- **Hard: 0003-28.** The session with no primary agent.
- **Hard: 0003-19, 0003-22.** The stream, the report and the relay's close.

## See also

- `plan/phase/0003-python/embedding.md` -- "A Python program as owner".
- `plan/phase/0003-python/lifecycle.md` -- "The host program" and the report.
- `plan/phase/0003-python/observability.md` -- the subscription and its losses.
- `crates/outrig/src/harness/report.rs`, `session.rs` -- the report and `shutdown`.
- `crates/outrig-cli/src/cli/run_new.rs` -- the status rule this task's fork 1 follows.
- `plan/next/host-program-approves-repo-bindings.md` -- the approver a program lacks.
