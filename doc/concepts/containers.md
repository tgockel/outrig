# Containers

The container is the agent's whole world. It's where MCP servers run, where shell commands
execute, where files get read and written. You define it with a `Dockerfile` you commit to your
repo, and outrig builds it with `buildah` and runs it with `podman`.

## Default location

[`outrig image add`](../usage/image.md#outrig-image-add) writes image files
under `.agents/outrig/images/<name>/`:

```
.agents/outrig/
├── config.toml
└── images/
    └── coding/
        ├── Dockerfile
        └── (any other files referenced by the Dockerfile)
```

Putting them under `.agents/outrig/` keeps outrig-specific build context separate from your
project's own Dockerfiles (which often live at the repo root or under `containers/`,
`docker/`, etc. for unrelated purposes). You can override the default by editing
`[images.<name>].dockerfile` and `.context` to point anywhere relative to the repo root.

## Dockerfile conventions

outrig expects a few things from your Dockerfile.

### `CMD ["sleep", "infinity"]`

The container's job is to stay running while the agent works. Every real process -- MCP servers,
shell commands the agent runs -- is launched via `podman exec` from outrig on the host. So your
`Dockerfile` ends with:

```Dockerfile
CMD ["sleep", "infinity"]
```

If you set a different `CMD` or `ENTRYPOINT`, the container will exit before outrig can attach
an MCP server to it. outrig doesn't override your `CMD`; it relies on this convention.

### Don't set up a user in the Dockerfile

outrig handles user identity entirely at run time -- see [Workspace](workspace.md). Don't
`useradd` a hard-coded UID in your Dockerfile, and don't set a `USER` directive. The build is
intentionally generic so the same image works for any host user (yours, a teammate's, CI's)
without rebuilding.

The image needs `useradd` and `groupadd` available (the standard `passwd`/`shadow` package on
Debian/Ubuntu, `shadow` on Alpine). outrig calls them once at start-up to materialize an
in-container user matching your host UID/GID.

### Install MCP servers

The image needs to contain the binaries and dependencies for every MCP server you reference in
the matching `[images.<name>.mcp]` config. There's no other place to put them -- outrig
doesn't fetch tools at run time.

```Dockerfile
RUN npm install -g @modelcontextprotocol/server-filesystem
```

If multiple MCP servers need different language toolchains (one needs Node, one needs Python),
install both in the same image. The agent's whole MCP set runs in one container per session.

## The `[images.<name>]` config block

A typical config has at least one `[images.<name>]` block plus a top-level
`default-image`:

```toml
default-image = "coding"

[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"   # relative to repo root
context    = ".agents/outrig/images/coding"              # relative to repo root
build-args = { NODE_VERSION = "20" }                          # extra Dockerfile ARGs

  [images.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
```

`dockerfile` and `context` are paths from the repo root (the directory containing
`.agents/outrig/`). `build-args` are extra Dockerfile `ARG`s -- whatever your Dockerfile needs
parameterized at build time.

The `[images.<name>.mcp]` map is covered in [MCP Servers](mcp-servers.md).

### Capability profiles

By default, outrig preserves podman's default Linux capability set. That keeps existing
toolchains and MCP servers working while still applying `--security-opt=no-new-privileges`.
When a container can run with less privilege, add a security block:

```toml
[images.coding.security]
capability-profile = "no-net-raw"
```

The supported profiles are:

- `default`: keep podman's default capability set.
- `no-net-raw`: drop `NET_RAW`, which blocks raw sockets without breaking most development
  tooling.
- `drop-all`: start from `--cap-drop=ALL`.

You can combine a profile with explicit overrides:

```toml
[images.web.security]
capability-profile = "drop-all"
cap-add = ["NET_BIND_SERVICE"]
```

Explicit `cap-add` values are rendered last, so a container can start from `drop-all` and add
back one narrow capability. Capability names may include or omit the `CAP_` prefix.

### Using a pre-built image

If you already have an image (from a registry, CI pipeline, or local build), set
`image-name` instead of `dockerfile` + `context`:

