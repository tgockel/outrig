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
