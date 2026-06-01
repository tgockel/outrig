# Usage

This section is the day-to-day operator's guide: what each subcommand does, what the REPL looks
like in practice, what comes back when something goes wrong.

- **[outrig init](init.md)** -- one-shot setup orchestrator: runs `config init` if needed,
  writes `.agents/outrig/config.toml`, and loops `image add` to scaffold image-configs.
- **[outrig config](config.md)** -- group of commands for outrig's configuration. v0:
  `config init` (writes the global `~/.outrig/config.toml`).
- **[outrig image](image.md)** -- group of commands for image-configs. v0:
  `image add` (scaffolds a Dockerfile under `.agents/outrig/images/<name>/`).
- **[AI-assisted design](ai-assisted-design.md)** -- use `outrig mcp self` when the
  built-in image templates do not fit.
- **[outrig run](run.md)** -- start an interactive agent session. The main subcommand.
- **[outrig mcp](mcp.md)** -- expose an image-config's MCP tools to an external client.
- **[outrig build](build.md)** -- pre-warm the image cache so the next `outrig run` is instant.
- **[Sessions](sessions.md)** -- `outrig ls`, `outrig logs`, `outrig discard`,
  `outrig clean`.
- **[Recipes](recipes.md)** -- common patterns (multiple image-configs, capturing
  transcripts, scripted single-prompt runs).

For exhaustive flag-by-flag reference, see [Reference -> CLI](../reference/cli.md).
