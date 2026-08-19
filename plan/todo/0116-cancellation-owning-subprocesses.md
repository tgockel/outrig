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
  tag survive. These belong with the live-podman work in 0128; the PID-level cases run against
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
