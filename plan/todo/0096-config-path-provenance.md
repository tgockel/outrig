# 0096 -- Config path provenance: resolve relative paths against the declaring file

## Context

outrig reads two config files with one schema -- `.agents/outrig/config.toml` and
`~/.outrig/config.toml` -- and merges them by name with repo precedence
(`crates/outrig/src/config/merge.rs:26`). The merge is a flat `BTreeMap::extend` per map:

```rust
let mut images = global.images;
images.extend(repo.images);
```

After that line nothing knows which file an entry came from. Every consumer then resolves paths
against a single `repo_root` threaded down from the CLI:

```rust
// crates/outrig/src/image.rs:211-212
let dockerfile = repo_root.join(cfg.dockerfile.as_ref().expect("build path validated"));
let context = repo_root.join(cfg.context.as_ref().expect("build path validated"));
```

The same assumption sits in `validate_image_source`'s on-disk existence checks
(`crates/outrig/src/config/validate.rs:1206-1222`), in `LaunchSpec::from_config`
(`crates/outrig/src/outrig_.rs:316`), and in `check_mount_list` for
`[[workspace.mounts]].host-path` and `[sidecars.<sc>.mounts]`.

The consequence: a global `[images.<name>]` using the build shape is *schema-legal today* --
both files deserialize into the same `Config` with `deny_unknown_fields` -- but its
`dockerfile` and `context` are joined onto the **repo root**, so it either fails with
`DockerfileMissing` or, worse, silently picks up a same-named path inside the repo. Only the
`image-name` shape works from global config. `doc/reference/config.md` documents the repo-root
rule in five places (lines 379-384, 415-416, 811, 832-833) and never says global build images
are unusable, because the interaction was never considered.

This is the prerequisite for a user-level image library
(`plan/next/user-image-library.md`): a scanned `~/.outrig/images/<name>/` project is precisely
an image-config whose paths are relative to something that is not the repo root. It also fixes
the latent global-`[[workspace.mounts]]` case on its own.

## Goal

Resolve every relative path in a config against the directory of the file that declared it, and
carry that provenance far enough that diagnostics can name the declaring file.

## Deliverables

- A `ConfigSource` recorded per entry, with a `base_dir()` accessor. Three variants to start:
  the repo root, the global config's directory, and a standalone project directory (the third
  exists so `plan/next/user-image-library.md` needs no further change to this type).
- Population in `Config::load_unvalidated` (`crates/outrig/src/config/mod.rs:159`), stamped onto
  each entry of `repo_cfg` and `global_cfg` **before** `merge(global_cfg, repo_cfg)` runs, since
  merge is where origin is lost.
- `ImageConfig` carries its source. So does each mount entry, for
  `[[workspace.mounts]]`/`[sidecars.<sc>.mounts]`, whose `host-path` has the same problem and
  whose global and repo lists are concatenated rather than overridden
  (`merge.rs:43-45`) -- a concatenated list is exactly the case where a single base directory
  cannot be correct for every element.
- Threading through the consumers so `repo_root` becomes a fallback for entries with no recorded
  source rather than the sole base: `image.rs` (`compute_tag_with_build_args`,
  `build_image_cmd`, `build_image_for`, `ensure_image_for`), `validate.rs`
  (`validate_image_source`, `check_mount_list`), and `outrig_.rs:316 LaunchSpec::from_config`.
- Path-bearing validation errors name the declaring file. `DockerfileMissing { image, path }`
  and `ContextMissing` today report a bare relative path, which for a global entry reads as a
  repo problem.
- Docs: `doc/reference/config.md` and `doc/concepts/containers.md:76-85` replace "relative to
  the repo root" with the declaring-file rule. Both are **symlinks** into
  `crates/outrig-cli/src/mcp_self/docs/`; edit the targets.

## Runtime behavior

| Declared in                     | Relative paths resolve against         |
|---------------------------------|----------------------------------------|
| `.agents/outrig/config.toml`    | repo root (unchanged)                  |
| `~/.outrig/config.toml`         | that file's directory (`~/.outrig/`)   |
| `--global-config <path>`        | `<path>`'s parent directory            |
| a standalone `image.toml`       | the project directory                  |

Absolute paths and `~`-prefixed host paths are unaffected. Repo-declared behavior is unchanged
in every case, so no existing config moves.

## Acceptance

- A global `[images.x]` with `dockerfile = "images/x/Dockerfile"` and `context = "images/x"`
  validates and builds from `~/.outrig/images/x/`, from a repo anywhere on disk.
- `--global-config /tmp/g/config.toml` resolves that file's relative image paths under `/tmp/g/`,
  with no reference to `$HOME` -- this is what makes the behavior testable without touching a
  real home directory.
- A global `[[workspace.mounts]]` entry with a relative `host-path` resolves against the global
  config's directory, while repo mount entries in the same concatenated list still resolve
  against the repo root.
- Repo-declared paths resolve exactly as before; existing repo configs and the fixtures under
  `crates/outrig/tests/fixtures/` need no edits.
- `DockerfileMissing` / `ContextMissing` name the config file that declared the path.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **Carrying provenance vs. rewriting paths -- Recommended: carry it.** The cheaper change is
   to rewrite every relative path to an absolute one during load, leaving the consumers alone.
   It is genuinely less invasive to the type surface. It is rejected as the lean because two
   downstream consumers need the origin itself, not just a correct path:
   `plan/next/user-image-library.md` warns when a repo image-config shadows a library project
   (naming both), and `outrig image ls` prints a source column. Rewriting throws that away and
   would have to be re-derived by string-prefix comparison against the config directories.
   Confirm before committing: if the shadow diagnostic can be satisfied some other way, absolute
   rewriting is the smaller diff.

2. **Where the field lives -- Resolved: `#[serde(skip)]` on the config structs.** `ImageConfig`
   uses `deny_unknown_fields`; a skipped field is never deserialized, so the two compose and no
   TOML key is introduced. The alternative -- a parallel `BTreeMap<String, ConfigSource>` on
   `Config` -- keeps the entry structs pure but has to be kept in step by hand through every
   merge and clone.

3. **`ConfigSource` for providers/models/agents -- Open.** Only images and mounts have paths
   today, so only they need a base directory. Stamping every map has a cost in churn and buys
   nothing until some other entry grows a path field. Start narrow.

## Dependencies

- **0094.** This task adds a `#[serde(skip)]` field to `ImageConfig` and changes the shape of
  `DockerfileMissing` / `ContextMissing`. Both are breaking changes today and both are insulated
  once 0094's `#[non_exhaustive]` sweep has landed, so the sweep goes first and this becomes
  additive.

Otherwise independent of everything in the queue, and a strict prerequisite for
`plan/next/user-image-library.md` and `plan/next/user-toolsets.md`.

## See also

- `crates/outrig/src/config/merge.rs` -- `merge`, where provenance is discarded.
- `crates/outrig/src/config/mod.rs` -- `Config::load_unvalidated`, the one place both files are
  read and the only place to stamp a source.
- `crates/outrig/src/image.rs` -- `compute_tag_with_build_args` and the other `repo_root.join`
  sites.
- `crates/outrig/src/config/validate.rs` -- `validate_image_source`, `check_mount_list`.
- `plan/next/user-image-library.md` -- the consumer that makes this load-bearing.
- `plan/done/0007-image-build.md` -- why `ensure_image` joins against the root without
  `canonicalize` (intentional symlinks are preserved); the same rule applies to any new base.
