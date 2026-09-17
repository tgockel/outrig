# 0002-40 -- Interceptor attach rolls back, and detach ends every bridge it started

## Context

0002-01 made `NetworkInterceptor` multi-container: one `Attachment` per container, holding a
`CancellationToken`, a `Vec<JoinHandle<()>>`, and a `Cleanup`. Two defects follow from operation
ordering and task ownership rather than from any race, so both reproduce deterministically.

### Attach mutates before it can roll back

`attach` (`crates/outrig/src/network.rs:282-333`) does, in order:

1. `install_audit_resolv_conf(container)` -- rewrites the container's `/etc/resolv.conf` to point
   at `127.0.0.1` (`network.rs:305`, `915`);
2. `apply_nft_rules(&cleanup, tcp_port, dns_port)` (`network.rs:307`);
3. spawn the accept and DNS loops;
4. `self.attachments.insert(..)` -- the first moment anything records that cleanup is owed.

If step 2 fails, `attach` returns `Err`, the sockets bound at `network.rs:291` drop, and nothing
undoes step 1. A container that is already running now has its resolver aimed at a loopback port
with nothing listening: DNS is dead, silently, and the caller has an `Err` that says nothing
about it.

### Detach does not end the connections it accepted

`tcp_accept_loop` spawns each accepted connection and drops the handle
(`tokio::spawn(handle_tcp(...))`, `network.rs:606`). `Attachment::tasks` holds only the accept
loop and the DNS loop (`network.rs:309-321`), and the attachment's `CancellationToken` is never
passed into `handle_tcp` (`network.rs:624`). So `detach` cancels and joins the two loops, deletes
the nft table, and returns while any bidirectional copy already in flight keeps moving bytes and
keeps writing audit records for a container the interceptor has declared detached.

`detach`'s doc comment already admits the resolver half. A buffer entry (since absorbed into this
task, and so no longer present in `plan/next/`) had recorded it as unreachable-today, on the
grounds that every planned caller detaches immediately before stopping the container. The audit's
finding is that the *attach failure* path reaches the same state with the container still
running, so the invariant that entry relied on does not hold.

### A scope guard is not sufficient, and 0002-39 does not cover this

An ordinary Rust scope guard cannot await. Restoring `/etc/resolv.conf` means running a `podman
exec`; deleting an nft table means running `nsenter`. Neither is expressible in `Drop`. And 0002-39
solves a different problem: it makes a *child process* die when its future is dropped. It runs no
compensating action. A canceled `attach` that has already rewritten the resolver needs work to
*happen*, not work to *stop*.

So the rollback needs an owner that outlives the caller's interest. Note what that rules out: an
`attach` that *returns* a handle the caller must later `roll_back().await` does not qualify,
because the caller can be cancelled before the handle is ever returned, or while mutation is in
progress. A rollback obligation handed to a cancelled caller is not an obligation. Whatever
fork 2 chooses must be able to finish the compensating work with nobody awaiting it.

### Teardown is lossy in four separate ways

- `Cleanup::delete_table` (`network.rs:398`) calls `try_capture_logged(..).await?` and discards
  the `Output`. `try_capture_logged` does not check exit status, so a non-zero `nft delete` is
  indistinguishable from success.
- `detach` (`network.rs:342`) always returns `Ok(())`; `teardown_attachment` (`network.rs:374`)
  only `tracing::warn!`s a cleanup failure. Adding resolver restoration adds a second failure
  with nowhere to go.
- `teardown_attachment` does `tokio::time::timeout(SHUTDOWN_GRACE, task)` and drops the result.
  Timing out a `JoinHandle` and dropping it **detaches** the task; it keeps running.
- Nothing drains completed bridge tasks, so a long session accumulates handles even before
  detach.

## Goal

`attach` either fully succeeds or leaves the container exactly as it found it, including under
cancellation. `detach` and `shutdown` return only once nothing they started is still running, and
report what failed.

## Deliverables

- **Rollback is armed before the first mutation, and owned by something that can await it.** Read
  and keep the original `/etc/resolv.conf` bytes, register the cleanup obligation, *then* mutate.
  On error or cancellation, restore the bytes and delete whatever nft state was created,
  tolerating failure when the container is already gone.
- **Cancellation is injected at every boundary**, including immediately before the
  `attachments.insert` at `network.rs:326` -- the window where the work is fully done and nothing
  yet records that it is owed.
- **The `Attachment` carries the resolver snapshot**, so `teardown_attachment` restores it and
  attach/detach become a genuine inverse pair -- the shape the absorbed buffer entry asked for.
- **Accepted bridges are tracked per attachment.** Pass the attachment's `CancellationToken` into
  `handle_tcp` and cover **every** await inside it: the sniff read, the upstream connect, the
  initial replayed writes, the bidirectional copy, and the final audit write. A copy loop that is
  cancelable only between reads is not cancelable during a stalled connect.
- **Termination is cancel -> grace -> abort -> final join**, in that order, with the final join
  unconditional. Dropping a timed-out `JoinHandle` is what currently makes "joined" untrue.
- **The carrier is drained continuously.** A `JoinSet` accumulates finished tasks unless
  something reaps them; reap as connections complete, not only at teardown.
- **Teardown reports.** `delete_table` checks exit status. `detach` aggregates its failures --
  resolver restore, nft delete, task join -- into its return rather than logging and claiming
  success. `shutdown` gets the same treatment across attachments.
- The `dns_preconfigured` branch (`network.rs:304`) is unaffected -- the resolver was baked in by
  `podman create --dns` and there is nothing to restore. Say so, so the asymmetry reads as
  deliberate.

## Acceptance

- **Injected nft failure leaves working DNS.** Force `apply_nft_rules` to fail against a running
  container; `attach` returns `Err` and the container's `/etc/resolv.conf` is byte-identical to
  before. Assert on the bytes, not on the absence of an error.
- **Canceled attach is equivalent to failed attach**, tested at each injection point including
  the pre-insert window. This is the criterion a scope guard cannot meet.
- **A held-open socket stops.** Open a connection through the bridge, hold it, call `detach`, and
  prove *after the call returns* that no further bytes move and no further audit records are
  written. Proving it after return is the point; a test that waits is testing something weaker.
- **A stalled bridge is terminated, not detached.** A connection blocked on an upstream connect
  is gone once `detach` returns -- the case the current `timeout`-and-drop silently fails.
- **Teardown failures surface.** An `nft delete` that exits non-zero makes `detach` report it.
- No leaked tasks: a session that attaches and detaches repeatedly does not grow its task count,
  and neither does one that opens and closes many bridges within a single attachment.

## Design forks

1. **Abort versus graceful close for in-flight bridges -- Recommended: cancel, then abort on a short
   grace.** Cutting a copy loop mid-stream is visible to the container as a truncated connection,
   which is honest for a detach but not free. A grace bounded in the hundreds of milliseconds
   matches `plan/done/phase/0002-sidecars/tasks/0002-32-subagent-tree-shutdown-grace.md`'s posture.

2. **Who owns the rollback -- Open between two shapes, both of which must survive an unawaited
   caller.** An independently spawned transaction task commits or rolls back regardless of who is
   listening, at the cost of state that outlives the `&mut self` borrow; this is the default. A
   handle-based protocol works *only* if the handle exists before the first mutation and dropping
   it transfers the obligation to a joinable async owner rather than discharging it -- at which
   point it is the transaction task with a nicer signature. A plain scope guard, and a handle
   that is returned only on success, are both unavailable.

3. **Whether partial nft state is reachable at all -- Verify, do not assume.** This task's earlier
   draft asserted that `apply_nft_rules` can fail midway and leave a partial table. `nft` applies
   a batch transactionally, so that may be false, in which case the defensive delete is still
   correct but the acceptance criterion must inject an *observed* partial state rather than a
   hypothesized one. Check before writing the test.

## Dependencies

- **Hard: 0002-39.** The cancellation half needs a child that dies with its future; otherwise a
  canceled `attach` still leaves the `podman exec` that rewrote `resolv.conf` running. 0002-39 is
  necessary and not sufficient -- see Context.

## See also

- `crates/outrig/src/network.rs` -- `attach` (282), `install_audit_resolv_conf` (915),
  `apply_nft_rules` call (307), `Attachment` (224), `detach` (342), `shutdown` (352),
  `teardown_attachment` (374), `Cleanup::delete_table` (398), `tcp_accept_loop` spawn (606),
  `handle_tcp` (624), `dns_loop` (805).
- `plan/done/phase/0002-sidecars/tasks/0002-01-interceptor-multi-container.md` -- where
  per-container attachment was built.
- `plan/done/phase/0002-sidecars/tasks/0002-32-subagent-tree-shutdown-grace.md` -- the existing
  grace-period posture.

