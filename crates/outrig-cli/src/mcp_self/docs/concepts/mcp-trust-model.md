# MCP Trust Model

OutRig treats the container as the trust boundary for MCP servers. The tools run inside the
container, see the container filesystem, and execute with the user mapping OutRig creates at
session startup. The host stays outside that boundary except for the workspace mount and the
runtime services OutRig explicitly connects.

That means an MCP server inside the container does not need a second application-level sandbox
just to be safe for normal agent work. The container is already the place where the agent is
allowed to inspect files, run commands, and coordinate tools.

## Filesystem tools

Filesystem MCP servers can be pointed at broad container paths, including `/`, when that is the
right shape for the container. "The agent can read everything" means everything in the container,
not everything on the host.

For a coding container, `/workspace` is usually the right default because it matches the mounted
repo. For a purpose-built operations container, a broader root can be useful if the image includes
reference files, generated config, local SDKs, or test fixtures outside the workspace.

## Least-privilege placement with sidecars

The container boundary does not have to be *one* container. Placing an MCP server in a
[sidecar](mcp-servers.md#sidecar-placement) narrows what that server can see to what its
container is granted: no workspace unless `workspace = "ro"`/`"rw"` is set, no extra paths
unless mounted, its own image's toolchain rather than the workspace's. An off-the-shelf MCP
image can run tools for the agent without ever seeing the repo.

Two properties keep the boundary deliberate. First, placement is repo-config-only: an image's
`org.outrig.mcp` label can declare servers for the image that carries it, but cannot direct a
server into some other container. Second, the agent cannot grow its own environment -- there is
no agent-invocable tool that starts sidecars; new containers come from the config or the
operator. Session network policy covers every container, so a sidecar is not a way around
audit or filter mode.

[Subagents](subagents.md) do not bend this. An agent can launch one with `outrig__subagent`, but a
subagent runs in the container that is already there, over the MCP connections that are already
open. It starts no container, connects no server, and can call nothing the operator did not
already grant the session -- it is a second consumer of a fixed environment, not a way to widen
one.

The one placement that *widens* rather than narrows is
[`view = "primary"`](mcp-servers.md#primary-filesystem-view). Such a sidecar joins the primary's
mount namespace with `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` and can read the primary's entire
filesystem -- which is the point, but it means the sidecar image is now **inside** the primary's
trust boundary, as trusted as the primary image itself, not beside it. What still bounds it: the
capabilities are scoped to the rootless user namespace (not host root) the primary already runs
in; it is opt-in and defaults to `"none"`; it is still a container, so cgroups, seccomp, network
policy, and `no-new-privileges` all still apply; and only the mount namespace is joined -- PID,
network, and cgroup stay the sidecar's own. Grant it only to a sidecar image you trust with the
primary's files.

## Shell tools

Shell MCP servers do not need command allowlists inside a normal OutRig container. Arbitrary code
execution inside the container is the point of giving an agent a development environment. If a
tool needs `bash`, package managers, compilers, or project-specific CLIs, install them in the
image and let the MCP server expose them.

Keep secrets and host-only material out of the container unless the agent should be able to use
them. Environment variables passed to MCP servers are part of the container tool boundary.

## Network tools

Network-capable MCP servers are bounded by the container network namespace and by the host network
policy around the container runtime. Future network interception can add host-level allowlists,
but MCP server config should still describe what the tool needs to do rather than pretending the
server is harmless.

## Practical posture

Configure MCPs liberally inside the container and keep the container boundary deliberate. Install
the tools the agent needs, expose useful paths, and rely on OutRig's host/container split instead
of building a narrow MCP config that prevents useful work.

## See also

- [Containers](containers.md) -- Dockerfile conventions and runtime user mapping.
- [MCP Servers](mcp-servers.md) -- declaring the tools that run inside the container.
- [AI-assisted design](../usage/ai-assisted-design.md) -- using `outrig mcp self` to design
  image-configs with an external AI tool.
