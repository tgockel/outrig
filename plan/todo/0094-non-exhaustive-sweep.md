# 0094 -- `#[non_exhaustive]` sweep on what stays public

## Context

A grep for `#[non_exhaustive]` across all non-test source in both crates returns **zero** matches.
Every public struct and enum on the reachable surface is exhaustive today, which means adding a
field to any of them, or a variant to any public enum, is a breaking change. The
`0.2.0-rc.1 -> Unreleased` CHANGELOG already lists four breaks of exactly this class, so this is
not a hypothetical growth axis -- it is the one the project is already on.

0093 answers *which* types stay public. This task insulates them. See 0093's Context for the
reachability tiers and the two visibility consequences; they are not repeated here.

**The row lists below are conditional on 0093's outcome.** If 0093 demotes `container`,
`mcp_proxy`, `network`, or `image`, the corresponding rows disappear rather than needing an
attribute -- that is the point of ordering the two tasks this way. Work from the surface diff
0093 captures, not from these tables alone.

## Goal

Make every type that stays public insulated against the additive change it is most likely to
receive next, without over-applying the attribute to types where it costs more than it buys.

## Deliverables

Priority scoring combines **(R)** reachability from a downstream `outrig = "0.2"` consumer,
**(G)** likelihood the type grows, and **(P)** pain of the eventual break. **P0** = before the
freeze, **P1** = strongly recommended, **P2** = nice-to-have.

### Config types -- serde surface (`crates/outrig/src/config/mod.rs`)

These are `pub` in `pub mod config`, are **both constructed by consumers** (struct-literal --
`library_surface.rs` builds `WorkspaceSpec`, `CapabilitySpec`, and `MountSpec` literals) **and
matched on**. They use serde with `deny_unknown_fields`.

**P0 -- `enum McpServerSpec`** (`config/mod.rs:1046`). Consumers `match` it, and its `Full`
variant just grew `args` -- a documented break. Fix: `#[non_exhaustive]` on the **enum** *and* on
the `Full` **variant**. The changelog proves it grows, and every downstream `match` plus every
`Full { .. }` literal breaks on each new field or variant. Highest-churn config type.

**P0 -- `enum LlmProvider`** (`config/mod.rs:262`). Consumers `match`; an internally-tagged serde
enum with an obvious growth axis. Fix: `#[non_exhaustive]` on the enum and on the `OpenAi`
variant. A new provider variant is a break today, and 0097 adds exactly one.

**P0 -- `struct ImageConfig`** (`config/mod.rs:896`). Public fields; consumers both build and
read it. Fix: `#[non_exhaustive]`. New image knobs are frequent, and 0096 adds a `#[serde(skip)]`
source field.

**P0 -- `struct SidecarConfig`** (`config/mod.rs:921`). Public fields; **just grew `args`**, a
documented break. Fix: `#[non_exhaustive]`. Same proven-growth story as `McpServerSpec`.

**P0 -- `struct ContainerSecurity`** (`config/mod.rs:828`). Public fields; grew `devices` and
`no_new_privileges`. Fix: `#[non_exhaustive]`. Security knobs are a classic append point, and a
hand-written `Default` already exists so internal construction is unaffected.

**P1 -- `struct Config`** (`config/mod.rs:76`). Top-level config with many `pub` fields. Fix:
`#[non_exhaustive]`. Grows every release -- recently `subagent_depth_max`, `tool_result_max`,
`network`. Consumers `load` it rather than construct it, so friction is low.

**P1 -- `struct Model`** (`config/mod.rs:277`). Public fields; grew `device` and
`context_length`. Fix: `#[non_exhaustive]`. Same append pattern.

**P1 -- `struct Agent`** (`config/mod.rs:359`). Public fields; grew `subagents` and
`subagent_depth_max`. Fix: `#[non_exhaustive]`. 0099 adds `subagent_width_max` next.

**P1 -- the small config enums**: `NetworkMode` (430), `NetworkAction` (462),
`CapabilityProfile` (868), `SidecarWorkspaceAccess` (952), `SidecarView` (977),
`SidecarStart` (995), `SidecarOnFailure` (1005), `MountAccess` (422), all in `config/mod.rs`.
Consumers `match` them. Fix: `#[non_exhaustive]`, selectively -- each new mode or profile breaks
a `match`. `CapabilityProfile`, `NetworkMode`, and `SidecarWorkspaceAccess` are likeliest to gain
variants; the `Access` / `Action` pairs are more genuinely closed, so treat those as P2.

**P1 -- `struct NetworkConfig`** (`config/mod.rs:704`). Public fields plus hand-rolled
`PartialEq` / `Default`. Fix: `#[non_exhaustive]`. Filtering policy is an active growth area --
see `plan/next/network-interceptor-mitm.md`.

