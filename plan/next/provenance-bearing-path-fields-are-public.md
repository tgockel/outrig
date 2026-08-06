# Provenance-bearing `host_path` fields are public on two of three types

## Context

0097 gave `ImageConfig` and `MountConfig` a private `ConfigSource`, recorded at load, that their
resolution methods use as the base directory for relative paths. Both types left the path field
itself `pub`:

```rust
pub struct MountConfig {
    pub host_path: PathBuf,
    ...
    source: Option<ConfigSource>,   // private
}
```

The two are a pair -- the source only means anything as the base for *that* value -- but nothing
enforces it. A caller who does `mount.host_path = other` keeps the old source, and
`resolved_host_path` then resolves a value the config file never contained against the directory
of the file that did not contain it. The result is a bind mount of the wrong host directory.

0108 hit the same problem when it added provenance to `[workspace].host-path` and closed it there
by making the primary fields private behind accessors and a `set_host_path` that clears the
source. That leaves the codebase answering the same question two ways.

## Goal

One rule for every provenance-bearing path field: the value and its base directory move together.

## Deliverables

- `MountConfig::host_path` becomes private, with a getter and a `set_host_path` that clears
  `source` -- a direct mirror of `Workspace::set_host_path`, because one source backs one path.
- `ImageConfig` is **not** a mirror, and this is the part to get right. One `source` backs *two*
  relative paths, `dockerfile` and `context`, so a per-path setter that clears it would rebase
  the sibling it did not touch from the declaring file's directory to `repo_root` -- turning a
  fix into a second instance of the same bug. Resolve fork 2 before writing any `ImageConfig`
  setter. The two shapes that work:
  - **Atomic pair.** One `set_build_paths(dockerfile, context)` that replaces both and clears
    `source`. Smallest change; forbids replacing one path alone, which is arguably the honest
    constraint given the fields share a base.
  - **Per-path provenance.** `dockerfile` and `context` each carry their own `Option<ConfigSource>`,
    after which independent setters are safe. More surface, but the only shape that lets a caller
    swap one path and leave the other alone.
- Existing readers move to the getters. `MountConfig::new` and `ImageConfig`'s constructors are
  unchanged: they already produce `source: None`.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

- Replacing a loaded mount's `host-path` and then calling `resolved_host_path` resolves against
  the `repo_root` argument, not the old declaring file's directory -- mirroring
  `set_host_path_clears_inherited_global_provenance` in `crates/outrig/tests/config_merge.rs`.
- For `ImageConfig`, a test that mutates `dockerfile` and `context` **independently** and asserts
  the untouched one still resolves against its declaring file's directory. Under the atomic-pair
  shape this is instead a test that no single-path setter exists to call.
- No behavior change for any path that is never reassigned, which is every in-tree caller.

## Design forks

1. **Whether the setters are worth it, versus making the types load-only -- Open.** Nothing in
   outrig reassigns these fields; the hazard is a library-API one. The alternative to setters is
   documenting the fields as read-after-load and leaving them public, which costs nothing and
   fixes nothing. 0108 chose setters for `Workspace` because the primary mount is read-write and
   the blast radius is the whole repo tree; extra mounts and image build paths are smaller
   targets, so the calculation is not automatically the same.

2. **Whether provenance should be per-field rather than per-struct -- Open, and blocking for
   `ImageConfig`.** `Workspace::source` already describes `host_path` alone, which is why its
   `set_config_source` is guarded while the other two are not, and why a single setter is safe
   there. `ImageConfig` is the case that forces the question: `dockerfile` and `context` share
   one `source`, so per-path setters and per-struct provenance cannot both be right. A
   `Sourced<PathBuf>` carrier would make the pairing structural for all three and retire the
   guard; the atomic-pair setter avoids it at the cost of a coarser API. `MountConfig` can land
   either way and does not need to wait on this.

## Dependencies

- **Soft: before the `0.2.0` freeze.** Making a public field private is a break that
  `#[non_exhaustive]` does not cover, same as 0108's field-type change. After the freeze this
  costs a major version and is probably not worth it.

## See also

- `crates/outrig/src/config/mod.rs` -- `MountConfig`, `ImageConfig`, and `Workspace`'s accessors
  as the shape to copy.
- `plan/done/0097-config-path-provenance.md` -- where the public-field choice was made.
- `plan/done/0108-global-workspace-block-dropped.md` -- where it was reversed for one type.
