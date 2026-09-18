# 0002-42 -- A provenance-bearing path and its base directory move together

## Context

0002-20 gave `ImageConfig` and `MountConfig` a private `ConfigSource`, recorded at load, that their
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
of the file that did not contain it. The result is a bind mount of the wrong host directory,
read-write if the mount says so.

0002-31 hit the same problem when it added provenance to `[workspace].host-path` and closed it there
by making the primary fields private behind accessors and a `set_host_path` that clears the
source. That leaves the codebase answering the same question two ways.

The 0.2.0 audit reproduced both halves downstream: a global mount whose `host_path` was
reassigned resolved under the *global config's* directory rather than the supplied repo root, and
the equivalent `ImageConfig` mutation did the same. It is filed as a release blocker for a reason
this entry only implied: `#[non_exhaustive]` does not make privatizing a public field
compatible, so the window for this fix closes when 0.2.0 freezes.

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
  - **Per-path provenance.** `dockerfile` and `context` each carry their own
    `Option<ConfigSource>`, after which independent setters are safe. More surface, but the only
    shape that lets a caller swap one path and leave the other alone.
- **The replacement accessors are named, not implied.** `MountConfig::host_path()` as the getter
  and `set_host_path` as the setter, mirroring `Workspace`; for `ImageConfig`, whichever pair
  fork 2 selects. The migration note has to tell a downstream caller what to write instead, and
  "use the accessor" is not that.
- **The TOML keys do not move.** Making a Rust field private changes `Serialize`/`Deserialize`
  and `JsonSchema` derivation unless the rename attributes come with it. `host-path`,
  `dockerfile`, and `context` must survive the change, asserted as *semantic* equality plus
  explicit key presence -- byte-identical output is a stronger claim than needed and breaks on an
  unrelated serializer change.
- Existing readers move to the getters. `MountConfig::new` and `ImageConfig`'s constructors are
  unchanged: they already produce `source: None`.
- `crates/outrig/public-api.txt` regenerated; `crates/outrig/CHANGELOG.md` records the
  privatization as a break with the accessor to use instead.

## Acceptance

- Replacing a loaded mount's `host-path` and then calling `resolved_host_path` resolves against
  the `repo_root` argument, not the old declaring file's directory -- mirroring
  `set_host_path_clears_inherited_global_provenance` in `crates/outrig/tests/config_merge.rs`.
- For `ImageConfig`, a test that mutates `dockerfile` and `context` **independently** and asserts
  the untouched one still resolves against its declaring file's directory. Under the atomic-pair
  shape the equivalent is a *compile-fail* test or an API-snapshot assertion that no single-path
  setter exists -- a runtime assertion cannot check the absence of a function.
- **Serde and schema round trip.** A config loaded, mutated through the new setters, serialized,
  and reparsed yields an equal config and the same resolved paths, with `host-path`,
  `dockerfile`, and `context` present by name in both the TOML and the generated schema.
- **A downstream compile fixture, not `e2e`-gated**, that reads and writes these paths through
  the new accessors -- the migration a consumer has to perform, proven to compile. It belongs
  beside `config_merge.rs` rather than in `library_surface.rs`, which is `#![cfg(feature =
  "e2e")]` and would not run.
- **Diagnostics do not blame the wrong file.** `ConfigValidationError`'s `declared_in` clause
  (`crates/outrig/src/config/validate.rs:22-28`) is rendered from the same provenance. After a
  mutation clears the source, an error about the new value must not name the old declaring file.
- **Mixed and absolute cases.** Under per-path provenance: what `config_source()` means when the
  two image paths have different sources, tested rather than left to the reader. Under either
  shape: replacing a relative path with an absolute one, and a `clone`/`merge` round trip, both
  preserve the pairing.
- No behavior change for any path that is never reassigned, which is every in-tree caller.

## Design forks

