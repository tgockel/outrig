# 0116 -- An internally owned subprocess dies when its future is dropped

## Context

`crates/outrig/src/process.rs` is the one place outrig spawns external binaries -- podman,
buildah, skopeo, git, and the namespace tooling all route through it. None of its helpers
configures `kill_on_drop`:

| Helper                | Site                | Shape                                     |
| --------------------- | ------------------- | ----------------------------------------- |
| `try_capture`         | `process.rs:141`    | `Command::output()`                       |
| `try_capture_logged`  | `process.rs:171`    | `spawn()` + `wait()`                      |
| `run_capture`         | `process.rs:216`    | `spawn()` + `wait()`                      |
| (stream capture)      | `process.rs:288`    | `spawn()` + `wait()`                      |
| (stream capture)      | `process.rs:317`    | `spawn()` + `wait()`                      |
| `spawn_stdio`         | `process.rs:311`    | returns the raw `Child` to the caller     |

Tokio's default is to *not* kill on drop, so if the enclosing future is canceled between spawn
and wait, the `Child` is dropped and the process keeps running, orphaned. `Command::output()` has
the same behavior and the same hazard -- it is not a safer shape, only a shorter one -- and it
backs the public `Container::exec_capture` (`crates/outrig/src/container/mod.rs:807`) plus
several runtime and image probes.

`Container::start_named` uses `try_capture_logged` for `podman run`. Cancellation there is worse
than a stray process: no `Container` value has been constructed yet, so container `Drop` cannot
clean up either. The audit reproduced it -- a fake `podman` that wrote its PID and slept,
`Container::start` wrapped in a 150 ms `timeout`, and `kill -0` still succeeding afterward.

outrig already knows this pattern: `crates/outrig/src/mcp.rs:223,630` and the CLI's watcher
(`cli/watcher.rs:153,262`) all set `kill_on_drop(true)`. The library's own process layer is the
gap, and it is the one with the widest blast radius.

## Two things `Drop` cannot do

**`Drop` cannot await, so "kills and reaps on drop" is not implementable as one mechanism.**
`kill_on_drop(true)` requests a kill and leaves the reap to tokio's background reaper, which runs
at some later point and not at all if the runtime is shutting down. A test that drops a future
and immediately asserts no zombie is racing that reaper. The design therefore needs two layers,
named separately:

- **Cooperative cancellation** -- the helper takes a cancellation signal, and on cancel it kills
  and then *awaits* `wait()`, so the caller has a confirmed reap before the future resolves.
  Confirmed reap is assertable only here.
- **The drop path** -- for a future that is simply dropped, which is what `timeout(..,
  Container::start(..))` does today, since no public entry point takes a token. Nothing can await
  in `Drop`, so this path can promise only **bounded eventual termination**: the process is
  signaled, and a supervisor reaps it within a stated bound. Acceptance on this path asserts
  termination within that bound, never an immediate confirmed reap.

The two must be bridged by something concrete, or the drop path has no bound at all. The
plausible shape is an owned supervisor task: the spawning helper registers the child with a
supervisor that outlives the caller's future, `Drop` signals it, and the supervisor performs the
kill-and-`wait()` and the engine-side cleanup. That is also the only way a dropped guard can
`podman rm` a container or clean a buildah working container -- a destructor cannot await those
either, so the name guard and the build-resource guards need the same owner.

**The handoff has to be atomic, or it reintroduces the bug it fixes.** This sequence is wrong:

1. spawn the child;
2. register it with the supervisor.

A cancellation landing between those two steps leaves a running child that nothing owns -- which
is the original defect, moved a few lines later and made harder to see. Acceptable shapes: the
supervisor performs the spawn and owns the child before control returns; or a synchronous
kill-on-drop guard owns it from the instant of spawn and is released only once registration is
acknowledged; or any equivalent transfer whose failure path cannot leave the child unowned. The
same rule governs handing container-name, buildah working-container, and temporary-tag
obligations to the supervisor: there must be no instant at which the resource exists and no owner
is responsible for it.

Whatever is chosen, **the goal, fork 2, and the acceptance criteria must name the same
mechanism.** The previous draft of this task did not: it distinguished the two layers correctly
and then asked a `timeout`-cancelled `Container::start` to prove the child "has been awaited",
which the drop path cannot do.

**Killing the client is not proof the engine cleaned up.** `podman` and `buildah` are clients of
a longer-lived engine. A `podman create` can complete on the engine side at the moment its client
dies; an image build can leave a named buildah working container or a temporary tag behind. A
test that only checks PIDs will pass while the engine holds the resource.

## Goal

Canceling any outrig future leaves no process, no container, and no engine-side build resource
behind -- confirmed synchronously where the caller cooperates, and within a stated bound where
the future is merely dropped. Ownership is structural, not a thing each call site remembers.

## Deliverables

- **One cancellation-owning child abstraction** in `process.rs` that every helper spawns through,
  implementing both layers and the bridge between them. `try_capture`'s `Command::output()` moves
  to it like the rest -- it is the same hazard behind a shorter call.
- **A stated termination bound for the drop path**, in the module doc and in the changelog, since
  it is the guarantee callers who never touch a token actually get.
- **An explicit decision for `spawn_stdio`.** It hands a raw `Child` to the caller, and
  `Container::exec_stdio` (`container/mod.rs:797`) re-exports that to library consumers. Either
  the returned child is kill-on-drop, which changes the public contract for anyone holding one,
  or it stays caller-managed and the contract says so in rustdoc. Do not leave it implicit;
  it is the one site where "internally owned" is false by design.
- **A container-name guard whose cleanup can await.** `Container::start_named` reserves a name
  before it owns a handle; the guard removes the container unless ownership is committed to the
  returned `Container`. `podman rm` is an async call, so the guard's `Drop` can only hand the
  obligation to the supervisor -- it cannot perform it.
- **The same for engine-side build resources** -- buildah working containers and temporary tags
  created during an image build. Same owner, same reason.
- Documentation of the guarantee in `process.rs`'s module doc, since it is now a property other
  modules are entitled to rely on, and of the `spawn_stdio` exception beside it.

## Acceptance

- **Drop path, bounded termination.** A fake `podman` on `PATH` that records its PID and sleeps;
  `Container::start` under a short `timeout`; `kill -0 <pid>` fails **within the stated bound**,
  polled rather than asserted instantaneously. This is the audit's repro, and the repro cleaned up
  after itself -- the test must too, or a failure leaks a sleeping process into the developer's
  session.
- **Cooperative path, confirmed reap.** The same fake, cancelled through whatever token-taking
  entry point fork 2 settles on: on return, the child is dead *and* reaped, asserted immediately.
  If no such entry point exists, this criterion has nothing to run against and fork 2 was
  answered wrongly.
- **Cancellation injected at the spawn-to-supervisor handoff itself**, deterministically rather
  than by racing a timeout. Both contracts hold across it: bounded termination if the future was
  dropped, confirmed reap if it was cancelled cooperatively. This is the window the atomic-handoff
  rule exists to close, so it needs its own test rather than being covered incidentally by the
  phase-boundary cases.
- The same shape at each phase boundary: canceled between `create` and `init`, between `init` and
  `start`, during `exec` (both `exec_capture` and `exec_stdio`), and during an image build.
- The equivalent for resource obligations: cancellation between reserving a container name (or
  creating a buildah working container, or a temporary tag) and the supervisor accepting
  responsibility for it leaves nothing behind.
- **Engine state, not only PIDs.** After a canceled create, `podman ps -a` lists no container
  with the reserved name; after a canceled build, no buildah working container and no temporary
  tag survive. These belong with the live-podman work in 0129; the PID-level cases run against
  fakes and stay in the ordinary suite.
- No zombies on the cooperative path, asserted after the awaited reap. On the drop path, no
  zombie **within the bound** -- the supervisor is what makes that assertable at all.
- Existing `crates/outrig/src/process_tests.rs` and `container_lifecycle.rs` still pass -- the
  abstraction must not change the success path's output capture or its stderr-tail behavior.

## Design forks

