# 0093 -- Shrink the reachable public surface before 0.2.0 freezes it

## Context

The workspace ships two crates at version `0.2.0-rc.1` -- `crates/outrig` (library) and
`crates/outrig-cli` (binary plus `lib.rs`) -- and neither sets `publish = false`. Once `0.2.0`
lands, every type a downstream `outrig = "0.2"` consumer can *name* becomes a SemVer commitment.
This task settles which types those are, before the answer is decided by accident.

"Reachable" means a type a downstream crate can name, construct, destructure, or `match` on. The
source of truth is `crates/outrig/src/lib.rs` -- the curated `pub mod` / `pub use` set -- and
`crates/outrig/tests/library_surface.rs`, the e2e test that exercises the *intended* facade
(`Outrig`, `LaunchSpec`, `WorkspaceSpec`, `SidecarSpec`, and the `config` / `error` / `mcp_proxy`
modules).

### The two visibility tiers on the `outrig` crate

`lib.rs:5-16` declares some modules `pub mod` and others private (`mod mcp;`, `mod process;`,
`mod repo;`, `mod nsfork;`, `mod outrig_;`, `mod tool_name;`):

- **`config`** -- `pub mod`. Reachable: the whole module tree.
- **`container`** -- `pub mod`. Reachable, including its `pub mod embedded`, `pub mod enter`, and
  `pub mod sidecar` submodules.
- **`error`**, **`image`**, **`mcp_proxy`**, **`network`** -- `pub mod`. Reachable.
- **`mcp`** -- private `mod`. Not reachable, except its re-exports: `McpClient`, `McpTool`,
  `McpToolResult`, `resolve_mcp_env`.
- **`outrig_`** -- private `mod`. Not reachable, except its re-exports: `Outrig`, `LaunchSpec`,
  `SidecarSpec`, the spec types, and `ToolHandle`.
- **`process`**, **`repo`**, **`nsfork`**, **`tool_name`** -- private `mod`. Not reachable, except
  `pub use process::Transcript` and `pub use tool_name::{RESERVED_SERVER, sanitize}`.

**Consequence 1:** a `pub` field inside a *private* module is not a hazard unless the type is
re-exported. `LaunchSource` (in `outrig_.rs`) is `pub(crate)` and never re-exported, so it is not
a hazard. But `Outrig`, `LaunchSpec`, `SidecarSpec`, `WorkspaceSpec`, `MountSpec`, `SecuritySpec`,
`CapabilitySpec`, `NetworkSpec`, `SidecarServerSpec`, `ToolHandle`, and `EmbeddedMcpPolicy` *are*
re-exported, and their public fields **are** hazards. Those are 0094's problem.

**Consequence 2 -- the finding this task exists for:** because `container`, `config`, `image`,
`network`, and `mcp_proxy` are `pub mod` rather than curated re-export lists, *every*
`pub struct` / `pub enum` / `pub fn` inside them is reachable. That is a far larger surface than
the tidy `lib.rs` re-export block suggests. The `container` module alone leaks `Container`,
`ContainerLaunchSpec`, `ContainerWorkspace`, `ContainerMount`, `ContainerCapabilities`,
`PrimaryView`, `ContainerInspect`, plus `sidecar::SessionMcpPlan` / `PlacedServer` / `SidecarPlan`
and `embedded::*` -- all with public fields, none protected.

### The CLI crate

`crates/outrig-cli/src/lib.rs` marks all 15 of its modules `pub mod`, while its own module doc
says the opposite:

> Internals of the `outrig` CLI binary, exposed as a library so the integration tests in `tests/`
> can reach them. End users should depend on the `outrig` crate (the library) instead.

So `CliError`, `LlmResolveError`, `ResolvedProvider`, `ResolvedAgent`, `MistralrsWeights`,
`RigAgent`, and `resolve_agent_with_overrides` are *technically* reachable if the crate is
published, but are not a supported public API. Every one of them is an active growth point:
`ResolvedAgent` has ~13 `pub` fields and gains one per feature, `LlmResolveError` gains variants,
`resolve_agent_with_overrides` already grew from 2 positional params to 4.

### Why this task is first