## Decisions

1. **Fork 2 resolved to a moving undo log, after `/simplify` overturned the first
   implementation.** This task shipped its second design. The first followed the Context's
   reasoning: `supervise::CleanupGuard`s armed before each change, released on commit, with the
   snapshot re-threaded through the `Attachment` so teardown could rebuild the same commands.
   It worked and was fully tested. `/simplify` produced an independent implementation that was
   smaller in a way that survived every attempt to explain it away, and it was adopted. The
   first is preserved on the `approach-a-reference` branch.

   What the guards cost was a *non-local* invariant. `commit()` released them, the
   `attachments.insert` recorded the obligation, and the gap between the two was safe only
   because no `.await` sat in it -- a property defended by a five-line comment rather than by
   the type. The undo log has no such gap: it is one `Rollback { pid, undo: Vec<Cmd> }` built on
   `attach`'s stack, *moved* into the `Attachment` on success and moved on into teardown. A move
   cannot be interrupted by a cancellation, so there is no window to reason about and no release
   protocol to get wrong. The same value serves attach-failure, cancellation, `detach`,
   `shutdown` and `Drop`, so the two undo commands are constructed once instead of at three
   sites from data held for the purpose.

   Both designs answer the Context's real question the same way, and it is the answer that
   matters more than the shape: `Drop` cannot await, but it can hand a command to
   `supervise::detach_cleanup`, which is synchronous and runtime-free. That is what makes the
   rollback survive a caller that is already gone -- including an embedder dropping the runtime
   the call was running on, which a rollback implemented as a spawned task would not survive.
   The Context's claim that "an ordinary Rust scope guard cannot await" is true and beside the
   point; it predates 0002-39 landing `detach_cleanup`.

2. **The pre-insert window is closed by the move, not by a rule.** The deliverables asked for
   cancellation to be injected immediately before `attachments.insert`. In the shipped design
   there is nothing to inject and nothing to remember: the `Rollback` is armed before the first
   mutation and is owned by something at every instant afterwards. The testable form of the
   claim is that a `Rollback` dropped without being discharged still issues its undos, and it is
   pinned by a plain `#[test]` with **no tokio runtime at all** -- which is the strongest
   available statement of the property, since it demonstrates the case where no runtime exists
   to run a spawned task.

3. **Fork 3 checked rather than assumed: partial nft state is not reachable.** `nft -f` applies
   a file as one kernel transaction, so the table either exists whole or was never created.
   That is recorded where the delete is armed. Unlike the first implementation, the shipped one
   does not track whether the apply succeeded: the delete is idempotent, so arming it
   unconditionally costs one doomed `nsenter` on a failed apply and buys the invariant that
   every armed undo is safe to run whether or not its mutation landed.

4. **The test seam is a closure over the command runner, not a trait over the commands.**
   `install_interception` and `Rollback::undo_now` are generic over `F: Fn(Cmd) -> Fut`;
   production passes the real runner, tests pass one that records `Cmd::render()` and can fail
   or hang on a substring. A `PATH`-shim binary was declined -- process-global, needs its own
   binary per fault, and would still not reach `attach`, whose socket binding needs real
   namespaces. CI runs the e2e row with `--no-run` (`ci.yml`), so anything reachable only from
   e2e is a test nobody executes, and these ordering rules are exactly the sort that rot
   silently.

   The seam's one weakness is known and was closed rather than accepted. Injecting *above* the
   runner means no test driving it exercises the runner itself, and breaking the runner's
   exit-status check passed the entire suite. R5's mechanism is now pinned directly by
   `a_non_zero_undo_reaches_the_caller`, which arms a genuinely failing command and drives the
   real `run_step`. The same reasoning added a test that runs the *actual* restore script under
   `sh` against a temporary file: the bytes travel as an argument so no quoting rule can mangle
   them, but the script still has to use `printf '%s'` rather than `echo`, and that is a claim
   about the script, not about the argv.

5. **The resolver's bytes travel as an argument.** `printf '%s' "$1" > /etc/resolv.conf` with
   the snapshot as the command's own last argv entry, rather than escaped into the script text.
   Content that never passes through quoting cannot be mangled by an escaping bug, so
   byte-exactness stops depending on a rule holding. Absence is a state, not an error: the
   presence probe prints a leading `1` or `0` ahead of the file, which also distinguishes an
   empty resolver from a missing one -- a bare `cat` reports both as nothing.

6. **A cut connection is recorded, and the record is written before the join returns.** The
   cancellation wraps the connection's whole exchange; the audit write sits outside it, so a
   connection detach cut still lands in the log, and `detach` joins the task, so it lands
   *before* `detach` returns rather than after. That is the difference between an interceptor
   that stopped and one that merely stopped being watched. A connection cut before its client
   ever spoke is recorded by address alone rather than dropped -- it consumed a redirect and a
   decision was taken about it, so the log should not be silent about it.

7. **Byte counts and the late client identity are written through as they happen.** `bridge`
   kept both in locals and merged them after `try_join!`, with a comment explaining that a
   cancelled sibling direction would take its total with it. An external cancellation is the
   same hazard one level up, and it cut the bridge before the merge -- so a connection that had
   moved bytes was recorded as having moved none, and a late `Host`/SNI that changed the
   decision vanished from the record.

8. **Teardown tolerance is decided against `/proc/<pid>/ns/net`.** Checking exit status would
   otherwise have quietly withdrawn `detach`'s documented tolerance for an already-dead
   container. The namespace rather than the pid: a zombie init keeps a `/proc/<pid>` entry, and
   the namespace is both what `nsenter` enters and what the container's `/etc` lives beside, so
   its absence is what actually means "there is nothing left to undo".

9. **The accepting half takes its carrier by reference.** `tcp_accept_loop` delegates to
   `accept_into`, which is handed the `JoinSet` rather than owning it. This exists for one
   reason and it is worth stating plainly, because it looks like indirection for its own sake:
   the claim that finished connections are taken back out as they complete is observable only
   in the set, and a test cannot see one the loop owns privately. Without the split, deleting
   the `reap_finished` call failed nothing -- a finished task is not an alive task, so no task
   count notices handles piling up, and a test that calls `reap_finished` itself pins the
   helper rather than the loop that has to keep calling it.

10. **A TCP accept failure does not take DNS interception down with it.** Connections run under
   a child of the attachment's token, so the accept loop can end the connections it holds --
   rather than parking on connections nothing is coming to end -- without reaching the DNS
   listener, which is a separate socket in a separate task and still working.

11. **`shutdown` became `Result<()>`, a public break taken inside the 0.2.0 window.** Reporting
    per-attachment failures and then discarding them at the session boundary would have been
    pointless. Failures are typed rather than rendered: `NetworkTeardownCause` names its
    container and boxes its source, and `NetworkTasksAborted`/`NetworkTaskPanicked` carry what
    would otherwise be prose. Exposing `tokio::task::JoinError` on the public surface follows
    the existing precedent of `tokio::process::Child` and `rmcp::service::ServiceError`.

12. **Every mechanism was broken deliberately to check the tests notice, and three of the
    checks failed the first time.** Fourteen breaks, each applied, run, and reverted: the
    late-identity write-through, the byte counters, the connection cancellation wrap,
    `stop_tasks`' abort-and-join, the DNS forward cancellation, the carrier reap, the
    accept-loop drain, the resolver presence probe, arm-before-mutate, `Drop` detaching its
    undos, bridges spawned loose, the runner's exit-status check, `printf '%s'` versus `echo`,
    and a failed undo cancelling the rest. All fourteen are caught now; three were not when
    this design was adopted.

    Two mechanisms had no test at all: the runner's exit-status check (see decision 4) and the
    accept loop's reap (see decision 9). The third, `stop_tasks`' abort, was pinned but failed
    by *hanging* rather than asserting -- its test parks on a `oneshot` the aborted task holds,
    and a detached task simply never wakes it, so the break read as a pass to any harness
    watching for a failure line. It is bounded now, which is a general lesson worth writing
    down: a breakage check that greps for failures cannot tell a hang from a pass, so a test of
    a termination property has to have a deadline of its own.

    Two further tests were rewritten during the alternative's own pass for the same reason: a
    counter break hit a field the audit test never asserted, and a drain break landed in a test
    where no connection was ever accepted.

13. **One acceptance criterion is met by a weaker construction than it asks for.** "A stalled
    bridge is terminated, not detached" names a connection blocked on an upstream connect. No
    portable, deterministic never-completing TCP connect exists: loopback resets, backlog
    saturation completes the connect under syncookies, and reserved addresses behave per host.
    The test instead parks a connection in the bidirectional copy where neither peer speaks nor
    closes -- an unbounded park with no timer -- and proves the task's frame was destroyed, by
    the client reading EOF the instant teardown returns. The connect is covered by the same
    single cancellation wrap that covers the copy, and breaking that wrap fails the test.

