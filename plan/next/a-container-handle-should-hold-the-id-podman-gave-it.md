# A `Container` handle addresses its container by name, which is reusable

Pre-existing, and the half of the name-versus-identity problem that 0116 did not reach.

`Container` stores the name it *asked* podman for and addresses everything through it:
`stop` runs `podman stop <name>`, and `exec_*`, `pid`, and the interceptor attach all name it
too. A `--rm` container that has exited frees that name, and if anything takes it -- another
session, a stray, a container made by hand -- a stale handle's next call lands on the
replacement. `stop()` is the one that hurts: it stops somebody else's container, and no later
cleanup can undo an outage.

0116 fixed this for *removals* by stamping `org.outrig.attempt` at creation and filtering on
it (`container::removal_cmd`), so every removal outrig issues for a container it made is
scoped to the attempt that made it. Nothing equivalent covers the other verbs, because a
label filter is not a way to address one container for `stop`, `exec`, or `inspect`.

## Sketch

Capture the container id and address by it. `podman run -d` and `podman create` both print the
id on stdout, and both calls already run through `process::run_capture_logged`, so the value is
in hand at construction -- it needs storing on `Container` beside `attempt`, and the call sites
switching from `&self.name` to it.

Three things to settle while doing it:

- **What an attached handle does.** `Container::attach` has no id and never ran a create;
  `inspect_existing` could read one. Either it resolves an id up front, or the by-name path
  survives for attached containers only, where the caller has taken that risk knowingly.
- **What the name is still for.** Log lines, `session.json`, `session_suffix`, and the panic
  hook's `TRACKED` set all want the human-readable name; only the *addressing* moves.
- **`force_remove_detached`** is public and takes a name, because its caller (the session
  watcher) has no handle. It stays by-name, and stays `Reissue::Once` for that reason.

## Worth doing with 0127 or 0129

0127 already reopens engine-resource ownership, and 0129 stands up the live-podman harness this
needs: proving it means creating a container, letting `--rm` free its name, taking that name
with a second container, and requiring the stale handle's `stop()` to leave the second one
running. That is a live-engine test, not a fake one.