```toml
[images.scratch]
image-name = "docker.io/library/ubuntu:24.04"

  [images.scratch.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

Exactly one of these two shapes must be set on each block:

- `dockerfile` + `context` (with optional `build-args`) -- build path.
- `image-name` -- use-existing-image path.

Setting both, neither, or `image-name` alongside `build-args` is a config-validation error.

`outrig build --image scratch` pulls the image if not already local:

```sh
$ outrig build --image scratch
[outrig] image-config: scratch
[outrig] image:            docker.io/library/ubuntu:24.04
[outrig] image ready
```

On subsequent runs when the image is already present:

```sh
$ outrig build --image scratch
[outrig] image ready (already pulled: docker.io/library/ubuntu:24.04)
```

`outrig run --image scratch` starts the container directly -- no buildah invocation.

## Named image-configs

You can declare multiple image-configs for the same repo and switch between them with
`--image`:

```toml
default-image = "coding"

[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"

  [images.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]

[images.planning]
dockerfile = ".agents/outrig/images/planning/Dockerfile"
context    = ".agents/outrig/images/planning"

  [images.planning.mcp]
  fs       = { command = ["mcp-server-filesystem", "/workspace"] }
  research = { command = ["mcp-research-tools"] }
```

```sh
$ outrig run                       # uses default-image = "coding"
$ outrig run --image planning      # different Dockerfile, different MCPs
```

This is useful when you want lighter-weight environments for different kinds of work -- e.g. a
`planning` image that has no compiler, no shell, and only research-oriented MCPs; a `coding`
image with the full toolchain. Agents can also pin their own default image via
`agents.<name>.image`; see [Providers, Models, and Agents](llm-providers.md).

Every image-config is built and cached independently. Switching between them is fast after
the first build.

## Image caching

outrig tags built images as `<image-config-name>:<hash>`, where the name is the `[images.<name>]`
block key and the hash is a content-addressed cache key combining the contents of the
`Dockerfile`, the `build-args`, the OutRig labels derived from `[images.<name>.mcp]`, and the
content of the build context (gitignore-aware when the context is in a git repo, otherwise a
tarball hash). So `[images.outrig-standard]` builds to `outrig-standard:<hash>`, which podman
shows as `localhost/outrig-standard`.

Repo-local build images carry an `org.outrig.mcp` label too. On a cache miss, outrig builds a
temporary image, reads any inherited/Dockerfile MCP label, overlays `[images.<name>.mcp]`, and
commits the final cache tag with the merged label. This keeps `outrig image inspect
<image-config-name>:<hash>` aligned with the declared servers that startup will use.

Because the name is the image's repository, **give image-configs repo-specific, lowercase names**
(e.g. `outrig-standard`, not `standard`) so `podman images` makes clear which repo an image came
from. Build-image names must be valid container image repository components -- lowercase
alphanumeric separated by `.`, `_`, or `-` -- and `outrig` rejects invalid names at config load.

A change to the `Dockerfile`, any file in the context, build args, or `[images.<name>.mcp]`
causes a rebuild on the next `outrig run` or `outrig build`. Otherwise the cache hit is
immediate. To force a rebuild without changing files, run `outrig build --no-cache`.

Image-name configs use podman's local image store directly; there is no `<name>:<hash>`
tag in that path. `--no-cache` on an image-name config re-runs `podman pull` even when the
image is already present locally. (The library `Outrig::launch` API, which builds from a raw
Dockerfile with no image-config name, falls back to the `outrig-cache:<hash>` repository.)

## What outrig sets in the run

outrig adds `--userns=keep-id`, `--security-opt=no-new-privileges`, the primary workspace
bind-mount, any configured extra workspace mounts, and the runtime user-mapping bootstrap
(see [Workspace](workspace.md)). Capability flags are emitted only when the selected
image-config opts into a capability profile or explicit `cap-drop` / `cap-add` entries.

outrig does not configure seccomp profiles, AppArmor policy, SELinux policy, read-only root
filesystems, or network egress policy in this container launch path. Network audit/filter mode
is a separate session-level interceptor; see
[Workspace](workspace.md#network-is-not-part-of-the-workspace).

## See also

- [outrig image add](../usage/image.md#outrig-image-add) -- the easiest way to
  scaffold a new image-config.
- [MCP Servers](mcp-servers.md) -- declaring and invoking the tools that run inside the
  container.
- [MCP Trust Model](mcp-trust-model.md) -- why MCP tools can be configured liberally inside the
  container boundary.
- [AI-assisted design](../usage/ai-assisted-design.md) -- use `outrig mcp self` when the
  templates do not fit.
- [Workspace](workspace.md) -- what the container sees of your repo.
- [Reference -> Config](../reference/config.md) -- every supported `[images.<name>]` key.
