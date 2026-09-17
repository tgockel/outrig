# 0051 -- `image-name` field on `[containers.<name>]`

## Context

Every `[containers.<name>]` block in `outrig.toml` requires a `dockerfile` +
`context` pair today (`src/config/mod.rs:160-169`), and `outrig run` always
shells out to `buildah build` via `image::ensure_image`
(`src/image.rs:141-160`, called from `src/cli/run.rs:87-89`). That fits the
"agent gets a custom toolchain image baked from the repo" use case the v0
docs describe (`doc/concepts/containers.md:67-89`).

It does *not* fit the cases where the user already has the image they want
to run:

- A registry-published image (`ghcr.io/org/agent-runner:nightly`) maintained
  by some other pipeline.
- A common base used as-is for a quick experiment (`docker.io/library/ubuntu:24.04`,
  `alpine:3.20`).
- An image built by external CI and pushed to an internal registry, where
  the Dockerfile lives in a different repo.

Today the workaround is a one-line `Dockerfile` (`FROM <ref>`) plus a
context directory containing nothing but that file. That's friction for no
benefit -- the build hash is dominated by the `FROM` line, the context
contributes nothing, and `buildah build` runs anyway.

## Goal

Add `image-name = "<ref>"` as an alternative to `dockerfile` + `context` on
a `[containers.<name>]` block. When set, outrig pulls the image if it's not
already local and runs the container against the pulled tag, skipping
`buildah build` entirely. Existing build-from-Dockerfile configs are
unaffected.