**P2 -- `NetworkPolicy` (611), `NetworkEntry` (489), `MountConfig` (413), `Workspace` (394)**,
all in `config/mod.rs`. Public fields. Fix: `#[non_exhaustive]`. Lower growth pressure, and
`NetworkPolicy` already has a builder.

**P2 -- `enum ImageSourceRef<'a>`** (`config/mod.rs:1013`). Returned by `ImageConfig::source()`;
consumers `match`. Fix: `#[non_exhaustive]`. Return-only, so the attribute alone fully insulates.

### The curated facade (`crates/outrig/src/outrig_.rs`, re-exported)

Constructed by consumers via builders, **but the fields are also `pub`** -- so the builder does
not actually insulate anything. A literal `LaunchSpec { .. }` compiles downstream today.

**P0 -- `struct LaunchSpec`**. All fields `pub`; it has builder methods, but literal construction
is still allowed. Fix: `#[non_exhaustive]` -- keeps the builders, forbids the literal. *The*
facade type, and it gained `network` and `embedded_mcp_policy` mid-cycle. The attribute makes the
builder the only construction path, which was the intent all along.

**P0 -- `struct SidecarSpec`**. Mixed `pub` / `pub(crate)` fields plus a builder; `image` is
already `pub(crate)`. Fix: `#[non_exhaustive]`. Same reasoning -- `image` is already correctly
hidden, so extend that discipline to the whole struct.

**P0 -- `enum EmbeddedMcpPolicy`**. Consumers `match` on `Merge` / `Ignore`. Fix:
`#[non_exhaustive]`. A third policy (e.g. `Error`) is plausible and would break every `match`.
Cheap.

**P1 -- `struct SidecarServerSpec`**. `command` and `env` are `pub`; built via `with_server_env`.
Fix: `#[non_exhaustive]`. A per-server option such as a timeout or cwd is plausible, and today it
breaks literals.

**P1 -- `SecuritySpec` / `NetworkSpec`**. `pub` fields, and both security and network grow. Fix:
`#[non_exhaustive]` -- they have `Default`, and `LaunchSpec`'s builders set them. These are the
two most-likely-to-grow axes; the hand-written `Default` on `SecuritySpec` keeps internal
construction working.

**P1 -- `WorkspaceSpec` / `MountSpec` / `CapabilitySpec`**. Two to three `pub` fields each, and
consumers build them as literals. Fix: an **explicit leave-as-is-or-add-`::new` decision** -- see
*Friction* below. `library_surface.rs` builds all three as literals, so annotating without
shipping a constructor breaks the very usage the facade test demonstrates.

**P2 -- `struct ToolHandle`**. Four `pub` fields, but **return-only** (`Outrig::tools()`). Fix:
`#[non_exhaustive]`. Consumers read it and never construct it, so the attribute alone fully
insulates. Near-zero friction, cheap win.

### Public error types (`thiserror`)

**P0 -- `enum OutrigError`** (`error.rs:11`). Reachable via `pub mod error`, and `error::Result`
appears in `load_project`'s signature. Consumers `match` it, and **many variants carry named
`pub` fields**: `Process`, `Spawn`, `Path`, `McpEnvResolveFailed`, `BuildArgResolveFailed`,
`McpStartupFailed(Box<..>)`. Fix: `#[non_exhaustive]` on the **enum** *and* on each field-bearing
variant. New variants land most releases; without the attribute each is a break, and adding a
field to a struct-like variant is also a break. One attribute, huge coverage -- do this first.

**P0 -- `struct McpStartupFailure`** (`error.rs`, ~200). Seven `pub` fields, carried in
`OutrigError::McpStartupFailed`; consumers read it. Fix: `#[non_exhaustive]`. A diagnostic payload
that naturally accretes fields, and return-only, so friction is minimal.

**P1 -- `enum ConfigValidationError`** (`config/validate.rs:22`). Reachable via
`OutrigError::ConfigValidation`; consumers `match`. **Variants gained and lost fields this
cycle** -- `SidecarNameInvalid` lost `image`. Fix: `#[non_exhaustive]` on the enum and on
field-bearing variants. The changelog *just* changed this enum's variant fields, which is direct
proof it churns, and 0096 changes `DockerfileMissing` / `ContextMissing` next.

**P1 -- `enum MountRuleViolation`** (`config/validate.rs:891`). Reachable via
`ConfigValidationError`. Fix: `#[non_exhaustive]`. Same class.

**P1 -- `enum EnvValueError`** (`config/env_value.rs:22`). Reachable; consumers `match`; variants
carry `var: String`. Fix: `#[non_exhaustive]`. New failure modes such as an empty value are
plausible.