A grep for `#[non_exhaustive]` across all non-test source in both crates returns **zero** matches.
Every public struct and enum on the reachable surface is exhaustive today, so additive changes to
any of them are breaking. The `0.2.0-rc.1 -> Unreleased` CHANGELOG already lists four breaks of
exactly this class (`McpServerSpec::Full` gaining `args`, `SidecarConfig` gaining `args`,
`Container::create_initialized` gaining a trailing param, `ConfigValidationError` variants losing
fields) -- direct evidence that this surface grows along precisely the axes the insulation
patterns would protect.

The obvious response is to annotate everything. That is 0094 and 0095. This task comes first
because **reducing reach strictly dominates annotating**: a type nobody can name has no SemVer
hazard at all, needs no attribute, and costs nothing to change later. Settling reach first deletes
roughly a dozen rows from 0094's table and can make 0095's `create_initialized` options struct
evaporate entirely. Annotating a type that is then de-published is wasted work.

This also finishes a direction already on record -- see
`plan/done/0064-audit-library-struct-visibility.md` and
`plan/done/0065-tighten-library-module-visibility.md`.

## Goal

Decide and enforce what `outrig = "0.2"` actually promises, so the remaining hardening work
applies only to types that are deliberately public.

## Deliverables

- **A decision on `container`, `mcp_proxy`, `network`, and `image`** in
  `crates/outrig/src/lib.rs:5-15`. Either demote them to `pub(crate) mod`, or keep the module
  public with `pub(crate)` internals and re-export only the handful of types the facade genuinely
  needs. If a downstream crate is *supposed* to drive `Container` directly, keep it -- but then
  the spec types and `create_initialized` must get 0095's treatment, and the task should say so
  explicitly rather than leaving it implied.
- **A boundary on `crates/outrig-cli/src/lib.rs`.** Gate the test-only surface behind
  `#[doc(hidden)]` or `#[cfg(any(test, feature = "internal-test-api"))]`, so the seven CLI types
  named above are not a public commitment. The module doc already states the intent; this makes
  the compiler agree with it.
- Whatever `pub use` additions the facade needs so `library_surface.rs` keeps working through the
  intended entry points rather than through leaked module paths.
- A note in the changelog for any reachability that is removed, since removing a public path is
  itself a breaking change and must land inside `0.2.0`.

## Acceptance

- `crates/outrig/tests/library_surface.rs` compiles and passes unchanged, or with changes that
  are *only* import-path adjustments. It defines the facade that must survive; if honoring it
  requires keeping a module public, that is the answer.
- The types listed under *Consequence 2* are either no longer nameable from a downstream crate, or
  are explicitly recorded as intended public API with a line saying why.
- The seven `outrig-cli` types named above are not reachable from a plain
  `outrig-cli = "0.2"` dependency.
- `crates/outrig-cli/tests/` and `crates/outrig/tests/` still build -- the integration tests are
  separate crates, so anything they reach has to stay reachable to them by whatever mechanism the
  boundary uses.
- `cargo public-api` (or an equivalent surface diff) shows the intended surface and nothing more.
  Capture the output; 0094 works from it.

## Design forks

1. **How much of `container` is public -- Open, and this task decides it.** The evidence is
   `library_surface.rs`: whatever it needs is public, whatever it does not need is a candidate for
   demotion. That test does not compile today, which is why 0092 gates this task -- the decision
   should be made against a suite that runs, not against a reading of the source.

2. **De-publish versus `#[doc(hidden)]` for the CLI crate -- Recommended: a cfg feature.**
   `#[doc(hidden)]` hides from docs but leaves the path nameable, so it documents intent without
   enforcing it. A `internal-test-api` feature that the integration tests enable actually removes
   the commitment. The cost is a feature flag that exists only for tests.

3. **`sidecar` and `embedded` submodules -- Recommended: `pub(crate)`.**
   `sidecar::SessionMcpPlan` / `PlacedServer` / `SidecarPlan` / `Placement` are pure planning
   internals and almost certainly not meant to be a contract. The e2e test only needs
   `embedded::LABEL_MCP`, which can be re-exported on its own.

## Dependencies

- **0092.** `crates/outrig/tests/library_surface.rs` is `required-features = ["e2e"]`
  (`crates/outrig/Cargo.toml:20-22`) and the e2e suite does not compile today. That test is both
  the evidence for the reach decision and the acceptance criterion for it, so it has to build
  first.

## See also

