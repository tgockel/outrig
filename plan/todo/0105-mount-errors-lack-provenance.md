# 0105 -- Mount validation errors cannot name the file that declared the path

## Context

`plan/done/0097-config-path-provenance.md` gave `ConfigValidationError::DockerfileMissing` and
`ContextMissing` a `declared_in` field, so an image path that fails an existence check names the
config file to go edit. Mount errors did not get the same treatment, and the asymmetry is
visible: a global `[[workspace.mounts]]` whose `host-path` is missing still reports

```
workspace mount host-path "shared" does not exist
```

with no hint that `shared` is meant to be found beside `~/.outrig/config.toml` rather than in the
repo. That is precisely the confusion `declared_in` exists to remove, and mounts are the shape
where it matters most: global and repo mount lists are *concatenated*
(`crates/outrig/src/config/merge.rs`), so a single failing list can hold entries from two files.

The reason it was skipped is mechanical, not principled. The two image variants already carried
`#[non_exhaustive]` from `plan/done/0094-non-exhaustive-sweep.md`, so adding a field was
additive. The mount errors do not:

- `MountRuleViolation` (`crates/outrig/src/config/validate.rs`) is a plain **tuple** enum --
  `HostMissing(PathBuf)`, `HostNotDirectory(PathBuf)`, `ContainerNotAbsolute(PathBuf)`,
  `ContainerRoot`, `ContainerDuplicate(PathBuf)`. Every variant would change shape.
- `ConfigValidationError::WorkspaceMountHostMissing`, `WorkspaceMountHostNotDirectory`,
  `WorkspaceMountContainerNotAbsolute`, `WorkspaceMountContainerDuplicate` are struct variants
  **without** `#[non_exhaustive]` -- 0094 Decision 5 sealed variants only where a field addition
  was already proven, and these were not on that list.

Both are breaking changes, and 0097 was required to be additive.

## Goal

Let a mount diagnostic name its declaring file, the way an image diagnostic already can.

## Deliverables

- `declared_in` reaches the four `WorkspaceMount*` variants and the two host-path
  `MountRuleViolation` variants, sourced from `MountConfig::config_source()` with the same
  repo-config fallback `ImageConfig::declared_in` uses
  (`crates/outrig/src/config/mod.rs`).
- The sidecar path too: `check_mount_list` is shared, and `validate_sidecar` wraps its violation
  whole into `ConfigValidationError::SidecarMount`, so the rendering has to carry through both
  callers rather than only the workspace one.
- Tests extending `mod config_path_provenance` in `crates/outrig/tests/config_merge.rs`, which
  already has `global_mount_is_not_satisfied_by_a_repo_path` -- the exact case whose message is
  currently misleading.
- Regenerate `crates/outrig/public-api.txt` and add a `### Changed` CHANGELOG entry; this is a
  breaking change to a public enum, unlike 0097.

## Acceptance

- A global `[[workspace.mounts]]` with a missing `host-path` reports the path *and* the file that
  declared it, so `global_mount_is_not_satisfied_by_a_repo_path` in
  `crates/outrig/tests/config_merge.rs` no longer produces a misleading message.
- A concatenated list holding one global entry and one repo entry reports each failure against its
  own declaring file, not against whichever config was loaded last.
- The sidecar path carries it too: a failing `[[sidecars.<name>.mounts]]` surfaces `declared_in`
  through `ConfigValidationError::SidecarMount` rather than losing it at the wrapping boundary.
- Every reshaped variant carries `#[non_exhaustive]` afterwards, so the next field is additive.
- `crates/outrig/public-api.txt` is regenerated and `crates/outrig/CHANGELOG.md` records the enum
  reshape under `### Changed` in the existing `[Unreleased]` section.

## Design forks

1. **Tuple variants vs. struct variants for `MountRuleViolation` -- Recommended: convert to
   struct variants.** Adding a second `PathBuf` to `HostMissing(PathBuf)` gives
   `HostMissing(PathBuf, PathBuf)`, which is unreadable at the match site and at the `Display`
   impl. Since the variants are changing shape either way, converting to named fields costs
   nothing extra and is what the rest of `ConfigValidationError` already looks like.

2. **Seal the variants while breaking them -- Recommended: yes.** If these variants are being
   reshaped once, they should carry `#[non_exhaustive]` afterwards for the same reason 0094 gave
   the image variants theirs, so the next field is additive. 0094 Decision 6's warning does not
   apply: these are return-only error variants that no downstream caller constructs.

3. **Whether `ContainerNotAbsolute` / `ContainerRoot` / `ContainerDuplicate` also gain it --
   Open.** A container-path rule violation is about the value itself, not about where a host
   directory was looked for, so provenance adds less. But an enum where half the variants can
   name their file and half cannot is the inconsistency this task exists to remove.

## Dependencies

None hard, and **not** blocked on a release boundary -- that was the first reading and it is
wrong. The crate is `0.2.0-rc.1` and `crates/outrig/CHANGELOG.md`'s `[Unreleased]` section
already reshapes this very enum: `SidecarNameInvalid`, `SidecarImageEmpty`, `SidecarMount`, and
`SidecarArgsWithoutEntrypoint` lost their `image` field, and `McpSidecarRequiresCommand` was
removed outright. The breaking window for `ConfigValidationError` is open now, so this is one
more bullet in a `### Changed` section rather than a `0.3.0` item.

0097 was additive because *it* was required to be, not because the enum was frozen. The real
cost of deferring past `0.2.0` is that the window closes and this waits for the next one.

## See also

- `crates/outrig/src/config/validate.rs` -- `MountRuleViolation`, `check_mount_list`,
  `validate_workspace_mounts`, `validate_sidecar`.
- `plan/done/0097-config-path-provenance.md` -- `declared_in` on the image errors, and the
  decision recording why mounts were left out.
- `plan/done/0094-non-exhaustive-sweep.md` -- Decisions 5 and 6, the sealing rules this follows.
