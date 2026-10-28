# 0003-40 -- The container facade stays beside the harness

## Context

The crate has two supported tiers (`crates/outrig/src/lib.rs`, "Supported surface"): the facade,
`outrig::Outrig`, and `harness`, the session built on it. The facade is what an embedder uses to
host a container and its stdio MCP servers while running an agent loop of its own:
`Outrig::launch(&LaunchSpec)`, `tools()`, `call_tool(server, tool, args)`, `exec_capture()` and
`shutdown()` (`crates/outrig/public-api.txt:2232-2237`); `image::{ensure_image, compute_tag,
compute_tag_for, probe_pulled, probe_cached, read_image_env}` (`:1911-1922`); `LaunchSpec` with
`with_mount`, `with_mounts`, `with_sidecar` and `with_embedded_mcp_policy` (`:2082-2108`);
`SidecarSpec`, `MountSpec`, `EmbeddedMcpPolicy`, `McpToolResult`, `ToolHandle` and
`CapabilityProfile` (`:1968-2265`); and `OutrigError::{Io, McpStartupFailed, Process}`
(`:1037-1078`). `lib.rs` says both tiers are "a SemVer commitment -- `harness` from the 0.3.0
release, before which it may still change", and `0003-19`'s Decisions record that the facade now
means "container" and sits in the supported tier beside `harness`. `scripts/check-public-api.py`
and the `public-api` CI job (`0002-48`) hold the snapshot, so any change to an item shows in
review.

The first application to embed the harness (A-08, `A-08-container-facade-stays-for-both-engines.md`
in the CocoClaw phase-0008 requirements) runs two engines in one binary for a while: its existing
loop over the facade, pinned today at crates.io `outrig = 0.2.1` with `default-features = false`,
and the new engine over `harness`, both from one dependency on `version/0.3.x`. It asks three
things of the branch (OR-A8-1 through OR-A8-3): that the items above keep their signatures and
behavior until it has removed its old engine, with a changelog migration note before any change
and a deprecation attribute at least one tagged release before any removal; that a facade-launched
container and a `harness::Session` coexist in one process without interfering -- session ids, the
image cache, podman invocations, the events directory and shutdown ordering of one never touching
the other; and that `image::ensure_image` and `compute_tag` give one image to both paths.

Two facts the requirement does not have. `outrig::process`, which A-08 lists among the items the
application uses, is a private module on this branch (`lib.rs`: `mod process;`); only
`OutrigError::Process` is public. Whether the 0.2.1 pin exposed a public `process` is the
application's to check against its own build; this task adds no re-export. And the crate declares
no `default` feature -- `[features]` holds `e2e` and `seal-tests` only -- so a `default-features =
false` build is the ordinary build, which nothing in CI builds as such;
`plan/next/ci-configuration-coverage.md`'s `cargo hack --each-feature` gap is adjacent.

The stability is policy already. What nothing proves is that the two tiers live together in one
process -- the panic hook's sweep removes containers by the `org.outrig.session=<sid>` label
(`lifecycle.md`, "Descendants, and the owner's abrupt death"), both tiers mint session ids, both
reach the image cache and podman -- or that an image ensured once serves both; and the branch's
changelog says nothing about the facade's standing for an embedder that depends on the branch
rather than on a release. Every task appended with this one adds surface beside the facade, so the
promise is recorded first and held by the gate from then on.

## Goal

An embedder on `version/0.3.x` can run a facade-launched container and a `harness::Session` in one
process, from one ensured image, with the facade's items unchanged and that promise written down.

## Deliverables

- **The promise, written where an embedder reads it.** `crates/outrig/README.md`'s supported surface
  and `crates/outrig/CHANGELOG.md`'s `[Unreleased]` state that the facade items listed in Context
  keep their signatures and behavior on `version/0.3.x` through the 0.3.0 release and on the 0.3.x
  line after it until the first application has removed its old engine (OR-A8-1); that a change to
  one is announced in the changelog with a migration note before it lands; and that a
  `#[deprecated]` attribute precedes a removal by at least one tagged release, which is the notice
  an application pinned to a git revision gets. `lib.rs`'s tier
  paragraph points at it. The snapshot gate is the enforcement; the statement is what it enforces
  for.
- **A coexistence test**, behind the `e2e` feature: one process launches a container through
  `Outrig::launch` and starts a `harness::Session` through `SessionBuilder`, makes one tool call on
  the first and runs one round on the second against a mock model, shuts the Session down and then
  the facade, and asserts that each report is clean, that the two session ids differ, that the
  Session's events directory holds only its own session, and that stopping either container leaves
  the other running until its own shutdown.
- **An image test**, behind `e2e`: `ensure_image` once for one `ImageConfig`, then
  `Outrig::launch` and `SessionBuilder::container(LaunchSpec::from_config(..))` from the same tag;
  `probe_cached` and `probe_pulled` show one image and no second build or pull, and both paths
  report the same `ImageTag`.
- **A `--no-default-features` build in CI**, one step on the existing matrix, so the application's
  build shape is the one CI builds (fork 2).
- **The `outrig::process` note** in the changelog entry: private on this branch; the application
  confirms its pin. No re-export (fork 3).
- `crates/outrig/public-api.txt` unchanged by this task, which the gate asserts.

## Acceptance

- `python3 scripts/check-public-api.py` passes with no diff: the task adds and removes no public
  item, and the items Context lists are byte-identical to `8fb6eac`'s snapshot.
- The coexistence test passes under `--features outrig/e2e` against a live podman: both reports
  clean, ids distinct, each container stopped only by its own shutdown.
- The image test passes under `e2e`: one build or pull, one cached image, one tag for both paths.
- `cargo build -p outrig --no-default-features` passes in CI.
- `cargo rustdoc -p outrig --lib -- -D warnings` passes with the tier paragraph's new pointer;
  `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the promise lives -- Recommended: `README.md` and `CHANGELOG.md`**, which the crate's
   front page includes and the release process reads. The alternative, a `doc/` page, documents
   the CLI, not the library an embedder depends on.
2. **The `--no-default-features` build -- Recommended: one CI step now.** The alternative waits for
   `plan/next/ci-configuration-coverage.md`'s `cargo hack` job, which would cover it and more; one
   step costs a line and lands with this task.
3. **Re-exporting `process` -- Recommended: no.** The requirement lists it among the items the
   application uses, and the branch has no public `process`; the gate shows that `8fb6eac` already
   lacks it, so the application's claim is checked against its pin rather than answered with a new
   public module.

## Dependencies

- **Hard: `0003-19`.** `harness::Session` and the tier paragraph that names the facade "container".
- **Hard: `0002-48`.** The snapshot gate the acceptance relies on.

## See also

- `crates/outrig/src/lib.rs` -- the two tiers.
- `crates/outrig/public-api.txt` and `scripts/check-public-api.py` -- the gate.
- `plan/done/phase/0003-python/tasks/0003-19-an-embedder-owns-the-session.md` -- the facade means
  container.
- `plan/phase/0003-python/lifecycle.md` -- "Descendants, and the owner's abrupt death", the sweep.
- `plan/next/ci-configuration-coverage.md` -- the feature-matrix gap this task's CI step overlaps.
