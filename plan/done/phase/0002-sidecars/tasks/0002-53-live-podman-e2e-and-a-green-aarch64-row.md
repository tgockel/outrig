# 0002-53 -- Run the e2e suite for real, on both architectures

## Context

0002-15 existed because a whole feature-gated test suite had rotted unnoticed: `e2e` was declared on
both crates and no CI job compiled it. 0002-15 added a matrix row, and that row runs `cargo test
--no-run`. So the suite compiles and links; it has never been executed against a live podman in
CI.

The 0.2.0 audit inherited that limit and said so. Among the things it explicitly does **not**
claim: live podman e2e execution (compiled only, as in CI), sanitizer and Miri cleanliness,
native AArch64 runtime and syscall behavior, and CUDA/Metal runtime coverage.

That matters more for this release than usual, because the queue ahead of it changes exactly the
code a compile-only suite cannot exercise: 0002-39 changes subprocess ownership and kill semantics,
0002-40 changes namespace-entering rollback and nft teardown, and 0002-37 changes what the
interceptor lets through. Every one of those is a runtime property. `--no-run` proves none of them.

AArch64 is the second half. outrig's container work is syscall- and namespace-heavy -- `nsfork`,
`container/enter`, the nft rules -- and none of it has a green native run on ARM. Claiming both
architectures are ready without one is a claim nobody has checked.

## Goal

A live podman e2e run on x86-64 and a green native AArch64 row, both before 0.2.0 is described as
ready on either architecture.

## Deliverables

- **One standard, applied to both architectures.** Fork 1 picks whether the evidence is a CI row
  or a recorded manual run; whichever it is, x86-64 and native AArch64 produce the *same* kind of
  evidence. The earlier draft alternated between "CI row", "CI run", and an acceptable one-off
  manual ARM run, which makes the acceptance criteria unfalsifiable.
- **A live e2e run on x86-64**: podman available, `cargo test --features e2e` actually run rather
  than `--no-run`. Expect to fix what it finds; a suite that has never run is a suite with unknown
  failures, and budget for that rather than treating a red first run as a blocker discovered late.
- **A native AArch64 run.** Native, not emulated -- the point is syscall and namespace behavior,
  which is what emulation is worst at.
- **One exact invocation, on both architectures.** Both crates declare `e2e`, so the feature has
  to be named per package or one crate's suite silently stays out:

  ```sh
  cargo test --workspace --locked --features outrig/e2e,outrig-cli/e2e
  ```

  Recorded verbatim, so "we ran e2e" cannot mean two different things on the two machines.
- **Durable evidence**: test counts, per-test names, the environment (podman version, kernel,
  arch), and an explicit list of anything skipped and why.
- **Whichever findings are real, fixed or filed.** Anything that is an environment artifact gets
  written down in the job, not silently retried.
- **The claim, once earned.** `README.md` / `doc/` say which architectures have a green live run.
  Until then they should not imply both.
- **Do not fold in the rest of `plan/next/ci-configuration-coverage.md`.** Its `cargo hack
  --each-feature` job, its cache-bucket and sccache cleanups, and its MSRV check are independent
  and stay in the buffer; its `macos-latest` x `local-llm,metal` item is contingent on 0002-46's
  decision and may evaporate. Cross-reference both ways so neither is done twice.

## Acceptance

- Evidence that the e2e suite **executed against a live Podman on each architecture** -- x86-64
  and native AArch64 both, with test names and pass counts, not a link step. `--no-run` anywhere
  in the live invocation is a failure of this task, and so is an ARM row that only compiles.
- Both architectures ran the exact invocation above and produce the same evidence shape, per
  fork 1's choice.
- **The lifecycle regressions from 0002-39 and 0002-40 run under live podman**, not only against
  fakes. Fakes prove the ownership logic; podman proves the container actually went away, that no
  buildah working container or temporary tag survived a canceled build, and that a `detach` really
  ended its bridges.
