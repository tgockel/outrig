# 0118 -- `LaunchSpec::from_config` lowers the network policy it was handed

## Context

`LaunchSpec::from_config` (`crates/outrig/src/outrig_.rs:505`) is the library's answer to "I have
a parsed `Config`, give me something `Outrig` can launch". It lowers workspace, extra mounts, the
image source, security, MCP placements, and sidecars. Then:

```rust
Ok(Self {
    source,
    workspace: Some(ws),
    mounts,
    security: SecuritySpec::from(&cfg.security),
    network: NetworkSpec::default(),   // <-- config.network is never read
    ...
})
```

`config.network` is not read anywhere in the function. A downstream probe loaded `[network] mode
= "audit"` and got a `LaunchSpec` in `Default` mode.

The `security` line one above it is what makes this dangerous rather than merely incomplete. An
embedder reading this constructor sees the adjacent security settings honored and reasonably
concludes the whole security-relevant config was lowered. Nothing warns them; the doc comment
above the function enumerates several deliberate omissions (`start = "manual"` sidecars,
`on-failure = "warn"`, the image's own `org.outrig.mcp` label) and network is not among them,
which reads as an assertion that network *is* handled.

The failure mode is silent: filtering or auditing the embedder configured is simply not active.

## Goal

`from_config` lowers `config.network` the way it lowers `config.workspace` and `cfg.security`,
so a `LaunchSpec` built from a config enforces what that config declared.

## Deliverables

- Lower all three `NetworkMode` values and, for `filter`, the `default`/`allow`/`deny` policy,
  into `NetworkSpec` -- reading through whatever effective accessor 0115 settled on, not through
  a raw field. The conversion belongs beside the existing `From<&ContainerSecurity>`
  impls rather than open-coded in `from_config`, for the reason those impls exist: both types
  are `#[non_exhaustive]`, so a new network key can only be wired through inside this crate.
- The `from_config` doc comment stops implying completeness -- state what is lowered and keep
  the omission list honest.
- `crates/outrig/CHANGELOG.md` records this as a fix, framed as "filtering you configured is now
  actually applied", because an embedder needs to know their previous build was not enforcing.

## Acceptance

- Downstream-shaped tests in `crates/outrig/tests/library_surface.rs` -- not unit tests inside
  `outrig_.rs`, since the point is what an external caller observes: a config with `mode =
  "default"`, one with `mode = "audit"`, and one with `mode = "filter"` plus an allow list each
  produce a `LaunchSpec` whose `network` matches.
- The filter case asserts the rules survive, not just the mode, and that they equal the *merged*
  global policy exactly. Lowering the mode and dropping `allow`/`deny` would be the same bug one
  layer down.
- **The default and audit cases assert the lowered spec carries no policy at all** -- not merely
  that the mode is right. A `NetworkSpec` that quietly holds rules in a non-filter mode arms
  itself the moment a caller flips the public mode field, which is a latent version of the bug
  this task exists to fix.
- A config that reached `from_config` through `merge` lowers the *merged* policy, so 0115's
  mode-only trust rule is still intact at launch. A lowering that reads the repo block directly
  would undo it.
- `crates/outrig/public-api.txt` regenerated if a conversion becomes public.

## Dependencies

- **Hard: after 0115.** That task may reshape `NetworkConfig` -- an optional declared mode with
  an effective accessor is its recommended shape -- and this task lowers exactly that field.
  Lowering the old shape first means writing the lowering twice, and the second write is the one
  that would be done in a hurry.
- Soft: after 0114, if 0114 reshapes `NetworkSpec`. The two do not overlap in code.

## See also

- `crates/outrig/src/outrig_.rs:505-561` -- `from_config`; `NetworkSpec::default()` at 558.
- `plan/next/launch-spec-security-lowering.md` -- the *same function*, and the same class of
  hazard for `[security]` keys: a config key that parses, validates, documents cleanly, and is
  silently ignored at launch. That entry is about the four hand-copied lowering sites; this task
  is about a block that is not lowered at all. Whoever takes this should read that entry, since
  a single conversion targeting `ContainerLaunchSpec` would close both.
