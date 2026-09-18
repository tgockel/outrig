# 0002-41 -- `LaunchSpec::from_config` lowers the network policy it was handed

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
  into `NetworkSpec` -- reading through whatever effective accessor 0002-38 settled on, not through
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
- A config that reached `from_config` through `merge` lowers the *merged* policy, so 0002-38's
  mode-only trust rule is still intact at launch. A lowering that reads the repo block directly
  would undo it.
- `crates/outrig/public-api.txt` regenerated if a conversion becomes public.

## Dependencies

- **Hard: after 0002-38.** That task may reshape `NetworkConfig` -- an optional declared mode with
  an effective accessor is its recommended shape -- and this task lowers exactly that field.
  Lowering the old shape first means writing the lowering twice, and the second write is the one
  that would be done in a hurry.
- Soft: after 0002-37, if 0002-37 reshapes `NetworkSpec`. The two do not overlap in code.

## See also

- `crates/outrig/src/outrig_.rs:505-561` -- `from_config`; `NetworkSpec::default()` at 558.
- `plan/next/launch-spec-security-lowering.md` -- the *same function*, and the same class of
  hazard for `[security]` keys: a config key that parses, validates, documents cleanly, and is
  silently ignored at launch. That entry is about the four hand-copied lowering sites; this task
  is about a block that is not lowered at all. Whoever takes this should read that entry, since
  a single conversion targeting `ContainerLaunchSpec` would close both.

## Decisions

- **The conversion is `impl From<&NetworkConfig> for NetworkSpec`, and its match on
  `NetworkMode` has no wildcard arm.** `NetworkMode` is `#[non_exhaustive]`, but it is declared
  in this crate, so an exhaustive match is legal here and is what gets written: a fourth mode
  has to fail to compile at this site. A wildcard falling through to `policy: None` would be the
  same defect this task exists to fix, one mode later -- a key that parses, validates, and is
  silently dropped on the way to launch. The `#[non_exhaustive]` attribute buys the caller-side
  guarantee the task's deliverable asks for (an outside crate cannot write this mapping at all);
  it must not also buy a wildcard on the inside.

- **Only `filter` carries a policy; `default` and `audit` lower to `None`.** Audit's policy comes
  from the interceptor (`NetworkPolicy::allow_all()`, `network.rs:469`), so a copy of the
  config's rules in a non-filter spec is not merely redundant -- it is a latent filter that
  arms itself the moment a caller assigns to the public `mode` field. This is the acceptance
  criterion that asks for "no policy at all" rather than "the right mode", and it is asserted
  as `policy == None` rather than as an emptiness check.

- **Nothing in the lowering knows about `merge`.** `merge` takes the global block as the base
  and overlays only the repo's declared *mode* (`config/merge.rs:56-58`), so
  `config.network.policy()` on a merged config already *is* the merged global policy. 0002-38's
  trust rule reaches launch by being upstream of this code rather than by being restated in it;
  the test that runs a config through `merge` before `from_config` is what pins that.

- **Reads go through `mode()` and `policy()`, never a field.** Required now that 0002-38 made
  the fields private, but also the right call on its own: `policy()` is what resolves an
  undeclared `default` to `Deny`, and `mode()` an undeclared mode to `Default`.

- **The lowering tests live in a new ungated `crates/outrig/tests/launch_spec_from_config.rs`,
  not in `library_surface.rs` as this task's Acceptance said.** `library_surface.rs` is
  `#![cfg(feature = "e2e")]` end to end, and CI runs the e2e matrix entry with `--no-run`
  (`.github/workflows/ci.yml:52`), so a lowering test placed there would compile in CI and
  never execute anywhere -- a regression in the very line this task fixes would fail nothing.
  The task's stated reason for naming that file was that the assertions must be what an
  *external caller* observes rather than a unit test inside `outrig_.rs`; an ungated
  integration test beside `config_merge.rs` satisfies that reason and runs on every
  `cargo test`. Every config in the new file is sidecar-free, which is what keeps it out of
  the feature: `from_config` reaches podman only to resolve a sidecar image.

  `library_surface.rs` still gains one test, `from_config_audit_mode_starts_the_interceptor`,
  because the two files answer different questions. The ungated file asserts the spec carries
  the mode; the e2e one asserts the spec reaches enforcement, by launching a config-declared
  `audit` session and checking the container resolves through the interceptor -- the same
  `nameserver 127.0.0.1` marker `added_sidecar_egress_obeys_network_policy` already uses.
  Without it, a lowering that populated the struct and a `launch` that ignored it would both
  pass.

- **`plan/next/launch-spec-security-lowering.md` is left open, deliberately.** Its single
  conversion targeting `ContainerLaunchSpec` would subsume this one, but it reshapes the public
  API across both crates and churns every `ContainerLaunchSpec` literal in the unit tests.
  Folding that into this task would have buried a security fix inside a refactor. The `From`
  impl added here is what that task would call.

- **`public-api.txt` was hand-edited to the two intended lines rather than replaced wholesale.**
  A full regeneration at the current toolchain also rewrites six unrelated lines from
  `std::io::error` to `core::io::error` -- compiler drift, not a surface change, and not this
  task's to land. Recorded because the snapshot's header only warns about the *tool* version
  moving; the compiler moves it too. 0002-48, which gates this file, will have to absorb the
  drift in one deliberate commit.

- **This task's `See also` was wrong about `plan/next/launch-spec-security-lowering.md`, and that
  entry was wrong about itself.** Both claimed a single conversion targeting `ContainerLaunchSpec`
  would close the `[security]` lowering *and* this one. It cannot: `ContainerLaunchSpec` has no
  network field, because interception is applied after the container starts
  (`NetworkInterceptor::attach`) rather than through podman's create argv -- its only
  network-adjacent knob is the `intercept_dns` bool on `ContainerCreateOptions`. The two are
  adjacent in hazard class and nothing else. The `plan/next/` entry's header was corrected; this
  file's `See also` is left as written, since it is the historical record of what the task was
  planned against.

- **Sidecars needed no lowering of their own, and get interception from this fix for free.**
  There is no per-sidecar `[network]` block and no `network` field on `SidecarSpec`: interception
  is session-scoped, and `Outrig::add_sidecar` attaches the session's interceptor to every
  sidecar independently of any spec. Launch-time sidecars start after the interceptor does, so
  a config-built session's sidecars are now covered by the same one-line change. Checked rather
  than assumed, because "the primary is intercepted and the sidecars are not" would have been a
  worse bug than the one being fixed.

- **`plan/next/one-network-lowering-for-both-crates.md` filed.** The CLI never builds a
  `LaunchSpec`, so it never reaches the new conversion and keeps its own string-keyed copy of the
  same mapping across three sites in `session_setup.rs` -- which already disagrees with the
  library about when an empty filter policy is rejected. Out of scope here: folding them together
  means moving the `--network` override and reshaping how `NetworkInterceptor` starts.
