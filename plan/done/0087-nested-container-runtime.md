# 0087 -- Nested container runtimes inside a session container

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
  capability checks at line 550.
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

In `crates/outrig/src/config/validate.rs`, next to `validate_capability_list` (line 550), check
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

## Decisions

- **The CLI needed wiring the plan did not name.** `crates/outrig-cli/src/cli/session_setup.rs`
  builds `ContainerLaunchSpec` / `ContainerCapabilities` straight from `ImageConfig.security` at
  both its primary-container site and `sidecar_launch_base`, bypassing `SecuritySpec` entirely.
  Without edits there `outrig run` would have parsed both keys and silently dropped them. Both
  sites now copy `devices` and `no_new_privileges` alongside the capability triple.

- **Three hand-written `Default` impls, not one.** The plan flagged `ContainerSecurity`, but
  `SecuritySpec` and `ContainerLaunchSpec` also derived `Default` and are reached through
  `::default()` (`outrig_.rs` sidecar/launch specs, `sidecar.rs` anonymous plans, and the
  minimal-argv unit test). A derived `Default` on any of the three yields `false` and silently
  unhardens every container built that way. The alternative -- a `NoNewPrivileges(bool)` newtype
  that keeps `#[derive(Default)]` working -- was rejected: it adds a public type and reads worse
  at every call site. The field stays a plain `bool` with the same name at all three layers.

- **No `default_true` helper was needed.** `ContainerSecurity` already carries container-level
  `#[serde(default)]`, and serde fills *every* omitted field from `Self::default()`, so the
  hand-written impl is what an absent `no-new-privileges` key resolves through. The plan's
  fallback of `#[serde(default = "default_true")]` was not used; the crate still has zero uses of
  `serde(default = "...")`. `empty_security_table_round_trips_to_hardened_defaults` pins this.

- **Emission went in `append_launch_flags`, not `build_podman_run_cmd`.** The plan file named the
  latter, but both the `run` and `create` builders delegate to the former. Putting devices and
  the conditional hardening flag there is what makes the "sidecars get both keys for free" claim
  true -- sidecars go out through `podman create`.
  `podman_create_args_carry_devices_and_privileges` covers that path.

- **Device entries are validated but not normalized.** Non-empty after trimming, absolute, and
  unique within one list. Entries are passed to podman verbatim rather than trimmed, so a
  leading space fails the absolute check rather than being silently repaired. Existence is not
  checked, per the plan.

- **`devices` was added to the sidecar block of `config-full.toml` but `no-new-privileges` was
  not.** The fixture now exercises the primary with both keys and the sidecar with only
  `devices`, which lets `config_schema` assert that a sidecar keeps the hardened default
  independently of a primary that opted out.

- **Acceptance criterion 1 was verified by hand rather than automated** (agreed scope: a
  nested-podman fixture image needs subuid/subgid delegation and nested storage config, which is
  multi-minute and host-sensitive). The automated e2e in `tests/container_security.rs` proves
  both primitives reach the kernel: `/dev/fuse` is absent from a default container and present
  with `devices`, and `NoNewPrivs` reads 1 by default and 0 when the key is cleared.

  The manual check ran a Fedora image carrying `podman`, `fuse-overlayfs`, and `shadow-utils`
  under `--userns=keep-id --device=/dev/fuse`, and produced a clean A/B on the same image with
  only the one flag differing:

  | Launch | `/proc/self/status` | setuid-root binary run as uid 1000 |
  |----------------------------|---------------------|------------------------------------|
  | default                    | `NoNewPrivs: 1`     | `uid=1000 gid=1000` -- setuid ignored |
  | `no-new-privileges = false`| `NoNewPrivs: 0`     | `uid=1000 gid=1000 euid=0(root)`   |

  That is exactly the mechanism `newuidmap` depends on, and it confirms the primitive does the
  job the task set for it. The full `podman run --rm alpine true` did not complete inside that
  throwaway image: `newuidmap` got past the setuid gate and then failed writing `uid_map`,
  because the image's `/etc/subuid` range has to be a valid subset of the session container's
  own namespace. That is image-recipe work, which this task deliberately leaves to the caller --
  outrig ships the primitives, not the nested-runtime recipe. Recorded as a known gap rather
  than papered over.

## Dependencies

None. The change is self-contained in `config`, `outrig_`, and `container`, plus their docs.

## See also

- `doc/concepts/containers.md` -- capability profiles and "What outrig sets in the run".
- `doc/reference/config.md` -- `[images.<name>.security]` and the validation-rules list.
- CocoClaw's `plan/next/nested-podman-in-agent-containers.md` -- the consumer side, blocked on
  this entry shipping.
