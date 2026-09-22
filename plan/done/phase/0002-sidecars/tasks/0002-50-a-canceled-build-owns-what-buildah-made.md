# 0002-50 -- A cancelled build owns the working containers buildah made for it

## Context

0002-39 made a dropped future kill the client it spawned. For `buildah build` that closed one leak
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
  its own -- the exact defect 0002-39 fixed for containers by stamping `org.outrig.attempt`.
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
  threaded through the build's callers -- which reopens 0002-39's fork 1 for one command. Take it or
  refuse it explicitly; do not leave it implied by the code.
- **`outrig clean` sweeps what is left**, if the marker makes that safe: strays predating this
  task are already on developers' machines and nothing else will collect them.

## Acceptance

- A live multi-stage build, cancelled mid-stage, leaves `buildah containers` exactly as it was
  before -- asserted against a real buildah, since only a real engine can answer it. This is the
  regression test; the shell fakes in `tests/cancellation.rs` cannot prove engine state, as 0002-39
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

- **After 0002-39** (landed), which built `CleanupGuard`, `supervise::detach_cleanup`, and the
  per-attempt label pattern this reuses.
- **Before 0002-52**, which cuts rc.3: this changes cancellation behavior, and the RC should ship it
  rather than describe it.
- **Shares a fixture with 0002-53**, which stands up the live-podman e2e row. If 0002-53 lands
  first, this task's acceptance runs in that harness rather than building its own.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-39-cancellation-owning-subprocesses.md` -- the task that
  caused this, its `## Decisions` entry on why it was deferred, and the `NameGuard` argument about
  names versus claims that this reuses.
- `plan/next/image-cleanup-releases-its-guard-on-failure.md` -- adjacent: the existing guards are
  released even when their removal failed, which this task should not inherit.
- `plan/next/panic-sweep-removes-by-requested-name.md` -- the same by-name-removal defect in the
  panic hook.
- `crates/outrig/src/image.rs` -- `build_image_with_build_args`, `commit_image_with_labels`,
  `build_standalone`.

## Decisions

- **No attributable marker exists at buildah's CLI surface, so the answer is cooperative
  shutdown.** Checked before designing anything, as the first deliverable asks. `buildah build
  --label` targets the final image and `--layer-label` the intermediate images; neither touches
  a stage working container. `BuilderOptions.ContainerSuffix` exists in buildah's Go API and is
  exposed by no CLI flag -- absent from 1.33.7's flag list and from current
  `docs/buildah-build.1.md`. And a marker would not be selectable anyway:
  `buildah containers --filter` accepts only `id`, `name`, and `ancestor`. A private `--root`
  / `--runroot` would make attribution trivial and was rejected on cost: every build would
  start from a cold layer cache and the result would have to be copied layer by layer into the
  store podman reads.

  So outrig asks buildah to stop and buildah removes its own containers, by the ids only it
  holds. Attribution moves to the one process with perfect knowledge of it, and outrig never
  names anything it cannot prove is its own -- which is the property 0002-39 established and
  the one a name-based sweep would have given up.

- **Cooperative shutdown did not become a call shape.** 0002-39 deferred this partly because
  "the suggested cooperative shutdown cannot run in `Drop`, which has no runtime to await on;
  it would have to become a stop signal threaded through the build's callers". It did not.
  `Owned::Drop` already spawns a detached reap task; the grace and the `SIGKILL` escalation
  live inside that task, so `Drop` still returns synchronously and no caller's signature
  changed. The claim a spawn makes is `Termination`, stated at `Cmd::spawn_owned` the way
  `Reissue` is stated at a cleanup rather than guessed from an argv.

- **The graceful policy is scoped to `buildah build` and nothing else.** `podman run`/`create`/
  `exec`, `buildah from`/`config`/`commit`, `podman pull` and the rest keep `Termination::Kill`.
  The test is not "would a grace be polite" but "does this command own something outrig cannot
  name". `commit_image_with_labels` is the near miss: it does create a working container, but
  outrig named it `outrig-label-<pid>-<nonce>` and a `CleanupGuard` already removes exactly
  that, so a grace there would add a weaker second route to a removal that is already provable
  -- and would leave buildah holding the container while the detached `buildah rm` ran.

