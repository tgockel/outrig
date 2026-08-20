# 0127 -- A cancelled build owns the working containers buildah made for it

## Context

0116 made a dropped future kill the client it spawned. For `buildah build` that closed one leak
and opened a smaller one: the client used to survive the drop, finish, and remove the working
containers it had created per stage. SIGKILLed, it never gets there.

Measured on buildah 1.42.1 -- a two-stage build killed mid-`RUN`, from an empty
`buildah containers`:

```
CONTAINER ID  BUILDER  IMAGE ID     IMAGE NAME                       CONTAINER NAME
213c716fa504     *     69e8c386a4ef localhost/outrig-cache:d8aa33... outrig-cache-working-container
```

So this is a measurement, not a worry. Two things make it more than a tidy-up:

- **The name is buildah's, not outrig's.** `<base-image>-working-container` is derived from the
  base image, so two builds from one base contend for it and nothing in the name identifies the
  build that made it. Removing by that name is removing by a string this process cannot prove is
  its own -- the exact defect 0116 fixed for containers by stamping `org.outrig.attempt`.
- **`build_standalone` has no engine-resource guard at all.** It builds straight into the
  caller's tag, so there is no temporary tag and nothing armed; everything above applies to it
  with one fewer layer.

Repeated cancellations therefore accumulate working containers, their mounts, and their storage,
and `outrig clean` cannot see them: it sweeps containers carrying `org.outrig.session`, and these
carry nothing of outrig's.

## Goal

A cancelled build leaves the engine as it found it, and what it removes is provably its own.

## Deliverables

- **An attributable marker on whatever a build creates**, or a per-build storage scope that makes
  attribution unnecessary. Establish first what buildah 1.42 actually offers -- whether stage
  working containers can be labelled, whether `--storage-opt` / a private root is viable for a
  build outrig then has to `podman`-read, and what each costs in cache reuse. The answer decides
  the shape of everything below, and it may be that no marker exists, in which case fork 1 stands.
- **A guard over the build that removes what it made**, armed before the spawn and scoped to that
  marker, in the shape `CleanupGuard` already provides -- so a cancelled, failed, or panicking
  build discharges the same obligation through `supervise::detach_cleanup`.
- **`build_standalone` gets the same guard.** It is the path with none today.
- **A decision on cooperative shutdown, recorded either way.** SIGTERM plus a bounded wait would
  let buildah do its own cleanup, but `Drop` cannot await, so it has to become a stop signal
  threaded through the build's callers -- which reopens 0116's fork 1 for one command. Take it or
  refuse it explicitly; do not leave it implied by the code.
- **`outrig clean` sweeps what is left**, if the marker makes that safe: strays predating this
  task are already on developers' machines and nothing else will collect them.

## Acceptance

- A live multi-stage build, cancelled mid-stage, leaves `buildah containers` exactly as it was
  before -- asserted against a real buildah, since only a real engine can answer it. This is the
  regression test; the shell fakes in `tests/cancellation.rs` cannot prove engine state, as 0116
  records.
- The same for each build entry point, since they differ in what they arm: the cached path
  (`build_image_with_build_args`), the transcript-logged path
  (`build_image_logged_with_build_args`), and `build_standalone`, which arms nothing today.
- A cancelled build removes **nothing** belonging to a concurrent build from the same base image.
  Two builds racing on `<base>-working-container` is the case a name-based cleanup gets wrong, and
  the one this task exists to get right.
- Cancelling repeatedly -- say twenty times in a loop -- leaves no growth in
  `buildah containers`, and none in `podman images` beyond what a completed build would leave.
- Deliberately breaking the guard fails at least one of the above, checked rather than assumed.

## Dependencies

- **After 0116** (landed), which built `CleanupGuard`, `supervise::detach_cleanup`, and the
  per-attempt label pattern this reuses.
- **Before 0128**, which cuts rc.3: this changes cancellation behavior, and the RC should ship it
  rather than describe it.
- **Shares a fixture with 0129**, which stands up the live-podman e2e row. If 0129 lands first,
  this task's acceptance runs in that harness rather than building its own.

## See also

- `plan/done/0116-cancellation-owning-subprocesses.md` -- the task that caused this, its
  `## Decisions` entry on why it was deferred, and the `NameGuard` argument about names versus
  claims that this reuses.
- `plan/next/image-cleanup-releases-its-guard-on-failure.md` -- adjacent: the existing guards are
  released even when their removal failed, which this task should not inherit.
- `plan/next/panic-sweep-removes-by-requested-name.md` -- the same by-name-removal defect in the
  panic hook.
- `crates/outrig/src/image.rs` -- `build_image_with_build_args`, `commit_image_with_labels`,
  `build_standalone`.