- **0002-37's live tier runs here too**: a forged `Host`/SNI connection is denied against a real
  interceptor, and a container that resolved the name through outrig's own DNS is allowed. Those
  are the two halves of the security fix that policy-level tests cannot reach, and omitting them
  would leave the release's headline blocker verified only in unit tests.
- Anything skipped is listed with a reason; a silent skip reads as a pass.
- Nothing in the repo claims an architecture is ready that does not have this evidence.

## Design forks

1. **CI rows versus recorded manual runs -- Open, but pick one for both architectures.** GitHub's
   ARM runners or a self-hosted box give a repeatable row that protects the *next* release and
   cost setup. A recorded manual run is cheap, satisfies this gate, and protects nothing
   afterward. Mixing them -- a CI row on x86-64 and a manual run on ARM -- is defensible only if
   the asymmetry is written down as a deliberate choice with an expiry, rather than arrived at
   because ARM was harder.
2. **Whether the live job gates every PR -- Recommended: no.** Live podman is slow and flaky in
   shared CI. A scheduled run plus a release-time run gets the coverage without the per-PR tax,
   matching 0002-48's posture on the snapshot gate.

## Dependencies

- **Soft: after 0002-39 and 0002-40**, whose regressions this is meant to exercise for real.
- Independent of 0002-52; it can run against the rc.3 tree or before it, but the release should not
  be described as ready on an architecture until this lands.

## See also

- `crates/outrig/src/nsfork.rs`, `crates/outrig/src/container/enter/`,
  `crates/outrig/src/network.rs` -- the syscall- and namespace-heavy code an AArch64 row exists
  to exercise, and which `--no-run` proves nothing about.
- `crates/outrig/tests/library_surface.rs`, `crates/outrig-cli/tests/e2e_quickstart.rs`,
  `crates/outrig-cli/tests/primary_view_e2e.rs` -- the `e2e`-gated suites that have never run.
- `plan/done/phase/0002-sidecars/tasks/0002-15-e2e-imageconfig-sidecars-bitrot.md` -- where the
  `--no-run` row came from, and why the class of gap it left is still open.
- `crates/outrig/tests/build_cancellation_e2e.rs` -- the live-engine harness 0002-50 built
  rather than waiting for this task: unique per-test base images so every assertion is a delta,
  and helpers for reaching a build's `RUN` window and waiting the engine back to clean. It is
  the shape this row's other engine-state checks should take, and it needs a real buildah, so
  it is one of the suites this has to actually run.
- `plan/next/ci-configuration-coverage.md` -- the sibling entry, deliberately not absorbed.
- `.github/workflows/ci.yml` -- the matrix this extends.

## Decisions

1. **Fork 1 -- CI rows on both architectures, because the premise that blocked this was
   false.** The `--no-run` row existed on a stated reason: "Its tests drive real podman, which
   the runner has no way to provide" (`ci.yml:34-36`), repeated in `CONTRIBUTING.md`. The
   `actions/runner-images` manifests say otherwise. `ubuntu-24.04` and `ubuntu-24.04-arm` both
   ship **podman 4.9.3** and **buildah 1.33.7** preinstalled -- the same versions `49dbb32e`
   measured the build-cancellation work against -- and the repo is public, so the ARM runner is
   free and `ci.yml:53` was already using one. Nothing had to be procured. The row had been
   compile-only for want of checking a claim.

   So the evidence is a CI row per architecture, in a new `live-e2e` job, and the invocation is
   written out literally rather than assembled from matrix fields. The labels are pinned rather
   than `ubuntu-latest`, because this job is evidence about an engine version and
   `ubuntu-latest` moves to 26.04 (podman 5.7.0) on a schedule of its own.

2. **Fork 2 -- per-PR, and the task's argument for the opposite rests on a misreading.** The
   fork recommended a scheduled plus release-time run "matching 0002-48's posture on the
   snapshot gate". 0002-48 chose the opposite: a per-PR job, having rejected a release-time gate
   *because this repo has no release workflow to hang one on*. Recorded so the citation is not
   repeated. Per-PR it is, and there is no `schedule:` trigger anywhere in `.github/` to have
   used in any case.

