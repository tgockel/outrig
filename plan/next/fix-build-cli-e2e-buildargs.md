# Fix stale `BuildArgs` field in `build_cli` e2e test

`crates/outrig-cli/tests/build_cli.rs` (gated `#![cfg(feature = "e2e")]`) constructs
`BuildArgs { container: None, all, no_cache }`, but commit `a11c4fc` ("rename
container-image config from 'container' to 'image'") renamed that field to `image`.
The test was not updated because CI only builds non-e2e targets
(`cargo clippy --all-targets` and `cargo test`, no `--features e2e`), so the breakage
is invisible to the default pipeline.

## Symptom

`cargo build --features e2e --tests` (or `cargo clippy -p outrig-cli --features e2e
--all-targets`) fails to compile the `build_cli` test target with five
`E0560: struct BuildArgs has no field named container` errors (lines ~77, 123, 158,
190, 202). This blocks compiling the *entire* `outrig-cli` e2e test suite as a unit,
though individual e2e tests still build via `--test <name>`.

## Fix

Rename `container:` to `image:` in the five `BuildArgs { .. }` literals in
`build_cli.rs`. Then confirm the whole e2e suite compiles:
`cargo build --features e2e --tests`. Consider adding a lightweight CI step that at
least *compiles* the e2e targets (`cargo build --features e2e --tests`) so this class
of rot is caught without needing podman/buildah at test time.

## Why deferred

Discovered while implementing `0071-standalone-image-build` (which adds the new
`image_build` e2e test). It is in a different command's test file (`outrig build`,
not `outrig image build`) and unrelated to that task's scope, so it was filed here to
keep the 0071 commit focused.