1. **Process versus process group -- Recommended: group, but measure it.** `podman run` may itself
   spawn children; killing only the direct child can leave the conmon/runtime side alive. A
   process group needs `setsid`/`process_group` at spawn time, which changes signal delivery for
   the success path too (Ctrl-C no longer reaches the child via the terminal group). If that
   matters for interactive `outrig enter`, the fallback is per-process kill plus an explicit
   `podman rm -f` in the scope guard, which is what the guard is for anyway.

2. **How the cancellation signal reaches the helpers, and which entry points take one -- Open,
   and it determines what the acceptance criteria can say.** A `CancellationToken` parameter is
   explicit and touches every call site. Relying on the caller dropping the future is invisible
   and can promise only the bound. The likely answer is a hybrid: every helper is drop-safe via
   the supervisor, and the paths that need a confirmed reap gain a token-taking variant. Decide
   *which* paths those are before writing tests -- the cooperative criterion above needs at least
   one to exist, and today none does.

## Dependencies

None. It is a prerequisite for 0117, whose rollback must survive cancellation and therefore needs
a child that does the same -- though note 0117 needs strictly more than this task provides; see
its Context.

## See also

- `crates/outrig/src/process.rs` -- the table above; `capture_stream` / `capture_all` /
  `capture_stderr_tail`.
- `crates/outrig/src/container/mod.rs:797,807` -- `exec_stdio` and `exec_capture`, the public
  surface this changes.
- `crates/outrig/src/mcp.rs:223,630`, `crates/outrig-cli/src/cli/watcher.rs:153,262` -- the
  `kill_on_drop(true)` sites that show the intended posture.

## Decisions

- **Fork 1: per-process kill plus engine-side cleanup, not a process group.** A process
  group would reach nothing the per-process kill misses. `podman run` and `podman exec` are
  clients of an engine; the workload is supervised by conmon, in the container's own PID
  namespace, and was never in the client's process group -- so `setsid`/`process_group`
  changes what a Ctrl-C reaches for interactive `outrig enter` and buys nothing back. What
  actually stops the workload is removing the container, which is what the name guard is
  for. The fork's fallback was the right answer, and for a stronger reason than "the guard
  exists anyway": the guard is the *only* thing that can work.

- **Fork 2: the cooperative form is crate-internal, and the stop signal is a future rather
  than a `CancellationToken` parameter.** Each helper gained a `*_until(cmd, .., stop)` twin
  taking `impl Future<Output = ()>`, and the existing name delegates to it with
  `std::future::pending()`. Three consequences, all of them the reason for the shape:

  - The 43 existing call sites do not move, and there is no shared never-firing token to
    allocate or reason about.
  - A caller supplies whatever it already has: `CancellationToken::cancelled()` for 0117,
    `tokio::time::sleep(budget)` for a caller with a deadline, and -- as the tests do -- an
    arbitrary condition. A `CancellationToken` parameter would have forced the deadline
    callers to build a token and a timer task to fire it.
  - No `tokio_util` type enters the public API before the 0.2.0 freeze. Library callers get
    the documented drop-path bound, which is what wrapping a call in `timeout` already gave
    them; 0124 is not handed a second frozen tokio type beside `exec_stdio`'s `Child`.

  Exactly one twin exists -- `try_capture_logged_until` -- because exactly one has a caller.
  See the `/simplify` note below for why the first cut had four.

  The cooperative path has production consumers rather than only a test. `Container::stop`
  bounds its trailing `podman rm -f`, which was an unbounded await whose result was already
  discarded: a wedged client there hung session teardown, and `stop` is the last thing to
  touch the container name, so a client still holding it is how the next run under that name
  fails. `McpClient::shutdown` is the other -- its hand-rolled kill-then-bounded-wait
  collapsed onto `Owned::terminate`, which upgrades it from "signal sent" to "reaped" on the
  ordinary outcome while keeping its existing bound for a process that cannot be reaped.

  Two details of that bound, both of them cases where the obvious version quietly loses a
  removal. The budget is `grace.max(MIN_REMOVAL_BUDGET)`, not `grace`: zero is a legitimate
  grace, meaning "do not wait for the container's processes", and reusing it directly would
  have reduced the defensive removal to no attempt at all. And when the budget *is* spent,
  `stop` hands the removal to the supervisor rather than dropping it -- bounding a wait must
  not become abandoning the work, so a slow engine costs `stop` its budget and not the
  container. Without that second half the bound would have been a new leak inside a task
  about leaks.

- **The exception is `spawn_stdio`, and it is kill-on-drop.** The child leaves the crate
  through `Container::exec_stdio` / `Outrig::exec_stdio` carrying `kill_on_drop(true)`:
  dropping the handle SIGKILLs the `podman exec` client. The signature does not move, the
  behavior does, and the reap becomes the holder's -- outrig does not supervise a child it
  has handed away. Written into the rustdoc beside the module's guarantee and into the
  changelog, per the deliverable's "do not leave it implicit".

  Chosen over leaving it caller-managed because the alternative keeps the exact leak the
  rest of the task removes reachable through the public API, and because `mcp.rs` had
  already reached the same conclusion for its own children.

- **Two owners, not one supervisor, because the two obligations cannot share one.** A tokio
  `Child` polls its own runtime's SIGCHLD driver, so its reap has to run on the runtime that
  spawned it; an engine-side removal must survive that runtime being torn down and outrig
  itself exiting. So:

  - **Children** are owned by `process::Owned`. Its `Drop` calls `start_kill` synchronously
    and hands `wait()` to a `Handle` captured at spawn. `kill_on_drop(true)` on the command
    is the backstop for a reap task that is never polled -- verified in tokio 1.53's source:
    `OwnedTasks::bind_inner` shuts a task down rather than panicking when the runtime is
    already closed, and the dropped task drops the `Child`, which kills it again and queues
    it on tokio's orphan queue.
  - **Engine resources** are owned by scope guards that hand a detached
    `std::process::Command` to `supervise::detach_cleanup`. That is the existing
    `spawn_detached_rm` shape, kept deliberately: an ordinary OS process needs no runtime
    (so `Drop` and the panic hook can both use it) and completes even if outrig exits a
    moment later, which a spawned task would not. What it adds is a reaper thread, which
    closes that shape's one defect -- every cleanup used to leave a zombie for the life of
    the process. `network.rs`'s `spawn_detached_delete` had the identical defect and moved
    onto the same call.

  One thread per obligation rather than a shared reaper: obligations arise only on
  cancellation and error paths, so they are rare, and a wedged `podman rm` must not delay
  the reaping of everything queued behind it.

- **The handoff is atomic because `Cmd::spawn_owned` is synchronous.** It is a plain `fn`,
  not an `async fn`, so no `.await` separates the `spawn()` from the `Owned` that owns its
  child and there is no instant at which the process exists unowned. That is the whole
  answer to the task's atomic-handoff rule, and it is why `spawn_owned` must stay
  synchronous -- a later `async fn` there would silently reopen the window.

  `process_tests::a_drop_at_the_spawn_handoff_leaves_no_child` pins the observable
  equivalent: one `poll!` drives the helper from entry through the spawn to its first await,
  the child publishes its pid on its own (it is a real process, indifferent to whether the
  future is polled), and the drop lands there.

- **The stated bound is "terminated synchronously, reaped as soon as the runtime is next
  driven."** Not a number. The kill is genuinely synchronous -- tokio's `ChildDropGuard::drop`
  signals before returning -- so the interesting half is the reap, and no honest bound on it
  can omit the runtime: `Drop` cannot await, so a caller that drops a future and then blocks
  its runtime thread will see the process dead but unreaped. That is not a defect to hide;
  it is what the layer can promise, and it is why every drop-path test polls rather than
  reading once.

  The margin is reported rather than asserted, following
  `plan/done/0109-subagent-tree-shutdown-grace.md`: measured 3-6 ms against a 5 s ceiling.
  The first draft of those tests polled with `std::thread::sleep` and failed -- blocking the
  runtime thread prevents the very reap being waited for. Worth recording because it is the
  same mistake a caller can make in production, and it is now stated in the module doc.

