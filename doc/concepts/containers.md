# Containers

The container is the agent's whole world. It's where MCP servers run, where shell commands
execute, where files get read and written. You define it with a `Dockerfile` you commit to your
repo, and outrig builds it with `buildah` and runs it with `podman`.

## Default location

[`outrig container add`](../usage/container.md#outrig-container-add) writes container files
under `.agents/outrig/containers/<name>/`:

```
.agents/outrig/
├── config.toml
└── containers/
    └── coding/
        ├── Dockerfile
        └── (any other files referenced by the Dockerfile)
```

Putting them under `.agents/outrig/` keeps outrig-specific build context separate from your
project's own Dockerfiles (which often live at the repo root or under `containers/`,
`docker/`, etc. for unrelated purposes). You can override the default by editing
`[containers.<name>].dockerfile` and `.context` to point anywhere relative to the repo root.

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
the matching `[containers.<name>.mcp]` config. There's no other place to put them -- outrig
doesn't fetch tools at run time.

```Dockerfile
RUN npm install -g @modelcontextprotocol/server-filesystem
```

If multiple MCP servers need different language toolchains (one needs Node, one needs Python),
install both in the same image. The agent's whole MCP set runs in one container per session.

## The `[containers.<name>]` config block

A typical config has at least one `[containers.<name>]` block plus a top-level
`default-container`:

```toml
default-container = "coding"

[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"   # relative to repo root
context    = ".agents/outrig/containers/coding"              # relative to repo root
build-args = { NODE_VERSION = "20" }                          # extra Dockerfile ARGs

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
```

`dockerfile` and `context` are paths from the repo root (the directory containing
`.agents/outrig/`). `build-args` are extra Dockerfile `ARG`s -- whatever your Dockerfile needs
parameterized at build time.

The `[containers.<name>.mcp]` map is covered in [MCP Servers](mcp-servers.md).

### Using a pre-built image

If you already have an image (from a registry, CI pipeline, or local build), set
`image-name` instead of `dockerfile` + `context`:

```toml
[containers.scratch]
image-name = "docker.io/library/ubuntu:24.04"

  [containers.scratch.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

Exactly one of these two shapes must be set on each block:

- `dockerfile` + `context` (with optional `build-args`) -- build path.
- `image-name` -- use-existing-image path.

Setting both, neither, or `image-name` alongside `build-args` is a config-validation error.

`outrig build --container scratch` pulls the image if not already local:

```sh
$ outrig build --container scratch
[outrig] container-config: scratch
[outrig] image:            docker.io/library/ubuntu:24.04
[outrig] image ready
```

On subsequent runs when the image is already present:

```sh
$ outrig build --container scratch
[outrig] image ready (already pulled: docker.io/library/ubuntu:24.04)
```

`outrig run --container scratch` starts the container directly -- no buildah invocation.

## Named container-configs

You can declare multiple container-configs for the same repo and switch between them with
`--container`:

```toml
default-container = "coding"

[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]

[containers.planning]
dockerfile = ".agents/outrig/containers/planning/Dockerfile"
context    = ".agents/outrig/containers/planning"

  [containers.planning.mcp]
  fs       = { command = ["mcp-server-filesystem", "/workspace"] }
  research = { command = ["mcp-research-tools"] }
```

```sh
$ outrig run                              # uses default-container = "coding"
$ outrig run --container planning  # different Dockerfile, different MCPs
```

This is useful when you want lighter-weight environments for different kinds of work -- e.g. a
`planning` container that has no compiler, no shell, and only research-oriented MCPs; a `coding`
container with the full toolchain. Agents can also pin their own default container via
`agents.<name>.container`; see [Providers, Models, and Agents](llm-providers.md).

Every container-config is built and cached independently. Switching between them is fast after
the first build.

## Image caching

outrig tags built images as `outrig-cache:<hash>`. The hash combines the contents of the
`Dockerfile`, the `build-args`, and the content of the build context (gitignore-aware when the
context is in a git repo, otherwise a tarball hash).

A change to the `Dockerfile` or any file in the context causes a rebuild on the next `outrig run`
or `outrig build`. Otherwise the cache hit is immediate. To force a rebuild without changing
files, run `outrig build --no-cache`.

Image-name configs use podman's local image store directly; there is no `outrig-cache:<hash>`
tag in that path. `--no-cache` on an image-name config re-runs `podman pull` even when the
image is already present locally.

## What outrig does *not* set in the run

outrig adds `--userns=keep-id`, `--security-opt=no-new-privileges`, the workspace bind-mount,
and the runtime user-mapping bootstrap (see [Workspace](workspace.md)). It does *not* drop
capabilities aggressively in v0 -- `--cap-drop=ALL` breaks many real MCP servers (npm
post-install, fork-heavy shells), so the v0 floor is the default podman cap set plus
`no-new-privileges`. Tightening that is deferred.

> **TODO: Incomplete** -- `--cap-drop` profile and seccomp policy aren't implemented yet.

## See also

- [outrig container add](../usage/container.md#outrig-container-add) -- the easiest way to
  scaffold a new container-config.
- [MCP Servers](mcp-servers.md) -- declaring and invoking the tools that run inside the
  container.
- [MCP Trust Model](mcp-trust-model.md) -- why MCP tools can be configured liberally inside the
  container boundary.
- [AI-assisted design](../usage/ai-assisted-design.md) -- use `outrig mcp self` when the
  templates do not fit.
- [Workspace](workspace.md) -- what the container sees of your repo.
- [Reference -> Config](../reference/config.md) -- every supported `[containers.<name>]` key.
