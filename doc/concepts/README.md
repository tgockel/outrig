# Concepts

This section explains the moving parts an outrig user actually has to think about. If you've
worked through the [Quickstart](../quickstart.md), you've already touched all four -- this is
where they're spelled out.

- **[Containers](containers.md)** -- the Dockerfile that defines the agent's environment, the
  `[images.<name>]` config block, named image-configs for switching between toolsets,
  and the UID/GID convention that keeps file ownership sane.
- **[MCP Servers](mcp-servers.md)** -- how outrig discovers and routes tool calls, the
  `<server>__<tool>` name prefix, lifecycle and crash behavior, stderr capture.
- **[MCP Trust Model](mcp-trust-model.md)** -- why the container is the MCP trust boundary and
  why tools can be configured liberally inside it.
- **[Workspace](workspace.md)** -- what the agent can reach on your filesystem, the direct
  bind-mount model, why outrig doesn't stage changes, how to review with git.
- **[Providers, Models, and Agents](llm-providers.md)** -- the three-layer LLM config:
  providers (where), models (what), agents (which preamble + image). Most users keep
  providers and models in the global config; agents are typically per-repo.
- **[In-process LLMs](in-process-llm.md)** -- a feature-gated provider that runs the
  model inside the outrig process itself, for questions whose content must not leave the
  host. Plumbing for the future egress filter, tool-use filter, and prompt-injection
  scanner.