**P1 -- `enum ApiKeyError`** (`config/api_key.rs:16`). Reachable. Fix: `#[non_exhaustive]`. Same.

**P1 -- `EmbeddedImageConfigError` / `StandaloneImageTomlError`**
(`container/embedded.rs:98/129`). Reachable via `OutrigError::EmbeddedImageConfigParse`. Fix:
`#[non_exhaustive]` -- unless 0093 de-publishes `container::embedded`, in which case skip.

**P2 -- `struct MistralrsDeviceParseError`** (`config/mod.rs:312`). A reachable unit struct. Fix:
`#[non_exhaustive]`, which gives it a private field. Trivial, low value.

### Residual reachable items

**P1 -- `struct ImageBuildOutcome`** (`image.rs:67`). Two `pub` fields, return-only from
`ensure_image`. Fix: `#[non_exhaustive]` -- cheap insulation.

**P1 -- `struct ContainerInspect`** (`container/mod.rs:153`). Two `pub` fields, return-only. Fix:
`#[non_exhaustive]`.

**P1 -- `struct PrimaryView`** (`container/mod.rs:195`). Namespace-join plumbing, likely to gain
fields. Fix: `#[non_exhaustive]`.

**P1 -- `ContainerLaunchSpec` (206), `ContainerWorkspace` (253), `ContainerMount` (261),
`ContainerCapabilities` (269)**, all in `container/mod.rs`. Fix: `#[non_exhaustive]`.
`ContainerLaunchSpec` has a hand-written `Default` and a `::workspace()` constructor, so friction
is low; the other three are literal-constructed and fall under the same caveat as the facade
value types.

**P1 -- `sidecar::SessionMcpPlan` (102), `PlacedServer` (49), `SidecarPlan` (58),
`enum Placement` (31)**, all in `container/sidecar.rs`. Fix: `#[non_exhaustive]`, **only if**
0093 kept them public. These are pure planning internals and were most likely never meant to be
a contract.

**P1 -- `embedded::McpServerSpecWithSource` (52), `StandaloneImage*` (69/75),
`enum McpDeclarationSource` (83)**, in `container/embedded.rs`. Public fields plus a `match`.
Preferred fix is de-publication in 0093 -- the e2e test only needs `embedded::LABEL_MCP`, so the
structs need not be public at all. `#[non_exhaustive]` only as the fallback if they stay.

**P2 -- the free functions re-exported from `lib.rs`**: `sanitize_tool_name`, `RESERVED_SERVER`,
`resolve_mcp_env`, `load_project`. Signatures are stable-ish, so **leave as-is** -- but watch for
positional-param creep. `resolve_mcp_env` and `load_project` are the two to watch; if either
grows past its current arity, give it an options struct rather than another parameter, following
0095's pattern.

All `container::*` rows here are contingent on 0093. If that task demotes the module, delete them.

If 0093 chose `#[doc(hidden)]` over a cfg boundary for `outrig-cli`, its `CliError`,
`LlmResolveError`, `ResolvedProvider`, `ResolvedAgent`, `MistralrsWeights`, and `RigAgent` remain
nameable and should get `#[non_exhaustive]` as a fallback. If 0093 gated them behind a feature,
skip them -- they are no longer a commitment.

## Already well-protected -- do NOT touch

These already apply the right pattern; changing them adds churn for no benefit. This list matters
as much as the tables above, because the failure mode of a sweep is over-application.

- **`NetworkPolicy` has a builder** (`NetworkPolicy::builder() -> NetworkPolicyBuilder`,
  `config/mod.rs:657/664`). The builder is the intended construction path and `library_surface.rs`
  uses it. Its fields are still `pub`, so a new field *would* break a literal -- hence the P2 row
  above -- but the *builder* is already future-proof: new options are new builder methods. Do not
  restructure it.
- **`ProxyServer<C = Arc<McpClient>>`** (`mcp_proxy.rs:120`) is **opaque**: its only field `inner`
  is private, `Clone` is hand-rolled with no `where C: Clone` bound, and all access is through
  methods. Adding internal state is already non-breaking. Leave as-is; just keep the field private.
- **`Container`** (`container/mod.rs:118`) is opaque -- every field private, all access via
  methods. The struct is safe. The hazards around it are the `pub`-field spec types it consumes
  and the `create_initialized` param list, which are 0095's problem, not `Container` itself.
- **`NetworkInterceptor`** (`network.rs:211`) -- opaque, private fields. Safe.
- **`ApiKeyRef(String)`** (`config/api_key.rs:31`) -- the tuple field is **private**
  (`pub struct ApiKeyRef(String)`, not `pub String`). Correctly sealed. Note the contrast with
  `ImageTag(pub String)`, which is not -- that is 0095.