1. **Whether the setters are worth it, versus making the types load-only -- Resolved: setters.**
   This entry originally left it open, on the grounds that nothing in outrig reassigns these
   fields and the hazard is a library-API one. The audit closed it: the hazard is real, it
   reproduced from a downstream consumer, and the alternative -- documenting the fields as
   read-after-load and leaving them public -- costs nothing and fixes nothing while freezing the
   public field in place for all of 0.2.x.

2. **Whether provenance should be per-field rather than per-struct -- Open, and blocking for
   `ImageConfig`.** `Workspace::source` already describes `host_path` alone, which is why its
   `set_config_source` is guarded while the other two are not, and why a single setter is safe
   there. `ImageConfig` is the case that forces the question: `dockerfile` and `context` share
   one `source`, so per-path setters and per-struct provenance cannot both be right. A
   `Sourced<PathBuf>` carrier would make the pairing structural for all three and retire the
   guard; the atomic-pair setter avoids it at the cost of a coarser API. `MountConfig` can land
   either way and does not need to wait on this. Note that per-path provenance forces a public
  answer to "what is this struct's source", which the atomic pair does not -- weigh that as part
  of the cost, not as a detail to settle during implementation.

3. **Whether `Model` joins this sweep -- Deferred to 0002-46, deliberately.** 0002-20's fork 3 said
   to revisit provenance for other path-bearing entries once one appeared, and `[models.<n>]`'s
   `model-path` is one. It is on a deprecated surface, so 0002-46 owns the decision; if 0002-46
   gives `Model` provenance, it inherits this task's stale-public-field problem and must adopt the
   same shape. Coordinate rather than letting two answers land.

## Dependencies

- **Hard: before the 0.2.0 freeze**, which is what puts it in this queue rather than the buffer.
  Coordinate fork 3 with 0002-46.
  Making a public field private is a break `#[non_exhaustive]` does not cover, same as 0002-31's
  field-type change. After the freeze this costs a major version and is probably not worth it.
- Land before 0002-48 regenerates the API snapshot.

## See also

- `crates/outrig/src/config/mod.rs` -- `MountConfig`, `ImageConfig`, and `Workspace`'s accessors
  as the shape to copy.
- `plan/done/phase/0002-sidecars/tasks/0002-20-config-path-provenance.md` -- where the public-field
  choice was made.
- `plan/done/phase/0002-sidecars/tasks/0002-31-global-workspace-block-dropped.md` -- where it was
  reversed for one type.

## Decisions

1. **Fork 2 resolved as the atomic pair, on a stronger ground than the entry gives.** The entry
   argues from the shared `source`; the decisive fact is that `ImageConfig::source()` *panics*
   unless `dockerfile` and `context` are both set or both absent, and `from_dockerfile` already
   takes them together. A per-path setter would therefore let a caller build a state the type
   already forbids -- it buys the ability to do something illegal. Per-path provenance also
   forces a public answer to "what does `config_source()` mean when the two disagree", and that
   accessor has two consumers: `declared_in`, which renders the `(declared in <file>)` clause,
   and the `outrig image ls` source column 0002-20 was designed for. One source, one answer.

   The `Sourced<PathBuf>` carrier was considered and rejected for this task. It would make the
   pairing structural for all three types, but its field must still serialize as a bare TOML
   string, which means hand-written `Serialize`/`Deserialize`/`JsonSchema` -- exactly the three
   impls 0002-31 counted as a deletion. It is also per-path provenance underneath, so it inherits
   the ambiguity above rather than resolving it.

2. **All three `MountConfig` fields went private, not just `host_path`.** Only `host_path` is
   paired with the source, so only it needed the break on hazard grounds. Taking the other two
   costs six accessors instead of two and leaves the struct reading one way rather than mixing an
   accessor-guarded field with two bare ones -- a reader no longer has to know which of three
   fields is special to know which syntax to use. It is free now and a major version later, which
   is the same argument that put the task in this queue. `Workspace::mounts` stays public because
   it is a `Vec` whose elements each carry their own provenance, not a scalar paired with one.

