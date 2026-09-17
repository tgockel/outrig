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

## Decisions

1. **Fork 1 -- struct variants, as recommended.** All five `MountRuleViolation` variants took
   named fields. The alternative, `HostMissing(PathBuf, PathBuf)`, is unreadable at the match
   site and worse in the `Display` impl, where thiserror's positional `{0:?}` / `{1:?}` gives
   the reader nothing to go on. The variants changed shape either way.

2. **Fork 2 -- sealed, as recommended.** Every reshaped variant carries `#[non_exhaustive]`.
   0094 Decision 6's warning does not apply: these are return-only error variants, so sealing
   removes no construction path that anyone had.

3. **Fork 3 -- resolved *yes*: every mount rule gets `declared_in`, not just the two host-path
   ones.** `ContainerRoot` therefore stops being a unit variant, which is the single largest
   break in the set. The reasoning is that the clause does not answer "what did this relative
   path resolve against" -- it answers "which file do I go edit", and that question is identical
   for all five rules. `ContainerRoot` is the strongest case rather than the weakest: its
   message carries no path *at all*, so the declaring file is the only handle it offers. Half a
   sealed enum able to name its file is the asymmetry this task exists to remove, and repeating
   it one level down would have been the same mistake in miniature.

4. **A duplicate names the rejected entry, not both sides of the collision.** `check_mount_list`
   detects duplicates against a `BTreeSet<PathBuf>` of already-claimed paths that carries no
   provenance -- the claim may have come from the other config file *or* from the block's own
   reserved set (the primary workspace mount). Teaching `reserved` to carry sources so the
   message could name both is a larger change for marginal benefit, and the entry being refused
   is the one the user edits. The narrower claim is written into the field's doc comment so a
   later reader does not assume the wider one.

5. **`ConfigValidationError::SidecarMount` is unchanged and deliberately still unsealed.** It
   wraps the violation whole, so the clause arrives through the violation's own `Display` with
   no code change at the wrapping boundary -- which is why that path needed a test rather than
   an edit. Sealing it too would have been a gratuitous break: 0094 Decision 5 seals a variant
   only where a field addition is proven, and none is proven here.

6. **The `WorkspaceMount*` variants were not collapsed into a single wrapping variant.** The
   temptation is real: `SidecarMount` already wraps `MountRuleViolation`, so five flat variants
   plus a five-arm map restate the same five rules twice, and `#[error("workspace mount
   {violation}")]` would have rendered byte-identically while deleting ~40 lines. Rejected on
   three grounds -- it *deletes* public variants rather than reshaping them, which is a strictly
   larger break for a crate with downstream integration consumers; the task's acceptance names
   these variants as reshaped-and-sealed; and 0094 Decision 5's principle that the enum *is* the
   validation documentation favors a flat list that reads as one line per rule.

7. **`declared_in` has no repo-config fallback**, matching `ImageConfig::declared_in` rather
   than `MountConfig::resolved_host_path`. The two look similar and are not: a base directory
   may sensibly default to the repo root, but a filename in an error message is a *claim*, and
   naming a config that never mentioned this mount would be a fabrication. A hand-built
   `MountConfig` reports `None` and renders no clause -- asserted directly, in six
   `mod config_validate` tests that build their configs with `parse`.

8. **Verification that the tests test something,** per 0097 Decision 12. With
   `MountConfig::declared_in` stubbed to return `None`, all five new/extended provenance tests
   fail and the other 132 pass. Run before trusting a green suite.

9. **An independent implementation converged on the same shape, which is why fork 3 can be
   considered settled rather than merely chosen.** A second pass written without sight of the
   first produced a structurally identical `src/` half -- same accessor, same struct variants,
   same sealing, same per-entry stamping, `validate_sidecar` untouched -- and resolved fork 3
   the same way. Two of its test choices were better and were adopted: the concatenated-list
   test is written as two flat scenarios rather than behind a local helper struct and enum, and
   the no-source contract is asserted **once** on the whole rendered string
   (`a_config_without_a_source_renders_no_clause`) instead of as a repeated
   `assert_eq!(declared_in, None)` in six rule tests that are not about provenance. The single
   exact-string assertion is the stronger claim anyway, and it is sufficient because
   `declared_in_clause` is shared by all ten variants.

## See also

- `crates/outrig/src/config/validate.rs` -- `MountRuleViolation`, `check_mount_list`,
  `validate_workspace_mounts`, `validate_sidecar`.
- `plan/done/0097-config-path-provenance.md` -- `declared_in` on the image errors, and the
  decision recording why mounts were left out.
- `plan/done/0094-non-exhaustive-sweep.md` -- Decisions 5 and 6, the sealing rules this follows.