- **Hand-written `Default` impls** on `SecuritySpec`, `ContainerSecurity`, `ContainerLaunchSpec`,
  `Workspace`, `NetworkPolicy`, `NetworkConfig` are correct and *complement* `#[non_exhaustive]`:
  they keep the crate's own construction working after external literal construction is sealed.
  Keep them, and prefer adding `Default` to any type this sweep annotates that lacks one.
- **`#[serde(deny_unknown_fields)]`** everywhere is good for forward-compat of the *wire* format.
  Do not change it -- see the serde caveat below.

## Friction and caveats

Applying `#[non_exhaustive]` is not free in this codebase. These are the concrete costs.

### On a struct

- **Blocks external struct-literal construction and exhaustive destructuring.** Downstream crates
  can no longer write `LaunchSpec { source, workspace, .. }` or destructure every field. For the
  facade specs (`LaunchSpec`, `SidecarSpec`) this is *desired* -- builders already exist. For the
  small value types `library_surface.rs` builds as literals -- **`WorkspaceSpec`, `MountSpec`,
  `CapabilitySpec`**, and `ContainerLaunchSpec` / `ContainerWorkspace` / `ContainerMount` if they
  stay public -- it is a real ergonomic regression unless a constructor ships alongside. Pair the
  attribute with a `::new(...)`, or keep those 2-3-field types exhaustive and accept the rare
  break. Do not annotate them silently.
- **Does not block field *reads*.** Return-only types -- `ToolHandle`, `ContainerInspect`,
  `ImageBuildOutcome`, `McpStartupFailure` -- take the attribute with essentially **zero**
  downstream friction. These are the cheapest wins in the sweep.
- **Within the crate the attribute has no effect** -- OutRig's own construction sites keep
  compiling. This is why sealing the facade is nearly free internally.

### With serde

- `#[non_exhaustive]` is a *Rust* construct and does not change (de)serialization. A
  `#[non_exhaustive]` struct still derives `Serialize` / `Deserialize`, and `deny_unknown_fields`
  is orthogonal.
- **Caveat:** `#[serde(deny_unknown_fields)]` cannot be combined with `#[serde(flatten)]`. Not an
  issue today -- no `flatten` in these types -- but a constraint to remember if a future field
  wants flattening under a non-exhaustive struct.
- **Enums:** the *wire* forward-compat story is separate. Adding an internally-tagged variant to
  `LlmProvider` or an untagged arm to `McpServerSpec` is a data-format change regardless of the
  attribute. `#[non_exhaustive]` protects the Rust `match`; it does nothing for a v0.2 binary
  reading a v0.3 config. That is a separate and pre-existing concern, and this task does not
  address it.

### With `Default` and derives

- A `#[non_exhaustive]` struct **can** still derive `Default`, `Clone`, `Debug`, `PartialEq` --
  derives run inside the defining crate. Types that hand-write `Default` keep working.
- Downstream `SomeStruct { ..Default::default() }` **does** work on a `#[non_exhaustive]` struct
  that implements `Default`. So implementing `Default` is a good mitigation to pair with the
  attribute wherever consumers might have used a literal.
- A `#[non_exhaustive]` enum cannot have its `#[default]` variant selected by the derive from
  *outside*, but the in-crate derive is fine. The config enums (`MountAccess`, `NetworkMode`,
  `CapabilityProfile`) all derive `Default` with a `#[default]` variant; downstream
  `Default::default()` calls still resolve. No friction there.

## Acceptance

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` pass, in the
  `default`, `local-llm`, and e2e-compile configurations.
- `crates/outrig/tests/library_surface.rs` compiles and passes. If a type it constructs as a
  literal was annotated, the test uses the new constructor -- and that constructor is part of this
  task's deliverable, not a follow-up.
- Every P0 row is either annotated or recorded as deleted because 0093 de-published it.
- No item on the do-not-touch list was modified.
- A surface diff against 0093's captured baseline shows only added attributes and any new
  constructors -- no unintended reachability change.

## Dependencies

- **0093.** Its outcome determines which rows above still exist. Annotating a type that 0093 then
  de-publishes is wasted work, and the `container::*` and `outrig-cli` rows are explicitly
  conditional on it.

## See also

- `plan/todo/0093-shrink-reachable-surface.md` -- the reachability tiers this task's Context
  refers to, and the surface baseline it works from.
- `plan/todo/0095-options-structs-and-sealing.md` -- the breaking changes that need replacement
  APIs rather than an attribute.
- `plan/todo/0096-config-path-provenance.md`, `plan/todo/0097-anthropic-native-api.md` -- the two
  queued features whose changes become additive once this sweep lands.
