# A `[security]` key is lowered onto `ContainerLaunchSpec` by hand at four sites

> **Adjacent to `plan/todo/0118-from-config-lowers-the-network-policy.md`**, which fixes a
> `[network]` block that `LaunchSpec::from_config` does not lower at all. This entry is the
> ergonomic half for `[security]`: four hand-copied lowering sites, any of which can be forgotten.
> A single conversion targeting `ContainerLaunchSpec` would close both, so read this before
> taking 0118.

## Problem

Adding one `[security]` key costs four identical hand-edits, and the failure mode for
forgetting one is a config key that parses, validates, documents cleanly, and is silently
ignored at launch:

- `crates/outrig-cli/src/cli/session_setup.rs` -- the primary, and `sidecar_launch_base`
- `crates/outrig/src/outrig_.rs` -- the library launch, and the library sidecar

Each site spells out the same block:

```rust
capabilities: ContainerCapabilities::from(...),
devices:           <src>.devices.clone(),
no_new_privileges: <src>.no_new_privileges,
unmask:            <src>.unmask.clone(),
```

The repo already names this exact hazard, at `crates/outrig/src/outrig_.rs`, as the reason
`From<&ContainerSecurity> for ContainerCapabilities` exists at all:

> Lives here rather than at each call site because both types are `#[non_exhaustive]`: a new
> capability knob can only be wired through inside this crate, so a caller's own copy of this
> mapping would keep compiling while silently dropping it.

That remedy covers the `capabilities` third only. `devices`, `no_new_privileges`, and `unmask`
are precisely "a caller's own copy of this mapping". All three of `ContainerSecurity`,
`SecuritySpec`, and `ContainerLaunchSpec` are `#[non_exhaustive]`, and `outrig-cli` builds its
spec as `::default()` plus field assignment, so nothing makes the CLI fail to compile when a
key is added. The container unit tests do not catch it either: they construct
`ContainerLaunchSpec` directly, so they exercise the argv and never the config -> launch
mapping.

`plan/done/0087-nested-container-runtime.md` recorded the `session_setup.rs` sites as the trap
its own plan missed. `0114`'s `unmask` hit the same four sites again.

## Not the fix: routing the CLI through `SecuritySpec`

`SecuritySpec` is the *facade's* public spec type -- it hangs off `LaunchSpec`/`SidecarSpec`
and is consumed only by `Outrig`. The CLI drives `container::Container` directly and never
touches `Outrig`, so `From<&ContainerSecurity> for SecuritySpec` is a sibling API surface
rather than an intermediate layer on the CLI's path. Sending the CLI through it adds a hop
through an otherwise-unused type and removes no copying, because `SecuritySpec` ->
`ContainerLaunchSpec` is itself field-by-field.

## Goal

One conversion whose *target* is `ContainerLaunchSpec`, so all four sites become one line and
a fifth security key is a one-line change. Two shapes worth costing:

- **Aggregate.** Give `ContainerLaunchSpec` a `security: SecuritySpec` field. Needs
  `PartialEq`/`Eq` on `SecuritySpec` and `CapabilitySpec`, and would also collapse the
  `ContainerCapabilities` / `CapabilitySpec` twinning. Largest churn, smallest end state.
- **Minimal.** An inherent `ContainerLaunchSpec::with_security(&ContainerSecurity)` beside the
  two existing `From` impls. Four sites become one line each; the three flat fields stay.

Either reshapes the public API across both crates, so `crates/outrig/public-api.txt` moves and
the unit tests that build `ContainerLaunchSpec` literals churn. That is why it is a task rather
than an edit folded into the key that exposed it.

## Acceptance

- Adding a hypothetical fifth `[security]` key requires editing one lowering site, not four.
- A test covers the config -> launch mapping for the CLI path, so a dropped key fails
  something. `plan/next/container-surface-test.md` is the natural home if it lands first.
