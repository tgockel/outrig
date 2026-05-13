# 0057 -- Runtime bind mounts

## Context

Today outrig has exactly one host bind mount: `[workspace]`. It is always read-write and
becomes the container workdir:

```toml
[workspace]
host-path      = "."
container-path = "/workspace"
```

That is the right default for the repo the agent is editing, but it is too coarse for
supporting material. A session may need to read sibling repos, generated docs, model
artifacts, SDK checkouts, or reference data without giving the agent write access to all of
it. The common desired shape is:

- mount the active repo at `/workspace` read-write;
- mount extra resource directories under `/resources/...` read-only;
- optionally opt a specific extra mount into read-write when the user really wants that.

This feature is separate from capability profiles. Mount policy controls what parts of the host
filesystem the container can see and mutate. Capability policy controls kernel privileges inside
the container. They should be queued and implemented independently.

## Goal

Support multiple runtime bind mounts for `outrig run`, `outrig mcp`, and public
`LaunchSpec` callers while preserving the existing read-write primary workspace.

## Goals and non-goals

**In scope:**

- Multiple runtime bind mounts for `outrig run`, `outrig mcp`, and public
  `LaunchSpec` callers.
- Per-mount access mode: `read-only` or `read-write`.
- Preserve the existing primary workspace behavior and defaults.
- Support extra mounts from both repo and global config, plus direct public API callers.
- Validate duplicate container targets and missing host paths before `podman run`.

**Out of scope:**

- Staging, copy-on-write overlays, or delayed apply.
- File mounts. v1 is directory-only.
- Named mount profiles.
- Read-only root filesystem for the image.
- Capability drops, seccomp policy, or network policy.

## User surface

### Config

The existing `[workspace]` block remains the primary workspace. It still defaults to
`host-path = "."`, `container-path = "/workspace"`, and read-write access.

Extra mounts are declared as nested array entries:

```toml
[workspace]
host-path      = "."
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "../shared-docs"
container-path = "/resources/shared-docs"
# access omitted -> "read-only"

[[workspace.mounts]]
host-path      = "/var/tmp/outrig-cache"
container-path = "/resources/cache"
access         = "read-write"
```

| Key                        | Type   | Required | Default     | Description                  |
|----------------------------|--------|----------|-------------|------------------------------|
| `workspace.mounts`         | array  | no       | `[]`        | Extra runtime bind mounts.   |
| `mounts[*].host-path`      | path   | yes      | --          | Host directory to mount.     |
| `mounts[*].container-path` | path   | yes      | --          | Absolute in-container path.  |
| `mounts[*].access`         | string | no       | `read-only` | `read-only` or `read-write`. |

Primary workspace path resolution is unchanged. Extra `host-path` values follow the same rule:
relative paths resolve against the repo root. Global config should normally use absolute paths,
but relative paths still resolve against the repo being launched so the merge behavior stays
predictable.

### Merge behavior

The primary workspace remains repo-owned, matching today's `merge` behavior. Extra mounts are
combined instead of replacing each other:

1. Global `workspace.mounts`, in file order.
2. Repo `workspace.mounts`, in file order.

If the same `container-path` appears more than once after merge, validation fails. The user must
pick one definition rather than relying on mount ordering to shadow an earlier entry.

### Public API

Keep `WorkspaceSpec` as the primary workspace. Add a separate mount list to `LaunchSpec`:

```rust
pub struct LaunchSpec {
    pub workspace: Option<WorkspaceSpec>,
    pub mounts: Vec<MountSpec>,
    // existing fields...
}

pub struct MountSpec {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

pub enum MountAccess {
    ReadOnly,
    ReadWrite,
}
```

Add builder helpers:

```rust
impl LaunchSpec {
    pub fn with_mount(mut self, mount: MountSpec) -> Self;
    pub fn with_mounts(mut self, mounts: impl IntoIterator<Item = MountSpec>) -> Self;
}
```

Existing constructors keep compiling and initialize `mounts` to an empty vector.
`LaunchSpec::from_container_config` copies `workspace.mounts` from the merged config into the
new field. `LaunchSpec::from_image(...).with_mount(...)` works without a primary workspace; in
that case outrig passes the extra mounts but does not set `-w`.

## Runtime behavior

`Container::start` should stop accepting only one optional `(host, container)` tuple. Replace the
internal startup input with a small launch-mount struct that carries:

- optional primary workspace;
- zero or more extra mounts;
- enough access-mode information to render each podman bind mount.

The primary workspace renders as it does today:

```text
-v <host>:<container>:rw[,Z] --userns=keep-id -w <container>
```

Each extra mount renders as another bind mount:

```text
-v <host>:<container>:ro[,Z]
-v <host>:<container>:rw[,Z]
```

SELinux handling stays centralized: if `selinux_enforcing()` is true, append `,Z` to every bind
mount option, not only the primary workspace. `--userns=keep-id`,
`--security-opt=no-new-privileges`, and `--pull=never` stay unchanged.

## Validation

Config validation rejects:

- an extra mount whose `host-path` does not exist;
- an extra mount whose `host-path` is not a directory;
- an extra mount whose `container-path` is not absolute;
- `container-path = "/"`;
- duplicate container paths, including collision with the primary workspace path;
- an `access` value other than `read-only` or `read-write`.

Nested container paths are allowed in v1 as long as they are not exact duplicates. Mounts are
rendered in declared order after the primary workspace. A later task can add stricter overlap
policy if real use shows it is needed.

## Deliverables

- `src/config/mod.rs`: add `Workspace.mounts: Vec<MountConfig>` and
  `MountAccess`.
- `src/config/merge.rs`: preserve repo-owned primary workspace fields, but concatenate
  global and repo extra mounts.
- `src/config/validate.rs`: add mount validation under the existing repo-root-aware pass.
- `src/outrig_.rs`: expose `MountSpec`, `MountAccess`, and the `LaunchSpec` mount builders.
- `src/container/mod.rs`: extract a podman-run argument builder so mount rendering has unit
  coverage without needing podman.
- `src/cli/session_setup.rs`: pass the merged extra mounts into container startup.
- Docs: update `doc/reference/config.md`, `doc/concepts/workspace.md`, and
  `doc/concepts/containers.md` once the feature is implemented.

## Acceptance

- Existing configs keep working with no edits.
- A config with one read-write primary workspace and one read-only extra mount starts a
  container with both mounts present.
- A process inside the container can read from a read-only extra mount but cannot write to it.
- A process inside the container can write to an explicitly read-write extra mount.
- Duplicate container targets fail during config validation with a message naming the path.
- Public API callers can launch from an image and add an extra read-only mount without using
  the CLI config path.

Verification commands:

```sh
cargo test
cargo build --no-default-features
cargo doc --no-default-features --no-deps
cargo test --features e2e --test container_lifecycle -- --nocapture
cargo test --no-default-features --features e2e --test library_surface -- --nocapture
```

## See also

- `0058-capability-profiles.md` -- kernel capability controls for the same container
  launch path.
- `0059-network-interceptor-plumbing.md` -- network egress controls; deliberately separate from
  filesystem mount policy.

## Dependencies

None.
