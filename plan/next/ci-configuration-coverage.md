# Close the remaining "declared but never compiled" gaps in CI

## Context

0092 existed because a whole feature-gated test suite had rotted unnoticed: `e2e` was declared
on both crates, and no CI job ever compiled it. 0092 fixed that one instance by adding a matrix
row. It did not fix the *class* -- there is still no mechanism that notices a declared
configuration nobody builds. Several more are uncovered today, found while reviewing 0092.

## The generalizing fix

A `cargo hack --each-feature --exclude-features cuda,metal` job fails automatically when a new
feature is declared and left uncovered, instead of waiting for someone to notice. Note
`--all-features` is **not** usable: it enables `cuda`, which needs `nvcc`.

## Remaining uncovered configurations

**macOS / `metal` is compiled by nothing.** Every job is `runs-on: ubuntu-latest`, and the only
workflows are `ci.yml` and `docs.yml`. `crates/outrig-cli/Cargo.toml`'s macOS-only dependency
block hardcodes `candle-core = "=0.10.2"` and `mistralrs-core = "=0.8.1"` rather than inheriting
from `[workspace.dependencies]`, which carries the same pins today -- nothing detects drift when
the workspace pins move. Meanwhile `doc/concepts/in-process-llm.md` advertises
`cargo build --features "local-llm metal"` as supported. A `macos-latest` x `local-llm,metal`
check job covers both. Highest-value gap of the set.

**`cuda` and `metal` features themselves are low-risk.** Both are consumed only via `cfg!()`
(`outrig-cli/src/llm.rs`, `build.rs`), never `#[cfg]`, so neither gates an item at compile time.
On Linux `metal = []` pulls no deps, so `--features local-llm,metal` is compile-identical to
`local-llm`. The real exposure is the macOS dependency block above, not these flags.

**`local-llm` + `e2e` together is compiled by neither row.** No test file is gated on both, so
no target is orphaned right now, but `--features local-llm` is the configuration users actually
install, and its e2e binaries would link against a lib built with a different feature set than
either row produces. Cheap to close via cargo-hack.

**`rust-version = "1.87"` is never verified.** All jobs use `dtolnay/rust-toolchain@stable`, so a
dependency bump or newly stabilized API can silently raise the true MSRV above the declared one.

**No `cargo publish --dry-run` / `cargo package` job.** `crates/outrig/build.rs` compiles
`src/container/enter/launcher.rs` at build time, so the published crate must ship that file.
There is no `include`/`exclude` in the manifest, so it does today -- but nothing enforces it, and
the project's constraint is that `cargo install` builds every feature from source with no
prebuilt binaries. Worth having before the 0.2.0 freeze.

## Efficiency items in the existing `cargo` job

**The `e2e` cache bucket duplicates `default` byte for byte.** `Swatinem/rust-cache`'s `key` is a
literal key component, so each matrix row gets its own bucket -- but both `e2e` features are
`e2e = []`, and `cargo tree --workspace --edges all` yields 1698 identical nodes with and without
them. So the e2e row cold-builds the same ~222 dependency crates the `default` row is building
concurrently, then stores a second multi-hundred-MB copy against the repo's 10 GB Actions budget,
adding eviction pressure on the other buckets. Fix: a `cache_key` matrix field where `default`
and `e2e` share `"default"`, plus `save-if: ${{ matrix.name != 'e2e' }}` so the two concurrent
rows don't race to save the same key. `local-llm` genuinely needs its own bucket (3906 nodes).

**`mozilla-actions/sccache-action` is dead weight, now paid three times.** Nothing sets
`RUSTC_WRAPPER=sccache` or `SCCACHE_GHA_ENABLED=true` anywhere in `.github/`, and there is no root
`.cargo/config.toml`; the action does not set them itself. Each row downloads sccache and runs a
"Post Run sccache-cache" step that caches nothing -- roughly 5-15 s of pure waste per job, and the
new e2e row makes it a third payer. `@v0.0.3` also predates upstream's note that sccache before
v0.10.0 "probably will not work." Either wire it up (and then reconsider its overlap with
`rust-cache`) or drop the step.

## Non-issue, recorded so it is not re-litigated

The e2e row's `clippy --all-targets` followed by `cargo test --no-run` is not a wasteful double
build. Measured on a 20-core box against a pruned workspace: clippy 5.1 s, the `--no-run` build
18.6 s after it versus 22.3 s standalone. Clippy and rustc share no artifacts, but the clippy
pass is nearly free and partly warms the build that follows. Clippy is the load-bearing gate;
`--no-run` adds codegen and link coverage on top. Keep both.

`cargo test --no-run` also does not build doctests, so doc examples on `#[cfg(feature = "e2e")]`
items would go unchecked. There are none today.
