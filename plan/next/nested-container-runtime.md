# Nested container runtimes inside a session container

## Context

An agent whose job is container work -- build an image, run a test suite in a throwaway
container, reproduce a CI step -- cannot do it inside an OutRig session today. A nested `podman`
fails, and no config surface can fix it.

`build_podman_run_cmd` (`crates/outrig/src/container/mod.rs:667-693`) always emits:

```
--userns=keep-id  --security-opt=no-new-privileges  --pull=never
```

Two of those hard-codes block the nested runtime, for separate reasons:

- **`no-new-privileges`.** The kernel ignores the setuid bit (and file capabilities) on every
  `execve` under `no_new_privs`. `newuidmap` and `newgidmap` are setuid-root, so a nested rootless
  podman cannot map its subordinate UID range. It falls back to a single-UID mapping, and image
  extraction then fails on any layer holding a file owned by a second UID or GID -- which includes
  `ubuntu:24.04` (`/var/log/wtmp`, root:utmp). The failure reads "potentially insufficient UIDs or
  GIDs available in user namespace".
- **No device passthrough.** The session container's rootfs is overlayfs. The kernel refuses
  overlay-on-overlay, so a nested podman needs `fuse-overlayfs`, which needs `/dev/fuse` bound into
  the container. There is no `--device` surface at all.

`ContainerSecurity` (`crates/outrig/src/config/mod.rs:797-820`) carries only
`capability-profile`, `cap-drop`, and `cap-add`. Capabilities are the wrong lever here:
`no_new_privs` is a separate process flag, and a device node is not a capability. Neither can be
reached from config, from `SecuritySpec`, or from the library facade.

CocoClaw is the motivating consumer -- it drives OutRig as a library and wants agent images that
can run `podman` -- but nothing about the fix is CocoClaw-specific. Any caller wanting a nested
runtime (podman, buildah, docker-in-docker, a VM launcher wanting `/dev/kvm`) hits the same two
walls.

## Goal

Let an image-config opt into the two primitives a nested container runtime needs -- device
passthrough and a `no-new-privileges` opt-out -- so a caller can compose a session container that
runs `podman` inside itself, with both defaulting to today's behavior.

OutRig stays a general container library. It gains device and privilege primitives; it does not
gain a `nested-containers` flag, and it does not learn what podman-in-podman is. Composing the
primitives into a nested-runtime recipe is the caller's job, and belongs in their docs, not
OutRig's config schema.

## Deliverables

- Two new keys on `ContainerSecurity`, kebab-cased like their neighbors:

  ```toml
  [images.coding.security]
  capability-profile = "default"
  no-new-privileges  = false          # default true
  devices            = ["/dev/fuse"]  # default []
  ```

- Both keys carried through the facade to the podman invocation: `SecuritySpec`
  (`crates/outrig/src/outrig_.rs:62`), `From<&ContainerSecurity> for SecuritySpec` (line 176),
  `ContainerLaunchSpec` / `ContainerCapabilities` (`crates/outrig/src/container/mod.rs:93-138`),
  and `From<&CapabilitySpec> for ContainerCapabilities` (`outrig_.rs:188`).
- `build_podman_run_cmd` emits one `--device=<path>` per entry and omits
  `--security-opt=no-new-privileges` when the key is false.
- Config validation for `devices` in `crates/outrig/src/config/validate.rs`, alongside the
  capability checks at line 378.
- Doc updates in the same commit: `doc/concepts/containers.md` and `doc/reference/config.md`.

## Config surface

`no-new-privileges` is a bool defaulting to **true** -- today's behavior. This breaks the
`#[derive(Default)]` on `ContainerSecurity`, which would yield `false` and silently invert the
safe default. Hand-write the impl:

```rust
impl Default for ContainerSecurity {
    fn default() -> Self {
        Self {
            capability_profile: CapabilityProfile::default(),
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            no_new_privileges: true,
            devices: Vec::new(),
        }
    }
}
```

and mark the field `#[serde(default = "default_true")]` so an omitted key deserializes to true
rather than to `bool::default()`. `ContainerSecurity::is_default()` (line 807) compares against
`Self::default()`, so it keeps working and the `skip_serializing_if` on `ImageConfig::security`
(line 852) still elides an untouched block. A round-trip test over an empty `[security]` table is
the cheapest guard against regressing this.

