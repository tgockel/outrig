# The session-root registration symlink can itself land inside an unignored build context

## Context

`image::refuse_session_dir_in_build_context` (added for #909) checks the directory a fresh
session is about to write into -- the explicit `--session-dir`, or an auto-allocated
`<root>/<sid>` -- before the image is keyed. An explicit `--session-dir` outside the build
context passes that check cleanly.

But `SessionStore::create`'s explicit-directory path also writes a second thing under the
*session root*: `<root>/<sid>`, a symlink to the explicit directory, so `outrig ls` and friends
can find it (`crates/outrig-cli/src/session.rs`, the `symlinked` write in `create`). If
`--session-root` (not `--session-dir`) resolves to a path inside the build context and that
symlink is not ignored there, the symlink itself is new content the context's hash did not see
when the tag was computed, and `ensure_key_still_holds` still discards the finished build as
`BuildContextChanged` -- the exact failure #909 fixed for the directory itself, still reachable
through the registry entry.

This is not a regression from the #909 fix: it fails the same way on `trunk` before it. Filed
because review of that fix's "check before anything is hashed" design surfaced it as the one
write the guard does not yet cover.

## Sketch

Extend the pre-hash check to the registry entry too: for an explicit `--session-dir`, also probe
`<session_root>/<sid>` (the symlink path) the same way the directory itself is probed, before the
tag is computed. `session_root` is already in scope at the call site
(`crates/outrig-cli/src/cli/session_setup.rs`); the symlink's own content (a path, not a
directory) does not need `directory_is_ignored`'s mkdir dance -- a plain file-shaped path check
against the context should do, since the symlink itself is the only thing landing there, not a
tree under it.

Related: `plan/done/phase/.../tasks/` entry for #909 once it lands (not yet filed at review time).
