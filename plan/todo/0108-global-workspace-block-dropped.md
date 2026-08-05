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
