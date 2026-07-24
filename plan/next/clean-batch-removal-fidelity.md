# `outrig clean` can report a stray it did not remove

Task 0086 coalesced the stray sweep's per-container `podman rm -f` calls into a single
batched invocation (`podman_remove_force_batch` in `crates/outrig-cli/src/cli/clean.rs`).
`execute_with` awaits that one call, then prints `[outrig] removed container <name>` for
every stray in the batch. The per-container line is therefore a claim about the batch's
exit status, not about that container.

Observed 2026-07-24 while cutting 0.2.0-rc.1, in a full parallel e2e run
(`clean_sweeps_stopped_recordless_labeled_containers`, `crates/outrig-cli/tests/mcp_sidecar_smoke.rs`):
`podman rm -f` exited 0 -- the "removed container" lines printed for all five names, and
`podman_remove_force_batch` returned `Ok` -- yet two of the five were still in the store
afterward (`outrig-straytest-1ba64594` exited, `outrig-20260722T172711-93c3` created). A
direct `podman rm -f <name>` removed each instantly. The batch had swept up three live
containers belonging to concurrently-running tests, so podman was being asked to remove
containers another process was tearing down at the same moment.

The test passes in isolation, so the trigger is concurrent mutation of the same containers,
not the batching alone. But the batching is what makes the failure silent: one `rm` per
stray would have surfaced a non-zero exit for the specific container that did not go away,
whereas the batch reports success for all five on an aggregate exit code that podman
apparently returns as 0 even when it skips some names.

Fix shape: stop inferring per-container outcomes from an aggregate exit status. After the
batch, re-list (the sweep already reads `podman ps -a`, so a second cheap read is in
keeping) and print `removed` only for names that actually disappeared, reporting the rest
as "could not remove" -- which is what the doc comment above `podman_remove_force_batch`
already promises: "clean should report a container it could not remove rather than claiming
success." Keep the single `rm -f` for the common path; this is about the reporting, not the
call count.

Worth pairing with test isolation: the stray sweep matches `org.outrig.session` across the
whole podman store, so any `outrig clean` test necessarily collides with other e2e tests
running in parallel. Scoping the sweep test's assertions to its own container name (it
already does) is not enough -- the batch it triggers still touches other tests' containers.