14. **Pre-existing flake seen once, not introduced here.**
    `process_tests::try_capture_logged_traces_spawn_and_exit_at_debug` failed once during this
    work in the shape `plan/next/lib-unit-test-flake.md` already records, and passed on ten
    consecutive reruns afterwards. This task adds unit tests that spawn `sh`, which will raise
    its rate; it remains filed rather than fixed here.

## Decisions from review

A code review rejected the first cut of this work with seven findings. Six are fixed below;
each was checked against real podman 4.9.3 rather than reasoned about, and three of the
checks contradicted what the code assumed.

15. **`podman exec` cannot be cancelled, so the resolver is no longer mutated with it.** The
    review's sharpest finding. `podman exec` starts the shell under conmon, so killing the
    client -- all a dropped future can do -- leaves the writer running. Measured: a `podman
    exec` whose client was SIGKILLed after 0.5s went on to complete its write two seconds
    later. A rollback racing that loses, and the container is left pointing at a DNS listener
    that was never installed -- the exact state this task exists to prevent, reached through
    the mechanism meant to prevent it. The read, install and restore now run through `nsenter
    -t <pid> -U -m`, which execs the shell directly: the same test under `nsenter` left the
    write undone. A byte-identical round trip through the real commands was confirmed against
    a live container.

    This is what carries `process::Owned`'s guarantee across the container boundary. 0002-39 made
    a dropped future kill its child; that only reaches the workload if the child *is* the
    workload.

16. **`nft -f` merges, so ownership has to be proved rather than assumed.** Decision 3 argued
    that arming the delete unconditionally was safe because the undo is idempotent. It is not:
    a plain `table` block applied onto an existing table merges into it -- measured, a second
    apply took the chain count from one to two, rc=0 -- so a stale table from a crashed run of
    the same session, or an operator's own, was merged into on the way in and deleted whole on
    the way out, taking rules this never made. The ruleset now uses `create table`, which fails
    on an existing table, and the name is probed with `nft list table` before the delete is
    armed. Both were confirmed live: `create` onto an existing table errors with `File exists`,
    and the probe exits 0 when the name is taken and 1 when it is free.

17. **Resolver shapes that cannot be put back are refused before anything is changed.** A
    dangling symbolic link reads as absent under `[ -e ]`, so installing would follow it and
    create its target while the undo removed the link. A snapshot containing a NUL, or larger
    than 64 KiB, cannot ride in the argument the restore carries it in -- `execve` refuses the
    first outright, which a test asserts at the process boundary rather than taking on faith.
    Each is an error from `attach` before the resolver is touched. The four probe verdicts were
    checked against the real states.

18. **Found while verifying: the container's resolver is a bind mount.** `rm` on it fails with
    `EBUSY`, so the absent-restore is unreachable for a container podman created -- the snapshot
    always says present. The branch stays, because a container without the mount is a real
    shape, and its failure is now reported rather than swallowed. Written down because the code
    reads as though `rm -f` always works.

19. **An attach that could not be undone says so.** A failed undo is no longer popped: it stays
    armed for the destructor to reissue, and `attach` returns `NetworkAttachNotUndone` carrying
    both the cause and the residue. A fully-undone failure still returns the plain cause, and
    that distinction is the point -- one is retryable, the other means the container may still
    be carrying interception that no attachment owns.

20. **A record the sink could not write is a teardown that did not complete.** `detach` treats
    a connection's task returning as proof its record landed, which held only if a failed write
    was kept rather than logged. The sink retains them, stamped with their container, and
    teardown collects them after joining -- joining being what makes "everything it was going
    to write has been attempted" true. Tested against `/dev/full`.

21. **Still open: the two commands a destructor hands over are unordered.** `Drop` submits the
    nft delete and the resolver restore as separate `detach_cleanup` calls, each spawning and
    returning, so the restore can land first and leave the container resolving through a
    redirect to a listener that is gone. Ordering needs `supervise` to gain a chained cleanup;
    its `Wait` holds one command with per-attempt retry and wedge deadlines, so this is surgery
    on a module outside this task. Worth recording honestly: ordering bounds the transient
    window but does not fix the case the finding calls indefinite. If the delete fails the
    redirect outlives the restore whichever ran first, and the delete is `Reissue::Once`
    because retrying a pid-selected command could enter a replacement namespace.

## Decisions from the second review

Four more findings, two of them regressions introduced by the previous round's fixes.

22. **An undo leaves the list only once its command has returned.** The previous round changed
    `undo_now` to pop before awaiting, so a caller cancelled mid-command took the command with
    it into the dropped frame and the destructor had nothing to reissue -- while the doc comment
    directly above claimed the opposite. It also kept failures newest-first for `Drop` to reverse
    a second time, undoing the resolver before the redirect aimed at it. Both are gone: the list
    is walked by index and an entry is removed only on success, so everything not yet discharged
    is in rollback-owned state across every await, in arming order.

23. **The table name is an identity, not a check.** Decision 16's preflight probe closed the
    stale-table case but not the race: another actor in the same namespace can create the name
    between the probe and the apply, and the undo would then delete their table. A check cannot
    close a window it sits outside of, so the name carries a per-attach random tail instead --
    a table by that name is one this attach created, and the question stops being asked.
    `create table` stays as the backstop, and the probe is gone.

24. **Audit failures are retained per container, not per record.** The previous round's fix kept
    every failed write in a vector drained only at detach. A container can open connections as
    fast as it likes and every one of them writes, so a sustained failure -- `ENOSPC` is the
    obvious one -- grew host memory and the eventual teardown error without bound. One
    representative error and a saturating count per container now, which is bounded by the
    number of attachments.

25. **`supervise` gained an ordered chain, which the previous two rounds deferred.** Handing the
    reaper one command at a time hands it no ordering, and the resolver must not go back while
    the redirect aimed at it is still there. `detach_cleanup_chain` carries the remaining
    commands on the `Wait` and starts each only once the one before it has ended, reusing the
    existing spawn and retry path. Sequenced rather than conditional: the later commands are
    owed whatever became of the earlier, so a failure advances the chain rather than abandoning
    it. `detach_cleanup` is now a one-command chain.

26. **Four tests were deleted by a careless edit and the suite total hid it.** Replacing an
    obsolete test with a splice between two anchors also removed everything between them --
    including both tests written that round for the highest-severity finding -- and because
    other tests were being added at the same time, the count went up rather than down. It
    surfaced only when a deliberate breakage reported that nothing caught it. Two lessons worth
    keeping: a breakage check that greps for failures cannot tell a hang, a compile error, or a
    deleted test from a pass, and an edit that removes a range has to state what it expects to
    be in that range.

## Decisions from the third review

Four findings, all on exceptional paths the earlier rounds had not reached.

27. **A chain the reaper will not take still runs.** `detach_cleanup_chain`'s two fallbacks --
    reaper unavailable, or its channel gone -- returned without the queued commands, so for the
    delete-then-restore pair the restore was simply dropped and a live container was left
    pointing at a stopped listener. They are launched now, unreaped and unordered, which is the
    lesser loss; the ordering cannot be kept on a path whose defining feature is that nothing is
    left to sequence it. The fallback moved into `hand_back` so a test can reach it: it is only
    entered when a process is out of threads, which nothing exercises by accident.

28. **An undo checks what it is about to overwrite.** The chain introduced a delay between two
    commands that both name a namespace by pid, and pids are reused -- so an undo running after
    its container exited could write one container's resolver into a replacement's mount
    namespace. The resolver undos now fire only if the file still holds what this attach
    installed. The comparison puts both sides through command substitution, because `$(cat ...)`
    strips a trailing newline and a literal does not; the first cut got that wrong and silently
    never matched, which a live check caught and a unit test would not have. The nft delete needs
    no guard: decision 23's name is unique to the attach, so it finds nothing in a stranger's
    namespace.

29. **An audit record is written whole or not at all.** Teardown aborts a connection that
    outstays its grace, and an abort lands at an await point, so a record written through
    `AsyncWriteExt::write_all` could be cut between chunks -- a partial line makes every record
    after it unparseable, which costs the whole file rather than one record. The bytes go out
    through `spawn_blocking`, which runs its closure to completion whether or not the handle is
    awaited, and which records its own failure so that survives a caller that is gone. Trying to
    write the cancellable version as a deliberate break does not compile, because holding the
    guard across an await is not `Send` -- weak evidence, but evidence.

30. **A sidecar that will not start reports what unwinding it could not do.** Both compensations
    -- detach, then stop -- had their results discarded, so a failed detach left a live sidecar
    carrying interception no attachment owned and the caller saw only the startup error, with no
    retry path. The library aggregates them into `SidecarNotUnwound`; the CLI, whose error type
    is its own, logs each with the sidecar named.

