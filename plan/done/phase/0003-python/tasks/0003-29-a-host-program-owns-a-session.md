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

## Decisions

The five forks were taken as recommended: a left-open session not proven stopped makes the exit 3,
and otherwise the status is the program's; the first forwarded signal starts the grace; one
subscription per session; a frozen dataclass mirror of the report with its `verdict`; `grace` is the
drain deadline.

- **A death during entry interrupts the start, and the engine waits for its cleanup** (the
  maintainer's ruling, 2026-10-20). Two readings of "takes the start's own failure path" were
  first offered and both set aside: let the start finish and then shut it down, which holds
  `outrig` for a pull, a build or a launch; or drop the start and leave its cleanup to drop paths
  nobody watches. The start is dropped at once, and the cleanup is awaited.
  - Each step already arms the cleanup for what it creates before creating it: the container's
    removal by its attempt label, buildah's working containers (`0002-50`), the network
    attachment's rollback, a `Supervised`'s group kill, the exec client's `kill_on_drop`. That
    detached removal is a process of its own and finishes after `outrig` exits. What was missing
    was the engine knowing when, and whether.
  - `supervise::Tracked` wraps the start and makes its `Owed` the owner of the cleanup the
    thread starts while it polls the start -- where a failure path drops what it made -- and
    while it drops it. Each piece started then is owed to it: a reaper command, held by a `Debt`
    until the reaper is done with it and failed with why when it gave up; a `Supervised` dropped
    unsettled, a check of its group the wait makes until the group is empty, failed after 10 s
    naming what is left; blocking work, through `supervise::blocking`, until it returns. A `Debt`
    dropped by a panic counts as failed. `Owed::settled` waits at most 90 s.
  - The bindings process is spawned on the supervisor's parent thread, so a start dropped while
    waiting for the spawn has the child dropped there: `spawn_supervised` carries the owner to
    that thread and holds a `Debt` until the job has run.
  - A first version took two marks of a process-wide counter around the drop, and failed under
    parallel tests: another test's failed cleanup fell inside the window. Destructors and polls
    run on one thread, so an owner per thread is exact. `/simplify` replaced a second version, a
    numbered process-wide ledger, with this one: a ledger per owner, and no work for a dropped
    group when nobody waits.
  - The session is then reported as `SessionEnd::NotStarted(Stopped)`. A cancelled `__aenter__`
    interrupts the start the same way, through the `session.shutdown` it sends. A start that fails
    on its own is settled the same way before its error is answered; when its cleanup is not
    proven, the error says so. Both kinds of start that did not finish take one path, which
    answers commands while it waits: a `session.shutdown` that arrives then -- an entry the
    program gave up on as the start failed -- holds the program's grace, and is answered with
    whether what the start made is stopped.
  - Such a start is recorded when the program was not told of its end, or when what it made is
    not proven stopped, so `outrig` exits 3: told, the program owns its end, but a stop not proven
    outranks that, and the Python side has no exception to raise for it on a cancelled entry.
- **A dropped pip install stops pip.** A dropped install ran on in `spawn_blocking`: had `outrig`
  exited meanwhile, pip would have run on orphaned and its stage directory stayed in the cache.
  `run_pip` polls pip, and kills and reaps it once nothing awaits the install. Its output goes to
  an unlinked file, since a pipe nobody reads would hold pip once full. The payload's unpacks are
  tracked too; they finish in moments and do not stop early.
- **`podman run` passes `--stop-timeout 2`.** Measured on this host: `podman rm -f` of a container
  whose PID 1 is `sleep infinity` took 10.6 s with podman's default and 2.3 s with this. A guard's
  removal and the panic sweep name no grace, so they waited the default; now they wait what the
  orderly stop's `-t 2` already waits. `podman create` containers run their own entrypoint and are
  unchanged.
- **The grace pauses while a shutdown the program asked for runs**, and while the cleanup of an
  entry it gave up on is awaited. Without that, the documented SIGINT example would be sent
  SIGTERM inside its own `__aexit__` under the default five seconds: the drain alone can take five,
  and the stop steps follow. The pause ends when the shutdown returns, not once the program has
  read its last events, so a program that stops reading cannot hold it. The grace is counted down
  in 100 ms ticks while no such wait runs. Only the engine knows when one does, so the
  escalation's timing lives in `owner/program.rs`, and the CLI maps each signal to `interrupt` or
  `terminate`; the library still installs no handler. Each further signal moves one step: the
  grace, then `Supervised::stop(STOP_GRACE)` -- `SIGTERM`, the bindings process's five seconds,
  `SIGKILL`, the group proven empty -- then `SIGKILL` at once. After a normal exit the program's
  group is left as `0003-27` left it.
- **`[host] grace-secs`**: default 5, at most 3600 (`HostGraceSecsTooLarge`), 0 stopping the group
  at the first signal; a repo's value wins, as for the top-level keys. Duration keys spell their
  unit (`plan/next/duration-keys-humantime.md`). `outrig run --grace <secs>` overrides it and
  `requires` a script.
- **Protocol v1.** The greeting is the program's first request, id 0, answered before the script
  runs; another version is refused with an `error` and the program's group killed, and
  `HostProgram::start` fails, so `start` now returns once the program has connected (30 s bound).
  Replies carry `id`; pushes never do, and go to the stream of the session they name. `H`, the
  program's number for a session, lets pushes route before the start answers. Every request is
  answered, commands queued for a session whose task ended included. A session's events are
  pushed once its start is answered, from `starting` on, which its subscription holds until then;
  a start that did not finish pushes only `events.end`. Every push for a session precedes the
  answer that ends it: the task waits for its event pump first.
- **Nothing waits on a program that is gone.** Lines are written under one mutex, each write
  also watching `gone`, so a grandchild holding the program's read end cannot hold up the stopping
  of what the program left. `admission.close` during a shutdown is applied at once through a
  `Closer`; a second `session.shutdown` gets the same report. A report counts as received once its
  answer was written before the program was gone; otherwise it is in `ProgramExit::left_open`, and
  the CLI prints it.
- **`/simplify` on the engine side** compared an alternative in one file, with no queue or writer
  task, no pushes before the start is answered, and a ticking grace. Its writes did not watch
  `gone`, its greeting had no bound, and it answered `admission.close` late during a shutdown, so
  this design stayed, and took those three simplifications from it.
- **Session directories.** `<session-root>/<id>/logs`, the session's directory created exclusively,
  the id minted again on a collision: ids are second-resolution and sixteen random bits, and two
  sessions started together share the second. The CLI passes its resolved root -- `--session-root`,
  `session-root`, the XDG default -- as `Config::session_root`. No `session.json`: session records
  stay in the CLI (`harness-components.md`); `plan/next/host-program-sessions-are-listed.md`.
- **The image** is `default-image`, ensured as `run-new` ensures one. With none, the CLI injects
  the built-in default through `builtin_image::primary_for`, now shared with `run-new`, and sets
  `default_image` to it, printing no note: `run-new`'s names `--image`, which a program does not
  take.
- **Paths are absolute on the wire.** Python makes them absolute against its working directory
  when the session is made, `..` kept, and the engine refuses a relative one. They are checked
  before anything starts by `validate_session_paths`: a workspace that is a directory; container
  paths absolute, not `/`, and unique, the workspace's reserved only when one is mounted, so a
  session without one may mount at `/workspace`; host directories mounted once, the workspace's
  included (`ConfigValidationError::WorkspaceMountHostDuplicate`, new); and, with a binding, each
  directory at its host path, the rule a configuration meets at its load
  (`check_binding_paths`). The start checks that last rule too, but only once the bindings'
  factories have run, which a program's mount must not reach.
- **Python.** `_host.py` holds the transport and the API; eight names are host-only. A request is
  a `concurrent.futures.Future` the reading thread settles, which a coroutine awaits through
  `asyncio.wrap_future` -- `/simplify` replaced a second module and a settle callback of its own
  with that. A request is encoded as UTF-8 before it is sent, so a path that is not UTF-8 raises
  `UnicodeEncodeError` at entry instead of reaching the engine as a line it cannot read. The
  greeting runs with `SIGHUP` still blocked, so a failed one exits 1 quietly. A request longer
  than the engine's 64 KiB line raises `ValueError` before it is sent: the engine discards such a
  line with no id to answer by. An entry that sends nothing ends the session's events itself, as
  the engine's `events.end` would. A `grace` is checked against the engine's hour when the
  session is made, since a shutdown the engine refused would leave the session running. A refused
  entry and a broken connection raise `RuntimeError` with the engine's text; no class of their
  own yet.
  `current_session()` raises `RuntimeError`, as `asyncio.get_running_loop()` does; `LookupError`
  would read as a missing key. A session is entered once, a failed entry counting. Once a
  shutdown is sent, the program waits for its answer through cancellation and re-raises the
  cancellation after; a not-proven verdict outranks any exception. A cancelled entry whose start
  finished first keeps that session's report. `Event.type` has no `org.outrig.` prefix, as Rust's
  `kind()` has none; `time` is an aware UTC `datetime`. The program's queue holds 4096 records,
  drops the oldest and reports merged gaps in place, and the mirror's `events.missed[0]` adds the
  program's own drops to the engine's. `session.id` is there to find the session's directory and
  to match the reports `outrig` prints.
- **Test hooks.** Sessions on the host are keyed by the program's number for them.
  `SessionBuilder::fail_stop` makes `Session::shutdown` itself report not proven, so the event,
  the verdict and the answer agree. A progress hook holds a start part way with `block_in_place`:
  a plain blocking wait there stalled its runtime worker, and the test could not see a flag
  raised before it for twenty seconds. `program_tests.rs` starts programs through `HostProgram`,
  since the bootstrap now waits for the greeting; its two tests that drive the bootstrap by hand
  answer the greeting themselves. `harness/program.rs` moved into `owner/`; the public paths are
  unchanged.
- The CLI's `run_program.rs` assertions on stderr fail under a developer's `RUST_LOG`, as before
  (`plan/next/run-program-tests-read-rust-log.md`); the suites here were run with it unset.