- **`try_capture`'s two `Command::output()` quirks are preserved deliberately.** tokio's
  `output()` -- unlike `std`'s -- does not redirect stdin, so children of `try_capture` have
  always inherited the parent's; and it folds a mid-read I/O failure into the same error as
  a failure to start. `StdioSpec::captured_inheriting_stdin` and an explicit `map_err` keep
  both. `Container::exec_capture` is public and runs through this, so refining either would
  have been a silent behavior change riding along in a cancellation fix.

- **Exec cancellation is proven at the process layer, not the container layer.**
  `exec_capture` and `exec_stdio` add argv construction on top of `try_capture` and
  `spawn_stdio` and nothing else, and both have their own drop tests. Reaching them through
  the public API needs `bootstrap_user` first, which does real `setns` work against a live
  container that a shell-script podman cannot provide. The live-engine exec case belongs
  with 0129. `try_capture` got a drop test of its own rather than being taken on faith,
  since "it delegates to the same abstraction" is a claim about the source, not a test.

- **The fake-runtime tests mutate `PATH`, and the synchronization is `OnceLock`, not a
  mutex.** This repo's convention for env mutation in tests -- a variable name unique to
  each test -- cannot apply to a name every process already reads. `OnceLock::get_or_init`
  blocks every other caller until the first returns, and every test in
  `tests/cancellation.rs` calls it before doing anything else, so each test's spawns
  happen-after the single write. The binary contains only these tests, which is what makes
  the argument hold; that is a constraint on the file, and it is written in the file.

  The fakes are steered by `fast.<verb>.<token>` marker files. Verb scoping is not
  incidental: the first cut keyed markers on the subject alone, and since `podman create
  --name N` and `podman init N` both mention `N`, the create-to-init boundary test was
  making the whole sequence return fast and proving nothing. Found by deliberately breaking
  each guard and checking which tests noticed -- which is also how the rest of the suite was
  confirmed to bite: neutering `Owned::drop` fails 4 unit and 7 integration tests, neutering
  the two resource guards fails 6 integration tests, and setting `kill_on_drop(false)` alone
  fails exactly one, the `spawn_stdio` contract test. That last split is the layering working
  as intended -- the explicit kill in `Drop` covers the helpers, and `kill_on_drop` covers the
  one child that leaves the crate, where no `Owned` is left to run a destructor.

- **What `/simplify` changed, and the one place its verdict was not taken.** An independent
  implementation was generated from the requirement alone and both were reviewed. The review
  returned *alternative simpler* -- roughly half the production logic -- and it was right
  about where the weight was. Five of its findings were taken:

  - `detach_cleanup` takes a `Cmd` rather than `(program, &[&str])`. The first cut invented a
    second argv representation beside the one the crate already had, and the cost showed up
    immediately in `network.rs`, which hand-rebuilt the nine-argument `nsenter ... nft delete`
    line instead of reusing `nsenter_nft(pid)`. The detached path would have silently diverged
    from the awaited one the day that helper changed.
  - The buildah-specific `BuildResourceGuard` became a general `supervise::CleanupGuard`
    holding a `Cmd`. One type instead of two, and the "arm before the resource can exist"
    contract is stated once.
  - **One `*_until` variant, not four.** Only `try_capture_logged_until` has a caller passing a
    real signal, and the other three were reached exclusively with `std::future::pending()`.
    Adding a fourth of anything on the strength of symmetry is how a module doubles its surface
    for nothing; the remaining three are four lines each on the day something needs them.
  - `Owned::terminate` no longer clears the handle. Clearing it made a later `Owned::wait`
    panic -- unreachable today, a footgun tomorrow. tokio caches the exit status on the
    `Child` (`FusedChild::Done`), so a second wait returns it and `Drop`'s `try_wait` sees the
    same, which makes the clearing unnecessary as well as hazardous.
  - A comment claiming `kill_on_drop` is what queues a dropped child for reaping was wrong:
    `Reaper::drop` queues unconditionally and `kill_on_drop` only controls the kill. Corrected
    rather than left as a plausible-sounding falsehood in a module whose whole value is that
    its guarantees are exact.

  Two of the alternative's choices were **not** taken, and they are the reason its architecture
  was not adopted wholesale:

  - It reaped **synchronously inside `Drop`**, with an unbounded blocking `waitid`. That is
    what let it drop the cooperative variants entirely -- a confirmed reap on every drop makes
    them redundant -- and it is most of its size advantage. It is also unsafe here: the CLI
    builds `new_current_thread` runtimes, so blocking that one thread stops the timer and
    signal drivers too, and an outer `timeout` guarding the very operation cannot fire. The
    children are podman and buildah on overlay and fuse filesystems, where uninterruptible
    sleep is a real state and SIGKILL is not prompt. It converts "the timeout fires and we
    recover" into "the process hangs with nothing left to rescue it", which is a worse failure
    than the leak being fixed, and `Drop` also runs during unwinding.

    The review's own remedy was to bound that wait -- but a bounded blocking wait no longer
    *confirms* the reap, and the confirmed reap is an acceptance criterion that has to be
    assertable with no polling. So the alternative is coherent only with the hazard, and
    removing the hazard puts the cooperative variants back. That is why the verdict was
    weighed and departed from rather than followed: its simplicity was purchased with the one
    thing that could not be kept.

  - It released each build guard *before* awaiting the cleanup it guards, at all three sites.
    A cancellation landing inside `cleanup_temp_image` therefore disarms the guard and then
    kills the `buildah rmi` it disarmed for -- the exact leak the guard exists to close, moved
    into a smaller window and left untested. Release goes after the awaited cleanup.

  The review also called `Container::stop`'s bounded removal scope creep, and that is a fair
  reading given it supplies its own justification for the cooperative path. Kept anyway, on
  the merits rather than the convenience: the call it replaced was an unbounded await whose
  result was already discarded, which is a hang waiting for a wedged engine, and with the
  supervisor fallback the removal still happens. Fork 2 asks which paths need a confirmed
  reap; a method whose last act is releasing a container name is a defensible answer, not a
  contrivance.

- **Not done here: engine-state assertions.** `podman ps -a` showing no container under the
  reserved name, and no surviving buildah working container or temporary tag, need a real
  engine and are 0129's, as the task directs. The fakes prove outrig issues the removal;
  only a live podman proves the engine honored it.

- **Review found the guard could delete a container it never created, which is worse than the
  leak it fixes.** `podman run --name N` fails when N is already in use, and that is an
  ordinary outcome -- another session, a stray that outlived its record, a container the user
  made by hand. The first cut of `NameGuard` removed **by name** on the way out of that
  failure, so a plain name collision destroyed someone else's container. The same is true
  under cancellation, where outrig never learns why the command ended, and of a cleanup still
  in flight when the caller retries under the same name.

  The fix is that each attempt marks what it asked for and removes only that: a fresh random
  `org.outrig.attempt` label goes on the `run`/`create`, and the guard runs
  `podman rm -f --filter label=org.outrig.attempt=<token>`. A create that collided made
  nothing carrying the token, so the distinction falls out of the mechanism rather than being
  a case someone has to remember -- which matters, because the first cut *did* remember it for
  `create_initialized`'s init step and still got the create step wrong.

  The first version of this fix used `--cidfile` instead, and a second review round found the
  hole in it: podman writes the cidfile *after* registering the container, so a cancellation
  landing in that interval left a container the guard could no longer name. A label has no
  such interval -- it is part of the creation request, so the container carries it from the
  instant it exists. That is also why the label *replaced* the cidfile rather than joining it:
  it covers strictly more, and two mechanisms for one obligation is one more than can be kept
  honest.

  Verified against real podman before building on either: a colliding `podman run` exits 125,
  `podman rm -f --filter label=...` removes only what carries the label, and it exits 0 when
  nothing matches. Two tests pin the rule, one for the plain failure and one for
  cancellation over a taken name, and both fail if the guard goes back to removing by name.

  The boundary of the fix is deliberate: `Drop for Container` and `Container::stop` still act
  by name. A constructed `Container` is one this process created, so its name is provably its
  own; the guard is precisely the code that cannot make that assumption.

