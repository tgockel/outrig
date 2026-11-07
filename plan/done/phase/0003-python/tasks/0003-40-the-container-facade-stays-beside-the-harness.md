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

## Decisions

- **The facade's items are as 0.2.1 published them, not only as `8fb6eac` had them.** The 71
  snapshot lines for the items Context lists are identical in `outrig-v0.2.1`'s `public-api.txt`
  and this branch's, so an application moving its pin from crates.io 0.2.1 to the branch meets no
  change in them. The changelog says "as 0.2.1 published them" on that evidence.
- **Fork 3 -- no re-export, and the note is firmer than "the application checks".**
  `outrig::process` was private in 0.2.1 too (`mod process;`); what it defines that 0.2.1 made
  public is `outrig::Transcript`, re-exported at the root, and that and `OutrigError::Process` are
  unchanged.
- **Context's premise about the sweep was wrong, and the corrected fact is the stronger one.** No
  removal goes by `org.outrig.session`. `Container::stop`, `Drop`'s detached removal and the
  panic sweep each remove by the container's `org.outrig.attempt` token, and the sweep runs from
  `with_panic_sweep` (or the hook only in a `panic = "abort"` build) since #349. So one tier's
  stop cannot reach the other's container by construction. `lifecycle.md`'s "a sweep from the
  panic hook" is corrected to say so.
- **The coexistence tests are in-crate, in `agent_tests.rs`'s `e2e` module.** A round against a
  mock model needs `agent::mock_http` and the round helpers, which are `#[cfg(test)]` and
  crate-private; every Session-round e2e test already lives there. The facade is driven only
  through its public calls, and `Outrig::primary()` (crate-private) is read for one thing: the
  container name, hence the session id, the facade minted -- `Outrig` exposes neither, and naming
  the id with `with_session_id` would have made "the ids differ" true by construction.
- **Both stop orders, one test each.** "Stopping either container leaves the other running" needs
  each to stop first once. After the first stop the survivor is asked to work rather than checked
  as running: the facade answers a second `call_tool` (its MCP server's `podman exec` client
  survived the Session's shutdown, which kills process groups), and the Session runs a second
  round (its interpreter's client survived the facade's). A working survivor implies a running
  container, so no separate `is_running` check. With removal mutated to filter on the session
  label key alone, both tests fail at that step. The two tests take a lock across the fixture's
  launch, so a cold image store builds `mcp-fs` once rather than twice in parallel -- the
  `outrig` unit binary runs before the integration binaries that otherwise build it first.
- **The image test pins the ensured tag through `harness::container_spec`.** `ensure_image` tags
  `outrig-cache:<K>`; `LaunchSpec::from_config` over a `dockerfile` block builds `<block>:<K>`
  (#456), a second image. `container_spec(.., Some(&tag), ..)` is the library's own way to give a
  session an ensured tag -- `run-new` and host-program sessions use it -- and it is
  `from_config` over the pinned tag, which only asks `podman image exists`. The README says so,
  and the test asserts `probe_cached(<block>:<K>)` is false; giving it `None` instead fails there.
- **"One image" is checked by image ID, not by name.** The tag's ID is read once ensured and
  again after both start, and each container's `.Image` must equal it, so a rebuild under the
  same tag or a different image under either path fails. `ContainerInspect::image_tag` is
  compared between the two containers only, not to the ensured string: podman may report a
  locally built tag as `localhost/...`, and CI's 4.9.3 and a developer's 5.x need not agree.
- **The CI step is `cargo check`, not `cargo build`, on the default row only.** It compiles the
  application's graph -- `outrig` alone, its dependencies' features unified with nothing of the
  CLI's -- and runs `build.rs`; codegen would build the whole dependency graph again for no
  question this asks. That graph differs from the workspace check's by 51 units, which the
  default row's `cargo rustdoc -p outrig --lib` builds anyway and the arm64 row would build for
  this step alone; the shape is a question of features, not of the architecture. The existing
  `cargo check -p outrig --target x86_64-unknown-linux-musl` already builds `outrig` alone, and
  could have carried the flag; a step of its own keeps the gnu target an application builds and
  keeps the musl step about the libc.
- **Follow-up:** `plan/next/session-ids-collide-within-a-second.md`. Both tiers mint ids with two
  random bytes per second, so two starts in one second clash on the container name one time in
  65,536; one process now mints several.
