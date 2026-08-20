# The panic-hook sweep still removes containers by requested name

Tracked upstream as <https://github.com/tgockel/outrig/issues/147>, which carries the
acceptance criteria; this entry is the repo-side sketch.

`crates/outrig/src/container/mod.rs` keeps `TRACKED` as a
`Mutex<BTreeSet<String>>` of container *names*, and `install_panic_hook` sweeps
it with `podman rm -f <name>`. Two defects follow, both pre-existing:

- **A panic during a colliding start deletes someone else's container.** A name
  is a request, not a claim: `podman run --name N` fails when N is already in
  use, and the sweep cannot tell "the container I made" from "the container that
  was already there". This is exactly the defect 0116 fixed for the cancellation
  path, where the guard now removes by a per-attempt `org.outrig.attempt` label
  instead. The panic hook did not move because it is a different trigger and the
  task had already grown; the mechanism it needs now exists.

- **A set cannot hold two reservations of one name.** `track` inserts and
  `untrack` removes, so if two starts ever reserve the same name, whichever
  finishes first untracks the other's obligation.

## Sketch

Make `TRACKED` a map from name to the attempt label that start stamped, and have
the panic hook replay `podman rm -f --filter label=org.outrig.attempt=<token>`
rather than reconstructing a removal from the name. That is already what
`NameGuard::drop` does, so the hook and the guard would share one selector.

Two constraints on the change:

- `container::force_remove_detached` is public and is used by the session watcher
  to reap sidecars it holds no handle for. It has only a name, so the by-name
  removal has to survive for that caller; it is safe there for the same reason
  `Drop for Container` is -- the name belongs to a container this process is
  known to have created.
- The second defect wants an obligation identity independent of the name -- a
  counted or keyed entry rather than a set of strings -- so that two reservations
  of one name are two obligations.

## Why it is worth doing

The panic hook is one of the four cleanup layers
`doc/concepts/containers.md` promises, and it is the one that runs when the
process is already in trouble. A layer that can delete an unrelated container is
worse than one that does nothing.
