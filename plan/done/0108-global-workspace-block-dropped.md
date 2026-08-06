# 0108 -- Global `[workspace]` primary fields are silently discarded

## Context

`merge` takes the repo's workspace struct whole (`crates/outrig/src/config/merge.rs:41`):

```rust
let mut workspace = repo.workspace;
let mut mounts = global.workspace.mounts;
mounts.extend(workspace.mounts);
workspace.mounts = mounts;
```

Only `mounts` is combined. `host_path` and `container_path` come from `repo.workspace`
unconditionally -- and `Workspace` is `#[serde(default)]`, so a repo config with **no
`[workspace]` table at all** still contributes `Workspace::default()` (`.` ->  `/workspace`,
`crates/outrig/src/config/mod.rs:446`). A global

```toml
[workspace]
container-path = "/src"
```

is therefore parsed, validated, merged, and thrown away. Nothing warns.

`doc/reference/config.md` describes this as intentional -- "`[workspace]` primary fields are
repo-owned: the repo config's `host-path` and `container-path` win as a block" -- but "win as a
block" reads as *repo overrides global when both are set*, not *global is unreachable*. The
documented behavior and the implemented behavior differ precisely when the repo is silent, which
is the case a user setting a machine-wide `container-path` would expect to work.

Found while establishing that `Workspace` needs no `ConfigSource` in
`plan/done/0097-config-path-provenance.md` -- the absence of a base-directory problem here is a
consequence of this bug, not of a design decision.

## Goal

Make the global `[workspace]` block either work or fail loudly, and make the doc match.

## Deliverables

- Decide the rule (see fork 1) and implement it in `merge`.
- Whichever rule wins, `doc/reference/config.md`'s "Resolution: which file wins" section states
  it in terms of what happens when the repo config is *silent*, not only when both declare.
- Tests in `crates/outrig/tests/config_merge.rs`'s `mod config_merge`: a global-only
  `[workspace]`, and a both-declare case pinning repo precedence.
- If the accepted rule is per-key precedence, `Workspace` gains a `ConfigSource` after all, since
  a global `host-path` would then be relative to `~/.outrig/` -- the 0097 rule applied to a key
  0097 could correctly skip. `Workspace::resolved_host_path`
  (`crates/outrig/src/config/mod.rs`) is the one call site to change.

## Acceptance

- A global `[workspace]` with no repo `[workspace]` at all behaves the way the chosen rule says,
  and a test in `mod config_merge` pins it -- today this case silently yields
  `Workspace::default()`.
- A both-declare case pins repo precedence, so whatever changes for the silent case, the
  documented block-wins behavior is unchanged where both files speak.
- `doc/reference/config.md`'s "Resolution: which file wins" states the rule in terms of the repo
  config being *silent*, not only in terms of both declaring.
- `mounts` still concatenates global-then-repo, unchanged.
- If the rule is "reject", the error names the global file; a config carrying a global
  `[workspace]` today fails loudly rather than being quietly ineffective.

## Design forks

1. **Per-key precedence vs. reject vs. keep-and-document -- Open.** Per-key
   (`repo.host_path.or(global.host_path)`) matches how every scalar at the top level already
   merges (`merge.rs:56-63`) and is the least surprising, but needs `Workspace`'s fields to
   become `Option` to distinguish "absent" from "defaulted", which touches the serde defaults and
   `Workspace::new`. Rejecting a global `[workspace]` outright is a two-line change and is
   defensible -- a machine-wide workspace path is a strange thing to want -- but it breaks any
   config that has one today, silently ineffective though it is. Keeping the behavior and only
   fixing the doc is the cheapest and the least satisfying.

2. **Whether `mounts` stays asymmetric -- Resolved: yes.** Concatenating extra mounts while
   replacing the primary pair is deliberate and documented, and 0097 made the concatenated case
   correct. Nothing here changes it.

## Dependencies

- **Soft: 0097.** Already landed. If fork 1 chooses per-key precedence, this task consumes 0097's
  `ConfigSource` for `Workspace`; the two are otherwise independent.

## See also

