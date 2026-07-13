# Overlap sidecar image-ensure with primary bootstrap

Task 0086 made sidecar bring-up concurrent *across sidecars* (`setup_sidecars_and_network`
now fans out image-ensure/label-read in Phase A, then starts containers in Phase C). It
deliberately left one overlap on the table: the sidecar Phase-A ensure work still begins
only *after* the primary is started and `bootstrap_user`'d (`setup`, around the
`Container::start_named` -> `bootstrap_user` sequence for the primary), even though the two
are independent -- sidecar image-ensure/label-read depend only on `image_cfg`/config, not
on the primary container existing.

The win is bounded: `bootstrap_user` is a short chain of `podman exec` round-trips (~100ms),
so overlapping it with sidecar ensure saves at most ~that, and only for sessions that
declare sidecars (single-container sessions have nothing to overlap). The cost is
restructuring the primary path's delicate abort tail (stop primary + finalize row + return
on bootstrap failure).

If pursued: split Phase A into a pure ensure/label-read future (no `plan` mutation, no
container starts) and run it as `tokio::join!(container.bootstrap_user(), phase_a_ensure)`,
keeping the label-merge (Phase B) and container starts (Phase C) after the join. Preserve
the primary-bootstrap abort semantics exactly: a primary `bootstrap_user` failure must still
stop the primary and finalize the row with exit 1, regardless of the sidecar future's state.