3. **The compile-only `e2e` row is deleted rather than kept beside the live job.** Its clippy
   step is carried over into `live-e2e` -- 0002-15 measured that as the load-bearing gate, and
   it still fails in seconds rather than after the suite has built and started pulling images --
   and its `cargo test --no-run` is strictly weaker than running the tests on the same
   architecture. Keeping both would build the e2e configuration twice on x86-64 and hold a
   second cache bucket. The now-dead `test_args` matrix field goes with it.

## Decisions -- what running the suite actually found

The first execution of a suite that had only ever been linked failed 11 tests out of 1354. Two
were already-filed test-side flakes. The other nine were one defect, and it was the serious
kind.

4. **The network interceptor installed no rules at all on nft 1.0.9, and therefore enforced
   nothing.** `nft_rules` emitted one statement:

   ```
   create table inet <t> { chain output { ... } }
   ```

   nft 1.0.9 -- what Ubuntu 24.04 ships, so what every GitHub runner and this dev box has --
   parses that, exits **0**, creates the table, and silently drops the nested block. Measured
   directly: `nft list table inet <t>` returns `table inet <t> { }`, and `--echo` reports only
   the table line where the working form reports the chain and every rule. Switching `create
   table ... { ... }` to `table ... { ... }`, or to flat `add chain` / `add rule` statements,
   installs the ruleset correctly. It is specific to `create` plus a nested body.

   The consequence is not subtle. With no chain in the table nothing is redirected to the
   interceptor, so `audit` mode records no connection at all and `filter` mode allows every
   connection, including the ones a `default = deny` policy exists to refuse. `attach` and
   `detach` both report success throughout, and the container reaches the network normally, so
   there is no symptom short of noticing that the audit log is empty.

   **This is a security-semantics defect present in 0.2.0-rc.3**, which is the class
   `0002-52`'s `## Decisions` records as forcing another candidate rather than a judgment call
   about severity. `0002-54` should treat it as such.

   The fix keeps `create`'s two properties -- it still fails rather than merging if a table of
   that name already exists, which is what the handle-identity teardown rests on, and `nft -f`
   is one transaction either way, so a failure leaves nothing behind. Both verified against nft
   1.0.9: a second `create table` of a live name exits 1 with "File exists" and adds nothing.

5. **No unit test could have caught it, and the one that existed was shaped so that it never
   would.** `nft_rules_redirect_tcp_and_dns_but_skip_loopback` asserted that the generated text
   *contained* each rule. Both the working and the broken form satisfy that, and nothing ran
   nft. It now asserts the shape -- `create table` alone on the first line, every other line an
   `add chain` or `add rule` -- which is the property that differs. That is a genuine
   regression test, but it is a weaker one than the suite this task turned on: the live tier is
   what found this, and it found it on the first run.

6. **Pre-existing flakes, both already filed, and both now more exposed.** Neither is folded in;
   both entries were updated with the new sighting.

   - `process::process_tests::run_streamed_forwards_stderr_to_tracing`
     (`plan/next/lib-unit-test-flake.md`) is a third test in the family that entry describes,
     failing by the same mechanism -- a thread-local subscriber asserting on tracing's global
     callsite cache. The entry named two tests; the fix has to cover the file.
   - `clean_sweeps_stopped_recordless_labeled_containers` is the next decision.

7. **`outrig clean` reported removing containers it had not removed, and that one is fixed
   here.** `plan/next/clean-batch-removal-fidelity.md` had it diagnosed down to the fix shape
   and warned it was self-perpetuating. Both halves were confirmed: it failed in the first full
   run, left `outrig-straytest-13d195a7` in the store, and then failed the *second* run because
   that leftover joined the next batch. Twelve `outrig-*` containers were sitting on this
   machine from previous tasks' runs for the same reason.

   Fixed rather than left filed, because the task's own gate depends on it: a per-PR job whose
   failure mode is "fails again every run once it has failed once" is not a gate. The
   implementation is the entry's proposal plus two things it did not specify -- a name the
   batch skipped is retried with a `podman rm -f` of its own, which removes it at once, and a
   sweep that still left something behind exits 1. `execute_with`'s stray hook now returns the
   names still present rather than `()`; it is not public surface (`outrig-cli` publishes only
   `outrig_cli::run()`), so `public-api.txt` does not move.

