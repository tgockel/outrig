# Standalone toolset images

## Context

Today, outrig images are repository-centric: a repo commits a `Dockerfile` under
`.agents/outrig/containers/<name>/`, and the matching `[containers.<name>.mcp]` block in
`config.toml` declares which MCP servers run inside. Task 0051 added `image-name` so a repo can
reference a pre-built image, and task 0053 added `/etc/outrig/container.toml` so an image can
embed its own MCP declarations. Together these pieces let a shared image carry its tool
definitions -- but the workflow to *create*, *publish*, *discover*, and *consume* such images
is still manual and undocumented.

The result is that every team wanting to share an Outrig environment across repos ends up
reinventing the same scaffolding: a CI pipeline that builds the image, a README explaining
which `image-name` to reference, instructions to omit the `[mcp]` block so the embedded config
takes over, etc. There's no `outrig`-native workflow for any of this.

## Goal

Establish a first-class workflow for **standalone toolset images** -- Outrig container images
that are self-contained development environments, independent of any specific repository. A
standalone image:

1. Bundles all tool binaries and runtime dependencies.
2. Ships `/etc/outrig/container.toml` with its full `[mcp]` table (and, eventually, other
   metadata sections).
3. Works out of the box when referenced via `image-name` with zero per-repo MCP config.
4. Can be built, validated, and published using outrig-native commands.
5. Can declare self-documentation (description, tool inventory, intended use cases) that
   outrig can surface to the user.

## Proposed surfaces

### 1. `outrig image init` -- scaffold a standalone image project

Creates a directory (defaulting to the current directory or a named subdirectory) with:

- `Dockerfile` -- a starter Dockerfile with the outrig conventions baked in
  (`CMD ["sleep", "infinity"]`, no `USER` directive, `passwd` installed, MCP server
  installation stubs).
- `container.toml` -- the `/etc/outrig/container.toml` that will be `COPY`'d into the image,
  pre-populated with commented-out MCP entries.
- `README.md` -- human-readable description of what the image provides.
- `.outrig-image.toml` -- image-project metadata (name, version, registry target, description).

