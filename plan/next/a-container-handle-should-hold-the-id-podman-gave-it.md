# A `Container` handle addresses its container by name, which is reusable

Pre-existing, and the half of the name-versus-identity problem that 0116 did not reach.

> **Partly done in 0117.** `Container` now captures the id from the `create`/`run` that made it
> and `stop` addresses by it; a creation that will not report an id fails while the attempt-label
> guard is still armed. What is left is the rest of this entry: `exec_*`, `pid` and `inspect`
> still address by name, and the live-engine test below was never written.

`Container` stored the name it *asked* podman for and addressed everything through it:
`stop` ran `podman stop <name>`, and `exec_*`, `pid`, and the interceptor attach all name it
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
in hand at construction. 0117 did that much and switched `stop`; the remaining call sites --
`exec_*`, `pid`, `inspect` -- still use `&self.name`.

Three things to settle while doing it:

- **What an attached handle does.** Settled in 0117 for `stop`: `EngineIdentity::Borrowed`
  carries no id, and a borrowed container is never stopped. `exec_*` and `pid` against a
  borrowed handle still address by name, and that is the case left to decide -- resolve an id
  through `inspect_existing`, or keep by-name where the caller took that risk knowingly.
- **What the name is still for.** Log lines, `session.json`, `session_suffix`, and the panic
  hook's `TRACKED` set all want the human-readable name; only the *addressing* moves.
- **`force_remove_detached`** is public and takes a name, because its caller (the session
  watcher) has no handle. It stays by-name, and stays `Reissue::Once` for that reason.

## Worth doing with 0127 or 0130

0127 already reopens engine-resource ownership, and 0130 stands up the live-podman harness this
needs: proving it means creating a container, letting `--rm` free its name, taking that name
with a second container, and requiring the stale handle's `stop()` to leave the second one
running. That is a live-engine test, not a fake one, and 0117 landed the `stop` change without
it -- what covers it there is that the command names the id, asserted on the built command.
