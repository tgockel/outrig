# Unreadable session records can be warned about but never removed

`SessionStore::list` now returns `SessionListing { sessions, skipped }` so one
unparseable `session.json` no longer aborts a listing, and `outrig ls` reports each
skip on stderr. That makes a bad record *survivable* but leaves it *permanent*:

- `outrig clean` -- the garbage collector, the command whose job this is -- consults
  `skipped` only to know an unreadable record still *owns* its container (so the
  container isn't force-removed as a stray); it never offers to remove the record.
- `outrig discard <id>` and `discard --session-dir` both hard-fail inside
  `get_by_id` / `get_by_path`.

So `ls` re-warns about the same entry on every invocation forever, and the only way
out is `rm -rf` by hand. (`resolve_session_arg` at least names the entry and its parse
reason now, rather than reporting `no session matching`.)

`SkippedSession` currently carries `entry: String` + `reason: String`; the resolved
`PathBuf` and the typed error are discarded at the point of capture, so no caller
*could* act on a skip even if it wanted to.

## Sketch

- Widen `SkippedSession` to carry the resolved directory and the underlying error.
- Let `clean` include over-cutoff unreadable entries in its preview. They can't pass
  the running-container check (no `container_name` to compare), so they need their own
  prompt line rather than silent inclusion.
- Let `discard --session-dir` remove a directory it can't parse, behind the existing
  `[y/N]`.

## Why it was deferred

Deleting records outrig can't parse is a behavior change that wants its own design and
its own confirmation semantics -- out of scope for the `ls`-fails-outright bug fix that
introduced the skip path.