- **The drain tasks abort on drop rather than detaching.** Dropping a bare `JoinHandle` leaves
  the task running, and a drain task holds one end of the child's pipe. Killing the child does
  not necessarily close the other end -- anything it left behind that inherited the descriptor
  keeps it open -- so a detached drain went on reading, and on growing an unbounded buffer,
  after the call was abandoned. `Drain` aborts in its destructor, which closes the read end and
  incidentally tells such a descendant, by `EPIPE`, that nobody is listening. That is what the
  regression test observes.

  The first version of that test asserted on `$$` from inside a subshell, which in POSIX sh is
  the *parent's* pid -- so it tracked the process the drop kills directly and passed with the
  abort removed. `$!` is the descendant's own pid. Worth recording because the test looked
  right and proved nothing.

- **One reaper thread, polling, rather than one thread per cleanup.** The first cut spawned a
  thread per obligation to keep a wedged `podman rm` from delaying the reaping of anything
  behind it. That trades one resource for another: a burst of cancellations, or one interceptor
  shutdown with many attachments, spends threads and stack reservations in proportion to how
  many cleanups are in flight. A single thread holding a `Vec<Child>` and polling `try_wait`
  gets both properties -- `try_wait` never blocks, so nothing queues behind a wedged cleanup --
  and costs nothing while idle, because it parks on the channel whenever it holds no children.

  If that one thread cannot be started, the cleanup is left running and unreaped rather than
  killed. The review suggested killing and reaping it instead; that is the wrong way round for
  this module, whose entire purpose is that the cleanup *happens*. A zombie is a smaller loss
  than a container that stays.

- **Two documentation claims were wrong and are corrected.** `doc/usage/run.md` said Ctrl-C
  signals and reaps the `podman exec` client. It does not: the MCP servers and their transports
  belong to the session, not the turn, so Ctrl-C abandons the request in flight and kills
  nothing. The CLI never calls `exec_capture`, which is what made the original claim sound
  plausible. And the `McpClient` type doc called the child "the server", which is true for a
  `podman exec` transport and false for `connect_via_podman_start`, where the child is a
  `podman start --attach` client and the server is the container's entrypoint.

  The changelog also understated `Container::stop`'s removal budget as `grace` when the code
  uses `max(grace, 1 second)`.

- **Second review round: three more defects, all real.**

  - **`Drain::join` took the handle out of the guard before awaiting it.** That moves it into a
    temporary, so a caller cancelled during the join drops the temporary -- which *detaches*
    the task rather than aborting it, leaving the reader, its descriptor and its buffer alive.
    It is also the likeliest place to be cancelled: the join is reached after the child has
    exited, so if it is still running, something the child left behind is holding the pipe.
    The handle is now awaited through `as_mut` and taken only once it resolves. The first
    descendant test did not catch this because it cancelled during the child's `wait`; a
    second one drives the helper past that point first.

  - **The reaper's bounds.** Receiving one child and then rescanning the whole vector made a
    destructor fan-out quadratic in its own size; queued arrivals are now drained before a
    single scan. And a cleanup that never returns was held forever, so repeated cancellation
    against a wedged engine accumulated live processes without limit -- each one now gets
    `WEDGED_CLEANUP` before it is killed, which bounds the population by arrival rate rather
    than by the life of the process. Admission is deliberately *not* bounded: refusing to
    start a cleanup is choosing to leak the resource it exists to remove, and with a lifetime
    bound the population cannot run away anyway.

  - **A failed reaper start was cached forever.** `OnceLock<Option<Sender>>` remembered the
    failure, so one bad instant under resource pressure -- exactly when cleanups pile up --
    disabled reaping for the rest of the process. Only a successful start is remembered now;
    callers retry.

  Two documentation claims were also wrong. The `McpClient` note said the distinction between
  transport and server was invisible for a `podman exec` client because the exec's process
  *is* the server; it is not, since conmon supervises the exec workload and killing the client
  closes a pipe rather than stopping the server. And the supervisor's own test started a
  `sleep 30` it never ended, so a passing run left it reparented and running -- it now blocks
  on a gate the test opens, and verifies the reap before returning, including on the failure
  path.

- **`Container::stop`'s removal floor moved from 1 s to 30 s, because 1 s fired on "slow".**
  A full `outrig-cli` e2e run left one `Exited (137)` primary behind. It could not be
  reproduced in isolation -- the suite that owns that container passed cleanly three times
  after -- but there was a mechanism that fit and that was mine: under parallel load a healthy
  `podman rm -f` can take longer than a second, and at that point `stop` was cutting the
  client short and falling back to a detached retry, turning a removal that would have
  completed into one that had to happen twice. The bound exists for a client that will never
  return, so it is now calibrated for that and nothing else. Two more full runs after the
  change left nothing.

  Recorded rather than quietly fixed because the observation was one leaked container in one
  run, and the honest summary is "a plausible mechanism I own, removed" rather than "a
  diagnosed bug". The first review called this whole `stop` change scope creep; this is the
  cost of it that the review was pointing at, and it is worth knowing that the calibration was
  the risky part rather than the bound itself.

- **One e2e failure was investigated and is not this task's.**
  `clean_sweeps_stopped_recordless_labeled_containers` failed once while the suite was run
  three times back to back with other outrig sessions live on the machine, then passed three
  isolated runs and two full-suite runs. `outrig clean` sweeps by label across the whole
  machine, so a concurrent sweep can remove the stray this test planted before the sweep under
  test sees it. Nothing on that path goes through code this task changed -- the stray is
  planted with a raw `podman run` and `cli/clean.rs` is untouched. Filed as
  `plan/next/clean-sweep-test-races-concurrent-sweeps.md`, and later confirmed outright
  rather than argued: with several unrelated outrig sessions live on the machine the test
  fails reproducibly in a full-suite run, and `trunk` at `a556ec4` fails it the same way from
  a scratch worktree minutes apart. The entry is broadened to the real problem -- the e2e
  suite assumes it is the only outrig on the machine.

- **Third review round: the label needed a floor and a lock.**

  - **`podman rm --filter` is podman 4.3, not 4.0.** The first claim was a guess and it was
    wrong -- checked against the man pages, the option is absent from 4.2 and present in 4.3.
    The reviewer's fix was either a 4.0-compatible flow or an enforced 4.3 minimum, and the
    intermediate design -- a `--cidfile` receipt as the portable primary with the label as a
    fallback for the instant before the receipt is written -- was built and then dropped on
    the user's call: podman is at 6.1, so carrying two mechanisms to serve 4.0-4.2 buys
    nothing. The floor is documented in the quickstart's prerequisites instead, which is
    where someone finds out before it matters rather than after.

  - **A caller could take the label and disable the cleanup.** `ContainerLaunchSpec::labels`
    is public and podman takes the last `--label` for a key, so a caller setting
    `org.outrig.attempt` replaced the token the guard filters on -- cleanup then matched
    nothing and the container leaked. It is now refused before anything is spawned, and
    outrig's own label is emitted *after* the caller's, so the ordering would save it even if
    the check were removed.

  - **The reaper could starve its own scan.** Draining the channel until `try_recv` reported
    empty is unbounded: sustained ingress keeps it accepting and neither the exit reaping nor
    the kill deadline ever runs. Each pass now takes at most `REAP_BATCH`. Deadlines are also
    stamped when the cleanup is *spawned* rather than when it is dequeued, since time spent
    queued is time the command has been running.

  Two tests were passing for reasons that did not hold. The mid-drain test treated a marker
  written just before the shell exits as proof the helper had reached the join; it can become
  visible while the helper is still in `wait`, so the old take-before-await bug could have
  returned undetected. It now waits for the *direct child's pid to leave the process table*,
  which only the helper's own `wait` can cause, so reaching the join is proven rather than
  timed. And the cancelled-collision test discarded a single `poll!`, which can stop at the
  SELinux probe or run to the ordinary error -- it now drives the future until the fake has
  acknowledged the collision and is still holding, asserting the start stays pending
  throughout.

