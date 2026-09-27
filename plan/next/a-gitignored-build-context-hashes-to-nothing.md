# A gitignored build context hashes to nothing

When an image-config's `context` sits in a git repo but the context directory itself is
gitignored -- a user who keeps `.agents/` out of version control, say -- `hash_git_context`
(`crates/outrig/src/image.rs`) lists nothing: `--cached` finds no tracked files under it, and
`--others --exclude-standard` drops every untracked one as ignored. The cache key then covers
only the `Dockerfile`, build args, and labels, so no edit to a file in the context ever
rebuilds, and `outrig build` keeps reporting a cache hit on the old image.

The gitignore exclusion exists to keep build output *inside* a source context (`target/`,
`node_modules/`) from busting the cache. When the context root is itself ignored, `.gitignore`
says nothing about which of its files are build inputs, so the tarball path is probably the
right hash for it.

Open question is the detection: `git check-ignore` on the context root (and how it treats a
directory, a prefix, and a path under an ignored parent), versus comparing an empty listing
against a non-empty directory. Pin the behavior with a test beside
`gitignored_file_does_not_affect_key_when_in_git` in `crates/outrig/src/image_cache_tests.rs`,
and update the image-caching section of `doc/concepts/containers.md` to match.

It is the same question `hash_git_tree` already answers for a directory git lists whole:
"does git's listing govern this directory?" There it is `is_worktree_root` (`git rev-parse
--show-prefix` is empty), because `is_git_context` (`git rev-parse --git-dir`) also succeeds
inside an uninitialized submodule, or one whose `.git` git rejects, and recursing would re-list
it as `./`. One predicate in a shared `hash_context(dir, hasher)` -- git only for a worktree root
or a subdirectory git neither ignores nor records as a gitlink, a plain tree otherwise -- would
serve the top level, the nested case, and the duplicated git-vs-tar dispatch in
`CacheKey::compute` / `compute_with_labels`.

A more general shape, if this area is reworked: walk the filesystem (the directory buildah
actually receives) and use git only as an exclusion filter
(`ls-files --others --ignored --exclude-standard --directory`). That drops the index-vs-worktree
patches (`dedup` for unmerged stages, the skip for deleted tracked files, the gitlink case), and
covers empty directories and directory modes, which reach buildah but not today's git-path key.
`TreeHasher` (`crates/outrig/src/image.rs`) already walks a tree that way, following symlinks as
buildah resolves them, for link referents and for directories git lists whole; the general
shape is that walk with an exclusion filter over the whole context.

Found while fixing #176. Tracked as #216; the empty-directory and directory-mode gap is #217.
