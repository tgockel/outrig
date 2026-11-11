# An admitted symlink can carry the #909 guard's "excluded" answer past what it actually covers

## Context

`image::refuse_session_dir_in_build_context` lets a directory stand if a git context's ignore
rules exclude it. The hasher `TreeHasher` (`crates/outrig/src/image.rs`) follows a symlink to
what it resolves to and hashes *that*, regardless of whether the *target* path is itself
ignored -- only the symlink's own listed name is tested against the rules.

So: `sessions/` is ignored, but an unignored symlink elsewhere in the context (`alias ->
sessions/`, say) is not. The guard, asked about `sessions/<sid>`, correctly says "excluded." But
`alias/<sid>` -- the same inode, reached through the unignored name -- is exactly what
`TreeHasher` hashes when it walks `alias`, because an admitted symlink's *target* is read and
hashed without asking whether that target is itself excluded. A session write under `sessions/`
after the tag is computed can therefore still reach the hash through `alias`, and the finished
build is still discarded.

Not a regression: this shape builds (and discards) the same way on `trunk` before #909; the
guard's exclusion is narrower than what the hasher actually admits, in this one corner, same
family as the whole-tree-fallback gap filed alongside this one.

## Sketch

Probably not worth chasing through the guard itself -- symlink aliasing of an ignored subtree is
a narrow, deliberately-contrived shape. If it is worth closing at all, the cheaper angle is
likely on the hasher's side: when `TreeHasher` follows a symlink, note the resolved path exists
under an otherwise-excluded directory and treat it as excluded too, rather than teaching the
guard to replicate symlink resolution it does not otherwise need.
