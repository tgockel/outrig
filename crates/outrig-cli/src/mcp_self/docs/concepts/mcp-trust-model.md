# Trust Model

OutRig's trust boundary is the session's container. What the agent runs, runs inside it: under
`outrig run` and `outrig run-legacy`, the MCP servers an image-config declares and every command
they run; under `outrig run-new`, the agent's own Python, in an interpreter OutRig mounts into the
container, and every program that Python starts. All of it sees the container's filesystem and
runs with the user mapping OutRig creates at session startup. Calling an MCP server's tools is one
way an agent acts, and writing Python is another; the boundary is the same for both.

The host stays outside that boundary except for what OutRig explicitly connects: the workspace
mount and any configured mounts, the runtime services OutRig attaches to the container, and, for
`outrig run-new`, the bindings the operator declares -- objects a process on the host holds for
the agent, which act there with the host user's authority (see
[Across the boundary: bindings](#across-the-boundary-bindings)).

That means a tool inside the container does not need a second application-level sandbox to be
safe for normal agent work. The container is already the place where the agent is allowed to
inspect files, run commands, and coordinate tools.

## The agent cannot grow its own environment

Every other property on this page depends on one rule: nothing the agent can invoke adds to what
the session was given. Containers, sidecars and MCP servers come from the configuration or from
the operator -- `/sidecar add` is typed by the person at the REPL, and no tool does it -- and
bindings come from the configuration, from a program that embeds OutRig, or from a host program's
`inject`. A binding a repository's configuration declares waits for the operator's approval,
because the agent can edit that configuration.

Agents the agent launches do not bend this. A `run` [subagent](subagents.md), launched with
`outrig__subagent`, runs in the container that is already there, over the MCP connections that
are already open. A `run-new` child agent is a kernel of its own in the same interpreter, given the
same bindings. Neither starts a container, connects a server, or creates a binding, and neither
can reach anything the operator did not already grant the session: each is a second consumer of a
fixed environment, not a way to widen one.

The rule governs what the agent can add, not what a granted thing can do. A binding to a library
that runs programs lets the agent run them on the host, and a sidecar with `view = "primary"`
reads all of the primary's files; each is a grant the operator makes.

## Inside the container

Configure tools liberally inside the container, and keep the container itself deliberate.

**Files.** A filesystem MCP server can be pointed at broad container paths, including `/`, when
that is the right shape for the container, and `run-new`'s Python reads whatever the container
can. "The agent can read everything" means everything in the container, not everything on the
host. For a coding container, `/workspace` is usually the right root for a server because it
matches the mounted repo; a purpose-built operations container may want a broader one, if the
image holds reference files, generated config, local SDKs, or test fixtures outside the
workspace.

**Commands.** A shell MCP server does not need a command allowlist inside a normal OutRig
container, and `run-new`'s Python runs any command with `subprocess`. Arbitrary code execution
inside the container is the point of giving an agent a development environment. If the agent needs
`bash`, package managers, compilers, or project-specific CLIs, install them in the image.

**Secrets.** Keep secrets and host-only material out of the container unless the agent should be
able to use them. Whatever reaches the container is the agent's to read: an MCP server's `env` is
readable, where `/proc` allows, by every other process the session user runs there. That is why
`outrig run-new` starts no MCP server and no sidecar: a server in the primary would run beside the
agent's interpreter as the same user, with whatever its `env` resolved, so no credential from an MCP
or sidecar block reaches a `run-new` session's container. For the same reason, OutRig turns off
podman's copying of the host's proxy variables into every container it starts.

**Network.** What the agent's code reaches over the network is bounded by the container's network
namespace and by the session's network policy, which covers every container of the session:
[`[network]`](../reference/config.md#network) audit mode records each connection and filter mode
refuses what the policy does not allow. A tool's configuration should still describe what the tool
needs, rather than treat the tool as harmless.

## Least-privilege placement with sidecars

The container boundary does not have to be *one* container. Placing an MCP server in a
[sidecar](mcp-servers.md#sidecar-placement) narrows what that server can see to what its
container is granted: no workspace unless `workspace = "ro"`/`"rw"` is set, no extra paths
unless mounted, its own image's toolchain rather than the workspace's. An off-the-shelf MCP
image can run tools for the agent without ever seeing the repo.

Two properties keep the placement deliberate. First, placement is repo-config-only: an image's
`org.outrig.mcp` label can declare servers for the image that carries it, but cannot direct a
server into some other container. Second, sidecars come from the config or the operator, as the
rule above says. Session network policy covers every container, so a sidecar is not a way around
audit or filter mode. Installing and removing that policy runs nothing from a container's image,
either: outrig rewrites and restores the container's `/etc/resolv.conf` itself, so a hostile
image gets no foothold outside its own namespaces while policy is being put in place.

The one placement that *widens* rather than narrows is
[`view = "primary"`](mcp-servers.md#primary-filesystem-view). Such a sidecar joins the primary's
mount namespace and can read the primary's entire filesystem -- which is the point, but it means
the sidecar image is now **inside** the primary's trust boundary, as trusted as the primary image
itself, not beside it. What still bounds it: the `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` the join needs
belong to OutRig's `outrig-enter` launcher, scoped to the rootless user namespace (not host root)
the primary already runs in, and the server is exec'd only after the launcher becomes the
session's uid/gid -- which clears those capabilities -- and sets `no_new_privs`, so that no setuid
binary or file capability the server runs can hand them back. The server owns what it writes and
can pass no privilege to anything it spawns, and that holds whatever the container was started
with: the launcher sets the flag itself, and a `view = "primary"` sidecar cannot opt out of
`no-new-privileges` (config validation refuses it). The placement is opt-in and defaults to
`"none"`; it is still a container, so cgroups, seccomp, and network policy all still apply; and
only the mount namespace is joined -- PID, network, and cgroup stay the sidecar's own. Grant it
only to a sidecar image you trust with the primary's files: it reads them all, even though it
writes as you.

## The configuration is inside the boundary

Part of the config that shapes a session is inside the boundary. `.agents/outrig/config.toml`
lives in the workspace, which is mounted read-write, so an agent can edit it in one session and
have the next session load the result. That is why what reaches the host's secrets stays
global-only: a provider carrying an `api-key` -- a reference to a variable in the operator's
environment, paired with the `base-url` it is sent to -- is refused in a repo config, as the
`[network]` policy keys are. A repo config may still name global providers from its models. A
repo config file may reference a host variable, in `build-args` or an MCP server's `env`, only
if the global config lists it in [`[repo]` `env-allow`](../reference/config.md#repo): the value
goes to code the repo controls, so a variable on that list is the repo's to route. A binding a
repo config declares starts only once the operator approves that exact declaration, because its
factory is code the host runs. For the same reason, a session whose workspace would contain the
global config is refused, whether a repo config declared that workspace or outrig picked it; the
global config's own `[workspace]` and `--volume` are the operator's choice.

A repo can also reach the host through an image. An `org.outrig.mcp` label is not a config file,
but a reference in a server's `env` there is read from the host the same way, so the label is held
to the same `env-allow` list. It is text the image's author wrote, whether that is a Dockerfile in
the repo, which the agent can edit, or an image the repo names.

## Across the boundary: bindings

A binding is the one thing that runs outside the container for the agent: an object a process on
the host holds, which the agent's Python uses under a name. It acts with the host user's
authority -- your files, your `ssh-agent`, your credential helpers, your environment less the
variables OutRig's config names -- and whatever its library does on the host is part of what it
grants, including programs a repository names, such as git's hooks. Every request the agent's code
makes of a binding runs, and is published as events: having the object is permission to use it.
A live object a host program injects acts with that program's authority in the same way. The
agents of a session share one interpreter and every binding, so the grant is the session's, not
one agent's. [Hosted Objects](https://tgockel.github.io/outrig/concepts/hosted-objects.html) says
what a binding grants and what crosses it.

## Practical posture

Configure tools liberally inside the container and keep the container boundary deliberate.
Install the tools the agent needs -- MCP servers for `outrig run`, the toolchains and programs
`outrig run-new`'s Python calls -- expose useful paths, and rely on OutRig's host/container split
instead of building a narrow configuration that prevents useful work. Bind on the host only what
the agent should act on there, and prefer a narrow object that names the effect over the library
that performs it.

## See also

- [Containers](containers.md) -- Dockerfile conventions and runtime user mapping.
- [MCP Servers](mcp-servers.md) -- declaring the tools `outrig run`'s agent calls.
- [Python Runtime](https://tgockel.github.io/outrig/concepts/python-runtime.html) -- the agent
  that acts by writing Python.
- [Hosted Objects](https://tgockel.github.io/outrig/concepts/hosted-objects.html) -- objects on
  the host the agent's Python uses.
- [AI-assisted design](../usage/ai-assisted-design.md) -- using `outrig mcp self` to design
  image-configs with an external AI tool.
