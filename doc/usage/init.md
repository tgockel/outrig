# `outrig init`

> **TODO: Incomplete** -- every command and behavior on this page describes outrig's intended
> behavior; the implementation isn't ready yet.

`outrig init` is the one-shot setup for a new repo. It's a thin orchestrator over two more
focused commands plus one inline repo-config phase:

1. If `~/.outrig/config.toml` is missing, run [`outrig config init`](config.md#outrig-config-init).
2. Create `.agents/outrig/` and `.agents/outrig/config.toml` if absent.
3. Offer to call [`outrig container add`](container.md#outrig-container-add) in a loop.

Each phase is idempotent. Re-running `outrig init` on a fully-set-up repo does nothing for
the first two phases; the container loop is always offered (you may want to add another
container-config later).

## Synopsis

```
outrig init [--force]
```

| Flag      | Default | Description                                                                |
|-----------|---------|----------------------------------------------------------------------------|
| `--force` | off     | Overwrite existing files. Propagates to `config init` and `container add`. |

## Run it

```sh
$ cd hello-outrig
$ outrig init
```

If `~/.outrig/config.toml` doesn't exist, outrig walks you through providers and models first
-- the same flow as `outrig config init`, inlined here so first-time users finish in one
session. Every prompt shows the default in `[default: ...]`; press Enter to accept it. Type
`?` and Enter at any prompt for an explanation of what's being asked plus the available
options.

```
[outrig] no global config found at ~/.outrig/config.toml -- let's create one.

? Pick a provider style [default: openai]:
? Provider name (used in models) [default: openai]:
? Base URL [default: https://api.openai.com/v1]:
? API key environment variable [default: OPENAI_API_KEY]:
? Define a model now? [Y/n]:
? Model name (used in agents) [default: fast]:
? Model identifier [default: gpt-4o-mini]:
? Provider for this model [default: openai]:
? Use this model as default-model? [Y/n]:

[outrig] wrote ~/.outrig/config.toml
```

If the global config already exists, that step is skipped:

```
[outrig] using existing global config at ~/.outrig/config.toml
```

Then the repo half:

```
[outrig] no repo config at .agents/outrig/config.toml -- let's create one.

? Workspace host-path [default: .]:
? Workspace container-path [default: /workspace]:
? Default agent name [default: coding]:
? Override default-model for this agent? [y/N]:
? Preamble (one line, edit later) [default: You are a careful coding assistant.]:

[outrig] wrote .agents/outrig/config.toml
```

The agent doesn't get its own `model` field unless you say "yes" to overriding the default --
otherwise it inherits `default-model` from the global config.

Finally the container loop:

```
? Add a container-config now? [Y/n]:

  ... (calls `outrig container add` -- see that page for the full prompt sequence)

? Add another container-config? [y/N]:
```

Press Ctrl-C at any prompt to stop; partial files are not written until all answers are
gathered.

## What gets written

A minimal `.agents/outrig/config.toml` (containers will be filled in by `container add`):

```toml
default-container = "coding"
default-agent     = "coding"

[workspace]
host-path      = "."
container-path = "/workspace"

[agents.coding]
# inherits default-model from the global config
preamble = "You are a careful coding assistant."
```

The global `~/.outrig/config.toml` is written by
[`outrig config init`](config.md#outrig-config-init); the per-container Dockerfile and
`[containers.<name>]` block come from
[`outrig container add`](container.md#outrig-container-add).

## Re-running

Without `--force`, `outrig init` skips work that's already done:

- Global config present -> skip the `config init` phase.
- Repo `config.toml` present -> skip the repo-config phase.
- Container loop -> always offered, since adding more containers later is the expected
  workflow.

With `--force`, every nested write rewrites in place. You probably want the more targeted
commands instead -- `outrig config init --force` or `outrig container add <name> --force`.

> **TODO: Incomplete** -- non-interactive mode (`outrig init --provider openai --model fast
> ...`) is deferred.

## See also

- [outrig config init](config.md#outrig-config-init) -- the global-config phase, runnable on
  its own.
- [outrig container add](container.md#outrig-container-add) -- scaffolds a container-config;
  `init` calls it in a loop.
- [Concepts -> LLM Providers](../concepts/llm-providers.md) -- what providers, models, and
  agents are and how they compose.
- [Reference -> Config](../reference/config.md) -- every key in both config files.
