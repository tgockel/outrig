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
`[images.<name>].dockerfile` and `.context` to point anywhere relative to the repo root -- or
absolutely anywhere, with an absolute path.

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

The image needs no user tooling for this. outrig writes the matching `/etc/passwd` and
`/etc/group` entries into the container itself, from the host, so an image with no `useradd`,
`groupadd`, or `getent` -- an unadorned `FROM docker.io/library/alpine`, or a distroless base --
works unchanged. On the unusual hosts where that isn't possible (a remote podman service, say),
outrig falls back to running `useradd`/`groupadd` inside the image and says so in the session
transcript; only then does the image need the `passwd` package (Debian/Ubuntu) or `shadow`
(Alpine).

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
build-args = { NODE_VERSION = "20" }                     # extra Dockerfile ARGs

  [images.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec shell-mcp-command"]
```

An image-config can come from three places, lowest precedence first: outrig's
[built-in default](../reference/config.md#the-built-in-default-image-config), the global
config, and the repo config. The built-in only appears when nothing else names an image at
all, and any block you declare under one of its reserved names shadows it entirely.

`dockerfile` and `context` are relative to the directory of the config file that declared the
block. For this repo config that is the repo root (the directory containing `.agents/outrig/`).
Declare the same block in `~/.outrig/config.toml` and the paths are relative to `~/.outrig/`
instead, so a build-from-Dockerfile image can live in your global config and be used from every
repo on the machine. See
[Config -> path resolution](../reference/config.md#path-resolution).

`build-args` are extra Dockerfile `ARG`s -- whatever your Dockerfile needs parameterized at
build time.

The `[images.<name>.mcp]` map is covered in [MCP Servers](mcp-servers.md).

### Capability profiles

By default, outrig preserves podman's default Linux capability set. That keeps existing
toolchains and MCP servers working while still applying `--security-opt=no-new-privileges`,
which stays on unless an image-config turns it off (see
[Devices and privilege escalation](#devices-and-privilege-escalation)). When a container can
run with less privilege, add a security block:

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

### Devices and privilege escalation

Two further keys in the same block cover what capabilities cannot express. A device node is
not a capability, and `no_new_privs` is a separate process flag, so each gets its own key:

```toml
[images.coding.security]
no-new-privileges = false          # default true
devices           = ["/dev/fuse"]  # default []
```

**`no-new-privileges = false` weakens the container boundary.** Under `no_new_privs` the
kernel ignores the setuid bit and file capabilities on every `execve`; clearing the flag puts
them back, so a process that finds a setuid-root binary in the image can use it to become
root inside the container. Treat this as a deliberate tradeoff rather than a neutral knob.

Two things bound the damage. The key is opt-in per image-config and defaults to true, so a
config that never mentions it keeps today's protection. And the container is still an
unprivileged rootless podman container in a user namespace -- clearing the flag grants the
container's own namespace-local root, not root on the host.

`devices` passes host device nodes through, one `--device=<path>` per entry, in declaration
order. This is the sharper of the two in the general case: `/dev/kvm` or a raw block device
hands out real hardware access. It is explicit per path and per image-config, and outrig does
not police which paths you may ask for.

The motivating case for both is a **nested container runtime** -- an agent whose job is to
build an image or run a throwaway container. A nested rootless podman needs `newuidmap` to
map its subordinate UID range, and `newuidmap` is setuid-root, so `no_new_privs` breaks it.
The session container's rootfs is overlayfs and the kernel refuses overlay-on-overlay, so the
nested runtime also needs `fuse-overlayfs`, which needs `/dev/fuse`. outrig supplies the two
primitives; assembling them into a working nested runtime (and installing the tools in your
Dockerfile) is yours to do.

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
[outrig] image ready: docker.io/library/ubuntu:24.04
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

## Sidecar containers

A session can own more than one container. Sidecars are extra podman containers declared under
the top-level `[sidecars.<sc>]` (or implied by an inline `image` key on an MCP entry) that host
MCP servers away from the workspace container; see
[MCP Servers -> Sidecar placement](mcp-servers.md#sidecar-placement) for the config surface.

Blocks are declared once and shared by any number of image-configs. A session starts the ones
its image-config's `[mcp]` entries name, so a block nothing references costs nothing.

```toml
[sidecars.tools]
image      = "mcp-tools"        # sibling [images.mcp-tools] block first, else raw podman ref
workspace  = "ro"               # "none" (default) | "ro" | "rw"
start      = "auto"             # "auto" (default)  | "manual"
on-failure = "abort"            # "abort" (default) | "warn"

[[sidecars.tools.mounts]]
host-path      = "~/.cache/example"
container-path = "/cache"
access         = "read-write"   # "read-only" (default) | "read-write"
```

The `image` key resolves exactly like `--image`: an `[images.<name>]` config name first
(Dockerfile-built sidecars get content-hash caching for free), then a raw podman ref, which
must be present locally. An optional `[sidecars.<sc>.security]` block reuses the
whole security surface of the primary -- capability keys, `no-new-privileges`, and `devices`
alike -- and each sidecar's block stands on its own, so opting one out of `no-new-privileges`
leaves the others hardened. `start = "manual"` declares a sidecar that does not start with the
session (a later release adds the surfaces that start one mid-session; until then its servers
are skipped with a notice).

**Naming and labels.** Sidecar containers are named `outrig-<sid>-<sc>`; an anonymous sidecar
uses its server's name as `<sc>`. Every session container -- primary included -- carries the
podman label `org.outrig.session=<session-id>`, and sidecars additionally carry
`org.outrig.sidecar=<sc>`. The session record lists sidecar container names next to
`container_name`.

(A library caller declares the same containers with `SidecarSpec`, which mirrors this block
minus `start` and `on-failure` -- a spec starts when `Outrig::add_sidecar` is called, and
launch-time sidecars are abort-only. Naming, labels, and lifecycle coupling are identical,
so `outrig clean` sweeps a library session's strays the same way.)

**Lifecycle coupling** is entirely outrig-managed (no pods, no `--requires`): sidecars start
after the primary and stop before it, and the same three cleanup layers -- explicit stop, Drop,
and the panic-hook sweep -- cover every container. In addition, sessions with sidecars run a
`podman wait` watcher on the primary: if the primary dies out from under outrig (manual
`podman kill`, OOM), the watcher reaps all sidecars and ends the session with an error.
A stray that survives even that (say, a SIGKILLed outrig) is caught by `outrig clean`, which
sweeps stopped, record-less containers carrying `org.outrig.session`; see
[Sessions -> outrig clean](../usage/sessions.md#outrig-clean).

Session `[network]` policy applies to every container: the network interceptor attaches to each
sidecar the same way it attaches to the primary, before any MCP server connects.

## What outrig sets in the run

outrig adds `--userns=keep-id`, the primary workspace bind-mount, any configured extra
workspace mounts, and the runtime user-mapping bootstrap (see [Workspace](workspace.md)). The
bootstrap runs from the host: a forked child joins the container's user namespace, becomes its
root, joins its mount namespace, and appends the missing `/etc/passwd` and `/etc/group` entries
before creating `/home/<user>`. No `podman exec` is involved, and nothing is written when podman's
`keep-id` mapping already planted the entries. A
`view = "primary"` sidecar is the one exception to `keep-id`: it runs `--userns=container:<primary>`
to join the primary's user namespace, plus `--cap-add=SYS_ADMIN`/`SYS_PTRACE`, the primary's
`/proc/<pid>/ns` directory, and the `outrig-enter` launcher as its `--entrypoint` (see
[MCP Servers](mcp-servers.md#primary-filesystem-view)). It keeps its own PID namespace, so the
launcher mounts a fresh `proc` over the primary's before the exec -- `/proc/self` in the
primary's procfs would name nothing. The launcher's argv carries
`--uid`/`--gid` with the session user's ids: it holds those capabilities only until the graft is
in place, then becomes that user for the exec, so the *server* runs unprivileged even though the
container was created privileged.
`--security-opt=no-new-privileges` goes on too unless the selected image-config sets
`no-new-privileges = false`. Capability flags are emitted only when that image-config opts
into a capability profile or explicit `cap-drop` / `cap-add` entries, and `--device=<path>`
flags only when it declares `devices`. Session containers additionally carry the
`org.outrig.session` label (and sidecars `org.outrig.sidecar`).

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