- **Fourth review round: the reaper's batch budget counted the wrong thing.**
  `REAP_BATCH` was applied to `waiting.len()`, the number of cleanups being *held*, rather
  than to arrivals accepted in the pass. So once that many were retained the bulk-admit loop
  never ran at all, and each queued arrival went back to costing its own full scan -- the
  quadratic the cap was added to prevent, reintroduced in exactly the backlog it was for. The
  budget now counts arrivals, including the blocking receive that opens the pass.

  Not covered by a test, deliberately. The defect changes only how much work a pass does; the
  loop makes progress either way, so the only observable is timing, and at the sizes involved
  the difference is microseconds. What the test does cover is the condition -- it now holds
  `REAP_BATCH + 8` blocked cleanups while a batch of fast ones is admitted and reaped, which
  is the state the broken budget stalled in. Its blockers also stopped spinning: dozens of
  poll loops forking a `sleep` every 20 ms is a fork storm, so they `exec sleep` and the test
  kills them, through a `Drop` guard so a failed assertion cannot leave seventy processes
  behind.

- **Left for later, with the mechanism now available: the panic hook removes by name.**
  `TRACKED` is a set of container names and `install_panic_hook` sweeps it with
  `podman rm -f <name>`, so a panic during a colliding start deletes the container that
  already held the name -- the same defect this task fixed for the cancellation path -- and a
  set cannot hold two reservations of one name. Both are unchanged from before this task and
  reachable only through a different trigger, so they stay out of a diff that has already
  been through four rounds. Filed with the fix sketch as
  `plan/next/panic-sweep-removes-by-requested-name.md`; the per-attempt label the guard now
  stamps is what makes it fixable.

- **The `cargo (local-llm)` CI failure is the known lib-test flake, not this change.**
  Asked in review, and answered with measurement rather than inference. That row runs
  `cargo clippy --all-targets --features outrig-cli/local-llm -- -D warnings` and
  `cargo test --features outrig-cli/local-llm`; both are clean on this head locally -- clippy
  exit 0, 47 test binaries, no failures. Every cargo row also runs the lib unit tests, and
  those carry a diagnosed flake in
  `process_tests::try_capture_logged_traces_spawn_and_exit_at_debug`
  (`plan/next/lib-unit-test-flake.md`): tracing caches callsite interest globally while the
  test installs its subscriber thread-locally, so one of the two events can go missing. It
  exits 101 with a panic that has nothing to do with the row it failed in, which is exactly
  the shape reported.

  Reproduced here by running the compiled lib binary in a loop: 1 failure in 80, 1 in 120,
  always that test. Worth checking rather than assuming, because this task added seven tests
  that call `run_capture` and `try_capture_logged` without a subscriber -- more traffic
  through the very callsites the flake turns on -- so making it worse was a live
  possibility. The measured rate is at or below the ~1-in-8 the buffer entry recorded, so it
  did not.

  Left unfixed here on purpose. Each of the entry's three candidate fixes gives something up
  -- a private callsite stops testing the code the test exists for, a mutex encodes an
  ordering a future caller can silently break, a separate binary cannot reach a `pub(crate)`
  helper -- so choosing between them is its own change, not a footnote in a cancellation
  diff. The entry now carries the CI symptom and the fresh measurements so the next person
  does not start from zero.

- **CI caught a missing happens-before in the fake, not in the guard.** `cargo (default)`
  failed three of the eleven cancellation tests -- the canceled `run`, the canceled `create`,
  and the create-to-init boundary -- each at the ceiling with `"<name>" was never removed,
  within 10s`. Every other row passed, including `arm64`, which runs the same tests.

  The fake wrote its journal record as its first action and the `labeled.<attempt>` file
  later, after the collision markers. Those three tests cancel on *first sight* of the
  record, so the kill could land in between: nothing ever carried the attempt label, the
  guard's `podman rm -f --filter label=` matched nothing, and `removed.<name>` was never
  written -- permanently, so a larger ceiling would not have helped. The split says the same
  thing. Every label-scoped removal cancelled on first sight failed; the 400 ms timeout test
  passed because 400 ms is grace enough for the label write, and the build and label-commit
  tests passed because their removals name a target directly instead of filtering.

  So the fake now publishes rather than records: each invocation is written under
  `pending.` and renamed to `inv.` at its own safe point, which for a creating verb is
  *after* the label. The prefix the test polls for cannot appear before what a cancellation
  triggered by that sighting is entitled to find. Confirmed by widening the window rather
  than by argument: a `sleep 0.1` between the record write and the label write fails exactly
  those three tests and no others, and with the publish order in place the same delay passes
  all eleven. 25 runs pinned to one core and 25 to two are clean.

  Publishing later needed the *other* window held too, or the ordering could have been
  fixed by making every cancellation land after creation, which would retire the case the
  guard's label design exists for. The pre-creation case,
  `a_start_canceled_before_creation_issues_a_removal_that_reaches_nothing`, holds it: the
  fake parks before creating anything, and the cancel there must both remove
  nothing and still *issue* a removal scoped to this attempt's label. The second half is
  what makes it more than a second copy of the collision test -- a guard that had stopped
  issuing removals altogether would satisfy "nothing was removed" while having abandoned the
  obligation. The marker moved with it: `hang.<verb>` / `collided.` became `hold.<verb>` /
  `holding.`, one mechanism named for the state rather than for one of its two causes, since
  outrig cannot tell a taken name from a slow create either.

- **The invocation matcher had the same name-embeds-verb collision the markers were scoped
  against.** `invocation` matched every fragment as a substring of the argv, verb included,
  and the boundary test's container is named `outrig-cancel-init-<pid>-<n>` -- so `init` was
  satisfied by `create --name outrig-cancel-init-...` and the test cancelled during `create`,
  one phase before the boundary it documents. It takes the verb as its own parameter now,
  matched against the argv's first word, so what a test waits for is the command it names.
  The name keeps the embedded verb deliberately: it is the trap, and leaving it there is what
  keeps the matcher honest. `expect_removed` also reports the removals the fake saw, since
  "no removal was issued" and "one was issued, scoped to something else" are different
  defects and the old message could not tell them apart.

- **The stop signal now covers the drain, not just the wait.** Review found
  `try_capture_logged_until` raced the caller's signal against `child.wait()` alone and then
  dropped it, leaving the two `Drain::join`s unbounded. A pipe outlives the process it was
  given to: the client can exit inside its budget while a descendant holds the read end,
  and the helper then parks on a stream nobody will close. `Container::stop` is the caller,
  so its `MIN_REMOVAL_BUDGET` bought nothing in exactly the state it was added for -- and
  `Drain::join`'s own docs already name that state as where a cancellation is most likely to
  land. The signal is pinned and awaited twice now, and both joins sit in one `select!`
  branch so a stop drops them together and each aborts through its guard. `biased`, so a
  drain that completed in the same instant the budget expired is reported as the output it
  is. A stop landing there returns `Canceled`, which `stop` already answers by re-dispatching
  the detached removal -- doing that for a removal that had in fact succeeded costs a
  no-op `podman rm` whose output was going to be discarded anyway.

  `process_tests::a_stop_while_draining_returns_and_aborts_the_readers` is the cooperative
  twin of the drop-path test beside it, in the same proven state: the direct child is reaped
  (which only the helper can do) before the stop fires, so the signal demonstrably lands
  after the exit. It hangs indefinitely without the fix -- confirmed by reverting the
  `select!` and watching the test run past 60 seconds -- and the descendant's death is what
  proves the readers were aborted rather than detached.

