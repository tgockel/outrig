# 0086 -- Sidecar startup and clean-sweep performance follow-ups

## Goal

Land the deferred efficiency items from task 0079's review pass; none block correctness.

## Deliverables

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

## Acceptance

- Observable behavior is unchanged: same tools, same deterministic label-collision
  errors, same session teardown semantics.
- Sidecar bring-up overlaps across sidecars rather than running serially.
- One watcher process per session instead of 1 + N `podman wait` children.
- `outrig clean` answers record-backed running-state from the single `podman ps -a`
  listing and batches its `podman rm -f` calls.

## Dependencies

None (follows up on completed task 0079; sequenced after 0085 since both touch
`setup_sidecars_and_network`).

## Decisions

Made during planning/execution (2026-07-13):

- **`join_all`, not `JoinSet`.** The codebase's only fan-out idiom is
  `futures_util::future::join_all` (`network.rs`); there is no `JoinSet`. `join_all`
  preserves input order (so the name-ordered label merge stays deterministic) and runs on
  the current task, so the borrowed `&Config`/`&Path`/`&Transcript`/`&plan` need no
  `'static`/`Send` and nothing is cloned to satisfy a spawn bound.
- **Three-phase bring-up.** `setup_sidecars_and_network` fans out image-ensure + label-read
  (Phase A, deduped by image ref -> memoization), then merges labels + decides starts
  serially in name order (Phase B, preserves deterministic collision errors), then starts
  containers concurrently and inserts in name order (Phase C). Network attach stays serial.
- **Memoized label read gated on `need_labels`.** An image is label-inspected only if a
  *non-anonymous* sidecar uses it. This is a correctness constraint, not just perf: eagerly
  inspecting an anonymous-only image would run a `podman image inspect` the serial path
  never ran and could newly fail a session on a malformed `org.outrig.mcp` label.
- **Shared image failures kept as `Display` text, re-wrapped via `io::Error::other`.**
  `CliError` is not `Clone`, so a deduped image's failure is stored as its message string
  and rebuilt per consumer. `OutrigError::Configuration` is *not* transparent (it prepends
  `"configuration: "`), so reconstructing through it would alter the error text;
  `io::Error` (via transparent `OutrigError::Io`/`CliError::Outrig`) round-trips the message
  verbatim. Only `Display` + exit-1 are observable (`app.rs`), so the variant change is not.
- **One `podman events` watcher per session** replaces the 1+N `podman wait` children:
  `podman events --since <started_at> --filter event=died --filter
  label=org.outrig.session=<sid> --format json`. `--since` replays a death that landed
  during the arm window; a **shared-list membership gate** on the sidecar-death line
  suppresses the replay of setup-time deaths of warn-dropped sidecars (which were never
  registered). The reader stops after the primary dies (so the reap's own sidecar deaths
  are not logged as spontaneous) and, on unexpected stream EOF, degrades (warns, never
  cancels, no respawn) to today's single-container "no auto-reap". `register_sidecar`
  becomes push-only; the label-filtered stream already covers a `/sidecar add`.
  `wait_for_container_exit` stays for attach-mode `outrig mcp`.
- **`outrig clean` running-state from one unfiltered `podman ps -a` (Option 1).** The single
  listing yields both the labeled stray rows (unchanged `classify_strays` input) and the set
  of *all* running container names. The record walk answers running-state by name from that
  set instead of a `podman inspect` per aged session. Unfiltered because a record's primary
  can be outrig-**unlabeled** -- not only pre-0079 sessions but every `outrig mcp --attach`
  session (it records the borrowed container's name) -- and a label-filtered sweep would
  miss those. Stray `podman rm -f`s are batched into one invocation.
- **Primary-bootstrap overlap deferred** to `plan/next/sidecar-primary-bootstrap-overlap.md`
  (bounded ~100ms win against restructuring the primary abort path).