3. **No compile-fail test; `public-api.txt` records the absence instead.** The acceptance offers
   a compile-fail test *or* an API-snapshot assertion. The repo has no `trybuild`, no `tests/ui/`,
   and no dev-dependency that would give it one, so the first option means adding a proc-macro
   test harness and a new dependency to assert that one function does not exist. The snapshot
   already enumerates `ImageConfig`'s surface line by line, and 0002-48 turns it into an enforced
   gate; a reviewer reading that block sees `set_build_paths` and no siblings.

4. **`validate.rs` moved to the accessors even though it did not have to.** It is a child module
   of `config`, so it can still see the private fields and compiled unchanged. Leaving it would
   have made the getters the rule for everyone except the one file that reads these values most,
   which is how a convention stops being one. The migration is `field.clone()` becoming
   `accessor().to_path_buf()` -- the same allocation, since both sides needed an owned `PathBuf`
   for the error variant.

5. **`ImageConfig`'s two `.unwrap()`s became `.expect("build path")`.** `image.dockerfile()`
   returns `Option<&Path>` rather than a clonable `Option<PathBuf>`, so the call had to be
   rewritten anyway; it now carries the same message `resolved_build_paths` uses two lines above
   for the identical invariant.

6. **`config_accessors.rs` is a new file, not a module in `config_merge.rs`.** An integration test
   already compiles against `outrig` as an external crate, so `config_merge.rs` would have proved
   the same thing. A separate file says what it is: the migration a consumer performs, in one
   place, with nothing else in it to read past. It also carries the schema assertions, which are
   new coverage -- nothing tested `schema_for!` output before, and `get_config_schema` publishes
   `schema_for!(ImageConfig)` to agents, so a key that silently moved would be advertised as
   writable and then rejected on write.

7. **The tests were checked against a disabled fix.** With `self.source = None` removed from both
   setters, five of the six new `config_path_provenance` tests fail. The sixth,
   `mount_provenance_free_setters_leave_the_base_alone`, correctly still passes: it asserts that
   `set_container_path` and `set_access` leave the base *unchanged*. The four
   `config_accessors.rs` tests also still pass, which is right -- they pin the surface and the
   serde/schema keys, not the clearing.

8. **`Workspace` gained nothing.** It already had this shape, and its `set_config_source` guard
   (`if host_path.is_some()`) has a reason `MountConfig` and `ImageConfig` do not share: its
   `host_path` has a built-in default (`.`) that belongs to no file, so stamping an undeclared
   workspace would move it off the repo root. A mount or image entry only exists because a file
   declared it, so there is nothing to guard against. The asymmetry is load-bearing, not drift.

9. **`print_build_header` takes two `&Path`s instead of an `ImageConfig`.** The mechanical
   migration had turned `cc.dockerfile.as_ref().expect("build path")` into
   `cc.dockerfile().expect("build path")`, preserving an assertion its only caller had already
   discharged: that caller is inside an `ImageSourceRef::Build { .. }` arm, where both paths are
   bound as non-`Option` `&Path`. Binding them and passing them deletes both `expect`s rather
   than relocating them. The equivalent cleanup in `validate_image_source` does *not* work the
   same way -- that function is what proves the shape, so it cannot call `source()`, which panics
   on the shapes it exists to reject -- and is filed as
   `plan/next/validate-image-source-re-asserts-its-own-shape.md`.

10. **`global_mount_project()` joins `global_image_project()`.** Four new tests opened with a
    byte-identical global mount declaration plus `create_dir_all(global/shared)`. The image side
    of the same feature already had that helper ten lines up; adding the mount-side mirror was
    the shape the file was asking for, not a new abstraction.

11. **Two hazards were seen and deliberately left.** `image_name` stays public, so a caller can
    still reach the state `ImageConfig::source` panics on -- and `set_build_paths` on a
    pull-source config now does so through a supported method rather than a field write. That is
    a *shape* invariant rather than a provenance one, which is this task's subject, but it wants
    the same pre-freeze window; filed as `plan/next/image-source-shape-is-a-whole.md` so the
    decision to defer is a decision and not an oversight.
