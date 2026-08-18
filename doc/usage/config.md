# `outrig config`

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
? Provider for this model [default: openai]:
? Model identifier [default: gpt-4o-mini]:

? Add another model? [y/N]:
? Use this model as default-model? [Y/n]:

[outrig] wrote ~/.outrig/config.toml
```

The prompts that follow depend on the style you pick.

`anthropic` asks the same connection questions with its own defaults
(`https://api.anthropic.com`, `ANTHROPIC_API_KEY`), and its models get one extra prompt:

```sh
? Model identifier [default: claude-sonnet-4-6]:
? max-tokens for this model [default: 64000]:
```

Anthropic's API requires an output-token ceiling on every request, so the generated config
carries one explicitly rather than leaving a model whose identifier outrig does not
recognize to run on the conservative fallback outrig would otherwise apply. See
[Concepts -> LLM Providers](../concepts/llm-providers.md#native-anthropic-style--anthropic).

If you pick `mistralrs` as the provider style -- **deprecated**, and offered only for builds
that still carry `--features local-llm` -- the provider itself has no follow-up
prompts -- it's just a tag. The weight-source prompts (`Use auto-download by model
ID?`, `HuggingFace model-id` or `Local model-path`, `revision`, `context-length`) are
asked once per model in the model loop, since each model carries its own weight spec.
New configs should pick `openai` and point it at a local Ollama/vLLM/`llama.cpp` server
instead; see [In-process LLMs](../concepts/in-process-llm.md).

### Help at any prompt

Type `?` and Enter at any prompt to get a short explanation of the field and its options. The
prompt is then re-displayed so you can answer:

```
? Pick a provider style [default: openai]: ?

  Which wire format / runtime this provider speaks. 'mistralrs' is DEPRECATED and will
  be removed -- prefer 'openai' pointed at a local Ollama/vLLM/llama.cpp server.
  openai  OpenAI Chat Completions wire format. Works with OpenAI, OpenRouter, vLLM, Ollama.
  anthropic  Anthropic's native Messages wire format. Talks to Claude directly.
  mistralrs  DEPRECATED (will be removed): in-process LLM via the mistralrs crate. Prefer
  'openai' pointed at a local Ollama/vLLM/llama.cpp server.

  See: https://tgockel.github.io/outrig/concepts/llm-providers.html

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