31. **Two more tests that did not pin what they claimed.** The orphaned-chain fallback had no
    test at all -- removing the call was invisible -- and the abort-atomicity test permitted an
    empty log, so a torn record passed whenever the abort landed before the write. Both are
    fixed: the fallback is reachable and tested, and the abort test now writes a second record
    afterwards so there is always a line a torn prefix would corrupt. That is the third round in
    a row where a deliberate breakage found a test asserting less than its name claimed, which
    is the argument for running them rather than trusting the names.

## Decisions from the fourth review

Two of the five findings were again defects introduced by the previous round's fixes, which is
what settled the audit question: the write path had been patched twice and generated a finding
each time, so this round replaced it rather than patching it a third time.

32. **The audit log has one writer, and teardown drains it.** `spawn_blocking` fixed the torn
    record by making the write uninterruptible, and bought a worse property doing it: a blocking
    closure cannot be aborted or joined, so a stalled write outlived the `detach` that reported
    success, and enough of them would exhaust the runtime's blocking pool. A single task owns
    the file now; producers queue over a bounded channel and wait for an acknowledgement, so the
    guarantee a caller had -- its task returning means its record is on disk -- survives, while
    the bytes themselves are out of reach of its cancellation. Teardown sends a drain marker and
    reports if it does not come back inside the grace, rather than inferring completion from the
    connections having joined.

33. **A failed write is rolled back to where the file ended.** `write_all` is a retry loop, not
    an atomic commit: a filesystem can take a prefix and then fail, and the leftover makes every
    record appended after it unparseable -- the file, not the record. The writer records the
    length before each append and truncates back on failure.

34. **The resolver sentinel is per attachment, and compared exactly.** The guard from decision 28
    compared against the interceptor's resolver text, which is identical for every attachment
    outrig has -- so a pid reused by *another* outrig container satisfied it and would have had
    the first container's resolver written into it. The sentinel now carries the attach's own
    table name, in a `#` comment resolvers ignore. And `[ "$(cat ...)" = ... ]` normalizes
    trailing newlines on both sides, so content that merely resembled the sentinel passed; the
    comparison is `cmp` against the exact bytes. Both were checked against a live container,
    along with the resolver still working with the comment line present.

35. **The fallback sequences with a shell.** Decision 27 launched an orphaned chain's remainder
    side by side and called it the lesser loss. It is not: for delete-then-restore, launching
    both concurrently is precisely the outage the ordering exists to prevent. The remainder now
    goes out as one `sh` that runs them in order -- a sequencer needing neither runtime nor
    reaper, which is what that path lacks. Every argv element travels as a positional parameter
    and is referenced by index, so nothing is interpolated into the script; one of those
    elements is a container's resolver, and no quoting rule gets to see it.

36. **A CLI sidecar failure distinguishes a clean unwind from residue.** "session unaffected" is
    a claim about the machine, not about the session's bookkeeping, and it was printed even when
    detach and stop had both failed and left a live sidecar still intercepted. The fallible tail
    returns which of the two it was, and only the clean case says so.

37. **One thing deliberately left untested, and named rather than faked.** The partial-write
    rollback has no test. Inducing a genuine short write needs process-global `RLIMIT_FSIZE`,
    and this suite runs its tests concurrently in one process, so that would trade a defensive
    branch's coverage for a flake in everything else. The first attempt at the test simulated
    the rollback by performing it by hand, which asserted nothing about the code; it was deleted
    rather than kept for the look of it. A dedicated single-test binary would be the way to
    cover it if it is worth the machinery.

## Decisions from the fifth review

Four of the five findings were defects in the previous round's fixes.

38. **The undo needs nothing a container might not have.** Decision 34 reached for `cmp` to get
    an exact comparison. A minimal image need not ship it, and a missing one exits 127 --
    which `|| exit 0` reads as "not this attach's", so the undo skipped silently while `detach`
    reported success over a resolver still pointing at a stopped listener. Checked live: alpine
    happens to have it as a busybox applet, and with it off `PATH` the skip is exactly that
    quiet. The check is now `case` over the file's text, looking for the marker -- shell
    builtins and `cat`, which the snapshot already needs.

    Containment of a unique marker also turned out to be the better question than equality of
    the whole file. Ownership is what the guard is asking about, and the marker carries the
    attach's table name; a resolver something appended to is still this attach's to put back.
    The "merely resembles" test from decision 34 was deleted rather than kept, because with a
    unique marker it was asserting a distinction that no longer means anything.

39. **The drain has one deadline, over both halves.** It timed the acknowledgement but not
    getting the marker *into* the queue -- and the queue is bounded, so a stalled writer with
    every slot full never accepts it. `detach` would have parked there forever and never
    reached the resolver and nft undos behind it.

40. **A rollback that cannot be verified stops the writer.** If the length before an append was
    never read, or the truncation itself failed, a prefix may be sitting in the file and the
    next record would be appended to it. The writer refuses further records from that point --
    but keeps receiving, answering and counting them, because a caller waiting on a record is
    owed an answer and a refused record is still a lost one. The first cut simply broke out of
    the loop, which dropped later records silently and lost the bounded reporting decision 24
    put in; the `/dev/full` test caught that.

41. **The fallback tail waits for the head it was handed.** Decision 35 fixed the ordering
    *within* the tail and left it racing the command already running. One thread waits the head
    out and then runs the rest in order -- a thread rather than a task because this is reached
    from destructors, where there may be no runtime, and only on a path that already means the
    process is out of threads.

42. **Every post-create sidecar failure reports what cleanup could not do.** Decision 36 covered
    the post-connect path; bootstrap failure and interceptor-attach failure still discarded
    their `container.stop` result and reported a clean unwind.

43. **Two tests that hung instead of failing, and one that was lost to a killed shell.** The
    drain test first used a FIFO and a reader thread to stall the writer; it was slow, and when
    it failed the thread kept the binary alive, so the failure looked like a hang. It is a
    plain full channel now, with the drain bounded so the break fails as an assertion. Separately,
    a `pkill -f` pattern matched the shell that was running it, killing several commands
    mid-edit -- one of which silently lost a test replacement, which then looked like the fix
    not working. Both are the same lesson as decision 31, learned again: what a check reports
    has to be distinguishable from what a broken harness reports.

## Decisions from the sixth review

44. **A resolver that cannot be read is not one that belongs to someone else.** Decision 38's
    guard discarded `cat`'s status, so an unreadable resolver looked like a marker mismatch: the
    undo exited zero, teardown struck it off, and `detach` reported success over a container
    still pointing at a stopped listener. The read's status is checked now, and absence -- the
    one case that legitimately owes nothing -- is tested for separately so it cannot be confused
    with a file that is there and could not be read.

45. **The fallback asks about the reaper before starting anything.** Decision 41 needed a thread
    to sequence a tail behind a head already running, on a path reached when the process is out
    of threads -- so the thread could fail too, and the restore was then abandoned. When there is
    no reaper and there is more than one command, the whole chain goes out as one shell from the
    start and no thread is wanted. The residual case -- the reaper existed and its channel has
    since gone, meaning its thread died -- still wants one, and if it cannot be had the tail runs
    unordered rather than not at all.

46. **Why recovery failed is kept, not collapsed to a boolean.** The writer poisons itself when
    it cannot prove a partial record was removed, and the reason is now recorded against that
    record rather than discarded -- so a teardown that follows immediately still learns the file
    may be corrupt and what stopped it being repaired. It replaces that record's write error
    instead of counting a second loss, since it is the more useful thing to be told about the
    same record.

47. **Audit losses are filed under an attachment, not a container name.** A drain that gave up
    leaves the writer still holding a record, and the failure that follows would have landed in
    the slot the *next* attachment of that name was reading. Each attach takes a generation that
    nothing reuses. The records themselves still carry the container's name; this is only about
    whose loss it is.

48. **Every post-create sidecar failure reports what stopping could not do, in both crates.**
    Decision 42 covered the CLI; `Outrig::add_sidecar`'s bootstrap and attach branches still
    discarded theirs. Both now go through one `unwound` helper. And the CLI's bootstrap residue,
    which decision 42 flattened into a message, is typed again -- flattened it mapped through the
    blanket conversion to a clean unwind and printed "session unaffected" over a container that
    was still running.

49. **One branch left untested because it cannot be reached in-process.** The reaper-availability
    check in decision 45 only takes its short path when `reaper()` is `None`, and the reaper is a
    process-global `OnceLock` that any earlier cleanup initializes. Breaking the check therefore
    fails nothing. It is a defensive reordering rather than a behavior with an observable, and it
    is recorded here rather than covered by a test that would only be asserting it compiles.

