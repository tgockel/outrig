# 0117 -- Interceptor attach rolls back, and detach ends every bridge it started

## Context

0078 made `NetworkInterceptor` multi-container: one `Attachment` per container, holding a
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

### A scope guard is not sufficient, and 0116 does not cover this

An ordinary Rust scope guard cannot await. Restoring `/etc/resolv.conf` means running a `podman
exec`; deleting an nft table means running `nsenter`. Neither is expressible in `Drop`. And 0116
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

1. **Abort versus graceful close for in-flight bridges -- Recommended: cancel, then abort on a
   short grace.** Cutting a copy loop mid-stream is visible to the container as a truncated
   connection, which is honest for a detach but not free. A grace bounded in the hundreds of
   milliseconds matches `plan/done/0109-subagent-tree-shutdown-grace.md`'s posture.

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

- **Hard: 0116.** The cancellation half needs a child that dies with its future; otherwise a
  canceled `attach` still leaves the `podman exec` that rewrote `resolv.conf` running. 0116 is
  necessary and not sufficient -- see Context.

## See also

- `crates/outrig/src/network.rs` -- `attach` (282), `install_audit_resolv_conf` (915),
  `apply_nft_rules` call (307), `Attachment` (224), `detach` (342), `shutdown` (352),
  `teardown_attachment` (374), `Cleanup::delete_table` (398), `tcp_accept_loop` spawn (606),
  `handle_tcp` (624), `dns_loop` (805).
- `plan/done/0078-interceptor-multi-container.md` -- where per-container attachment was built.
- `plan/done/0109-subagent-tree-shutdown-grace.md` -- the existing grace-period posture.

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
   point; it predates 0116 landing `detach_cleanup`.

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