- **Not fixed here: a killed `buildah build` leaves its working container.** Also from
  review, and real -- measured on buildah 1.42.1, where a SIGKILLed two-stage build leaves an
  `outrig-cache-working-container` behind from an empty `buildah containers`. This task
  caused it: the client used to survive the drop and run buildah's own stage cleanup on its
  way out. Left for a follow-up rather than patched, for two reasons. The resource is named
  by *buildah*, after the base image, so a removal scoped to it would be a removal by a name
  this process cannot prove it owns -- the trap `NameGuard` exists to avoid. And the
  suggested cooperative shutdown cannot run in `Drop`, which has no runtime to await on; it
  would have to become a stop signal threaded through the build's callers, which is a call
  shape, not a patch. Queued with the measurement as
  `plan/todo/0127-a-canceled-build-owns-what-buildah-made.md` -- numbered rather than
  buffered, since it has to ship in rc.3 (0128) rather than be described by it, and the
  tasks behind it moved back one. Its live-engine assertion shares 0129's fixture, where
  every other engine-state check already sits.
  `plan/next/image-cleanup-releases-its-guard-on-failure.md` records the adjacent
  pre-existing defect review raised: the cleanup helpers discard their outcome and their
  callers release the guard regardless, so a transient failure disarms the retry.

- **Declined: an admission cap on `detach_cleanup`.** Review asked for cleanups to be
  admitted *before* spawning, with a hard cap on pending and live children and a
  non-blocking overflow policy. The mechanism it describes is accurate -- `REAP_BATCH`
  bounds arrivals per pass, not the population -- but the remedy inverts the module's
  purpose. For a `Drop` caller, "non-blocking overflow" can only mean discarding the
  cleanup, which trades a bounded number of live processes for an unbounded number of
  leaked containers; blocking instead is worse, since `Drop` and the panic hook are the two
  callers that cannot wait.

  The population is already bounded twice. An obligation arises only per engine resource
  *this process created*, so the ceiling is outrig's own resource count and not an external
  arrival rate -- there is no producer that can outrun it. And `WEDGED_CLEANUP` kills
  anything still running after a minute, so even against a permanently wedged engine the
  population drains rather than accumulates. If pid or descriptor pressure ever did become
  the binding constraint, the honest lever is that deadline, which costs a retry, and not
  admission control, which costs the container.

- **A deferred removal needs an identity, and a name is not one.** Review found the hole this
  task's own argument predicts: `Container::stop`, having spent its budget, killed the
  foreground client and handed the removal on as `podman rm -f <name>`. That retry resolves
  its target *whenever the engine gets to it*, which is exactly the window in which the
  container can be gone and the name can belong to a replacement -- so the fallback added to
  protect a container could destroy someone else's. `Drop for Container` had the same shape
  for the same reason.

  The fix is the mechanism already built rather than a new one: `NameGuard::release` now
  hands its attempt token to the `Container` it releases to, and one `removal_cmd(name,
  attempt)` builds every owned removal -- filtered on `org.outrig.attempt` where an attempt
  is known, by name only where it is not, which is the attached case that never removes
  anything. The guard, `stop`'s fallback and `Drop` share that one selector now, which is
  also what `plan/next/panic-sweep-removes-by-requested-name.md` needs to fix the last
  by-name sweep. Two unit tests pin the argv of both branches; the by-label one asserts the
  container's *name* does not appear, since that is the string a replacement would share
  with it.

  `stop`'s foreground removal stays by name deliberately. It runs immediately, in-line,
  between a `podman stop` and this call -- there is no interval for a name to change hands
  in -- and its "no such container" outcome is already absorbed.

- **A negative test now waits for the cleanup rather than for a clock.** The collision tests
  slept 300 ms and asserted nothing had been removed. Review's objection is sound: a
  regression that reached for the container by name could be held off by scheduling or a
  cold engine and land just after the sleep expired, destroying the container the test
  exists to protect while the test reported success. `expect_not_removed` now waits for a
  removal carrying either this attempt's label *or* the container's name, and only then
  asserts. Either shape ends the wait on purpose -- requiring the correct one would make
  this assert the guard's scoping, which is `expect_removal_scoped_to`'s job, and a
  regression that issued the wrong shape would time out instead of failing on the point.
  The fake publishes a removal's invocation only after recording what it removed, so the
  sighting is a happens-before rather than another race.

- **A descendant assertion must accept a zombie.** Three drain tests waited for an
  intentionally orphaned writer to leave the process table. That bar is wrong for a process
  outrig does not own: it exits, is reparented to pid 1, and disappears only when pid 1
  reaps it -- which a container whose pid 1 is a shell never does. The tests would then fail
  on a runner where outrig had behaved perfectly, and leave the zombie behind. Descendants
  are held to `has_stopped` now, which counts `Z` as stopped; outrig's own children keep the
  strict bar, since reaping them is the guarantee under test.

  `a_zombie_counts_as_stopped_but_still_occupies_a_slot` makes a real zombie and checks the
  `/proc` parse against it, because the case never arises on a host with a reaping pid 1 --
  the parse would otherwise be exercised only where getting it wrong is expensive, and it
  fails silently, by reporting "not a zombie". The state character is read after the *last*
  `)` rather than by counting fields, since the comm field can contain both spaces and
  parentheses.

- **The kill-on-drop exception was documented where it was implemented, not where it is
  used.** `process::spawn_stdio` carried the whole story and the two public methods that
  return its child -- `Container::exec_stdio` and `Outrig::exec_stdio` -- carried none of
  it, which is backwards for a 0.2.0 API freeze: the caller who needs to know is the one
  reading the public page. Both now state that the handle is kill-on-drop, that the reap is
  the caller's, and that what dies is the host-side client rather than the in-container
  process. `McpClient::shutdown`'s comment claimed the same call left "the server confirmed
  gone"; `terminate` confirms the transport client and nothing else, and the comment says so
  now.

- **A cleanup that exits non-zero is retried, not abandoned.** Review found the hole in the
  new supervisor: it reaped by `try_wait` and dropped the entry on *any* exit, so a
  `podman rm` that lost a moment of engine or storage contention discharged nothing and
  nobody ever learned. The guard that owed the removal is gone by then, there is no caller
  to report to, and no sweep comes later -- a transient failure meant a permanent leak.

  `Wait` now keeps the `Cmd` and replays it: `CLEANUP_RETRIES` (2) further attempts, backing
  off 250 ms and doubling. Cheap to keep -- a program name and an argv -- and safe to
  replay, because a removal that runs twice has to be harmless anyway; `CleanupGuard`'s
  docs already say so, since on most paths the ordinary code removed the resource first.
  Finite because the other failure mode is a removal that can never succeed, and a cleanup
  this module killed for wedging is not retried at all: the next attempt would wedge on
  whatever the last one did.

  Two tests, one for each half. The retry test's script fails once and succeeds after, so
  its success marker can only appear if a second attempt ran -- it fails with
  `CLEANUP_RETRIES` set to 0, checked. The cap test counts attempts and then waits past the
  backoff a fourth would have used, so "no more" is observed rather than inferred from a
  loop exiting.

- **`stop`'s foreground removal was scoped too, and the reasoning that kept it by name was
  wrong.** The round before this one, that removal was left naming the container on the
  grounds that it runs inline between a `podman stop` and itself, with "no interval for a
  name to change hands in". Review pushed back and is right: the `podman stop` has already
  *returned*, so with `--rm` the container is gone and the name is free before this command
  resolves it. The interval is short, not absent, and what it costs is somebody else's
  container. It uses `removal_cmd` like the other two now, so every removal outrig issues
  for a container it made is scoped to the attempt that made it.

- **The `exec_stdio` docs described an ending nobody can take.** They said the handle is
  kill-on-drop *and* that the caller should `wait()` to reap the kill -- but the handle is
  gone after a drop, so that sequence does not exist. The two public methods now name the
  three real endings: hold and `wait()` for the command's own exit, hold and `kill()` then
  `wait()` to stop it and see it stop, or drop for the unobserved kill where the reap falls
  to tokio's orphan queue. `McpClient::shutdown`'s rustdoc had the same conflation its
  inline comment did -- it promised the *server* waited for and killed -- and now says
  transport, with container teardown named as what owns the server.