- **A stopped child has to be able to write, which cost a second round of measurement.** The
  first live run still leaked. `Drain` aborts when dropped, and a dropped future drops the
  drain before it drops the `Owned`, so the pipe's read end was gone before the signal went
  out; buildah's next write to stderr got `EPIPE`, and Go turns that into `SIGPIPE` on fds 1
  and 2 and dies on the spot. Measured directly against buildah 1.33.7: with the read end held
  open the build exits 255 and leaves nothing, with it closed the build exits on signal 13 and
  leaves its working container. `Owned` now holds a duplicate of each pipe's read end until the
  reap. `a_graceful_child_can_still_write_while_it_stops` fails without it, checked by
  disabling the keepalive rather than assumed.

- **Holding the pipes open required a matching invariant on the drains, which review caught.**
  The keepalive fixed the `SIGPIPE` death but created a worse failure one step over: with the
  read end held open, a drain that stops before the child does leaves nobody reading, and the
  child blocks once the 64 KiB pipe buffer fills. `run_streamed` awaits the child *before*
  joining its drain, so that is a permanent hang -- on the ordinary path, with no cancellation
  and no grace to end it. Two ways in, both reachable without any cancellation: `lines()`
  yields `Err` for invalid UTF-8, which a `RUN` writing binary to stderr produces, and
  `capture_stream` returned early when a transcript write failed. Before the keepalive, both
  merely closed the pipe and killed the child.

  So the keepalive's real precondition is "something reads this until EOF", and the drains now
  hold it: `run_streamed` reads bytes and converts lossily, so invalid UTF-8 cannot end it, and
  `capture_stream` remembers a transcript failure and reports it *after* draining rather than
  instead of draining. Checked by reverting the drain and watching
  `a_graceful_child_writing_invalid_utf8_still_finishes` hang for its full timeout.

- **The temporary tag is bounded, because `build_standalone` made it caller-controlled.** The
  cached path always hands `temporary_build_tag` a `<repo>:<16 hex>`; a standalone project
  hands it whatever ref it declared, and an OCI tag may be 128 characters. Echoing a
  128-character tag back under a pid and nonce pushes the *build* past the limit and fails a
  build that used to succeed. The echoed part is now capped at 32 characters and the nonce
  rendered as fixed-width hex, so the whole tag is bounded by construction rather than by the
  caller being brief.

- **An unknown age has to survive podman's encoding of it.** The skip-and-report rule was right
  and the implementation leaked anyway: podman serializes an unset creation time as Go's zero
  `time.Time` (`-62135596800`, or `0001-01-01T00:00:00Z`), and clamping that to the epoch reads
  "no idea when this was made" as "made in 1970" -- which clears every cutoff and makes the one
  container the sweep must not touch its most eligible target. A value at or before the epoch
  is now read as no value at all.

- **The non-`RUN` window is a known gap, recorded rather than closed.** buildah registers its
  handler only while a `RUN` instruction's command is executing. A build stopped during a pull,
  a `COPY`, the commit, or the seam between two `RUN`s still ends where it stands. This is not
  theoretical: the first version of the e2e tests cancelled the instant the working container
  appeared -- which is *before* the `RUN` starts -- and leaked on roughly one cancellation in
  three. The tests now step over that gap deliberately (`RUN_SETTLE`), because the claim being
  measured is about a cancellation mid-`RUN`; the gap itself is what
  `outrig clean --build-containers` exists to collect. Nothing outrig can do closes it: a
  process that has already died cannot be asked again.

- **`build_standalone` builds into a temporary tag, and the caller's tag is never guarded.**
  It had no guard at all, and now arms the same `temp_tag_guard` the cached paths use, with
  `buildah tag` promoting the result on success. The final tag is deliberately *not* covered:
  it is the caller's requested output, may already exist, and may belong to something else --
  removing it on cancellation would be exactly the by-name removal this task exists to avoid.
  Note that the `buildah rmi` after a successful `tag` is an untag, not a delete, because the
  image carries two names by then.

