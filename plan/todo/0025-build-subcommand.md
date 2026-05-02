# 0025 -- `outrig build`

## Goal

Pre-warm the image cache outside of `outrig run`. Useful for verifying a Dockerfile, in CI
that wants to fail fast on broken setups, and for keeping `outrig run` snappy.

## Deliverables

- `src/cli/build.rs::execute(args: BuildArgs) -> Result<i32>`:
  - Load + merge + validate config (0005).
  - Determine target container-configs: `--container-config <name>` (single) or `--all`
    (every `[containers.<n>]`); the two are mutually exclusive (clap-derive `ArgGroup`).
  - For each target: call `image::ensure_image(...)` (0007). With `--no-cache`, pass
    `--no-cache` through to buildah and skip the cache-tag check.
  - Stream buildah stderr via `[buildah]` tracing prefix.
  - Print a clean summary on success (matching `doc/usage/build.md` examples).
  - On first failure under `--all`: exit non-zero, print the buildah stderr tail.
- Replace the "not implemented" stub from 0001 with this real implementation.
- `tests/build_cli.rs` (`#[cfg(feature = "e2e")]`):
  - Build a fixture container-config; assert image exists in `buildah images`.
  - Re-run; assert cache hit (very fast, `[buildah]` doesn't appear).
  - With two container-configs and `--all`: both build.
  - `--no-cache` rebuilds even on cache match (verify by timestamp).

## Acceptance

- `cargo test --features e2e build_cli` passes.
- `outrig build` prints `image ready` on success; second run prints `image ready (cache hit)`.
- `outrig build --all` builds every container-config; first failure short-circuits.
- Drop the `> TODO: Incomplete` marker on `doc/usage/build.md`.

## Dependencies

- 0007-image-build

## Notes

- Output format must match `doc/usage/build.md` exactly (`[outrig] container-config: ...`,
  etc.).
- `--no-cache` interaction with the cache key: don't change the key (so subsequent normal
  builds still hit), just skip the existence check and pass `--no-cache` to buildah.
