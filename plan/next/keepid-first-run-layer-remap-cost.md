# First `--userns=keep-id` run silently pays a multi-minute layer remap

`build_podman_run_cmd` (`crates/outrig/src/container/mod.rs:704`) emits `--userns=keep-id` on
every primary container. On a rootless host whose storage driver cannot use native idmapped
mounts, podman satisfies that flag by materializing a **physical ID-mapped copy of the whole
image layer** before the container can start. The copy is cached per (layer, mapping), so only
the first run after an image build pays it -- but nothing tells the user that.

Measured 2026-07-26, podman 4.9.3, `overlay` driver, `outrig-standard:60e465cbb1bcd8e6`
(2.36 GB):

| Run                             | `podman run -d` wall clock |
|---------------------------------|----------------------------|
| First, cold ID-map cache        | 3m05s                      |
| Subsequent, warm               | 222ms                       |

Interrupting mid-copy discards the partial result, so a user who gives up after ~60s and
retries pays the full cost again from zero and never converges. That is exactly what happened:
two consecutive `outrig run` attempts were killed at ~60s each, both leaving a session dir with
an empty `logs/` and no container, which reads as a hang. Killed attempts also leave the copy's
storage behind -- `~/.local/share/containers/storage/overlay` had grown to 17 GB.

`RUST_LOG=debug` now names the stalled command (`outrig::process` spawn/exit pair, added
alongside this entry), which is enough to identify *what* is slow. It does not explain *why*,
and the explanation is non-obvious enough that it deserves surfacing.

Fix shape, in rough order of value:

1. Make the wait legible rather than silent. The `starting container` `ProgressSpan`
   (`crates/outrig-cli/src/cli/session_setup.rs:537`) could note, when the elapsed time crosses
   a threshold, that a first-time `--userns=keep-id` remap of a large image is expected to take
   minutes and must not be interrupted. A heartbeat beats a spinner here -- the operation is
   genuinely long, not stuck.
2. Check whether `--userns=keep-id` is needed at all when the container's target uid/gid
   already equal the host's. `bootstrap_user` (`container/mod.rs:447`) resolves the user
   *inside* the container afterward; if the image already has a matching uid, the mapping may
   be redundant and the flag droppable for that case.
3. Document the one-time cost in `doc/concepts/containers.md` next to the keep-id rationale,
   and point at `podman system df` / the storage growth it implies.

**Not** a timeout. Wrapping podman in `tokio::time::timeout` would convert a legitimate slow
operation into a spurious failure, and would guarantee the retry loop above never terminates.
The problem is that a correct 3-minute wait looks identical to a hang, not that the wait is
too long to permit.