- **`temporary_build_tag` had a latent bug that only `build_standalone` could reach.** It split
  the caller's ref on the last `:`, which is right for the cached path's `<repo>:<16 hex>` and
  wrong for an arbitrary ref: `localhost:5000/team/img` yielded a "tag" of `5000/team/img`,
  which no engine accepts, and a ref with no tag silently built into the `outrig-cache`
  repository instead of the caller's. Now only a `:` after the last `/` introduces a tag. No
  existing caller's output changes.

- **`outrig clean --build-containers` is opt-in, and the flag *is* the safety.** The user chose
  this over reporting only. There is no marker, so the sweep is "every buildah working container
  older than the cutoff", including one a person made with `buildah from` -- which is why it is
  off by default, previews every container before asking, and removes by container id even
  though the decision to remove was reached from a name and an age. A container whose creation
  time cannot be read is reported and skipped: an age that cannot be established is not an age
  past the cutoff, and the cutoff is the only thing keeping this away from a build in flight.

- **The new sweep gets its own listings rather than widening `podman ps -a`.** Adding
  `--external` to `list_all_containers` would have been fewer lines and would have fed buildah
  working containers into the two existing sweeps' inputs, where `parse_all_containers` would
  drop them from `labeled` for lacking `org.outrig.session` but still admit them to the
  *running* set -- so a working container could shadow a session's `container_name` and make
  the record walk skip a session that had actually finished. Two listings, joined on id in
  `cli/build_containers.rs`, and the other two sweeps see exactly what they saw before.

- **The temporary-tag envelope is now one function, because this made it a third copy.**
  `into_temp_tag` holds the arm-before / cleanup / release-after ordering that
  `build_image_with_build_args`, `build_image_logged_with_build_args` and now
  `build_standalone` all depend on. That ordering is load-bearing, and
  `plan/next/image-cleanup-releases-its-guard-on-failure.md` already records a latent bug in
  it -- the unconditional `release()` disarms the retry even when the cleanup failed. Adding a
  third site for a known bug was the argument for collapsing them: the queued fix now lands
  once. `commit_image_with_labels` keeps its own variant, since it guards a working container
  rather than a tag.

- **The fake `buildah build` publishes after its trap, not before.** Found by the harness
  failing under load rather than in isolation. `tests/cancellation.rs`'s whole design is that
  an invocation becomes visible to a test only once acting on it is safe; the parking block
  first added here published *before* installing its signal handler, so a test cancelling on
  first sight raced the trap and measured the window before the client could catch anything.
  The invariant is the file's, not this task's -- but it is one a new verb can break silently,
  which is why the ordering is now commented where it happens.

- **Filed rather than fixed: the stray sweep treats an unknown age as ancient.**
  `parse_all_containers` maps a missing `Created` to `SystemTime::UNIX_EPOCH`, which clears
  every cutoff, so an undatable labeled stray is removable regardless of `--older-than` -- the
  inverse of the rule adopted here. Fixing it changes what the existing sweep removes, which is
  outside this task; queued as `plan/next/stray-sweep-treats-an-unknown-age-as-old.md`.

## Verification

Run live against **buildah 1.33.7 / podman 4.9.3** (not the 1.42.1 the original measurement
used; buildah's signal handling predates both). `crates/outrig/tests/build_cancellation_e2e.rs`,
6 tests, all passing:

- A cancelled two-stage build leaves `buildah containers` as it found it, and does so in
  **148-215 ms** -- fast enough to prove buildah unwound rather than absorbed the stop and ran
  the `RUN sleep 30` to completion.
- The same for each entry point: the cached path, the transcript-logged path, and
  `build_standalone`.
- Twenty cancellations in a loop leave no working container and no change to the image store.
- Cancelling one of two builds racing on the same base leaves the other's container, asserted
  on its id, and that build still succeeds.
- The negative control -- `image::ungraceful_build_termination`, an `e2e`/`test`-only wrapper
  that scopes the old behavior to the one call under test -- leaves the working container
  behind, so the five results above are the mechanism working rather than the engine having
  been clean anyway.

Two `network::tests::the_accept_loop_*` unit tests fail on this branch and **fail identically on
trunk** (checked in a clean worktree at 92798483); they are unrelated to this change.
