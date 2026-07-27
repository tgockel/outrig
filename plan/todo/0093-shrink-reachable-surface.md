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