`devices` is `Vec<String>`, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`. Entries are
host paths passed to podman as `--device=<path>`; podman's own `src:dst:perms` form is out of
scope for this task -- take the plain path and say so in the reference doc.

Both keys belong on `SecuritySpec` directly, not folded into `CapabilitySpec`. A device node is not
a capability and `no_new_privs` is not a capability; putting them in `CapabilitySpec` would make
`with_capabilities` (`outrig_.rs:378`) a lie. `SecuritySpec` is already the aggregate that owns
`capabilities`, so it grows two sibling fields and `ContainerLaunchSpec` grows the matching pair
next to its existing `capabilities`.

## Runtime behavior

`build_podman_run_cmd` currently closes with an unconditional pair:

```rust
cmd.args(["--security-opt=no-new-privileges", "--pull=never"])
```

That becomes a conditional `--security-opt=no-new-privileges` followed by an unconditional
`--pull=never`, with the device flags emitted alongside the capability flags. Argument order is
load-bearing for the unit tests, so pick a position and keep it: devices immediately after
`append_capability_flags`, then the security-opt pair, keeps the diff on existing expectations
small.

The tests at `container/mod.rs:900-1170` assert exact argument vectors and every one of them
needs its expectation updated even though behavior is unchanged for default configs. Add cases
for: devices emitted in declaration order, `no-new-privileges = false` dropping exactly one flag
and nothing else, and both keys together on top of a `drop-all` profile.

Sidecars reuse the same `[images.<name>.sidecars.<sc>.security]` block
(`doc/concepts/containers.md:246`, `crates/outrig/src/config/mod.rs:878`), so both keys reach
sidecar containers with no extra plumbing. Say so in the docs rather than leaving it to be
inferred.

## Validation

In `crates/outrig/src/config/validate.rs`, next to `validate_capability_list` (line 378), check
each `devices` entry:

- non-empty after trimming,
- absolute (a relative device path is always a mistake),
- no duplicates within one image-config's list.

Match the surrounding error style -- the existing capability errors name the image-config and the
offending key. Do not check that the device node exists on the host: config validation runs on
machines that are not the launch host, and podman's own error is clear enough.

Add the new rules to the validation-rules list in `doc/reference/config.md:727-733`.

## Security

`no-new-privileges = false` restores setuid escalation inside the sandbox. A process in the
container that finds a setuid-root binary can use it. This is a real weakening of the container
boundary, not a neutral knob, and the docs must say that in those terms.

Two things keep it honest:

- It is opt-in per image-config and defaults to true, so every existing config keeps today's
  protection with no edit.
- The container is still an unprivileged rootless podman container in a user namespace. Dropping
  `no_new_privs` does not grant host root; it grants the container's own namespace-local root, which
  is exactly what a nested runtime needs.

`devices` is the sharper edge of the two in the general case -- `--device=/dev/kvm` or a raw block
device hands out real hardware access -- but it is explicit per path and per image-config, which is
the right granularity. Do not add a device allowlist in this task; note the question and move on.

Both keys should appear in `doc/concepts/containers.md` under the security discussion, not buried
in the reference, so that a reader deciding whether to set them meets the tradeoff first.

## Acceptance

- An image-config setting `no-new-privileges = false` and `devices = ["/dev/fuse"]`, built from a
  Dockerfile carrying `podman` and `fuse-overlayfs`, launches a session in which
  `podman run --rm alpine true` succeeds inside the container.
- An image-config setting neither key produces a byte-identical `podman run` argument vector to
  the one produced before this change.
- `devices` entries appear as `--device=<path>` in declaration order; `no-new-privileges = false`
  removes exactly `--security-opt=no-new-privileges` and leaves `--pull=never` and
  `--userns=keep-id` in place.
- Config validation rejects an empty, relative, or duplicated device entry with an error naming
  the image-config.
- An empty `[images.<name>.security]` table round-trips to `no-new-privileges = true` and is
  elided on serialization.
- `doc/concepts/containers.md` no longer states that `--security-opt=no-new-privileges` is
  unconditional (lines 269-281 and the capability-profiles section at 91-118), and
  `doc/reference/config.md:424-446` documents both keys.

## Open questions

- **`--userns=keep-id` stays hard-coded.** A nested rootless podman works under keep-id once
  `newuidmap` is usable, so this task does not need a userns knob. A caller wanting `--userns=host`
  or an explicit map is a separate ask; do not speculatively add it.
- **SELinux.** On an enforcing host, a nested runtime also wants `--security-opt=label=disable`.
  OutRig already computes a `selinux` flag for mount options (`append_bind_mount`), so the
  information is in hand, but no config surface exposes security-opt generally. Left out
  deliberately: add it when someone runs into it, rather than guessing at the shape now.
- **Device allowlist.** Whether a future policy layer should constrain which device paths an
  image-config may request. Not for this task.

## Dependencies

None. The change is self-contained in `config`, `outrig_`, and `container`, plus their docs.

## See also

- `doc/concepts/containers.md` -- capability profiles and "What outrig sets in the run".
- `doc/reference/config.md` -- `[images.<name>.security]` and the validation-rules list.
- CocoClaw's `plan/next/nested-podman-in-agent-containers.md` -- the consumer side, blocked on
  this entry shipping.