- `crates/outrig/src/lib.rs` -- the `pub mod` / `pub use` set this task curates.
- `crates/outrig-cli/src/lib.rs` -- the module doc that already states the intended boundary.
- `plan/done/0064-audit-library-struct-visibility.md`,
  `plan/done/0065-tighten-library-module-visibility.md` -- the same direction, earlier.
- `plan/todo/0094-non-exhaustive-sweep.md` -- annotates whatever survives this task.
- `plan/todo/0095-options-structs-and-sealing.md` -- the signature changes, several of which
  evaporate depending on this task's outcome.

## Decisions

1. **The library's reach was confirmed, not shrunk -- the task's premise inverted under
   evidence.** `/home/travis/projects/open-source/cococlaw` is a live downstream consumer on
   `outrig = "0.1.0"`. It uses the facade (`Outrig`, `LaunchSpec`, `MountSpec`, `SecuritySpec`,
   `ToolHandle`, `EmbeddedMcpPolicy`, `MountAccess`), five `config` types, `error::OutrigError`,
   and -- decisively -- `image::{ImageTag, compute_tag, probe_cached, probe_pulled}`. It also
   intends to adopt `container` and likely `mcp_proxy`. Demoting those modules would have broken
   a real consumer, so **all six `pub mod`s are recorded as intended public API.** This restates
   `plan/done/0064` Decision 1 ("outrig is the runtime-core crate") with a consumer behind it
   rather than an intention.

2. **`container::{sidecar, embedded, enter}` stay public, reversing design fork 3.** The fork
   guessed they were "pure planning internals, almost certainly not meant to be a contract." They
   are the contract: a caller driving `Container` directly needs `SessionMcpPlan`, `SidecarPlan`,
   `PlacedServer`, and `Placement` to say anything about sidecars. `embedded::LABEL_MCP` did not
   need a root re-export -- its own path is legitimate now -- so `library_surface.rs` compiles
   **unchanged**, not merely with adjusted imports.

3. **`network` joins them.** It had no downstream consumer, making it the one place a gate would
   have cost nothing today. It stays public anyway: splitting egress policy out of the
   runtime-core set to save one type would fragment the story, and a caller driving containers
   directly is the same caller who eventually wants policy over them.

4. **Consequence for 0095, stated rather than implied** (the task asked for this explicitly):
   because `Container` is deliberately public, `ContainerLaunchSpec`, `ContainerWorkspace`,
   `ContainerMount`, `ContainerCapabilities`, `PrimaryView`, and `Container::create_initialized`
   all need the options-struct and sealing treatment. `image::ImageTag`'s public tuple field is
   load-bearing downstream (`ImageTag(name)`), so making it opaque requires shipping a
   constructor in the same change.

5. **This task grows 0094/0095's scope instead of shrinking it.** The task expected to delete
   roughly a dozen rows from 0094's table. It deletes five. Recorded so 0094 does not open
   expecting a smaller job.

6. **Only five paths were removed from the library:**
   `embedded::{mcp_config_to_labels, merged_mcp_config_to_labels, primary_scoped_mcp, merge_mcp}`
   and `sidecar::bootstrap_needed`. The criterion was mechanical -- no consumer outside
   `crates/outrig/src`, and not named in any public signature -- rather than a judgment about
   what *ought* to be public, and the demotions were checked against cococlaw as well as this
   workspace.

   What fell out are the intermediate stages of the label pipeline, which take half-resolved
   internal shapes as arguments. Its entry points stayed: `parse_standalone_image_toml`,
   `standalone_config_to_labels`, and `parse_standalone_image_labels` for authoring and reading a
   standalone image, and `merged_mcp` for reading one back off a live container. A first draft of
   this note claimed a tidier rule -- "what an image author needs stays, the crate's own merge
   machinery goes" -- but that does not survive contact with the list, since `merge_mcp` went
   private while `merged_mcp`, the same merge one layer up, stayed. The mechanical criterion is
   the honest description.

   `merged_mcp` is the one item whose only current caller is an integration test. It stays public
   anyway: it takes a live `&Container`, so it cannot be pushed down into a unit test the way a
   pure function could, and it is the read-side counterpart of the label writers that are already
   public. It gained the doc comment it was missing, since 0.2.0 freezes it.

