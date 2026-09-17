# 0032 -- `outrig build`

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

## Decisions

- **Refactored `ensure_image` into three composable phases** (`compute_tag`,
  `probe_cached`, `build_image`) plus a thin orchestrator that still bears the
  `ensure_image` name. The build CLI needs to interleave its verbose header
  (`container-config` / `dockerfile` / `context` / `cache key` lines from
  `doc/usage/build.md`) between the cache probe and the buildah invocation, so
  it has to drive the phases itself rather than treating `ensure_image` as a
  black box. Wrapping with a callback or recomputing the cache key in the CLI
  were both worse: callbacks pollute the signature for the run path that
  doesn't care; recomputation reads the entire context twice. The
  three-helper split makes both call patterns (CLI's interleaved form, run's
  one-shot form) cheap, and the run path's `ensure_image(.., false).tag` call
  remains a one-liner.
- **`ensure_image` returns `ImageBuildOutcome { tag, cache_hit }`** instead of
  bare `ImageTag` so the CLI knows whether to print the cache-hit summary
  line or the verbose header. This is the breaking signature change. All
  five call sites (`src/cli/run.rs` plus four tests) updated in this commit
  -- a one-character `.tag` access in each.
- **Used `--container <name>` (matching `doc/usage/build.md` and `outrig run`),
  not `--container-config <name>`** as written in the task spec's prose. The
  doc fixes the contract -- the spec note "Output format must match
  `doc/usage/build.md` exactly" pins the user-facing surface, and `outrig run`
  already uses `--container`. CLI ergonomic consistency wins.
- **`[outrig]` lines go to stderr** (via `eprintln!`/`eprint!`), matching
  `print_banner` in `src/cli/run.rs:302` and keeping interleaving with
  `[buildah]` tracing output (which also goes to stderr) clean.
- **Removed the `OutrigError::NotImplemented` variant** because the build stub
  was its only consumer. Per the project convention against
  backwards-compat shims, dead code is deleted rather than left behind.
- **Empty Dockerfile is the short-circuit-test trigger.** Buildah's parser
  rejects a `FROM`-less Dockerfile fast, so the `--all` first-failure test
  doesn't pay an unrelated base-image pull just to verify ordering.
- **`--all` builds serially.** Concurrent buildah processes would interleave
  `[buildah]` tracing lines into garbage, and buildah's storage operations
  serialize internally anyway. The summary-per-line output in
  `doc/usage/build.md` reads naturally as a per-step log.
- **Empty `[containers.*]` table under `--all` is a configuration error**
  (`"--all requires at least one [containers.<name>] block"`), not a no-op
  exit. Silent success on a misconfigured tree would just mask the bug.
- **The CLI's `cache_hit` recomputation (via `probe_cached`) duplicates work
  that `ensure_image` would otherwise do internally.** Justified: the CLI
  needs the result *before* deciding what to print, then `build_image` runs
  the rest. Worth the second probe (sub-millisecond `buildah images
  --quiet`).
