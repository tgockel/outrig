# 0058 -- Capability profiles

## Context

`doc/concepts/containers.md` currently documents the v0 security floor:

```text
--userns=keep-id --security-opt=no-new-privileges
```

outrig does not drop capabilities today. That was intentional for v0 because
`--cap-drop=ALL` can break real toolchains and MCP servers in surprising ways. The missing piece
is not "always drop everything"; it is a runtime policy surface that lets users pick a reasonable
profile and then override it when a specific container needs more or less.

This feature is separate from runtime bind mounts. Capability policy changes the kernel
privilege set available inside the container. Bind-mount policy changes which host paths are
visible and whether they are writable.

## Goal

Add container-scoped Linux capability profiles and explicit capability overrides without
changing the default container privileges.

## Goals and non-goals

**In scope:**

- Container-scoped capability controls for `outrig run`, `outrig mcp`, and public
  `LaunchSpec` callers.
- Named profiles for common modes.
- Explicit `cap-drop` and `cap-add` override lists.
- Preserve today's podman default capability set unless the user opts in.
- Render predictable `podman run --cap-drop ... --cap-add ...` arguments.

**Out of scope:**

- Seccomp profile selection.
- AppArmor or SELinux policy authoring.
- Read-only root filesystem.
- Mount access policy.
- Network interception or network allowlists.
- Per-MCP-server capability policy. Capabilities apply to the one session container.

## User surface

### Config

Capability controls live under the selected container config:

```toml
[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

[containers.coding.security]
capability-profile = "no-net-raw"
cap-drop = ["MKNOD", "SETFCAP"]
cap-add  = ["NET_BIND_SERVICE"]
```

| Key                  | Type   | Required | Default   | Description               |
|----------------------|--------|----------|-----------|---------------------------|
| `capability-profile` | string | no       | `default` | Named capability profile. |
| `cap-drop`           | array  | no       | `[]`      | Extra caps to drop.       |
| `cap-add`            | array  | no       | `[]`      | Caps to add back.         |

Accepted profiles:

- `default`: preserve current behavior; emit no capability flags unless explicit lists are set.
- `no-net-raw`: drop `NET_RAW`, which blocks raw sockets while leaving most development tools
  alone.
- `drop-all`: emit `--cap-drop=ALL`; users may add narrowly required caps back with
  `cap-add`.

### Public API

Add a public security field to `LaunchSpec`:

```rust
pub struct LaunchSpec {
    pub security: SecuritySpec,
    // existing fields...
}

pub struct SecuritySpec {
    pub capabilities: CapabilitySpec,
}

pub struct CapabilitySpec {
    pub profile: CapabilityProfile,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
}

pub enum CapabilityProfile {
    Default,
    NoNetRaw,
    DropAll,
}
```

Add builder helpers:

```rust
impl LaunchSpec {
    pub fn with_security(mut self, security: SecuritySpec) -> Self;
    pub fn with_capabilities(mut self, capabilities: CapabilitySpec) -> Self;
}
```

Existing constructors keep compiling and initialize security to `CapabilityProfile::Default`
with empty override lists. `LaunchSpec::from_container_config` copies
`[containers.<name>.security]` into the public launch spec.

## Runtime behavior

Capability rendering expands the selected profile first, then explicit drops, then explicit
adds:

1. `default`: no profile-generated flags.
2. `no-net-raw`: `--cap-drop=NET_RAW`.
3. `drop-all`: `--cap-drop=ALL`.
4. For every explicit `cap-drop`: add another `--cap-drop=<cap>`.
5. For every explicit `cap-add`: add `--cap-add=<cap>`.

`cap-add` is intentionally last so a user can start from `drop-all` and add back one narrow
capability:

```toml
[containers.web.security]
capability-profile = "drop-all"
cap-add = ["NET_BIND_SERVICE"]
```

The existing `--security-opt=no-new-privileges` stays enabled for every profile. This feature
does not add `--privileged` or any equivalent escape hatch.

## Validation

Config validation rejects:

- an unknown `capability-profile`;
- empty capability names;
- capability names with characters outside `^[A-Z0-9_]+$` after optional `CAP_` stripping;
- duplicate names within `cap-drop` or within `cap-add`;
- the same capability appearing in both explicit `cap-drop` and explicit `cap-add`.

Accepted capability names may be written as `NET_RAW` or `CAP_NET_RAW`. Validation normalizes to
the form podman accepts without the `CAP_` prefix before rendering. Do not hard-code a full Linux
capability enum in v1; podman and kernels vary, and unknown-but-well-formed names should produce
podman's native error if unsupported.

## Deliverables

- `src/config/mod.rs`: add `ContainerSecurity`, `CapabilityProfile`, and capability list fields
  under `ContainerConfig`.
- `src/config/validate.rs`: validate profile strings and capability names alongside existing
  container validation.
- `src/outrig_.rs`: expose `SecuritySpec`, `CapabilitySpec`, `CapabilityProfile`, and builders.
- `src/container/mod.rs`: fold capability rendering into the same extracted podman-run argument
  builder used by runtime bind mounts.
- `src/cli/session_setup.rs`: pass selected container security into `Container::start_named`.
- Docs: update `doc/reference/config.md` and `doc/concepts/containers.md` after implementation.

## Acceptance

- Existing configs keep working and emit no new `--cap-drop` / `--cap-add` flags.
- `capability-profile = "no-net-raw"` renders `--cap-drop=NET_RAW`.
- `capability-profile = "drop-all"` plus `cap-add = ["NET_BIND_SERVICE"]` renders both
  `--cap-drop=ALL` and `--cap-add=NET_BIND_SERVICE`.
- Invalid profile and malformed capability names fail during config validation.
- Public API callers can launch with `CapabilityProfile::NoNetRaw` without using CLI config.
- E2E coverage either inspects the created podman container's configured cap-drop/cap-add lists
  or runs a small in-container behavior check when the local podman/kernel combination supports
  it.

Verification commands:

```sh
cargo test
cargo build --no-default-features
cargo doc --no-default-features --no-deps
cargo test --features e2e --test container_lifecycle -- --nocapture
cargo test --no-default-features --features e2e --test library_surface -- --nocapture
```

## See also

- `0057-runtime-bind-mounts.md` -- host filesystem exposure controls for the same container
  launch path.
- `0059-network-interceptor-plumbing.md` -- network egress controls; deliberately separate from
  kernel capability policy.

## Dependencies

- **Hard: 0057**. Reuses the podman-run argument builder extracted for runtime bind
  mounts so capability rendering has unit coverage without needing podman.