This is distinct from `outrig init` (which scaffolds a repo's `.agents/outrig/` tree) and from
`outrig container add` (which adds a container-config to an existing outrig project). It creates
a standalone project whose sole output is a published container image.

```sh
$ outrig image init rust-dev
[outrig] created rust-dev/
[outrig]   Dockerfile
[outrig]   container.toml
[outrig]   .outrig-image.toml
[outrig]   README.md
```

### 2. `outrig image build` -- build and validate a standalone image

Builds the image from the current image-project directory, validates that the resulting image
contains a parseable `/etc/outrig/container.toml`, and optionally runs a smoke-test
(start container, initialize each declared MCP server, confirm `tools/list` returns).

```sh
$ outrig image build
[outrig] building outrig-rust-dev:0.1.0 ...
[outrig] validating embedded container.toml ...
[outrig]   mcp servers: fs, shell, git (3 servers, 14 tools)
[outrig] image ready: outrig-rust-dev:0.1.0

$ outrig image build --smoke-test
[outrig] building outrig-rust-dev:0.1.0 ...
[outrig] validating embedded container.toml ...
[outrig] smoke-testing MCP servers ...
[outrig]   fs: 5 tools ✓
[outrig]   shell: 1 tool ✓
[outrig]   git: 8 tools ✓
[outrig] image ready: outrig-rust-dev:0.1.0
```

### 3. `outrig image push` -- publish to a registry

Thin wrapper around `podman push` that tags according to `.outrig-image.toml` metadata and
pushes to the configured registry.

```sh
$ outrig image push
[outrig] pushing ghcr.io/myorg/outrig-rust-dev:0.1.0 ...
[outrig] done
```

### 4. `outrig image inspect` -- show what an image provides

Given a local or remote image ref, start it briefly, read its `/etc/outrig/container.toml`,
and display the declared MCP servers with their tool inventories. This is the "what's in
this image?" command for consumers.

```sh
$ outrig image inspect ghcr.io/myorg/outrig-rust-dev:0.1.0
Image: ghcr.io/myorg/outrig-rust-dev:0.1.0
Description: Rust development environment with cargo, clippy, and documentation tools.

MCP servers:
  fs    - @modelcontextprotocol/server-filesystem
          tools: read_file, read_multiple_files, write_file, ...
  shell - mcp-shell-server
          tools: shell_execute
  git   - mcp-server-git
          tools: git_status, git_diff_unstaged, git_commit, ...
```

### 5. Extended `/etc/outrig/container.toml` sections

Beyond `[mcp]`, standalone images benefit from additional metadata sections that outrig can
read but older versions ignore (forward-compat via `serde(default)` on the struct):

```toml
# /etc/outrig/container.toml

[image]
description = "Rust development environment with full toolchain"
version     = "0.1.0"
homepage    = "https://github.com/myorg/outrig-rust-dev"
tags        = ["rust", "cargo", "documentation"]

[defaults]
# Suggested workspace path (consumer can override)
container-path = "/workspace"

[defaults.env]
# Default env vars for all MCP servers in this image
CARGO_HOME  = "/usr/local/cargo"
RUSTUP_HOME = "/usr/local/rustup"

[mcp]
fs    = { command = ["mcp-server-filesystem", "/workspace"] }
shell = { command = ["mcp-shell-server"], env = { ALLOW_COMMANDS = "cargo,git,rg,find,ls,cat" } }
git   = { command = ["mcp-server-git", "--repository", "/workspace"] }
```

### 6. Minimal repo-side config for standalone images

A consuming repo that uses a standalone image needs only:

```toml
# .agents/outrig/config.toml
default-container = "rust-dev"
default-agent     = "coding"

[workspace]
host-path      = "."
container-path = "/workspace"

[agents.coding]
container = "rust-dev"
preamble  = "You are a careful coding assistant. The repo is mounted at /workspace."

[containers.rust-dev]
image-name = "ghcr.io/myorg/outrig-rust-dev:0.1.0"
# No [containers.rust-dev.mcp] needed -- image provides everything.
```

This is already supported today (tasks 0051 + 0053), but the workflow to *produce* such
images and the tooling to *validate* them is what this proposal adds.

## `.outrig-image.toml` schema

```toml
name        = "outrig-rust-dev"
version     = "0.1.0"
description = "Rust development environment with full toolchain"
registry    = "ghcr.io/myorg"           # default push target
dockerfile  = "Dockerfile"              # relative to project root
context     = "."                       # relative to project root

[labels]
# OCI labels to bake into the image
"org.opencontainers.image.source" = "https://github.com/myorg/outrig-rust-dev"
"org.opencontainers.image.description" = "Outrig standalone toolset: Rust development"
```

## Relationship to existing features

| Existing feature | Role in this workflow |
|---|---|
| `image-name` (0051) | Consumer-side: reference published standalone images |
| Embedded `container.toml` (0053) | Producer-side: image ships its own MCP config |
| `outrig mcp show-merged` (0053) | Consumer-side: inspect effective MCP set |
| `outrig container add` (0031) | Repo-local containers; standalone images are the registry-published counterpart |
| `outrig build` (0032) | Builds repo-local containers; `outrig image build` is the standalone counterpart |

## Phasing

This could be broken into several implementation tasks:

1. **Extended `container.toml` schema** -- add `[image]` and `[defaults]` sections (parsed
   but initially unused; forward-compat).
2. **`outrig image init`** -- scaffold a standalone image project.
3. **`outrig image build`** -- build + validate (embedded config parse + optional smoke test).
4. **`outrig image inspect`** -- read and display image metadata and tool inventory.
5. **`outrig image push`** -- thin wrapper around podman push with metadata-driven tagging.
6. **Documentation** -- concept page for standalone images, updated quickstart showing both
   repo-local and standalone workflows.

## Open questions

- **Should `outrig image build` use buildah directly or delegate to `podman build`?** The
  existing `outrig build` uses buildah for repo-local containers. Standalone images could use
  either; buildah is already a dependency.
- **Should there be an `outrig image pull` command, or is `image-name` + `outrig build`
  sufficient?** Currently `outrig build --container <name>` pulls image-name configs. A
  dedicated `outrig image pull` might be clearer for standalone workflows.
- **How much of this is outrig's job vs. standard container tooling?** The value-add is
  validation (ensuring the image is a well-formed outrig environment) and discovery (showing
  what tools an image provides). The build/push mechanics are thin wrappers. If the wrappers
  add friction rather than clarity, the commands could be reduced to just `init` + `inspect`.
- **Should standalone images be publishable to an outrig-specific registry or index?** An
  "Outrig Hub" of community images is tempting but out of scope for v0. Standard OCI
  registries are sufficient.

## Designing standalone images with `outrig mcp self`

`outrig mcp self` (task 0055) is the AI-assisted design server that exposes OutRig's docs,
config schema, base-image suggestions, MCP server suggestions, and advisory validators over
stdio MCP. It is already the recommended path for designing repo-local container-configs when
the `outrig container add` wizard's templates don't fit. Standalone toolset images are the
same design problem at a different scope: instead of producing a `.agents/outrig/containers/`
tree, the AI produces a standalone image project.

### How `outrig mcp self` supports standalone image design today

The existing tools are already useful:

| Tool | How it helps standalone image design |
|------|--------------------------------------|
| `list_docs` / `get_doc` | AI reads the container conventions page, embedded `container.toml` spec, and trust model. |
| `get_config_schema` | AI sees the `McpServerSpec` schema for the `[mcp]` table it needs to embed. |
| `list_base_images` | Curated starting points for the `FROM` line. |
| `list_mcp_server_suggestions` | Known MCP server install recipes and command forms. |
| `validate_dockerfile` | Advisory checks (CMD, no USER, useradd present). |
| `validate_config` | Validates the `[mcp]` block that will become the image's `container.toml`. |

An AI attached to `outrig mcp self` can already be prompted:

```text
Design a standalone Outrig toolset image for Rust development. The image should:
- Be based on rust:1-bookworm
- Include filesystem, shell, and git MCP servers
- Ship /etc/outrig/container.toml with the full [mcp] table
- End with CMD ["sleep", "infinity"]
- Not set a USER directive
- Include passwd for runtime user bootstrap

Read the OutRig docs first (especially concepts/containers and concepts/mcp-servers),
then validate both the Dockerfile and the container.toml [mcp] block.
Return the complete Dockerfile and container.toml file contents.
```

### Gaps that this proposal fills for `outrig mcp self`

The current `outrig mcp self` tools are oriented toward repo-local containers. For standalone
images, the following additions would make the workflow complete:

1. **`validate_container_toml`** (new tool or extension of `validate_config`): Accept a full
   `/etc/outrig/container.toml` file (including `[image]` metadata and `[defaults]` sections)
   and validate it as an embedded image config, not just a `[containers.<name>]` fragment.

2. **`get_standalone_image_conventions`** (new tool or doc page): Return the specific
   conventions for standalone images that go beyond repo-local containers:
   - The image must ship `/etc/outrig/container.toml` (not optional).
   - The `[mcp]` table should be self-sufficient (consumer should need zero config).
   - The `[image]` section should include description and version.
   - The `[defaults]` section should declare the expected workspace mount point.
   - The `COPY container.toml /etc/outrig/container.toml` line in the Dockerfile.

3. **Expanded doc page**: A new `doc/concepts/standalone-images.md` page that `get_doc` can
   serve, covering the full lifecycle from design through publish. This gives the AI the
   context it needs to produce a complete standalone image project rather than just a
   Dockerfile + config pair.

### Suggested prompt for standalone image design via `outrig mcp self`

This prompt should appear in the updated `doc/usage/ai-assisted-design.md` page:

```text
Design a standalone Outrig toolset image that I can publish to a registry and use
across multiple repositories with zero per-repo MCP configuration.

The image should provide: [describe your toolset -- language, tools, MCP servers]

Requirements:
- Produce a Dockerfile and a /etc/outrig/container.toml
- The container.toml must contain a complete [mcp] table and an [image] metadata section
- The Dockerfile must COPY container.toml to /etc/outrig/container.toml
- Follow all OutRig container conventions (read the docs first)
- Validate both files before returning them

Also produce a minimal consuming config.toml showing how a repo would
reference this image with image-name and no [mcp] block.
```

### Client setup for standalone image design

The same client setup as for any `outrig mcp self` use case:

```sh
# Claude Code
claude mcp add outrig-self -- outrig mcp self

# Then prompt:
# "Design a standalone Outrig toolset image for [your use case].
#  Use your outrig-self tools to read the docs and validate the output."
```

See [AI-assisted design](../doc/usage/ai-assisted-design.md) for full client setup
instructions for Claude Desktop, Codex CLI, Cursor, and others.

### What `outrig design prompt` provides for standalone images

For AI tools that don't support MCP, `outrig design prompt` prints a self-contained prompt.
The standalone image workflow should be included in that prompt's context, so the AI
produces the right artifacts even without interactive tool access. This means the
`outrig design prompt` output should include:

- The standalone image conventions
- The `container.toml` schema (including `[image]` and `[defaults]`)
- A worked example of a complete standalone image project
- The consuming-repo config showing `image-name` with no `[mcp]` block

## Non-goals

- Hosted build service.
- Automatic dependency resolution between images.
- Runtime image composition (multi-image sessions).
- Package-manager-style versioning constraints (semver ranges, lock files).

## Dependencies

- **Soft: 0051** (image-name) -- consumer-side support already landed.
- **Soft: 0053** (embedded container.toml) -- producer-side support already landed.
- **Soft: 0055** (outrig mcp self) -- the design tool that makes standalone image
  authoring AI-assisted.
- No hard blockers; this is new surface area.
