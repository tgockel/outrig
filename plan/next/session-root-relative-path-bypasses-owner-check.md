# Resolve substring session matches through the store

The `clean` path from #876 now probes the store-resolved directory at preview
and removal, including for existing records with stale relative `session_dir`.

The shared `cli::resolve_found` substring fallback still builds `Found::Session`
from the persisted `session_dir`, whereas exact-ID resolution uses
`SessionStore::get_by_id`'s resolved directory. Consequently `discard <substring>`
can still bypass a live launch's owner check when the record contains a path
relative to another working directory. Exact-ID discard is already safe.

Resolve readable substring matches through the store too. Cover `discard` with
both a live owner and an owner claimed during confirmation, no running container,
and a stale relative path in the record. Check the shared resolver's other callers
(`logs`, etc.) for the same stale-path behavior.