- `crates/outrig/src/config/merge.rs:41-45` -- the three lines in question.
- `crates/outrig/src/config/mod.rs` -- `Workspace`, its `Default`, and `resolved_host_path`.
- `plan/done/0097-config-path-provenance.md` -- where this was found, and why `Workspace` was
  left out of the provenance sweep.

## Decisions

- Primary workspace fields merge per key: a repo declaration wins, then a global declaration,
  then the built-in default. This matches top-level scalar precedence and makes a global-only
  workspace effective without preventing a repo from overriding either field independently.
- `host_path` and `container_path` become `Option<PathBuf>`, with `host_path()` /
  `container_path()` accessors applying the documented `.` and `/workspace` defaults. `None`
  *is* "the file did not declare this key", so the state the merge runs on is the state the type
  carries -- no flag can go stale against the value beside it.

  This reverses the call made mid-task. The first cut kept the public `PathBuf` fields and hid
  declaration in two private booleans, on the grounds that a field-type change is a public break.
  It is, but `#[non_exhaustive]` does not cover field types and the queue is pre-freeze work for
  `0.2.0`, so the break is free now and a major version later; sibling task 0110 takes the same
  class of break on purpose. The bespoke encoding would have outlived the window that made it
  avoidable.

  What it deletes: the two booleans, the `WorkspaceDef` shadow struct, both hand-written serde
  impls, and the hand-written `Default`. What replaces them: plain derives with
  `skip_serializing_if = "Option::is_none"`, and an `inherit_missing_primary_fields` that is two
  `is_none` checks. Five call sites moved to the accessors.
- The two primary fields are *private*, behind `host_path()` / `container_path()` (effective
  value), `declared_host_path()` / `declared_container_path()` (declaration state), and
  `set_host_path()` / `set_container_path()`. `host_path` is paired with the `ConfigSource` it
  resolves against, and a public field lets a caller replace the value while leaving the pairing
  behind -- the replacement would then resolve against a directory it never came from, and the
  primary mount is read-write. `set_host_path` clears the source; a hand-set value belongs to no
  config file. This diverges from `MountConfig` and `ImageConfig`, which still carry the same
  hazard on their public `host_path`; `plan/next/` should pick that up for all three.
- `Workspace` records the source of the selected `host-path`. An inherited relative global path
  therefore resolves beside the global config; repo and programmatically built paths retain the
  existing repo-root fallback. `set_config_source` stamps only a *declared* `host-path`, since
  the built-in `.` belongs to no file.
- `Config::load` makes the global config path absolute *before* reading it, and reads through the
  resolved path. `--global-config` accepts any path, and a relative one left every inherited path
  -- the primary bind mount included -- meaning whatever the working directory was at launch.
  Resolving after the read would have left a narrower version of the same bug: two consultations
  of a process-global working directory, so a change in between could load one file and stamp
  another's origin. `std::path::absolute` is lexical, so no symlink is resolved and no I/O is
  done. Pre-existing since 0097 for images and mounts; this task widened its blast radius to the
  read-write primary mount, so it is fixed here.
- An unresolvable global path is an error, not an empty config. A *missing* file still loads as
  empty -- that contract is unchanged and tested -- but a path that cannot be given a meaning at
  all (empty, or an unreadable working directory) can only be honored by discarding an explicit
  `--global-config`, which is the exact failure mode this task exists to remove.
- Serializing a *merged* config is a lossy flattened snapshot, and is documented as such rather
  than made lossless. Provenance is `#[serde(skip)]`, so an inherited relative path is written as
  the text its source file used and re-reads against the repo root. Nothing in outrig writes a
  merged config back to disk; `outrig init` serializes a freshly built one.
- Extra mounts keep their existing asymmetric merge: global entries first, then repo entries.
- `Config::workspace` gains `skip_serializing_if = "Workspace::is_default"`, the predicate
  `NetworkConfig` and `ContainerSecurity` already use. Without it a config that declares no
  workspace serializes as a bare `[workspace]` table -- harmless to re-parse, but noise the
  block's neighbours all suppress.
- `source_base_dir` factors out "declaring file's directory, else `repo_root`" beside
  `resolve_against`. `Workspace` made that idiom's third copy; `MountConfig::resolved_host_path`
  and `ImageConfig::base_dir` now share it.