## Decisions from the seventh review

50. **The undo compares the whole installed text, and this reverses decision 38.** Round five
    asked for an exact comparison; decision 38 moved to marker containment to shed a `cmp`
    dependency, and containment turns out to be too permissive -- a resolver manager that changed
    the nameservers and left the comment alone would have had its work reverted. Both constraints
    hold together with neither `cmp` nor containment: compare the whole text with `[ = ]`, both
    sides through command substitution so they lose trailing newlines alike. The marker still
    makes the text unique to this attachment; comparing all of it is what keeps a later change.
    The containment test from decision 38 asserted the behavior this replaces and was deleted.

51. **A drain that gave up does not close the books.** It left the writer still holding records
    and then took the attachment's slot anyway, so a failure arriving afterwards landed where
    nothing would ever read it. The generation now stays registered, and `shutdown` -- the only
    thing that outlives every attachment -- sweeps whatever the writer ended up recording.

52. **The integrity cause sits beside the write failure, not on top of it.** Decision 46 replaced
    the retained error, which meant a reader could learn the file might be corrupt or learn what
    broke the append, never both. `NetworkAuditUnwritten` carries an optional second cause.

53. **The fallback waits the head out in place rather than racing it.** Decision 45's last resort
    -- no reaper thread *and* no thread to be had -- ran the tail immediately, which is the
    concurrency the ordering exists to prevent. It polls the head to a bounded deadline instead.
    A destructor blocking is bad; on a path that needs the reaper dead and threads exhausted, it
    is the least bad of three.

54. **A sidecar that will not stop stays in the session.** The `on-failure = warn` attach path
    dropped the container from the session's map and discarded the stop failure, so a live
    unmanaged sidecar was left with nothing holding a handle to it. It is removed only once it
    has actually stopped; otherwise it moves to cleanup-only ownership and teardown gets another
    go. `Container::stop_or_keep` (decision 68) is what hands it back.

## Decisions from the eighth review

55. **A sidecar the session has warned it is skipping does not serve MCP.** Decision 54 kept a
    failed-stop container in the session's `sidecars` map so teardown could try again, and that
    map is what `container_for` answers from -- so `connect_mcp_clients` would have started an
    MCP server in a container that had just failed to attach to the interceptor. Under
    `mode = "filter"` that is MCP served with no policy on it, which is worse than either the
    leak or the warning. Such containers move to a cleanup-only `abandoned` list: kept, because
    one that would not stop is still running, and out of the way, because nothing should treat
    it as somewhere to put work. The MCP warn path had the mirror-image bug -- dropping the
    container *before* stopping it -- and now stops in place and moves the same way.

56. **The writer is ended before the sweep that reports it.** `shutdown` swept immediately after
    attachment drains that may have timed out with records still in the writer's hands, so a
    failure landing afterwards went into a map whose only collector had already run. The sink's
    sender is taken and the writer joined first; a writer that will not finish is reported, and
    the sweep still runs, because that is a reason to say so rather than to collect nothing.

57. **The undo's comparison keeps trailing newlines.** Command substitution strips them from
    both sides, so a file that gained or lost a terminal newline compared equal to one that had
    not -- byte-distinct state the undo would then overwrite. A `.` inside each substitution
    fixes it. Getting there took a second try: `$(cat f; printf .)` takes `printf`'s exit
    status, which silently undid decision 44's read check; `$(cat f && printf .)` keeps `cat`'s.

58. **A dependent chain does not advance on a head that merely stopped being waited for.**
    Decision 53 polled the head to a deadline and then started the tail regardless, so a wedged
    nft delete could still overlap the resolver restore. The tail runs only if the head was
    positively reaped; otherwise the rest is abandoned and said so, because discharging it out
    of order is worse than not discharging it.

59. **A second branch that cannot be reached in-process.** The in-place wait in decision 58 is
    entered only when the reaper thread has died *and* no thread can be created, so breaking it
    fails nothing -- the same shape as decision 49. Recorded rather than covered by a test that
    would assert only that it compiles.

## Decisions from the ninth review

60. **The resolver marker is its own nonce, not the nft table's name.** The resolver is written
    before the table is created, so whatever is in it is readable inside the container first --
    and a marker that *was* the table name would hand an actor in that namespace the one thing
    it needs to create the table ahead of outrig, making `create table` fail and the rollback
    delete a table this attach never made. Two independent nonces mean publishing one says
    nothing about the other. `Target::for_attach` draws both, which is also what makes the
    property testable: the constructor is where it lives, so breaking it fails a test.

61. **Closing the audit writer aborts and joins instead of dropping it.** `close` moved the
    handle into a `tokio::time::timeout`, and a timed-out handle that is dropped *detaches* its
    task -- which would go on appending records and recording losses after the sweep meant to be
    the last word on both. That is the defect this whole task began with, reintroduced one level
    down in a function written to fix a different half of it.

62. **A bootstrap failure hands its container back.** `start_one_sidecar` consumed the container
    in `stop`, so under `on-failure = warn` a sidecar that would not stop was in neither the
    session's active map nor its cleanup-only list -- nothing but `Drop`'s detached best effort.
    It stops in place and pushes the handle into the caller's abandoned list instead, which the
    concurrent start path collects per task since nothing may hold a mutable borrow across them.
    Decision 68 later made that the shape of the API rather than a rule each site follows.

63. **A wedged cleanup head is killed rather than having its tail dropped.** Decision 58 chose
    abandoning the tail over running it out of order. Both are bad: the resolver undo is lost
    for good, after the listeners it points at have stopped. `WEDGED_CLEANUP` is this module's
    definition of wedged and what it does to a wedged command everywhere else is kill it, so the
    head is killed, reaped, and the rest runs in order. What is lost is a command that was never
    going to finish.

64. **The undo checks the file's byte count as well as its text.** A shell variable is not a
    byte string: implementations differ on what command substitution does with an embedded NUL,
    so a resolver with one added after installation could compare equal to one without. `wc -c`
    reads the file rather than a variable. That is one more utility assumed present -- weighed
    against `cmp` in decision 38 and taken, because `wc` is in busybox and coreutils alike and
    the alternative is a guard that cannot see a whole class of change.

65. **`wc` is optional, and its absence does not retire the undo.** Decision 64 added a byte-count
    check and with it a second assumed utility. An undo that skipped itself because `wc` was
    missing would leave the interceptor's resolver installed in a container that has no listener
    left -- worse than the NUL-smuggling case the check exists for. So the count is guarded by
    `command -v wc`: present, it is checked; absent, the text comparison stands alone.
    `neither_undo_needs_anything_beyond_a_shell_and_cat` runs both undos with a `PATH` holding
    only `cat` and `rm`, so an undeclared dependency fails as an assertion.

66. **An aborted writer records the log's doubt where the sweep will find it.** Decision 61's
    abort ends the task, not the syscall: `tokio::fs` runs writes on a blocking pool, one already
    submitted completes regardless, and the rollback that would have undone a partial record dies
    with the task. Saying so only in the error `close` returns loses it -- `shutdown` logs that
    error and *reports* the sweep. So `close` writes an integrity marker into the loss map before
    returning, and the sweep carries it. The loss has no container to name, because the writer
    serves every attachment, so `NetworkAuditUnwritten` renders an empty name as "for this
    session" rather than `container ""`.

67. **A chain advances only on a head proved gone -- collected, not merely signalled.** Decision
    63 kills a wedged head so the tail can run. A kill that was refused, or a head that cannot be
    reaped, leaves it possibly still running, and a tail started then is the overlap the chain
    exists to prevent. `proved_gone` returns true only from a `try_wait` that collected the head;
    anything else abandons the rest and says so. It is a standalone function taking its patience
    as an argument, which is what makes it testable: the branch that calls it needs a dead reaper
    *and* a failed thread spawn, while `proved_gone` itself takes a real child and a 100 ms
    budget. What it proves is local -- a `podman exec` client's container-side process outlives
    its killed client, and no kill here can establish otherwise.

68. **A stop that fails hands the container back, by signature.** Decisions 62 and the sidecar
    unwind paths each had to remember to keep a handle whose stop failed, in five places across
    two crates, and `/sidecar add` did not. `Container::stop_or_keep` returns
    `Option<(OutrigError, Self)>`: `None` means gone, `Some` carries the failure *and* the
    handle. Keeping it is no longer something a call site can forget, and the two sites that
    stopped a container inside a map can now remove it first -- the handle comes back if the stop
    fails. It replaces the `stop_in_place` this task briefly added, so the public surface gains
    one method rather than two.