8. **0002-39's engine-state half was the only acceptance item without a test.** 0002-37's and
   0002-40's live tiers already existed and had simply never been executed:

   - Forged `Host`/SNI denied
     -- `a_forged_sni_does_not_grant_a_hostname_allow`
   - A name resolved through outrig's own DNS allowed
     -- `a_resolved_name_grants_a_hostname_allow`
   - `detach` really ended its bridges
     -- `detach_restores_the_resolver_and_cuts_what_it_accepted`
   - nft teardown is by handle, not by name
     -- `detach_leaves_a_table_that_replaced_the_one_it_created`
   - No working container and no stray tag after a canceled build
     -- `build_cancellation_e2e.rs`, six tests
   - **No container under the reserved name after a canceled create**
     -- **`container_cancellation_e2e.rs`, new here**

   0002-39 deferred the last one here by name: "After a canceled create, `podman ps -a` lists no
   container with the reserved name ... These belong with the live-podman work in 0002-53."

9. **The new file cancels repeatedly rather than once, because there is no container-side
   negative control to copy.** `build_cancellation_e2e.rs` can take its own mechanism away --
   `image::ungraceful_build_termination` -- and watch the leak return. The container path has no
   equivalent; `container::is_tracked` is an in-process registry, not a switch. Without one, a
   single cancel that happened to land *before* podman created anything would satisfy "no
   container survives" and prove nothing. So each test cancels five times and asserts that
   enough of them landed somewhere that counts.

   **What counts differs by path, and finding that out took two goes** -- see the `/simplify`
   decisions. `create` is observable: `podman create` returns, the engine holds a container, and
   the future is still in `podman init`, a window hundreds of milliseconds wide. That test waits
   until the engine demonstrably holds the container and cancels then: **4/5**. `start` is not
   observable, because `podman run -d` prints the id only once the container is up and
   `start_named` returns as soon as it has parsed it -- the interval is shorter than the `podman
   ps` that would observe it, and polling for it wins **0 times in 5**. That test cancels on a
   swept timer instead and asserts the cancels landed while the call was in flight: **5/5**. It
   cannot also claim the engine had created the container by then; `create` is what claims
   that.

10. **Measured, and filed rather than fixed: removing a *running* container costs ten seconds.**
    Every removal of a container that had actually started took **10.4-12.0 s**, against
    **25-600 ms** for one cancelled before its `sleep` was running. The difference is not
    outrig's cleanup being slow -- it is that `sleep infinity` is PID 1 in the container's PID
    namespace, and PID 1 has no default signal dispositions, so it discards podman's SIGTERM and
    the stop waits its full grace before SIGKILL. That is every session teardown, not just a
    cancelled one, and the fake-driven tests cannot see it at all. Added to
    `plan/next/primary-image-needs-no-sleep.md`, which already owns the decision about that
    appended command, rather than opening a second entry about the same line.

11. **One more test was too tight for a loaded machine, and was widened rather than filed.**
    `mcp_handshake::stderr_captured_on_crash` waits for a crashed MCP server's stderr to reach
    its file, and allowed two seconds. It passed on the first two full runs and failed the
    third, on an empty file, with the machine under no particular strain -- the drain races
    every other test binary in the run for a core. The test's own comment already anticipated
    this ("`--features e2e` runs can be CI-bound"); the window is now ten seconds, expressed as
    a deadline. A ceiling like that bounds a hang and nothing else: a capture that works still
    returns the moment the bytes land, so a generous one costs nothing. This is the kind of
    thing only running the suite finds, and the kind that would have read as "the ARM row is
    flaky" if the first time it fired had been in CI.

