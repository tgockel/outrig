# User image library: an indexed `~/.outrig/images/`

## Context

A repo declares image-configs in `.agents/outrig/config.toml` with Dockerfiles under
`.agents/outrig/images/<name>/`. A user has no equivalent. If you want a Git agent or a
web-search utility available in every repo you work in, today you either install the tools into
every repo-specific image, or keep a standalone project somewhere and remember its path --
`outrig image build` takes a *directory argument*
(`crates/outrig-cli/src/image_setup/build.rs`), so nothing is indexed and nothing is discoverable.

The sidecar work (`plan/done/0079`-`0090`) already removed the need to install a tool into the
primary image: an MCP server can run in its own container next to the primary. What is missing is
a place to author those tool images once, per user.

The unit already exists and is the right one. `outrig image init` writes a self-contained project
-- `Dockerfile`, `image.toml` (`[image].ref`, optional `[build]`, required `[mcp]`), `README.md`
-- and `outrig image build` stamps the `[mcp]` table into the image's `org.outrig.mcp` label so a
consuming repo needs no MCP config of its own (`plan/done/0069`-`0072`). The only thing that
project cannot be is *found*.

Longer term a prebuilt catalog of common tools is wanted. Nothing like it exists today: the
"catalog" is a two-variant Rust enum (`crates/outrig-cli/src/image_setup/render.rs:107-167`,
`McpServer::{Fs, Git}`), and every response it feeds is stamped "suggestions only -- not a
registry". This entry is user-defined only, but the index is layered so a catalog can slot in
underneath later without moving anything.

## Goal

Give a user a `~/.outrig/images/` directory of standalone image projects that outrig discovers
automatically, so any repo on the machine can reference one by name the same way it references a
repo-local image-config.

## Deliverables

- **Library root** resolved as a sibling of the global config file: `<dir of resolved global
  config>/images/`. `~/.outrig/images/`, `$XDG_CONFIG_HOME/outrig/images/`, and
  `--global-config /p/x.toml` -> `/p/images/` all fall out of the one rule, reusing
  `global_config_path_with` (`crates/outrig-cli/src/paths.rs:125`). The `--global-config` case is
  what makes this testable without touching a real `$HOME`.
- **Indexing** at config load: scan `<root>/*/image.toml`. The directory name is the
  image-config name and must satisfy the existing `^[a-zA-Z][a-zA-Z0-9_-]*$` project-name check
  (`plan/done/0070-standalone-image-init.md`) -- it has to be both a clean image-ref token and a
  valid TOML bare key. Each hit synthesizes a build-shape `[images.<name>]` entry whose
  `ConfigSource` is the project directory (`plan/todo/0097-config-path-provenance.md`).
- **Precedence**, low to high: scanned library < global `[images.*]` < repo `[images.*]`. Same
  name-keyed, whole-entry, repo-wins rule as every other map in `merge.rs`.
- **Shadow diagnostic**: when a higher layer overrides a library project, warn and name both
  paths. Silent shadowing is the failure mode this indexing introduces -- a repo whose
  image-config happens to be called `git-tools` would otherwise quietly take over.
- **Malformed entries degrade, they do not abort.** A directory whose `image.toml` fails to parse
  warns and is skipped. This differs from `image.toml` handling in `outrig image build`, which
  hard-errors, and deliberately so: a broken project in the library must not make every session
  in every repo unstartable.
- **Content-hash tags for library builds.** Tag `<name>:<content-hash>` using
  `CacheKey::compute_with_labels` (`crates/outrig/src/image.rs:111`) with the labels derived from
  `image.toml`, so `ensure_image_for` skips on a cache hit and an edited Dockerfile rebuilds
  automatically. See fork 1.
- **`image.toml` gains a `[sidecar]` table** -- run-defaults for the image when it is used as a
  sidecar. See the config sketch and fork 2.
- **CLI**: `outrig image init --user <name>` scaffolds into the library root;
  `outrig image build --user [<name>]` builds one or all library projects; `outrig image ls`
  lists every visible image-config with its source, shape, and cache state. `image ls` is already
  reserved in `doc/usage/image.md:8`, and it is the surface that makes "automatically indexed"
  legible -- without it the index is invisible until something fails.