69. **Two test-only container handles, because every stop ends at podman.** The retention rule
    needs a container whose stop fails and one whose stop cannot. `Container::unstoppable` is
    owned with an empty name, which podman rejects (exit 125, "name or ID cannot be empty") and
    which fails to spawn at all where podman is absent -- so the test does not depend on the
    machine. It carries an attempt token like any other owned container, so `Drop` removes by a
    label nothing wears rather than by the empty name. `stops_cleanly` is the borrowed
    counterpart, whose stop is a no-op. Both are `#[cfg(test)]`, which is why the CLI's
    `/sidecar add` paths are covered by the shape of decision 68 rather than by their own tests:
    reaching them needs a started container, and `outrig-cli` cannot see another crate's test
    constructors.

## Decisions from the twelfth review

70. **The unsequenced fallback carries the whole obligation, not a wait.** The thread that takes
    over when the reaper is gone waited the head out with an unbounded `child.wait()`, discarded
    the result, and launched the tail regardless. Every part of the `Wait` policy was lost on that
    path: a wedged head held the thread for as long as it held out, a `wait` that failed started
    the resolver restore with nothing proving the delete had ended, and a replayable head that
    exited non-zero was never retried. The thread now drives the same `still_owed` the reaper
    polls, so the fallback differs from the reaper in nothing but who is turning the crank. The
    in-place path -- no reaper *and* no thread -- keeps `proved_gone` and a bounded block, because
    it may be a destructor and cannot sit out a retry budget. `Wait` grew a `patience` field so a
    test can watch a wedge deadline pass instead of sleeping out a minute.

71. **A head that cannot be collected does not advance its chain.** `try_wait` failing used to
    advance, on the grounds that holding an uncollectable child achieves nothing. True, but the
    conclusion does not follow: nothing will ever prove that head ended, and the tail is the
    resolver restore, which beside a live redirect delete is the overlap the chain exists to
    prevent. The obligation ends there and the rest is abandoned and logged -- the same rule
    `proved_gone` already applied one path over, now applied on both.

72. **What a stopped writer lost is what it was holding, by owner.** `close` recorded one
    synthetic loss under the session sink's own `(0, "")` key with a count of one, whatever it had
    actually taken: an anonymous entry that named no attachment, claimed one record however many
    were queued, and left every real owner unreported. A record is counted into `pending` when it
    is offered and released by whoever takes it off the sink's hands -- the writer once it has
    answered, or the send itself if the queue would not take it -- so what is left when the writer
    stops is exactly what it accepted and never accounted for. `close` converts those entries,
    each under its own attachment and with its own count, and a writer that ends abnormally is
    accounted for the same way as one that had to be aborted. A writer stopped holding nothing
    now records nothing, where before it invented a loss.

73. **The redirect table is removed by handle, and only while its name is still there.** The undo
    kept the table's name and deleted by it unconditionally, so an actor with `NET_ADMIN` in the
    namespace could delete outrig's table, create its own under the observed name, and have
    teardown delete that. The undo is armed by name before the apply, because no handle exists
    until the kernel has made the table, and narrowed to `nft list table inet <name> ; delete
    table inet handle <n>` as soon as the apply's `--echo --handle` reports it -- one nft
    invocation, so one kernel transaction, which makes it a check rather than a race. Each half
    covers what the other cannot: the handle is never reissued, so a recreated table does not
    answer to it, and the nonce name is what stops the undo deleting by handle in a namespace it
    reached through a reused pid, where handle numbering starts over. Measured against nft 1.0.9:
    a stale handle exits 1 and leaves the replacement, a missing name exits 1 and leaves a
    stranger's table whatever its handles. `--echo` dates from nft 0.9.0, no newer than the
    `create table` this already required. The residual window is the instant between the apply
    committing and the narrowing, reachable only by a cancellation landing exactly there; what it
    falls back to is the name-based delete, which is where this started.

    The delete-and-recreate itself is an e2e test rather than a unit one, because what is being
    asserted is the kernel's behavior and not outrig's command string: the unit test pins the
    shape of the undo, and `detach_leaves_a_table_that_replaced_the_one_it_created` attaches to a
    real container, swaps the table out through `nsenter`, and checks that detach reports a
    teardown it could not finish and leaves the replacement standing. It needs no traffic, which
    is why it passes on a machine where the record-carrying e2e tests cannot reach a host
    fixture.

## Decisions from the thirteenth review

74. **A record is counted only once nothing can stop it being queued.** The pending count was
    taken before the `send` that offers the record, and the queue is bounded, so a producer
    cancelled while waiting for room left its owner charged for a record the writer never saw.
    Either outcome was wrong: reported as a writer-held loss if the writer was later stopped, and
    dropped in silence if it closed cleanly, which is a connection record missing from the log
    with nothing saying so. `reserve` is cancellation-safe -- tokio guarantees nothing was sent if
    the future is dropped -- and the permit it returns sends synchronously and infallibly, so the
    count now sits between the reservation and the send with no await between them. The loss of a
    record cancelled that early is still reported, by the aborted-task error that cancelling a
    connection produces; what changed is that it is no longer *also* miscounted here.
    `leave_pending` has one caller left, the writer.

75. **An apply that cannot be identified fails the attach.** An echo with no handle in it, one
    that is not UTF-8, one for a different table, an nft too old for `--echo` -- each left removal
    by name alone armed for a teardown a whole session away, which is exactly what a table
    replaced under that name answers to. It is an error now, and the name-based undo that is
    still armed removes what the apply made *immediately* rather than later, which is the one
    moment at which removing by name is still safe.

76. **The cancellation window between commit and narrowing stays open, deliberately.** The handle
    exists only in the output of the transaction that assigned it, so a cancellation that never sees
    that output has nothing narrower to arm. Closing it would mean one of two worse things: arming
    nothing, which leaves a live container redirecting to a listener that has stopped with nothing
    coming to remove it, or resolving the identity at removal time, which is the check-then-act this
    was built to avoid. Spawning the apply so a dropped future still records the handle was rejected
    for the reason decision 2 gives: a task does not survive the runtime an embedder is tearing
    down, so it would not hold on the path that needs it. What is exposed is an actor holding
    `NET_ADMIN` in the container's own network namespace -- not granted by default -- who can also
    delete and recreate the table within the interval between nft's commit and the undo being
    issued. `a_cancelled_apply_leaves_the_name_alone_armed_and_nothing_narrower` pins the behavior
    so it stays a decision rather than becoming an accident.

## Decisions from the fourteenth review

77. **The undo carries one selector, and the namespace is checked in Rust.** `nft list table inet
    <name> ; delete table inet handle <h>` looked like a conjunction and was not: the two are
    separate commands in one transaction, and the list succeeding says the name resolves
    *somewhere* in that namespace, not that it resolves to the table the handle names. In a
    namespace that was not this attach's, each could pick a different table and the delete would
    take the wrong one. The name is gone. What is left is exact where it is used: nftables draws
    table handles from a per-namespace counter and never reissues one -- measured against nft
    1.0.9, create/delete/create gives 1, 2, 3, and a `flush ruleset` in between does not send it
    back to 1 -- so within a namespace a handle names the table that transaction created or it
    names nothing.

78. **"Within a namespace" became a check instead of an assumption.** `container_alive` asked
    whether `/proc/<pid>/ns/net` existed, which a pid handed on to another container answers yes
    to. `Rollback` now records the namespace instance -- the nsfs device and inode -- before it
    arms anything, and every undo, awaited or from the destructor, is issued only while the pid
    still leads to that same instance. This is the guard the resolver markers were covering for
    from the far side, and it covers the nft delete, which has no content to mark. A namespace
    that cannot be identified fails `Rollback::new`, which runs before the first mutation, so the
    refusal costs nothing: there is nothing to undo yet.

    It is not absolute, and the doc comment says so: nsfs inode numbers return to a pool when a
    namespace is destroyed, so a pid *and* an inode both coming round to one new container would
    defeat it. The handle is the other term -- that container's own tables are numbered from its
    own counter -- and the check is made immediately before the undo is issued rather than at
    some earlier point.

## Decisions from the fifteenth review

79. **The nft undo checks the namespace from inside it.** Decision 78's check is made before the
    command is handed over, and `nsenter -t <pid>` resolves that pid again when it runs -- so a
    pid checked and then handed to another container in between would carry the undo into a
    namespace where the handle names a stranger's table. The undo now enters first and asks
    `readlink /proc/self/ns/net` about the namespace it is *in*, exiting zero without acting if
    it is not the one recorded. Nothing can move a process between namespaces from outside, so
    the check and the action are bound by being in the same process, which no ordering of two
    commands could achieve. Only the user and network namespaces are entered, not the mount
    namespace, so the shell, `readlink` and `nft` are the host's and this asks nothing of the
    container's image -- which is why the resolver undos, which do enter the mount namespace,
    keep their content markers instead. The Rust-side check stays as the cheap first answer and
    as the guard for the resolver undos.

    The first version of that script read its arguments one slot along from where they were
    passed, so the comparison could never match and the undo retired itself every time -- a guard
    that always says "not our namespace" removes nothing, ever. A unit test asserting on the
    rendered command could not see it; the e2e, which runs the thing, failed immediately. The
    test that now covers it runs the script with `nft` replaced by `echo`, which needs no
    namespace and no root, and checks both branches -- the same shape the resolver-undo tests
    already used, and the reason those never had this bug.