7. **The CLI crate is where the real shrinkage landed** -- and it is total. `outrig-cli`'s
   published surface went from 14 modules to `run() -> ExitCode`. The chosen mechanism is a
   cfg feature (design fork 2's recommendation) rather than `#[doc(hidden)]`, because
   `doc(hidden)` leaves the path nameable and so documents the boundary without enforcing it.
   Integration tests reach the internals through a **dev-dependency on self**
   (`outrig-cli = { path = ".", features = ["internal-test-api"] }`); this was spiked before
   being committed to, and all 20 test files compile through it unchanged.

   This is a boundary, not a cleanup: roughly 14 of the 24 files in `tests/` are unit tests
   wearing an integration test's clothes, and they are the whole reason the module tree was ever
   public. Moving them inline would let the feature and the self-dependency disappear entirely,
   but that is a ~4k-line test move with no bearing on what `0.2.0` freezes. Filed as
   `plan/next/relocate-unit-shaped-tests.md` rather than recorded here as settled.

8. **`src/main.rs` needed a new entry point.** A bin target is a separate crate, so it could no
   longer reach `pub(crate) mod cli`. The lib grew `pub fn run() -> ExitCode` as its single
   promised path, which doubles as the thing the CHANGELOG can point users at.

9. **The first `macro_rules!` in the repo.** `internal_modules!` expands 14 module names into
   cfg'd visibility pairs. Spelling both arms out is 42 lines that bury the only interesting
   content -- the module list. Noted because it sets a precedent the repo did not previously
   have.

10. **Dead-code suppression is per-item, not crate-wide.** Closing the gate makes every
    test-facing item unused by construction, so a plain `cargo install outrig-cli` printed 12
    warnings. The first attempt was one crate-level
    `#![cfg_attr(not(feature = "internal-test-api"), allow(dead_code, unused_imports))]`,
    justified by the claim that CI's `clippy --all-targets` turns the feature on and so keeps the
    lints live. **That claim was false** -- measured, not assumed: with the feature on every
    module is `pub`, which makes `dead_code` vacuous for anything inside them, so the blanket
    allow switched dead-code detection off in *every* configuration. It also immediately hid a
    real finding (see 11). Replaced with a `cfg_attr` on each of the ten items that genuinely
    exist for tests, which keeps the lint live across the rest of the crate.

11. **Two genuinely dead fields fell out of the visibility change.** `SessionSetup.image_cfg`
    (a cloned `ImageConfig`) and `SessionSetup.session_dir` were written and never read -- all
    three destructuring sites discard them with `field: _`. Making the module tree crate-private
    is what exposed them to `dead_code` for the first time. Removed rather than annotated; this
    is the concrete payoff of the boundary, and it would have stayed invisible under the blanket
    allow.

12. **Four modules never needed the gate.** `builtin_tool`, `mcp_self`, `session_tool`, and
    `subagent` are named by no integration test, so they are unconditionally `pub(crate)`
    alongside `paths`. The macro's list now means exactly "the modules some test reaches."

13. **CI gained a `cargo check` step, because this task created a configuration it never
    built.** Every existing step is `clippy --all-targets` or `cargo test`, and both pull
    dev-dependencies, which turn `internal-test-api` on. So the shape `cargo install outrig-cli`
    produces -- lib and bin, feature off -- was compiled nowhere in CI once the feature existed.
    The snapshots are also `exclude`d from both packages: 74 KB of generated review material is
    no use to a consumer.

    `outrig-cli` excludes `tests/` as well, and that one is a correctness fix rather than a
    diet. Cargo strips a path-only dev-dependency when publishing, so the self-dependency that
    turns `internal-test-api` on does not survive into the `.crate` -- while `cargo package`'s
    verify step builds only lib and bins, so publishing would still succeed. The result would be
    a published crate carrying 19 test files that fail to compile for anyone who ran them
    (`cargo vendor` users, distro packagers, auditors). Excluding the directory is the one-word
    fix; adding a `version` to the self-dependency instead would make the crate depend on itself
    from the registry.

14. **Surface snapshots are committed, not scripted.** `cargo-public-api` 0.52.0 replaced the
    hand-rolled rustdoc-JSON walker used during exploration. `crates/*/public-api.txt` hold the
    output with the pinned tool version in a header comment; the format shifts between releases,
    so a diff with no source change means the tool moved. No CI job -- drift detection was not
    asked for, and gating CI on an unstable rustdoc JSON format would break on nightly churn
    rather than on real changes.