- **Docs**: `doc/usage/image.md` (the `image` command group and the library layout),
  `doc/reference/config.md` (`[images.<name>]` precedence, the `image.toml` `[sidecar]` schema),
  `doc/concepts/containers.md` (where image-configs come from). All three are **symlinks** into
  `crates/outrig-cli/src/mcp_self/docs/`; edit the targets, there are no second copies.

## Config sketch

```
~/.outrig/
  config.toml
  images/
    git-tools/
      Dockerfile
      image.toml
```

```toml
# ~/.outrig/images/git-tools/image.toml
[image]
ref         = "git-tools"
description = "Git and GitHub tooling"

[sidecar]                     # NEW -- run-defaults when this image hosts a sidecar
workspace = "ro"              # "none" (default) | "ro" | "rw"

  [[sidecar.mounts]]
  host-path      = "~/.config/gh"
  container-path = "/gh"
  access         = "read-only"

[mcp]
git = { command = ["mcp-server-git", "--repository", "/workspace"] }
```

The `[sidecar]` table takes the same keys and the same validation as `[sidecars.<sc>]`
(`workspace`, `view`, `args`, `mounts`, `security`) minus `image`, which is the project itself,
and minus `start`/`on-failure`, which are properties of a consuming session rather than of the
image. A `[sidecars.<sc>]` block whose `image` names a library project inherits these defaults;
explicit keys in the block override them per key.

Consuming it from a repo needs nothing new -- the name resolves like any other image-config:

```toml
# .agents/outrig/config.toml
[sidecars.git]
image = "git-tools"           # library project; workspace = "ro" and the mount come with it

[images.coding.mcp]
git = { sidecar = "git" }
```

## Runtime behavior

Indexing runs at config load, after the two files are read and before validation, so a library
project is indistinguishable from a declared image-config by the time anything resolves a name.
`--image`, `default-image`, `agents.<n>.image`, and `[sidecars.<sc>].image` all resolve through
the same map (`resolve_image_config`, `crates/outrig-cli/src/cli/session_setup.rs:1145`), so none
of them need to learn about the library.

Because the podman image store is machine-global, a library image built for one repo is a cache
hit for every other repo on the machine. The cost lands on the first run after an edit: a
content-hash miss means a real build, and `plan/next/keepid-first-run-layer-remap-cost.md`
records that a first `--userns=keep-id` run on a fresh image can cost minutes rather than
seconds. Editing a widely-attached library Dockerfile is therefore a slow next-session, and
`outrig image build --user` exists partly so that cost can be paid deliberately.

## Acceptance

- A project at `~/.outrig/images/git-tools/` is usable as `--image git-tools` from a repo
  anywhere on disk, with no entry in either config file.
- The same project is usable as `[sidecars.<sc>].image = "git-tools"`, and the session applies
  the `[sidecar]` defaults from its `image.toml`.
- `--global-config /tmp/g/config.toml` indexes `/tmp/g/images/` and nothing under `$HOME`.
- A repo `[images.git-tools]` overrides the library project and warns, naming both paths.
- A library directory whose `image.toml` is malformed warns, is skipped, and does not prevent a
  session that never references it from starting.
- `outrig image ls` shows repo, global, and library image-configs with source and cache state.
- `outrig image init --user git-tools` creates the project; `outrig image build --user` builds
  every library project; a second `build` with no edits is a cache hit and skips the build.
- `outrig image inspect git-tools:<hash>` shows the `[sidecar]` block alongside the `mcp:`
  section.
- A registry image whose `org.outrig.sidecar` label requests `mounts`, `view`, or `security` has
  those keys reported by `image inspect` and **not** applied to the session (fork 2).

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **Build path for library projects -- Resolved: content-hash tags, revisiting 0071.**
   `plan/done/0071-standalone-image-build.md` decided a standalone build has no project-level
   cache: it tags a stable caller-named ref, so `--no-cache` only forwards to buildah. That was
   right for a one-off `outrig image build <dir>` where the user is deciding when to build. It is
   wrong for an image that is implicitly resolved by every session on the machine, where a stale
   stable tag means an edit never takes effect and a rebuild-every-time means every session pays
   a build. Tagging `<name>:<content-hash>` with the `image.toml`-derived labels reuses
   `CacheKey::compute_with_labels` and `ensure_image_for` unchanged. `outrig image build <dir>`
   keeps its existing stable-ref behavior; only the `--user` path is content-hashed.

