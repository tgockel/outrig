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