80. **A table that could not be identified is removed once, here, or not at all.** Decision 75
    left the name-based undo armed when the handle could not be read, on the grounds that the
    removal runs immediately. It does -- unless that removal fails or is cancelled, and then it
    escapes into `Drop`, which issues it at some later moment against whatever holds the name by
    then. It is taken off the rollback *before* it is run. A failure is reported as part of the
    attach error rather than retried, and a cancellation takes it into the dropped frame with
    nothing left behind. What is given up is the leaked table in that case, which is the
    fail-closed direction: a table nobody removes is worse than nothing only for this container,
    where a delete aimed at a name is worse for whoever else holds it.

81. **A removal that ran and failed is not a removal.** `stop` read every outcome but a timeout
    as success, so podman refusing the removal -- a storage error, a container the engine will
    not let go of -- left the handle disposed and untracked with nothing retrying and nothing for
    `Drop` to do. The outcomes are now a named three: removed, timed out (handed to the detached
    retry, which is what licenses disposing), and failed, which is reported with the handle
    intact so `stop_or_keep` hands it back. The filter form exits zero when it matches nothing,
    measured against podman 4.9.3, so the ordinary "`--rm` already did it" case is still a
    success rather than an error.

82. **A stop that worked settles what a failed detach left open.** The unwind paths reported
    residue whenever detach had failed, even when the stop that followed succeeded -- telling a
    caller a container "may still be running and still intercepted" when it no longer exists.
    Stopping it takes its namespaces, and with them the interception the detach could not undo.
    So a successful stop supersedes the detach failure for what the caller is *told*, and the
    detach failure is logged rather than discarded. Residue means something is still running,
    and it now only says so when something is.

## Decisions from the sixteenth review

83. **A handle the echo did not give is asked of the namespace.** Decisions 75 and 80 were caught
    between two bad answers for an apply whose echo could not be parsed: run the delete-by-name
    once and lose the obligation if that is cancelled, or keep it armed and aim a delete-by-name
    at whatever holds the name when it finally runs. Both follow from having only a *name*, and
    that premise was wrong -- `nft --handle list table inet <name>` reports the handle too
    (measured against nft 1.0.9: `table inet outrig_h1 { # handle 1`). So the echo stays the rule
    and a listing is the fallback, and what the fallback produces is a handle, which is exact
    wherever it is later run and therefore safe to keep armed. One parser reads both forms.

    The fallback is a second look and a ruleset can move between the commit and it, which is
    exactly the atomicity the echo has and this does not. It is taken immediately, only on a path
    where the alternative is no identity at all, and it is logged. With no handle from either
    source the attach fails and the delete-by-name stays armed for the caller's rollback to run
    now, while the name is still this attach's: losing it would leave a container redirecting to
    listeners that are about to stop, with nothing coming to undo it.

84. **`podman stop` is bounded, because `-t` does not bound it.** `-t` is how long podman waits
    for the *container's* processes before killing them; it says nothing about the client asking
    for that, or about an engine that never answers. The call was unbounded, and it sits on the
    path of every sidecar compensation and every shutdown, so a wedged client was a wedged
    session. It gets the grace the container is owed plus the client's own floor, and a timeout
    hands the container back rather than reporting a stop.

85. **A removal that did not answer is not a removal.** The timeout used to detach a retry and
    then report a completed stop -- "gone, and the handle with it" about a container whose state
    was unknown, and if those retries also failed there was nothing observable left and nothing
    orderly coming. It is reported instead, with the handle intact: the caller decides whether to
    try again or to abandon it to teardown, and `Drop` still has the detached removal for a
    handle that is simply let go. Both bounded engine calls now read their outcome through one
    classifier, which is where the "everything but a timeout is success" bug lived.

86. **The identity guarantee is qualified rather than made absolute.** A pid, the nsfs inode
    behind it, and a table handle are each recyclable, so three recycled values together would
    satisfy every check the undo makes. The stronger identity is an open descriptor on the
    namespace, which cannot be recycled while held -- and it is not used because these undos have
    to outlive what armed them: issued from a destructor with no runtime, through commands
    `supervise` spawns and may re-spawn, where an inherited descriptor does not survive the
    `exec`. Carrying one would mean outrig's own launcher in place of `nsenter` on the one path
    that has to work while everything else is being torn down. What is claimed is "unlikely", and
    the doc comment says so in those words.

87. **Two tests that passed for the wrong reason, and what they cost.** The first pair written
    for decisions 84 and 85 asserted only that `stop_or_keep` came back with a `Canceled` -- which
    the *other* bounded call produces just as readily, so each test was satisfied by the one it
    was not about. A paused clock made it worse rather than better: a bounded call waiting on a
    real child leaves the runtime idle, so every deadline in the test fires at once and which
    call "timed out" is not something the test chose. Both breaks passed the sweep, which is the
    only reason this was noticed. They run on real time now against an injectable budget, with a
    stand-in that outlives the budget by ten seconds and no more, and they assert *which* argv
    was given up on. The general rule, again: a test of a termination property has to say what
    terminated, and a breakage sweep is the thing that asks whether it does.

## Decisions from the seventeenth review

88. **Ownership of the table comes from the transaction that made it, or not at all.** Decision
    83's fallback -- asking the namespace for a handle the echo did not give -- could adopt a
    *replacement's* handle as this attach's own, since a ruleset can move between the commit and
    the second look, and then delete that table at teardown. The fallback is gone. With no handle
    from the echo there is nothing exact to arm, the attach is refused, and the delete-by-name
    that was armed before the apply is taken off the rollback and run once, here, while the name
    is still this attach's. It is never deferred: a teardown or a destructor would aim it at
    whatever answers to the name by then.

    What that costs is the case where the immediate removal also fails or is cancelled: the
    table stays in the container and nothing comes back for it. When there is a caller to tell,
    it is told -- `Rollback` gained a residue channel for exactly this, so the error carries "a
    table is left in place" rather than the machine quietly differing from what the caller was
    told. A cancellation has no one to tell, and that is the residual. This reverses decision 83
    and re-settles decisions 75 and 80: a leaked table is this container's problem, a delete
    aimed at a name is whoever holds that name's, and the second is not outrig's to risk.

89. **The stop names the container, not the name.** A handle kept for a retry can outlive the
    name it was created under: the container goes away, the name is free, something else takes
    it, and the retry stops *that*. The removal has been scoped to the attempt label since
    `NameGuard` was written, on the stated grounds that "a name is a request, not a claim" -- the
    stop had never been. It uses the id podman prints from the `create`/`run` that made the
    container, which the engine never hands out twice. A handle with no id -- one outrig only
    borrowed -- falls back to the name and never reaches a stop anyway, since stopping a borrowed
    container returns early.

90. **A caller's grace cannot panic the process.** `grace + MIN_REMOVAL_BUDGET` is `Duration`
    addition, which panics on overflow, on a public method: `Duration::MAX`, or anything within
    thirty seconds of it, would have brought the process down before any cleanup ran. It
    saturates. A checked add returning a typed error was the other option and was not taken: a
    grace that large is not a mistake this can usefully report back, and saturating gives the
    caller what they asked for -- a budget longer than the machine will be up for.

    Adding the id pushed `Container::handle` to eight arguments, which clippy refuses. The three
    that say *which container this is to podman* moved into one `EngineIdentity` -- the name that
    was asked for and can be granted again, the attempt label that scopes a removal to what one
    request made, and the id the engine never hands out twice. Grouping them is better than an
    `allow`, because the distinction between the three is the whole subject of this decision.

## Decisions from the eighteenth review

91. **With no identity there is no removal, at any point.** Decision 88 still ran the
    delete-by-name once, on the reasoning that it was safe in the instant after the commit. It is
    not, and "immediately" only made the window small: the delete is an await, so a cancellation
    inside it strands the table with nothing owning it, and an actor that replaced the table
    first has *that* deleted instead. Both were reachable. The command now comes off the rollback
    unrun, and what was made is reported as left behind -- with no await between the decision and
    the error, so the outcome is the same whether the caller waits for it or walks away.

    This is the end of a line that ran through decisions 75, 80, 83 and 88, each of which tried
    to keep the delete-by-name in some form. It cannot be kept in any form: a name stops being
    this attach's the moment the transaction that created it commits. What is given up is the
    table itself on a path where nft would not say what it had just made -- leaked, and said so
    in the error, which is the only thing that makes it something an operator can act on.