2. **Trust in a `[sidecar]` label -- Resolved, and load-bearing.** A `[sidecar]` block read from
   a locally-authored library `image.toml` is user-authored and honored in full: it is the same
   trust tier as `~/.outrig/config.toml`, which the user also wrote. The same block read back
   from an `org.outrig.sidecar` label on a **pulled** image honors only `workspace` and `args`.
   `mounts`, `view`, and `security` from a label are reported by `outrig image inspect` as
   requests and never auto-applied.

   The reason is not hypothetical. A label-declared `mounts = [{ host-path = "~/.ssh" }]` would
   let a registry image name a host path for the host to bind in. `view = "primary"` grants
   `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` in the primary's user namespace
   (`plan/done/0090-primary-view-sidecars.md`) -- a real posture change that `SECURITY.md` and
   `doc/concepts/mcp-trust-model.md` treat as an explicit user decision. Letting an image opt
   itself in inverts that. This extends the discipline already in `embedded.rs:114-125`, where
   `PlacementInLabel` and `ArgsInLabel` reject config-only keys in `org.outrig.mcp` for the same
   class of reason.

   The stamping itself is unconditional -- `outrig image build` writes the whole `[sidecar]`
   block to the label so `image inspect` can show it. Provenance gates *honoring*, not
   *recording*.

3. **`image.toml` stays the authoring format -- Resolved.** A library project is exactly an
   `outrig image init` project, not a new file format. That keeps one authoring surface, lets a
   project be published or cloned as-is, and means a future catalog can distribute the same
   shape. Rejected: a per-library manifest listing entries, which would be a second source of
   truth to keep in step with the directory.

4. **A prebuilt catalog -- Open.** The intended shape is a fourth index layer below the user
   library, so precedence becomes catalog < library < global < repo and nothing already written
   moves. Today's seed is `McpServer::ALL` plus the already-vestigial `host_env` field in
   `crates/outrig-cli/src/mcp_self/suggestions.rs`, which is declared and always empty. Open:
   whether catalog entries are vendored projects, refs to registry images, or both; and how
   `SUGGESTIONS_NOTE`'s "not a registry" framing changes once one exists.

5. **Image GC -- Open.** `outrig clean` sweeps session directories and stray labeled containers
   and has never removed image tags (`plan/done/0086-sidecar-startup-performance.md`). Content-
   hash tagging a library that is edited regularly accumulates `<name>:<hash>` tags that nothing
   reaps. This is pre-existing for repo images; the library makes it easier to hit, because the
   images are shared across every repo. Worth a `--images` mode on `outrig clean`, scoped
   separately.

6. **Nested library layout -- Open.** Only `<root>/<name>/image.toml` is scanned; no recursion,
   no grouping. A namespaced layout (`<root>/<org>/<name>/`) would want a naming scheme, and the
   image-ref token rules constrain what separators are available. Defer until a catalog forces
   the question.

## Dependencies

- **Hard: 0097** (`plan/todo/0097-config-path-provenance.md`). A library project's `dockerfile`
  and `context` are relative to the project directory. Until an image-config carries its own base
  directory, every path in the index resolves against the repo root.
- **Soft: 0092** (`plan/todo/0092-e2e-imageconfig-sidecars-bitrot.md`). The e2e feature does not
  compile today, so any end-to-end acceptance here needs that fix first.

## See also

- `crates/outrig-cli/src/paths.rs` -- `global_config_path_with`, the rule the library root hangs
  off; `image_dir` / `image_dir_rel` for the repo-side equivalents.
- `crates/outrig-cli/src/image_setup/{init,build}.rs` -- the project scaffolder and builder that
  `--user` extends.
- `crates/outrig/src/container/embedded.rs` -- `StandaloneImageToml`, the label constants, and
  the `PlacementInLabel` / `ArgsInLabel` precedent for fork 2.
- `crates/outrig/src/image.rs` -- `CacheKey::compute_with_labels`, `ensure_image_for`,
  `build_standalone`.
- `plan/done/0070-standalone-image-init.md`, `plan/done/0071-standalone-image-build.md`,
  `plan/done/0072-oci-label-config-surface.md` -- the standalone-project decisions this builds on
  and, in fork 1, revisits.
- `plan/done/0090-primary-view-sidecars.md`, `doc/concepts/mcp-trust-model.md`, `SECURITY.md` --
  why `view = "primary"` cannot be image-declared.
- `plan/next/user-toolsets.md` -- the attach mechanism that makes a library tool reach a session
  without repo config.
