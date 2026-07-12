# Sidecar startup and clean-sweep performance follow-ups

Deferred efficiency items from task 0079's review pass; none block correctness.

- **Concurrent sidecar bring-up.** `setup_sidecars_and_network` runs image-ensure ->
  label read -> `podman run` -> bootstrap fully serially per sidecar. Sidecars are
  independent; fan the ensure/label phase out (`JoinSet`), apply `merge_sidecar_labels`
  over the collected results in name order (keeps deterministic collision errors), then
  start containers concurrently. The ensure phase could also overlap the primary's
  bootstrap.
- **Memoize duplicate sidecar images.** Two sidecars sharing one `image` re-run tag
  compute + ensure + label inspect. A per-session `image -> (tag, labels)` map fixes it.
- **One watcher process per session.** The watcher holds 1 + N idle `podman wait`
  children. `podman events --filter event=died --filter label=org.outrig.session=<sid>`
  watches every session container with a single process (`podman wait` with multiple
  args waits for *all*, so it cannot batch).
- **`outrig clean` running-checks.** The record walk still spawns one
  `podman ps`-equivalent per aged session while the new label sweep already fetched
  every labeled container's state in one `podman ps -a` call; answer record-backed
  running-state from that listing and keep per-name probes only for pre-label sessions.
  Batch the stray `podman rm -f` calls into one invocation while there.
