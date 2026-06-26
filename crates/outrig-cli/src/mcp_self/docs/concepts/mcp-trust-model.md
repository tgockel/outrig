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
