# Concepts

This section explains the moving parts an outrig user actually has to think about. If you've
worked through the [Quickstart](../quickstart.md), you've already touched the first five -- this
is where they're spelled out. OutRig has two agent loops: `outrig run`'s agent acts by calling
the tools of MCP servers in the container, and `outrig run-new`'s, a preview, acts by writing
Python there. The pages say which they describe.

- **[Containers](containers.md)** -- the Dockerfile that defines the agent's environment, the
  `[images.<name>]` config block, named image-configs for switching between toolsets,
  and the UID/GID convention that keeps file ownership sane.
- **[MCP Servers](mcp-servers.md)** -- the tools `outrig run`'s agent calls: how outrig
  discovers and routes them, the `<server>__<tool>` name prefix, lifecycle and crash behavior,
  stderr capture.
- **[Trust Model](mcp-trust-model.md)** -- why the container is the trust boundary, why tools can
  be configured liberally inside it, and why the agent cannot grow its own environment.
- **[Workspace](workspace.md)** -- what the agent can reach on your filesystem, the direct
  bind-mount model, why outrig doesn't stage changes, how to review with git.
- **[Providers, Models, and Agents](llm-providers.md)** -- the three-layer LLM config:
  providers (where), models (what), agents (which preamble + image). Most users keep
  providers and models in the global config; agents are typically per-repo.
- **[Subagents](subagents.md)** -- `outrig run`'s extra agent loops the agent launches itself,
  sharing the session's container and tools: the `outrig__` built-in tools, the explicit result
  inbox they report through, and the depth limit on subagents launching their own.
- **[Python Runtime](python-runtime.md)** -- `outrig run-new`'s agent: executions, rounds and
  turns, names that persist, channels and `runtime.wait`, child agents, `@outrig.agent`,
  `outrig.Agent`, and skills.
- **[Hosted Objects](hosted-objects.md)** -- bindings: objects on the host the agent's Python uses
  under a name, which act with your authority, and what crosses to them.
