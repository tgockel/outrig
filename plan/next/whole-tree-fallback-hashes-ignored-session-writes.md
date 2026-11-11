# `hash_git_tree`'s all-ignored-counts-as-admitted fallback can defeat the #909 guard

## Context

`image::refuse_session_dir_in_build_context` treats a directory a git context's rules exclude as
no conflict: the session's future writes there will not be hashed, so there is nothing for the
build to discard. That reasoning assumes the hasher's exclusion is unconditional.

It is not. `hash_git_tree`'s own doc (`crates/outrig/src/image.rs`) spells out one case where it
is not: "Where the rules exclude every file ... the whole tree counts." If the *build context
itself* sits under a directory every rule excludes (an `.agents/`-style context kept out of
version control on purpose, say), the hasher falls back to hashing everything, session directory
included, regardless of its own `.gitignore` entry. A session directory the #909 guard allowed
through on the strength of its own ignore rule can still turn a finished build into a discarded
`BuildContextChanged` once this fallback is the one actually in effect.

Not a regression: `trunk` already builds (and already discards) under this shape before #909;
the guard's exclusion is simply less complete than the hasher's, in this one corner.

## Sketch

Before trusting a git context's ignore rule as an exclusion, ask the same question
`hash_git_tree` effectively answers for the whole context: would every file in this context come
back excluded? If so, the fallback is in effect and nothing is actually excluded, session
directory included. Likely cheapest as a shared helper both `hash_git_tree` and
`refuse_session_dir_in_build_context` call, so the two cannot drift again.
