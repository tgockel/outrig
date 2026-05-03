# `outrig config`

> **TODO: Incomplete** -- every command and behavior on this page describes outrig's intended
> behavior; the implementation isn't ready yet.

`outrig config` groups commands that read and write outrig's configuration files. In v0 only
`outrig config init` is implemented; the rest of the group (`config get`, `config set`,
`config list`) is reserved for later -- the same shape as `git config`.

## `outrig config init`

`outrig config init` walks you through the global config at `~/.outrig/config.toml` (or
`<XDG_CONFIG_HOME>/outrig/config.toml` if `XDG_CONFIG_HOME` is set) -- the user/machine-level
providers and models that any outrig repo can reuse.

It's the first phase of [`outrig init`](init.md), which runs `config init` automatically when
no global config is present.

### Synopsis

```
outrig config init [--force]
```

| Flag      | Default | Description                                                                |
|-----------|---------|----------------------------------------------------------------------------|
| `--force` | off     | Overwrite an existing global config. Without it, outrig refuses to clobber.|

### Run it

Every prompt shows the default in `[default: ...]`; press Enter to accept it. Type `?` and
Enter at any prompt for an explanation of what's being asked plus the available options.

```sh
$ outrig config init
[outrig] writing global config to ~/.outrig/config.toml

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

### What gets written

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

### Re-running

Without `--force`, `outrig config init` refuses if the global config already exists:

```
$ outrig config init
error: ~/.outrig/config.toml already exists; pass --force to overwrite.
```

To change one field, edit the file directly. There is no plan to support partial re-init in
v0 -- the config is small enough to edit by hand.

> **TODO: Incomplete** -- non-interactive mode (`outrig config init --provider openai
> --model fast ...`) is deferred.

## See also

- [outrig init](init.md) -- end-to-end orchestrator; runs `config init` automatically when
  the global config is missing.
- [Concepts -> LLM Providers](../concepts/llm-providers.md) -- what providers, models, and
  agents are and how they compose.
- [Reference -> Config](../reference/config.md) -- every key in the global config file.
