# The temporary session root is the user's own

## Context

With no `session-root`, the library puts a session's directory under
`std::env::temp_dir()/outrig-sessions`: `SessionBuilder`'s default container (`harness/builder.rs`)
and a host program's sessions (`owner/session.rs`) both do, and the CLI falls back to the same path
when it knows no data directory (`paths.rs`). Nothing checks who owns that directory. On a host
shared with other users, one of them can create `/tmp/outrig-sessions` first, writable by all, and
replace a session's directory, before its log is opened, with one holding an `events.jsonl` FIFO.
The line sink (`line_sink.rs`) opens an existing path with `create` and `append`, takes a file
that is not regular as it is, and skips its owner-only mode for one, so the session's events go
to a reader the other user chose. Found by the review of `0003-29`; it predates that task.

## Shape

- The fallback root is the user's alone: created `0700`, then checked to be a directory, not a
  symlink, owned by the current uid, and not writable by group or others -- or a per-user path,
  such as `outrig-sessions-<uid>`, checked the same way. Session directories are created below it
  as they are now.
- The events log is opened without following a symlink, and refused when it exists and is not a
  regular file the user owns.

## Acceptance

- A pre-existing `outrig-sessions` owned by another uid, or writable by others, fails a session
  with no `session-root` before anything starts, saying why.
- A FIFO or a symlink at a session's `events.jsonl` is refused rather than written to.
