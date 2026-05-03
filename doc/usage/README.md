# Usage

> **TODO: Incomplete** -- the [`outrig init`](init.md), [`outrig config`](config.md),
> [`outrig container`](container.md), and [`outrig build`](build.md) pages describe intended
> behavior; those subcommands are not yet wired up. [`outrig run`](run.md) and
> [Sessions](sessions.md) document subcommands that are implemented today.

This section is the day-to-day operator's guide: what each subcommand does, what the REPL looks
like in practice, what comes back when something goes wrong.

- **[outrig init](init.md)** -- one-shot setup orchestrator: runs `config init` if needed,
  writes `.agents/outrig/config.toml`, and loops `container add` to scaffold containers.
- **[outrig config](config.md)** -- group of commands for outrig's configuration. v0:
  `config init` (writes the global `~/.outrig/config.toml`).
- **[outrig container](container.md)** -- group of commands for container-configs. v0:
  `container add` (scaffolds a Dockerfile under `.agents/outrig/containers/<name>/`).
- **[outrig run](run.md)** -- start an interactive agent session. The main subcommand.
- **[outrig build](build.md)** -- pre-warm the image cache so the next `outrig run` is instant.
- **[Sessions](sessions.md)** -- `outrig ls`, `outrig logs`, `outrig discard`.
- **[Recipes](recipes.md)** -- common patterns (multiple container-configs, capturing
  transcripts, scripted single-prompt runs).

For exhaustive flag-by-flag reference, see [Reference -> CLI](../reference/cli.md).
