# 0007 -- Image build with cache

## Goal

Build images via buildah, content-addressed by Dockerfile + build-args + context content, so
unchanged inputs cache-hit instantly.

## Deliverables

- `src/image.rs::CacheKey::compute(dockerfile: &Path, build_args: &BTreeMap<String,String>,
  context: &Path) -> Result<String>` -- blake3 over:
  - The Dockerfile bytes.
  - A canonicalized build-arg block: each `KEY=value\n` sorted.
  - The context content hash:
    - If the context is inside a git repo (detect via `git -C <ctx> rev-parse --git-dir`):
      `git ls-files <relative-context>` then for each file `git hash-object <path>`,
      concatenated and blake3'd. This honors `.gitignore` for free.
    - Else: stream `tar --sort=name -cf - -C <ctx> .` into blake3.
  - Final tag is `outrig-cache:<first-16-hex>`.
- `src/image.rs::ImageTag(pub String)`.
- `src/image.rs::ensure_image(cfg: &ContainerConfig, repo_root: &Path) -> Result<ImageTag>`:
  1. Compute the cache key.
  2. `buildah images --quiet outrig-cache:<key>`. If non-empty, return early.
  3. Else `buildah build --tag outrig-cache:<key> --file <dockerfile> --build-arg KEY=val ...
     <context>`. Stream stderr via `[buildah]` tracing prefix.
- `tests/image_cache.rs`:
  - Same inputs -> same key (deterministic).
  - Different Dockerfile bytes -> different key.
  - Different `build_args` -> different key.
  - A change in any context file -> different key.
  - A git-ignored file in the context -> same key (when context is in git).

## Acceptance

- `cargo test image_cache` passes (no podman/buildah needed -- cache key is pure).
- `cargo test --features e2e image_build_smoke` builds a tiny fixture image, then runs again
  and cache-hits within ~100 ms.
- buildah's stderr appears in test output prefixed with `[buildah]`.

## Dependencies

- 0005-config-merge-validate
- 0006-process-wrappers

## Notes

- Don't pass `OUTRIG_UID`/`OUTRIG_GID` build-args -- `doc/concepts/workspace.md` explains why.
- The fixture image for the e2e test should be the smallest Dockerfile that succeeds:
  `FROM docker.io/library/alpine:latest` plus maybe `RUN apk add --no-cache shadow` for
  `useradd`/`groupadd`. Keep it offline-ish (no big package installs).
- `tar` for non-git contexts: pipe to `blake3::Hasher` via a child process; don't read into
  memory.

## Decisions

- **Layer the project cache on top of buildah's own layer cache.** Buildah caches
  layers (`--layers`, BuildKit-style), so a re-`buildah build` of an unchanged
  Dockerfile is fast -- but it still parses the Dockerfile, scans the context, and
  evaluates each instruction's cache key, costing hundreds of ms to seconds. The
  acceptance bar is `~100 ms` for a cache hit, which only a tag lookup
  (`buildah images --quiet outrig-cache:<key>`) reliably hits. Two further
  benefits the layer cache doesn't give us: (1) hashing `git ls-files` ignores
  untracked `target/`, `node_modules/`, etc. so stray artifacts can't bust the
  cache, without a `.dockerignore`; (2) `outrig-cache:<hash>` is a deterministic
  shared identifier across processes / agents on the same host. The two caches
  compose -- on a miss, buildah's layer cache still helps speed up the build.
- **Used `git hash-object --stdin-paths` instead of one process per file.** Same
  per-file SHA1 output as the literal `git hash-object <path>` form in the spec,
  but a single subprocess for any number of files. The spec prescribes the
  result; the optimization preserves it. Paths from `git ls-files -z` are sorted
  defensively before being fed to stdin so the hash stream is order-stable
  regardless of any future change in `git ls-files` output order.
- **Reproducibility flags on `tar`** (`--mtime=UTC 1970-01-01 --owner=0
  --group=0 --numeric-owner` in addition to the spec's `--sort=name`). Without
  these, the tar archive embeds filesystem mtimes and uid/gid, so a fresh
  checkout into a different directory produces a different hash and always
  cache-misses. Same literal-vs-intent tradeoff as `--stdin-paths`: the spec
  demands deterministic content addressing; these flags deliver it.
  `tests/image_cache.rs::tar_path_key_is_mtime_independent` proves the property
  by bumping mtimes to 2030 on one of two byte-identical contexts and asserting
  equal keys.
- **Added `try_capture` to `src/process.rs`** as a sibling of `run_capture` that
  doesn't error on non-zero exit. Two callers in this task want exit code as
  information (git-repo detection via `git rev-parse --git-dir`; the
  `buildah images --quiet` cache probe), and the same shape will return in
  0008+. `run_capture` now delegates to `try_capture` to avoid duplicating
  the spawn path.
- **`ensure_image` joins paths against `repo_root` without `canonicalize`** --
  preserves user-intentional symlinks. `src/config/validate.rs` does the same
  for the existence check.
- **On buildah build failure, `stderr_tail` in `OutrigError::Process` is left
  empty** because `run_streamed` already routed buildah's stderr to
  `tracing::info!` lines prefixed with `[buildah]`. Fabricating a synthetic
  "see logs above" string in `stderr_tail` would leak abstraction; the argv
  and exit code in the error remain useful. The smoke test installs
  `tracing_subscriber::fmt()` so those lines are visible under `--nocapture`.
- **`git ls-files -z .`** (NUL-separated) for filename robustness; the
  newline-separated form breaks on filenames containing `\n`.
- **No doc TODO markers dropped this task.** The image-cache section
  `doc/concepts/containers.md:132-140` has no per-section marker; the
  file-level `> TODO: Incomplete` correctly stays since runtime container
  behavior arrives in 0008+.