Non-goals: configurable pull policy (always/missing/never), digest
verification beyond what podman already does, mirroring/registry auth knobs
(podman's existing config still applies).

## User surface

```toml
default-container = "coding"

# Build-from-Dockerfile -- existing form, unchanged.
[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec mcp-server-shell"]

# Use a pre-built image -- new form.
[containers.scratch]
image-name = "docker.io/library/ubuntu:24.04"

  [containers.scratch.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

Exactly one of these two shapes must be set on each block:

- `dockerfile` + `context` (with optional `build-args`) -- build path.
- `image-name` -- use-existing-image path.

Setting both, neither, or `image-name` alongside `build-args` is a
config-validation error. `[containers.<name>.mcp]` works the same way for
both shapes.

`outrig run` and `outrig build` both accept image-name configs:

```sh
$ outrig build --container scratch
[outrig] container-config: scratch
[outrig] image:            docker.io/library/ubuntu:24.04
[outrig] image ready (already pulled)

$ outrig run --container scratch
# (no buildah invocation; container starts directly)
```

A first-time pull streams `podman pull` output between the verbose header
and the `image ready` line, mirroring how the build path streams `buildah
build`.

## Architecture

### Config schema

`ContainerConfig` (`src/config/mod.rs:160-169`) becomes:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContainerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build_args: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerSpec>,
}
```

The XOR is enforced at validate time, not parse time. Provide a small
helper on the type so callers don't repeat the match:

```rust
pub enum ContainerSourceRef<'a> {
    Build  { dockerfile: &'a Path, context: &'a Path,
             build_args: &'a BTreeMap<String, String> },
    Image  { image_name: &'a str },
}

impl ContainerConfig {
    pub fn source(&self) -> ContainerSourceRef<'_> { ... }
}
```

`source()` panics if validation hasn't run -- which is fine because every
real call path already goes through `Config::load`.

### Validation

`src/config/validate.rs:183-198` (the per-container block) gains:

- Reject when neither `image_name` nor (`dockerfile` && `context`) is set.
- Reject when `image_name` is set *and* (`dockerfile` || `context` ||
  `build_args` non-empty).
- Reject when only one of `dockerfile` / `context` is set.
- Reject empty `image_name` (`""`).
- Path existence checks (`DockerfileMissing` / `ContextMissing`) only run
  on the build path -- skip when `image_name` is set.

New error variants on `ConfigValidationError`:

- `ContainerSourceMissing { container }` -- neither shape set.
- `ContainerSourceConflict { container, fields: Vec<&'static str> }` --
  e.g. `["image-name", "dockerfile"]`.
- `ContainerImageNameEmpty { container }`.
- `ContainerHalfBuilt { container, missing: &'static str }` -- only one
  of dockerfile/context set.

### `src/image.rs`

`ensure_image` (line 141) becomes a dispatcher over `cfg.source()`:

- `Build`: existing flow -- `compute_tag` (line 80) -> `probe_cached`
  (line 91) -> `build_image` (line 102). Unchanged.
- `Image`: new helpers -- `compute_tag` returns `ImageTag(name.to_string())`,
  `probe_pulled` runs `podman image exists <ref>` (mirrors `probe_cached`'s
  shape), `pull_image` runs `podman pull <ref>` and streams stderr through
  `process::run_streamed` with the `[podman]` prefix.

`ImageBuildOutcome` (line 38) is unchanged. `cache_hit = true` means "no
fetch/build was needed" for both paths.

`compute_tag` for image-name configs returns the user's literal string as
the tag. `Container::start` (`src/container/mod.rs:57`) takes
`&ImageTag` and passes it to `podman run`, which already accepts any
ref form (`name:tag`, `registry/name:tag`, `name@sha256:...`), so nothing
downstream needs to know the tag's provenance.

### `src/cli/build.rs`

`build_single` (line 80) and `build_all` (line 100) branch on
`cfg.source()`:

- Build path: existing behavior preserved.
- Image path: probe via `probe_pulled`. On hit, print
  `[outrig] image ready (already pulled: <ref>)`. On miss, call
  `print_build_header` (line 128) -- adapted to print an `image:` row
  instead of dockerfile/context/cache-key -- then `pull_image`, then
  `[outrig] image ready`.

`print_build_header` is generalized to accept either a build summary or
an image summary; an internal helper for each keeps the function's
formatting block contained.

`--no-cache` on `outrig build` for an image-name config: skip
`probe_pulled` and re-run `podman pull` so a moved tag is refreshed.
(Sub-decision: whether to also `podman rmi` first -- listed below.)

### `outrig run`

`src/cli/run.rs:87-89` already calls `image::ensure_image(container_cfg,
&repo_root, false)` and threads the resulting `ImageTag` into
`Container::start`. No change at the run-call site -- the dispatcher
inside `ensure_image` covers both shapes.

### Docs

- `doc/concepts/containers.md:67-89` -- add a sibling sub-section to the
  existing TOML example showing the image-name shape.
- `doc/concepts/containers.md:129-137` (the "Image caching" section) --
  note that image-name configs use podman's local image store directly;
  there is no `outrig-cache:<hash>` tag in that path.
- `doc/reference/config.md` -- the field table for `[containers.<name>]`
  picks up `image-name` with a one-line description and a "mutually
  exclusive with dockerfile/context/build-args" note.

## Sub-decisions

- **Schema shape: flat struct vs. untagged enum.** This spec recommends
  the flat struct + XOR validation because `serde(flatten)` interacts
  badly with `serde(untagged)` (silent variant fallback, worse parse
  errors), and `deny_unknown_fields` works cleanly on a flat struct.
  An untagged enum (`ContainerSource { Image, Build }`) would be more
  type-safe but the existing `McpServerSpec` precedent isn't flattened,
  so applying the pattern here imports a known footgun. Re-open if the
  flat approach grows a third variant.
- **Pull policy.** v0 is "missing" (pull only when not local). A
  `pull-policy = "always" | "missing" | "never"` field is the obvious
  follow-on once someone hits a pinned-tag use case where they want to
  force a refresh without `--no-cache`. Out of scope here.
- **`--no-cache` semantics for image-name.** Re-run `podman pull` and
  trust podman's "already up to date" path. Could also `podman rmi`
  first; that's stricter but punishes users with a pinned digest who
  legitimately want to re-pull metadata. Defer the rmi-first option.
- **Cache-hit summary wording.** `image ready (already pulled: <ref>)`
  vs. `image ready (cache hit: <tag>)` for the build path. They read
  differently on purpose; a future cleanup could unify them.
- **Digest verification.** `image-name = "name@sha256:..."` already
  works because podman accepts that ref form. Anything stronger
  (independent verification, signature checking) is out of scope.
- **Auth / registry config.** Out of scope -- defer to whatever
  `~/.config/containers/auth.json` and podman config already provide.

## Files

- `src/config/mod.rs` -- `ContainerConfig` field types change;
  `ContainerSourceRef` accessor added.
- `src/config/validate.rs` -- new XOR validation, new error variants on
  `ConfigValidationError`. Existing `DockerfileMissing` / `ContextMissing`
  paths gated on the build shape.
- `src/image.rs` -- `ensure_image` dispatches; `compute_tag` returns the
  literal name on the image path; new `pull_image` + `probe_pulled`
  helpers shaped after `build_image` / `probe_cached`.
- `src/cli/build.rs` -- `build_single`, `build_all`, and
  `print_build_header` cover both shapes.
- `doc/concepts/containers.md` -- TOML example + caching note updated.
- `doc/reference/config.md` -- schema table updated.
- `tests/config_validate.rs` (or wherever container validation lives
  today) -- parse/validate matrix covering: image-name only,
  dockerfile only, both forms set, neither set, image-name +
  build-args, half-built (dockerfile without context), empty
  image-name.
- `tests/` (e2e, behind `--features e2e`) -- a config with
  `image-name = "docker.io/library/alpine:3.20"`, run `outrig run` and
  assert (a) no `buildah build` is invoked, (b) the workspace bind-mount
  is reachable from inside the container.

## Acceptance

- A `[containers.<name>]` block with `image-name = "<ref>"` (and no
  dockerfile/context/build-args) parses, validates, and runs.
  `outrig run --container <name>` starts the container without
  invoking `buildah` at all.
- A block with only `dockerfile` (no `image-name`, no `context`) is
  rejected at validate-time with an error naming the missing field.
- A block with both `image-name` and `dockerfile` is rejected with an
  error listing the conflicting fields.
- A block with `image-name` and a non-empty `build-args` is rejected.
- A block with neither shape is rejected.
- An empty `image-name = ""` is rejected.
- `outrig build --container <image-name-config>` pulls the image if
  not present (streaming `[podman] ...` lines), prints
  `[outrig] image ready` on success, and prints
  `[outrig] image ready (already pulled: <ref>)` on a subsequent run.
- `outrig build --all` covers image-name configs in its per-line
  summary, distinguishing `(pulled in Ns)` from `(cache hit)` for
  image-name entries.
- `outrig build --no-cache --container <image-name-config>` re-pulls
  even when the image is already local.
- Existing dockerfile/context configs continue to behave exactly as
  before (no observable change in tag format, build output, or cache
  semantics).
- The `[containers.<name>]` documentation in
  `doc/concepts/containers.md` shows both forms with worked examples.

## Dependencies

None. This is a self-contained change against current `trunk`.

## Decisions

- **Flat struct + XOR validation** (as recommended by the spec). The
  `ContainerConfig` struct stays flat with all fields `Option`, and a
  `ContainerSourceRef` enum is derived by `source()` after validation.
  This keeps `deny_unknown_fields` working cleanly and avoids serde
  `untagged`/`flatten` footguns.
- **`source()` panics on unvalidated configs.** Every real call path goes
  through `Config::load` which validates first. The panic catches misuse
  in tests that construct `ContainerConfig` directly without setting a
  valid shape.
- **Pull probe uses `podman image exists`.** Mirrors the shape of
  `probe_cached` (which uses `buildah images --quiet`). Exits 0 iff the
  image is present; no stdout parsing needed.
- **`--no-cache` for image-name configs skips the probe and re-runs
  `podman pull`.** Does not `podman rmi` first (matches the spec's
  deferral of the `rmi-first` option).
- **Pre-existing e2e test breakage** (missing 5th arg to
  `connect_via_podman_exec`) is unrelated to this task; those tests were
  already broken on trunk and are gated behind `--features e2e`.