12. **`quickstart_real_api` is the one skip, and it reports as a pass.** It returns early unless
    `OUTRIG_E2E_REAL_API=1` and an `OPENAI_API_KEY` are both set, printing
    `[outrig-e2e] skipping quickstart_real_api` to stderr -- but the harness prints
    `test quickstart_real_api ... ok`, which is exactly the "a silent skip reads as a pass" case
    this task's acceptance names. It is named here so the count below is not read as covering
    it. Nothing else skips: `enter_embedded.rs`'s early return cannot fire under
    `OUTRIG_REQUIRE_ENTER=1`, which both CI and the runs below set.

## The evidence

From the `live-e2e` rows on this task's own pull request (#168), which is the point of shaping
the job the way fork 1 chose: the evidence is a CI row, so it is reproducible rather than
recounted.

**The environment, printed into each row's log by the job's own `Record the engine` step** --
identical on both but for the architecture, which is what makes a difference between the rows a
difference in the architecture:

| | x86-64 | aarch64 |
| --- | --- | --- |
| runner | `ubuntu-24.04` | `ubuntu-24.04-arm` |
| kernel | `6.17.0-1022-azure x86_64` | `6.17.0-1022-azure aarch64` |
| podman | 4.9.3, rootless, `overlay` | 4.9.3, rootless, `overlay` |
| buildah | 1.33.7 | 1.33.7 |
| nft | 1.0.9 | 1.0.9 |

**The result**, from

```sh
cargo test --workspace --locked --features outrig/e2e,outrig-cli/e2e
```

run verbatim on both:

- **1357 passing assertions over 55 test targets, 0 failures, on each row.**
- **1355 distinct test names on each row, and the two sets are equal** -- neither row ran a test
  the other did not, and nothing was filtered out on either. (The two-name gap to 1357 is
  `embedded_image`, which exists in both crates.)
- x86-64 took 13m48s, aarch64 15m03s, both inside the job's 75-minute ceiling with room to
  spare.

**What the aarch64 row proves that nothing did before.** `ci.yml`'s old comment said the arm64
row "still never *execs* that helper, so the AArch64 syscall numbers in `launcher.rs` remain
unverified by CI -- the graft is a manual check against a live session". Three tests now exec a
natively built `outrig-enter` on AArch64 and all three pass:

- `primary_view_sidecar_sees_the_primary_filesystem`
- `primary_view_sidecar_from_library_sees_the_primary_filesystem`
- `primary_view_sidecar_on_glibc_runs_its_own_dynamic_loader`

That is the first execution of those syscall numbers anywhere. The comment is rewritten rather
than left to be read as still true.

