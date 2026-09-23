# Close the remaining "declared but never compiled" gaps in CI

> **Partly queued.** The live-e2e and AArch64 coverage **landed** -- 0002-53 added a
> `live-e2e` job with an `ubuntu-24.04` row and an `ubuntu-24.04-arm` row, and deleted the
> compile-only `e2e` matrix row this entry's efficiency items were written about; see the two
> notes below. The `cargo publish --dry-run` item has **landed** too -- 0002-52 added a
> `package` job; see the entry for it. What stays here: the generalizing `cargo hack
> --each-feature` job, the MSRV check, and the sccache cleanup. The `macos-latest` x
> `local-llm,metal` item, the `cuda`/`metal` note, and the `local-llm` + `e2e` gap all
> **evaporated** when the 0.3 line removed the in-process backend: the three features, the macOS
> dependency block, and the ML dependency pins are gone.

## Context

0002-15 existed because a whole feature-gated test suite had rotted unnoticed: `e2e` was declared
on both crates, and no CI job ever compiled it. 0002-15 fixed that one instance by adding a matrix
row. It did not fix the *class* -- there is still no mechanism that notices a declared
configuration nobody builds. Several more are uncovered today, found while reviewing 0002-15.

## The generalizing fix

A `cargo hack --each-feature` job fails automatically when a new feature is declared and left
uncovered, instead of waiting for someone to notice. `--all-features` is usable as well now:
the `cuda` feature that needed `nvcc` went with the in-process backend.

## Remaining uncovered configurations

**`rust-version = "1.87"` is never verified.** All jobs use `dtolnay/rust-toolchain@stable`, so a
dependency bump or newly stabilized API can silently raise the true MSRV above the declared one.

**~~No `cargo publish --dry-run` / `cargo package` job.~~ Landed in 0002-52** as a `package`
job running `cargo package --locked -p outrig -p outrig-cli` with `OUTRIG_REQUIRE_ENTER=1`.

One correction to what this entry claimed: "there is no `include`/`exclude` in the manifest" had
gone stale before it was read. Both crates carry `exclude` lists now, so a dropped build input is
an ordinary edit away rather than hypothetical, which is what made the gap worth closing.

The variable is the load-bearing half and was not obvious. Without it `build.rs` degrades a
launcher it cannot compile to a cargo warning and an empty artifact, so the verify build that
`cargo package` already performs goes green on exactly the archive this job exists to reject.
With it, a missing `src/container/enter/launcher.rs` fails the build of the packaged tarball --
which is stronger than checking the file list, because it tests what the file is for.

0002-52 first built a 500-line Python checker around this and removed it again; that is recorded
in its `## Decisions`, along with what `cargo publish` already covers, so the larger version does
not get re-proposed.

## Efficiency items in the existing `cargo` job

**Moved rather than resolved, and 0002-53 made it one bucket worse.** That task deleted the
`e2e` matrix row this described, and added `live-e2e-x86-64` and `live-e2e-aarch64`: four
buckets became five. Its own record first claimed the two new ones "are genuinely distinct
because they are different architectures", which is true of them relative to *each other* and
not the comparison that matters. `live-e2e (aarch64)` runs on `ubuntu-24.04-arm` -- the same
runner label as the `cargo (arm64)` row -- with the same toolchain and targets, and `e2e = []`
adds no dependency nodes, so it cold-builds the same ~222 crates that row is building
concurrently and stores a second copy. The duplication is exact and unconditional. (The x86-64
pair is `ubuntu-24.04` against `ubuntu-latest`: the same image today, legitimately divergent
once `ubuntu-latest` moves to 26.04.) This entry's fix applies verbatim to the new pair.

The measurement that started it stands: **the `e2e` cache bucket duplicated `default` byte for
byte.**
`Swatinem/rust-cache`'s `key` is a
literal key component, so each matrix row gets its own bucket -- but both `e2e` features are
`e2e = []`, and `cargo tree --workspace --edges all` yields 1698 identical nodes with and without
them. So the e2e row cold-builds the same ~222 dependency crates the `default` row is building
concurrently, then stores a second multi-hundred-MB copy against the repo's 10 GB Actions budget,
adding eviction pressure on the other buckets. Fix: a `cache_key` matrix field where `default`
and `e2e` share `"default"`, plus `save-if: ${{ matrix.name != 'e2e' }}` so the two concurrent
rows don't race to save the same key.

**`mozilla-actions/sccache-action` is dead weight, now paid three times.** Nothing sets
`RUSTC_WRAPPER=sccache` or `SCCACHE_GHA_ENABLED=true` anywhere in `.github/`, and there is no root
`.cargo/config.toml`; the action does not set them itself. Each row downloads sccache and runs a
"Post Run sccache-cache" step that caches nothing -- roughly 5-15 s of pure waste per job.
(0002-53 removed the e2e row that had made it a third payer, and did not add the step to
`live-e2e`.) `@v0.0.3` also predates upstream's note that sccache before
v0.10.0 "probably will not work." Either wire it up (and then reconsider its overlap with
`rust-cache`) or drop the step.

## Non-issue, recorded so it is not re-litigated

`clippy --all-targets` followed by a `cargo test` of the same feature set is not a wasteful
double build. Measured on a 20-core box against a pruned workspace: clippy 5.1 s, the `--no-run`
build 18.6 s after it versus 22.3 s standalone. Clippy and rustc share no artifacts, but the
clippy pass is nearly free and partly warms the build that follows. This is why `live-e2e` kept
the clippy step when it inherited the deleted row's job.

`cargo test --no-run` does not build doctests, so doc examples on `#[cfg(feature = "e2e")]`
items would have gone unchecked. Moot now that the suite is run rather than only linked, and
there are none today in any case.