- **The retry needed a policy, because not every argv still means the same thing later.**
  The retry as first written replayed *any* cleanup, and review caught what that costs.
  `podman rm -f <name>` and `nsenter -t <pid> ... nft delete table` both select by an
  identity the engine and the kernel hand out again; a retry landing 250 ms after the
  identity moved on would delete a replacement container, or enter a replacement namespace
  and drop *its* table. That is worse than the leak the retry exists to prevent -- a leak
  costs disk, this costs someone else's running work.

  So the caller now states what it has. `Reissue::Safe` is for a selector nothing else can
  ever become: the per-attempt label, and buildah's `outrig-tmp-<pid>-<nonce>` /
  `outrig-label-<pid>-<nonce>` names. `Reissue::Once` is for a bare container name or a pid,
  and gets the pre-retry behavior exactly. `container::removal_cmd` returns the policy beside
  the command rather than leaving the two to be paired at each call site -- they are one
  decision -- and its unit tests now pin the policy as well as the argv.

- **A signalled cleanup is not a failed one.** The same round found the retry re-issuing
  cleanups that had been *killed*, which is how `blocked_cleanups_do_not_delay_the_others`
  came to leak 72 `sleep 3600` processes per run: the test kills its blockers, the supervisor
  read the signal death as failure, and each respawn was a process the test never learned the
  pid of. Measured before believing and after fixing -- 72 strays from one `cargo test -p
  outrig --lib`, 0 now.

  The rule that fixes it is the honest one rather than a special case for the test: only an
  *exit code* buys another attempt. A signal says something stopped the command -- this module
  for wedging, an operator, the OOM killer -- and in each of those the reason to stop it still
  holds a moment later. It also subsumes the wedged-kill case that was handled separately
  before. The cost is that an OOM-killed removal is not retried, which is the right way round:
  spawning more processes is a poor answer to memory pressure.

  The test now records every generation an obligation starts, asserts there was exactly one,
  and kills whatever the markers name at teardown -- so a future retry regression fails the
  assertion instead of quietly leaving processes behind.

- **A spawn that fails may consume the budget too.** `detach_cleanup` dropped the obligation
  outright when `Command::spawn` failed, which loses a resource to precisely the moment
  cleanups arrive in bulk: an `EAGAIN` from a full process table, or an `ENOMEM`. A transient
  failure now queues the obligation with its backoff and the same finite budget, while a
  permanent one -- `NotFound`, `PermissionDenied` -- still ends it, since a missing binary
  will still be missing in 250 ms. Not directly tested: forcing `EAGAIN` means exhausting the
  process table, which a unit suite has no business doing. The permanent half is covered by
  `missing_cleanup_binary_does_not_panic`, which would hang or spin if the classification were
  inverted.

- **Filed, not fixed: a handle addresses its container by a name that can be reused.**
  `Container::stop` runs `podman stop <name>`, so a stale handle whose `--rm` container has
  exited can stop whatever took the name -- an outage no later removal can undo. Removals are
  covered (that is what `removal_cmd` and the attempt label do); `stop`, `exec` and `inspect`
  are not, because a label filter cannot address one container for those verbs. The fix is to
  keep the id podman printed at create and address by it, which touches every call site that
  names the container, so it is
  `plan/next/a-container-handle-should-hold-the-id-podman-gave-it.md` rather than another
  amendment here. Pre-existing: this task changed how outrig *removes*, not how it addresses.

- **`Reissue::Safe` had to be earned, not asserted.** Review checked the claim behind the
  buildah classification and found it did not hold: `temp_nonce` was
  `SystemTime::now().as_nanos()`, and `outrig-tmp-<pid>-<nonce>` separates two builds only if
  that reading does. It does not -- two builds reaching it inside one clock tick get the same
  value, NTP can step it backwards, and the pid is shared by concurrent builds in one process.
  A detached cleanup selects by these names seconds later, so a collision means each build
  removing the other's temporary tag or working container. The nonce is 128 random bits now,
  the same as `container::attempt_token`, which is what makes "a retry cannot misdirect" true
  rather than merely intended.

- **A signal leaves the outcome unknown, which is what a replayable cleanup is for.** The
  previous round made any signalled cleanup terminal. That was too broad, and review was right
  about the half that matters: an OOM kill or an operator's SIGKILL says nothing about whether
  the container went away, and for a `Reissue::Safe` selector the honest response to "unknown"
  is to issue the removal again. Signals now take the same bounded retry path as a non-zero
  exit.

  One exception survives, narrowed to the case that earns it: a cleanup **this module** killed
  for wedging is terminal whatever the budget says, because the next attempt is the same
  command against the engine that just held it for `WEDGED_CLEANUP`, and the deadline exists to
  stop spending processes on exactly that. `Reissue::Once` never reaches the retry arm at all --
  its budget is zero.

  `blocked_cleanups_do_not_delay_the_others` moved to `Reissue::Once` as a result. It ends its
  blockers by killing them, which is now a retryable case, and replaying there would respawn a
  sleeper the test never learns the pid of -- the 72-per-run leak again. The blockers exist to
  test the reaper's scheduling, not the reissue policy; `a_signalled_cleanup_is_retried` covers
  that, by parking its first attempt so the test can kill it and letting only a second attempt
  leave the marker. The generations assertion stays, so flipping those blockers back to `Safe`
  fails the test rather than leaking quietly. Re-measured after the change: 0 strays.

- **A retry that cannot spawn keeps the budget the first attempt would have kept.** The
  transient-spawn-failure handling added last round covered only the *initial* spawn; a retry's
  `EAGAIN` still ended the obligation outright. Both paths classify the error the same way now
  and reschedule with the same backoff, so a full process table costs attempts rather than the
  resource.

- **`shutdown`'s `Ok` promises the handoff, not the reap.** The rustdoc written last round said
  the timed-out path kills *and reaps*, but the implementation discards `terminate`'s own
  timeout, so a client that cannot be collected returns `Ok` with the reap still owed -- to
  `Owned`'s `Drop`, which is where it belongs. The doc says that now, and still promises the
  reap on the path that can keep it: a transport exiting within the grace is reaped before the
  call returns.

- **An obligation with no child had nowhere to go when the reaper was unavailable.** Queueing
  a transient spawn failure created a childless `Wait`, and the fallback beneath it was written
  for the other case: it logged "cleanup runs unreaped" -- true of a running child whose reap
  was lost, false of an obligation where nothing had started -- and dropped it. Both failures
  come from the same pressure, since a process table that will not fork is often a process that
  cannot spawn a thread either, so the two are *correlated* rather than independent.

  The fallback now looks at what it is holding. A running child is left alone, as before, on
  the module's standing trade that a zombie is the smaller loss than a container that stays. A
  childless obligation gets one more attempt, inline and unreaped, because `Drop` cannot wait
  out a backoff and an unreaped cleanup still beats no cleanup -- and if that fails too, the log
  says the resource is abandoned rather than implying something is still in flight.

  This is as far as a runtime-free destructor reaches. A genuinely durable recovery -- a record
  on disk that a later run replays -- is a different mechanism, and the layer that already
  exists for resources nobody removed is `outrig clean`. Not covered by a test: the path needs
  thread creation to fail, which a unit suite should not arrange, for the same reason the
  `EAGAIN` half above is not directly tested.

- **A kill in flight is not proof that the kill is what ended it.** The wedge exception keyed
  off the `killed` flag alone, and review found the race: a cleanup can exit on its own between
  the `try_wait` that reported it running and the `kill` that follows, the kill then succeeds
  against a process that is already a zombie, and the next scan files a real non-zero exit as
  "we stopped it" -- suppressing a retry that was owed. The status settles what the flag cannot:
  a killed process carries a signal and no exit code, so an exit *code* means the command
  reached its own ending whatever this module did afterwards.

  `ended_by_our_kill` is that rule, and `a_kill_that_lost_the_race_does_not_count_as_ours` pins
  it against constructed `ExitStatus` values. Unit-tested rather than raced for on purpose:
  reproducing it needs an exit inside the microseconds between two calls at the far end of a
  60-second deadline, which no test can arrange, while the rule that decides it is a pure
  function.