**The acceptance-critical tests, green on both rows**, named individually because a total does
not say which ones ran: `a_forged_sni_does_not_grant_a_hostname_allow` and
`a_resolved_name_grants_a_hostname_allow` (0002-37's live tier);
`detach_restores_the_resolver_and_cuts_what_it_accepted` and
`detach_leaves_a_table_that_replaced_the_one_it_created` (0002-40's); the six
`build_cancellation_e2e` tests and the two new `container_cancellation_e2e` ones (0002-39's).

**Skipped: one, `quickstart_real_api`**, which returns early without `OUTRIG_E2E_REAL_API=1` and
an `OPENAI_API_KEY`, and is counted among the passes by the harness. It is the only one; see
decision 12.

**Not claimed.** These rows say nothing about sanitizer or Miri cleanliness, or about CUDA and
Metal, which the 0.2.0 audit also listed and which stay open. What they do settle is live podman
execution on both architectures, which was the rest of that list.

## Decisions from the /simplify pass

Four review passes ran over the change. Most of what they found was applied; what was not became
three `plan/next/` entries, because each is a production change with its own fixture churn.

1. **The new test had the defect it was written to measure.** `engine_holds` shelled out with a
   blocking `std::process::Command`, from inside a `tokio::select!` arm racing the creation
   future. `select!` polls both arms from one task, so for the 33-100 ms of every probe the
   creation was not polled at all -- the poll was *widening the window it was trying to land
   inside*. It is `tokio::process::Command` now.

   **Re-measuring through the fixed instrument overturned the result.** The 8/8 this task first
   recorded for both paths became 4/5 for `create` and **0/5** for `start`: without the stall,
   `start_named` always returns before a `podman ps` can see the container. The 8/8 was the
   instrument, not the engine. `start` therefore cancels on a swept timer now and asserts
   something it can actually establish -- that the cancel landed in flight -- and decision 9 is
   rewritten to say which path proves which half. Decision 10's figures were taken through the
   same distorted instrument and are corrected there too: what was measured was the cost of
   removing a *running* container, which is real, but it was not what it was labelled.

2. **`sweep()` was dead on arrival.** `cancel_once` ends by polling until the name is free, so
   every path that reaches `sweep` reaches it with nothing to sweep, and every path that would
   have left something panicked before it. Nine lines and one `podman ps` per attempt, deleted.
   It was a carry-over from `build_cancellation_e2e.rs`, where the equivalent does real work
   because that file builds images.

3. **`await_engine_free` polls at 250 ms, not 50 ms.** The figure it returns is printed, never
   asserted, and the wait is dominated by podman's ten-second stop grace besides -- so 50 ms was
   two hundred `podman ps` invocations to time something to a precision `podman ps` does not
   have. `await_engine_holds` keeps its 25 ms, because *that* one decides whether the cancel
   lands inside the window. Roughly 700-1300 podman spawns per run became 200-350.

4. **`Landing` and its `Vec` went.** The struct existed so a helper could `.max()` the free times
   for one log line. Each attempt prints its own now and the caller counts a `usize`.

5. **A claim this task made about CI caches was wrong, and is corrected in the entry that owns
   it.** The record said `live-e2e`'s two buckets "are genuinely distinct because they are
   different architectures". True of them against each other, and not the comparison that
   matters: `live-e2e (aarch64)` shares a runner label, a toolchain, and -- since `e2e = []` adds
   no dependency nodes -- a dependency graph with the `cargo (arm64)` row, so it cold-builds the
   same crates concurrently and stores a second copy. Deleting one duplicate bucket and adding
   another is a net of one. `plan/next/ci-configuration-coverage.md` now says so, and keeping the
   `arm64` row is a recorded decision in `ci.yml` rather than an oversight: it answers in minutes
   where the live job answers in fifteen, and its `cargo check` is the only AArch64 build of the
   published lib-and-bin-only shape.

6. **Filed rather than fixed, all three because they are production changes:**

   - `plan/next/nft-install-is-not-verified.md` -- the flat script fixes the instance; nothing
     checks what the engine committed. `install_interception` already parses the `--echo --handle`
     output for the table handle, and the broken form echoed *only* that line where the working
     one echoes a handle per object, so counting them catches the class. The obstacle is that the
     unit-test fake's echo reproduces the broken shape byte for byte and about eight attach tests
     assert success against it: the fixture was modeling the defect.
   - `plan/next/mcp-startup-error-loses-its-stderr.md` -- the 10 s widening in decision 11 is the
     test-side half of a user-facing defect. `enrich_startup_error` waits 250 ms for a `podman
     exec` child and then reads the stderr tail regardless, so a loaded host turns an
     `McpStartupFailed` into "(empty)". The test was already routing around it, asserting on the
     file rather than on `payload.stderr_tail`.
   - `plan/next/clean-verify-above-the-seam.md` -- decision 7's fix is correct but sits *below*
     `execute_with`'s injection boundary, so the batch/list/retry/list sequence has no test and
     the hook's signature now carries a strategy. It also records a cheaper signal than the extra
     `podman ps`: `engine::remove_batch` captures podman's stdout, which already names what it
     removed, and throws it away.

7. **Two reuse findings declined, with reasons.** `engine_holds` was not folded into
   `container_lifecycle.rs`'s `podman_ps_lists`, and the several `poll-until-deadline` loops and
   `E2E_LOCK` definitions were not lifted into `tests/common/mod.rs`. Both are real -- there are
   five polling loops with five intervals and seven copies of the lock -- but both are
   cross-file refactors of suites this task only had to run.
   `plan/next/test-helper-consolidation.md` is where that belongs.
