# `outrig init`

> **TODO: Incomplete** -- every command and behavior on this page describes outrig's intended
> behavior; the implementation isn't ready yet.

`outrig init` walks you through setting up the two config files outrig needs:

- `~/.outrig/config.toml` (global) -- providers and models, the user/machine-level pieces.
- `.agents/outrig/config.toml` (repo) -- workspace, agents, and (after `init-container`)
  containers.

After both configs are written, `init` chains into [`outrig init-container`](init-container.md)
to scaffold the first container.

## Synopsis

```
outrig init [--force]
```

| Flag      | Default | Description                                                                    |
|-----------|---------|--------------------------------------------------------------------------------|
| `--force` | off     | Overwrite existing config files. Without `--force`, outrig refuses to clobber. |

## Run it

```sh
$ cd hello-outrig
$ outrig init
```

If `~/.outrig/config.toml` doesn't exist, outrig walks you through providers and models first.
Every prompt shows the default in `[default: ...]`; press Enter to accept it. Type `?` and
Enter at any prompt for an explanation of what's being asked plus the available options.

```
[outrig] no global config found at ~/.outrig/config.toml -- let's create one.

? Pick a provider style [default: openai]:
? Provider name (used in models) [default: openai]:
? Base URL [default: https://api.openai.com/v1]:
? API key environment variable [default: OPENAI_API_KEY]:

? Add another provider? [y/N]:

? Define a model now? [Y/n]:
? Model name (used in agents) [default: fast]:
? Model identifier [default: gpt-4o-mini]:
? Provider for this model [default: openai]:

? Add another model? [y/N]:
? Use this model as default-model? [Y/n]:

[outrig] wrote ~/.outrig/config.toml
```

If the global config already exists, outrig skips that step and re-uses what's there.

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

`init` then automatically runs [`outrig init-container`](init-container.md) so you finish with a
working container too. Press Ctrl-C at any prompt to stop; partial files are not written until
all answers are gathered.

### Help at any prompt

Type `?` and Enter at any prompt to get a short explanation of the field and its options. The
prompt is then re-displayed so you can answer:

```
? Pick a provider style [default: openai]: ?

  A provider style is the wire format outrig uses to talk to your LLM endpoint.

  openai     OpenAI Chat Completions wire format. Works with OpenAI itself, OpenRouter,
             Together, vLLM, Ollama, and any compatible endpoint.
  anthropic  (TODO: not yet wired in v0) Native Anthropic API.

  See: doc/concepts/llm-providers.md

? Pick a provider style [default: openai]:
```

The help text comes from the same descriptions used in
[Reference -> Config](../reference/config.md); prompts and reference stay in sync.

## What gets written

A minimal `~/.outrig/config.toml`:

```toml
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
```

A minimal `.agents/outrig/config.toml` (containers will be filled in by `init-container`):

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

## Re-running

`outrig init` without `--force` refuses if either file already exists:

```
$ outrig init
error: ~/.outrig/config.toml already exists; pass --force to overwrite.
```

To re-run a specific piece, edit the file directly or use `outrig init-container` for an
additional container. There is no plan to support partial re-init in v0 -- the configs are
small enough to edit by hand.

> **TODO: Incomplete** -- non-interactive mode (`outrig init --provider openai --model fast ...`)
> is deferred.

## See also

- [outrig init-container](init-container.md) -- the second half of `init`, runnable on its own.
- [Concepts -> LLM Providers](../concepts/llm-providers.md) -- what providers, models, and
  agents are and how they compose.
- [Reference -> Config](../reference/config.md) -- every key in both config files.
