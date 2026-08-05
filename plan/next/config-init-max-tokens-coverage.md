# `config init` emits `max-tokens` only for Anthropic models

## Problem

`max-tokens` is settable in two places -- `[models.<name>]` and `[agents.<name>]`, resolved
`agent.max_tokens.or(model.max_tokens)` in `llm.rs::resolve_agent`. The init walk covers one
corner of that surface:

| Init path                            | Prompts for `max-tokens`? |
|--------------------------------------|---------------------------|
| `[models.<name>]`, anthropic         | yes, default `64000`      |
| `[models.<name>]`, mistralrs         | no                        |
| `[models.<name>]`, every other style | no                        |
| `[agents.<name>]`, all styles        | no                        |

The anthropic arm (`prompt_anthropic_model`) exists because that API *rejects* a request with
no `max_tokens`, so init had to produce one to make a generated config work on the first turn.
That reasoning is sound and the arm should stay. It just means the key got built for the
provider that fails loudly without it, and nothing generalized from there.

The agent row is the wider gap: `init/repo.rs::render` builds an `Agent::default()` and sets
only `model` and `preamble`, so no generated config has ever carried an agent-level ceiling --
including the per-agent override that `config.md` documents as the higher-priority tier.

## Severity, honestly

Not a bug. Every uncovered path has a working fallback, which is why this sat unnoticed:

- OpenAI-compatible endpoints treat an absent ceiling as "use remaining context", which is
  usually what you want.
- mistralrs maps `max_tokens` to `sampling_params.max_len` (`llm/mistralrs.rs:371`), and
  `None` leaves the length limit to the runtime. Worth confirming what mistralrs actually
  does with `max_len: None` before relying on this -- the crate is behind the optional
  `local-llm` feature and is not vendored in this checkout, so it was not verified here. If
  `None` turns out to mean "generate until context exhaustion", the local path is the one
  case where a prompted ceiling earns its keep.

So the cost is not breakage, it is that the generated config is silent about a knob the user
later has to discover from the docs, and that a long-running local model has no emitted place
to cap output. Worth weighing against prompt-walk length before doing it -- the walk is already
long, and adding a question to every model is a real UX cost for a key most users can leave
unset.

## Sketch

- Lift the ceiling prompt out of `prompt_anthropic_model` into the shared part of
  `prompt_models_loop`, keeping the anthropic default at `64000` and defaulting **blank
  (omit)** everywhere else. Blank-to-omit is what `ask_optional_u32` already does, so an
  unset key stays unset and today's non-anthropic output is byte-for-byte unchanged unless
  the user types a number.
- Reword `MODEL_MAX_TOKENS_FIELD`'s description, which is currently anthropic-specific
  ("Anthropic requires one on every request") and would be misleading on an OpenAI model.
  Probably two `Field` constants rather than one hedged string.
- Decide whether the agent walk gets a prompt at all. Leaning no: the model-level key covers
  every agent pointed at it, and the agent-level one is a per-role override that a
  single-agent generated config has no use for. If it is added, it belongs behind the same
  blank-to-omit default, and `init/repo.rs::render` needs the field plumbed through
  `render(..)`'s argument list.
- If a new `Field` lands, append it to the module's `DOC_SYNC_FIELDS` -- `prompt_doc_sync.rs`
  discovers fields from those slices manually and will not catch an omission.

## Acceptance

- Accepting every default on a non-anthropic walk produces a config with no `max-tokens` key,
  identical to today's output.
- Entering a number on an openai-style model emits `max-tokens` under `[models.<name>]`, and
  the result passes `Config::load_from_str` + `validate`.
- The anthropic walk still emits `max-tokens = 64000` on defaults --
  `config_init_scripted.rs::writes_anthropic_config_with_max_tokens` passes unchanged, or its
  script is updated in exactly one place if a shared prompt shifts the question order.
- `config.md`'s claim that init "prompts for `max-tokens` when it writes an Anthropic model"
  is updated to match whatever the walk actually does.

## See also

- `crates/outrig-cli/src/config_init.rs` -- `prompt_models_loop`, the three-arm `match` on
  `LlmProvider`, `prompt_anthropic_model`, `ask_optional_u32`.
- `crates/outrig-cli/src/init/repo.rs` -- `render`, where the agent is built.
- `crates/outrig-cli/src/llm.rs:413` -- the `agent.or(model)` precedence this mirrors.
- `doc/reference/config.md` -- the three-tier ceiling section, and line 371, which is the
  sentence Acceptance says must be updated. It is a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/reference/config.md`, so there is one file to edit
  rather than two copies to keep in sync.
