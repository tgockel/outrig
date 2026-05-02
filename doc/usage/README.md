# Usage

> **TODO: Incomplete** -- these pages describe outrig's intended behavior; the implementation
> is in progress.

This section is the day-to-day operator's guide: what each subcommand does, what the REPL looks
like in practice, what comes back when something goes wrong.

- **[outrig init](init.md)** -- interactive setup of `~/.outrig/config.toml` (providers, models)
  and `.agents/outrig/config.toml` (workspace, agent). Chains into `init-container`.
- **[outrig init-container](init-container.md)** -- scaffold a container-config with a
  Dockerfile under `.agents/outrig/containers/<name>/`.
- **[outrig run](run.md)** -- start an interactive agent session. The main subcommand.
- **[outrig build](build.md)** -- pre-warm the image cache so the next `outrig run` is instant.
- **[Sessions](sessions.md)** -- `outrig ls`, `outrig logs`, `outrig discard`.
- **[Recipes](recipes.md)** -- common patterns (multiple container-configs, capturing
  transcripts, scripted single-prompt runs).

For exhaustive flag-by-flag reference, see [Reference -> CLI](../reference/cli.md).