92. **A container id is what podman prints, not whatever it prints.** `engine_id` took any
    single token, which makes a wrapper script, a shim, or an engine with a different output
    format able to hand back a *container selector* -- and that selector is then what every stop
    the handle issues names. It requires the full 64-character hex id podman actually writes.
    Anything else fails the creation, before the name guard is released, so the attempt label
    removes what was made rather than leaving it under a handle that cannot name it. The fallback
    to the name is therefore no longer reachable for a container outrig created; only a borrowed
    one has no id, and a borrowed one is never stopped.

    The cost is that an engine printing a short id, or podman changing its output, breaks
    creation rather than degrading quietly. That is the direction to fail in: the alternative is
    a handle whose stop names something it was never told about, discovered at teardown.

    The wiring that refuses it is a `?`, and a breakage sweep showed nothing catching a change
    from that `?` back to a fallback -- reaching it needs a podman whose output a test cannot
    choose. So the state itself is gone: `EngineIdentity` is an enum of `Owned { name, attempt,
    id }` and `Borrowed { name }`, and "a container outrig made, whose id it did not get" cannot
    be constructed. The `ownership` argument went with it, since the two shapes already say which
    it is. A compiler refusing to build the bad state beats a test hoping to catch it at the
    other end, which is the only place its cost shows up.

    The enum alone was not enough: `id: String` still accepts an empty one, and the break that
    wrote `unwrap_or_default()` compiled and passed. So the id is a `ContainerId` newtype with a
    private field, no `Default` and no `From<String>`, whose only constructor reads it out of
    what the engine printed. Every fallback that would produce an id from something else --
    a default, the container's name -- now fails to compile, which is the outcome a sweep cannot
    reach by breaking things one at a time.

    The cancellation tests' fake podman printed nothing for a `create`, and so became an engine
    outrig now declines to work with: two of them failed. The fake prints an id, which is what
    the real one does. That is the visible cost of failing closed, and it is the right shape for
    it to take -- a stand-in that does not answer like podman is told so at creation rather than
    at some teardown much later.

## Decisions from the nineteenth review

93. **Doc comments that my own edits corrupted.** Four blocks carried duplicated fragments, and
    one of them was worse than untidy: `intercepted_resolv` still said the resolver's marker
    carries the attach's *table name*, which decision 60 reversed precisely so that publishing
    the resolver does not disclose the table name to anyone in the namespace who could then
    pre-create that table. A maintainer trusting that paragraph would have collapsed the two
    nonces back into one. The table-name block had also drifted onto `attach_nonce`, leaving
    `nft_table_name` undocumented. Repaired, and the marker doc now names the test that pins the
    separation.

94. **Only a namespace that is provably gone discharges an obligation.** `namespace_id` mapped
    every probe error onto "not ours", so a transient failure against a live namespace -- the
    kernel has its own reasons to fail this -- cleared every armed undo, skipped the residue that
    was already owed, and let `detach` return `Ok(())` with the redirect table still installed.
    The probe is tri-state now: `Is`, `Gone`, `Unknown`. Only `Gone` clears. `Unknown` leaves the
    undos armed and becomes a teardown failure, and the destructor leaves them to be reissued.
    The classification is a function of its own so the three answers are testable without a
    namespace that misbehaves on demand.

95. **A detach that had to abort the accept loop still waits its connections out.** The loop
    drains its own `JoinSet` before returning, so joining it is normally the whole story -- but a
    loop that outlasts the grace is aborted, and that *drops* the set, which asks for the
    connections' abort without awaiting it. One could then queue an audit record behind the drain
    marker, after `detach` had claimed completeness. Every connection holds a clone of a sender
    it never sends on; the attachment keeps the receiver, and teardown waits for it between
    stopping the tasks and draining the sink. `stop_tasks`'s doc said the stronger thing and now
    says what it does.

96. **One writer owns the audit log, and says so to the filesystem.** The writer's rollback
    truncates the file back to where a failed record began, and `ftruncate` acts on the inode:
    two interceptors pointed at one path would have had the first destroy the second's already
    acknowledged records, with the second reporting a clean teardown. The file is claimed with a
    non-blocking exclusive `flock` for the writer's lifetime, which also refuses a second outrig
    process. Regular files only -- truncation means nothing for a device, and the tests that need
    every write to fail point at `/dev/full`, whose single inode they would otherwise queue on.

97. **A confirmed stop supersedes an attach that could not be undone.** `NetworkAttachNotUndone`
    says a container may still be carrying interception nothing owns. Stopping that container
    ends the claim, but it was still handed to the caller -- so `/sidecar add` could print
    "obligation(s) left" next to "session unaffected". One helper unwraps it to what actually
    stopped the attach and logs the superseded obligations, used by both the library unwind and
    the CLI's.

98. **Three boxed error variants terminated the source chain.** `#[error("{0}")]` sets `Display`
    and nothing else, so `source()` returned `None` and a consumer walking the chain never
    reached the cause the payload carries. They are `#[error(transparent)]`, which keeps the
    rendering and forwards the source. `McpStartupFailed` has the same shape and is left alone:
    it predates this task.

99. **Two claims in the docs were wider than the code.** Audit records are *written*, not synced
    -- the acknowledgement means the record is in the file and readable, not that it survives the
    host losing power, and a sync per connection is not a cost this log will pay. And "reported
    rather than logged" is true of the interceptor's API; a session teardown logs what it gets
    back and carries on stopping containers, because stopping them is the more urgent half. Both
    now say which.

## Decisions from the twentieth review

100. **A `stat` that failed is not "not a regular file".** The audit lock was guarded by
     `is_ok_and(|f| f.is_file())`, which took the same branch for a metadata error as for a
     device -- pairing the new truncating rollback with no lock, which is the one combination the
     lock exists to prevent. The error propagates now, and the exemption is taken only on a file
     type that was positively read and is not regular.

101. **Two grace windows, two errors.** The wait for connections to finish reported
     `NetworkTasksAborted`, the same variant `stop_tasks` pushes when the loops have to be
     aborted -- and both can expire in one teardown, which put the same variant in the causes
     twice with the worse of the two unreadable. The second is worse: the first says the accept
     and DNS loops were stopped, the second says a *bridge* outlived that, possibly still moving
     bytes for a container the caller has been told is detached.
     `NetworkConnectionsUnfinished` says so.

102. **Two more ways the kernel says a task is gone.** `classify_namespace` read only `ENOENT` as
     gone, so a container exiting concurrently with its detach could answer `ESRCH` -- which Rust
     leaves uncategorized -- or `EACCES`, and be reported as a teardown failure against a
     namespace that had simply ended. `ESRCH` is matched raw. `EACCES` is ambiguous on its own
     and is settled by asking whether `/proc/<pid>` is there at all, which is the only place that
     question is asked. Nothing could have acted wrongly on the old reading -- both undos carry
     their own in-target guards and go out `Reissue::Once` -- so what this fixes is a false
     report.

103. **An aggregate's `source()` stays `None`, and this is the disagreement.** The review asked
     for `#[error("{0}")]` with the boxed field marked `#[source]`, so a consumer walking the
     chain could downcast to the list of causes. Tried, and not kept: thiserror hands the *box*
     to the chain, so the downcast target is `Box<NetworkTeardownFailure>` rather than the
     payload, and naming the same field as both the `Display` and the source makes every rendered
     chain print the aggregate twice. `NetworkTeardownFailure`'s own rendering already names every
     cause with its container, so a walker that stops there has printed the whole story -- which
     a test now pins -- and a consumer wanting structure matches the variant, which is public and
     boxed for exactly that. `McpStartupFailed` did become `transparent`, since its payload
     carries a real inner source and it was the one left inconsistent.

## Decisions from the twenty-first review

104. **A message assembled across source lines carried the gap to the reader.** The new
     `NetworkConnectionsUnfinished` string picked up ten literal spaces where a line break was
     meant, and every rendered teardown error would have shown them. The test asserted on a
     fragment that fell before the gap, which is how it passed over it: it now reads the message
     end to end and refuses a double space anywhere in it. The string is one line, which is the
     shape that cannot grow a gap in the first place.

105. **Two doc claims narrowed at the claim rather than after it.** The audit wording said
     records are "written by the time the session is down" and then qualified it a sentence
     later; the teardown wording said failures are "returned rather than logged" and then
     explained where. Both now say the narrow thing first -- "*in the file*, not synced to the
     disk", and "returned to the caller of the interceptor", with what `outrig` the command does
     with that spelled out. A qualifier a sentence behind the claim is a qualifier a reader can
     miss, which is what happened to the reviewer three times running.

