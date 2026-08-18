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
- **[In-process LLMs](in-process-llm.md)** -- **deprecated**, pending removal: a
  feature-gated provider that runs the model inside the outrig process itself. Run local
  models under an OpenAI-compatible server (Ollama, vLLM, `llama.cpp`) and point a
  `style = "openai"` provider at it instead.
- **[Subagents](subagents.md)** -- extra agent loops the agent launches itself, sharing the
  session's container and tools. The `outrig__` built-in tools, the explicit result inbox
  they report through, and why a subagent cannot launch subagents.