- **A per-attempt fact was stored with per-obligation lifetime.** The flag the wedge exception
  reads belonged to the `Wait`, and `start_retry` reset `since` and `retry_at` without it. That is
  reachable through the very race the bullet above describes: an attempt wedges, the kill goes
  out, the command had already exited with a *code* inside the window, `ended_by_our_kill`
  correctly declines to claim it, and the exit-code arm schedules a retry -- which inherits the
  kill. The retry then has no wedge deadline at all, since the deadline only fires while nothing
  has been tried against the attempt yet, and any signal that ends it is filed as this module's
  own, abandoning a `Reissue::Safe` obligation that was owed the attempt. The flag also went up
  when `child.kill()` returned an error, so the module could claim a killing it never delivered.

  `Kill` replaces the `bool` and carries the third state it could not: `Failed`, for a deadline
  that passed and a kill that would not go out. It keeps that kill from being re-issued and
  re-logged on every 50 ms poll, and it claims nothing about how the attempt ends, which is the
  honest answer -- a signal this module did not send did not come from this module. `start_retry`
  resets it to `Untried` beside `since`, for the same reason `since` is reset.

  Review's own remedy went further: retry every non-success status, which is to say drop the
  wedge exception. Declined, for the reason that has not changed since the round that added it --
  the next attempt is the same command against the engine that just held the last one for
  `WEDGED_CLEANUP`. The narrower half of the same objection was upheld; see the round below.

  `Kill::Failed` is reached only when `kill(2)` refuses a parent's signal to its own unreaped
  child, which a unit suite cannot arrange -- the same reason the `EAGAIN` paths above are not
  directly tested. `ended_by_our_kill` covers the classification; the assignment does not.

- **The admission tests were asserting against a premise they supplied themselves.**
  `admit_arrivals` took the pass's running count as a parameter, so both tests handed it the
  right value and checked what came back. `reap_loop` kept the actual bookkeeping, and regressing
  *that* -- back to `waiting.len()`, or dropping the charge on the receive that opens the pass --
  left both tests green. Which is the fourth round's defect exactly, guarded by a suite that
  would not have caught it.

  The pass is one function now. `admit_pass` is the whole thing the loop runs -- opening receive,
  budget, and the disconnect decision -- against the caller's own `held`, and `reap_loop` is a
  `while` over it. The intermediate vector and the threaded counter go with it, so the production
  code got shorter, and the boundary is observable from a test that supplies nothing but a
  channel and a backlog. The disconnect rule got a test it never had: a closed channel ends the
  loop only while nothing is held, since children in hand are still owed their reaps.

  Each of the three was checked by reintroducing the defect it names. The mutation that charges
  the budget against the held population is the interesting one: it fails the unit test *and*
  stalls two of the behavioral tests, so the condition the fourth round could only construct is
  now load-bearing in more than one place.

- **`Blockers` re-signalled numbers it had already retired.** `disarm`'s own doc stated the rule
  -- a confirmed-gone pid is stale, and signalling it again could reach whatever the kernel has
  since given the number to -- and the test broke it on every panic path, because `disarm` ran
  once at the very end and the generations assertion sits between it and the confirmations. A
  panic there ran `Drop` against seventy-odd numbers that no longer meant anything. The markers
  made it worse rather than safer: they still record a retired pid, so clearing `pids` alone was
  never going to be enough.

  Retirement is continuous instead. `confirm_all_gone` retires each pid the instant it is
  observed gone, and `kill_all` skips a retired number from *both* of its sources. A pid it has
  not reached yet is still live and stays armed, including when the assertion inside it is the
  one that fails, so the guard still covers the case it was written for. The rogue-generation net
  the markers provide is untouched, and `disarm` is gone: there is nothing left for it to do, and
  leaving the markers armed through the end of the test means a generation that starts *after*
  the last assertion is still caught.

  Not mutation-tested, unlike the rest of this round: the property is "a signal that was not
  sent", which is only observable by instrumenting the kill. What was checked is that the guard
  still works -- a deliberately failed run leaves no sleepers behind.

- **`/simplify` on this round, and what it changed.** An independent implementation of all three
  fixes, written against the pre-round file in a worktree, arrived at the same three shapes: a
  per-attempt tri-state, one `admit_pass` covering the whole pass, and a retired-pid set. Where
  they differed it was consistently shorter, and its version was taken: `try_iter().take(budget)`
  for the bounded drain rather than a hand-rolled `while` (`Take` stops without consuming the
  arrival it does not have budget for, so nothing is dropped); a `retired` check inside
  `kill_all` rather than a separate step that folded the markers into the armed set; a blocking
  `wait()` in the retry test rather than a polling loop; and a disconnect test covering both
  branches rather than only the empty one. Same behavior, 33 fewer lines.

- **"A signal after our kill" was still a causation claim the evidence did not support.** Review
  raised the wedge exception twice. The second time it narrowed the ask -- require the collected
  signal to *be* `SIGKILL` -- and that half is right. The round above declined it on the grounds
  that a module which has already delivered a `SIGKILL` was about to abandon the obligation
  anyway, so the outcome is the same either way. That argument does not survive the code it is
  defending: `Kill::Failed` -- deadline passed, kill refused -- is *retried*, while `Kill::Sent`
  is not. The rule is keyed on what this module caused rather than on the deadline having passed,
  so it has to be held to the causation standard it set for itself. `Child::kill` returning `Ok`
  says a signal was queued, not that it is the one `wait` went on to collect.

  Reachable rather than theoretical: `spawn_cleanup` sets no process group, so a detached cleanup
  sits in outrig's own, and a Ctrl-C or an operator's `SIGTERM` during teardown reaches every
  cleanup in flight -- including one already past its deadline with a `SIGKILL` on the way.
  Collecting that `SIGTERM` abandoned a `Reissue::Safe` removal whose engine-side outcome was in
  fact unknown, which is the case the replay exists for.

  Narrowed rather than removed: `SIGKILL` after a delivered `SIGKILL` is still terminal, so the
  exception keeps the case it was written for, and `Reissue::Once` still never reaches the retry
  arm. The OOM killer is the one this cannot separate, since it sends `SIGKILL` too -- unfixable
  from an exit status, and the conservative way round, because a machine that is out of memory is
  not one to spend three more processes on. The claim in the round above that a `SIGKILL` check
  "buys no precision" was wrong: it buys everything except the OOM case.

- **A test comment -- and this commit's own message -- still described removal by name.**
  `a_dropped_container_handle_removes_its_container` said a constructed handle is removed "by
  name, not by id" because its name is provably its own. That is the reasoning an earlier round
  already overturned when `stop`'s removal was scoped: `Drop for Container` passes `self.attempt`
  to `removal_cmd`, which takes the label branch whenever there is a token, so an owned handle
  has removed by label since that round. The name branch is what attached containers would use,
  and `Drop` returns early for those, so nothing reaches it there. The test still passed because
  the fake records a removal under what it actually removed rather than under the argv that
  selected it.

  Low severity and worth fixing anyway: that comment was the only thing in the test saying *why*
  the assertion has the shape it does, and it argued for the mechanism this task removed. The
  commit message had inherited the same sentence and is corrected with it. By-name removal
  survives in exactly one place now -- `spawn_detached_rm`, behind `force_remove_detached` and
  the panic hook, whose callers hold nothing but a name. That is the hazard already filed in
  `plan/next/panic-sweep-removes-by-requested-name.md`, now cross-referenced to
  <https://github.com/tgockel/outrig/issues/147>, which carries the acceptance criteria.

  The round after found two more of the same kind, both in `cancellation.rs` and both describing
  a design this task *considered and rejected*: that the guard removes by "the id podman
  recorded". It does not, and `NameGuard`'s own doc says why -- a `--cidfile` is written after the
  container exists, so a cancellation in that interval leaves behind exactly the container the
  guard is for, which is the reason the selector is a label carried in the creation request. A
  comment arguing for the rejected alternative is worse than no comment, because it reads as the
  rationale for the test beneath it and would point a later change back at the leak. Only one of
  the two was flagged; the other turned up on a grep for the same claim.
