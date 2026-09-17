# 0029 -- mistralrs weight-source fields belong on the model

## Goal

`LlmProvider::Mistralrs` currently carries `model-id`, `model-path`, `model-file`,
`revision`, and `context-length`. Those describe a specific set of weights, which
is a *model* concern; only the runtime fact ("use the in-process mistralrs
runtime") belongs on the provider. The current shape forces a separate
`[providers.<name>]` entry per model, and leaves `[models.<name>].identifier`
ambiguous for the mistralrs case (mistralrs has no API identifier to send -- the
weights *are* the model).

Reshape the schema so a single `mistralrs` provider can back many `[models.*]`
entries, each with its own weights spec.

## Sketch of the target shape

```toml
[providers.local]
style = "mistralrs"
# no other fields -- this is just "use the in-process runtime"

[models.phi3-fast]
provider       = "local"
model-id       = "microsoft/Phi-3-mini-4k-instruct-gguf"
model-file     = "Phi-3-mini-4k-instruct-q4.gguf"
# revision     = "main"
# context-length = 4096

[models.llama-local]
provider   = "local"
model-path = "/var/cache/outrig/models/llama-3-8b-instruct.q4.gguf"
```

For openai-style providers, `[models.<name>]` keeps its existing `identifier`
field (the string sent over the wire). The schema needs to express "identifier
required when provider style is openai; weights-source required when style is
mistralrs." A flat optional-field struct + cross-reference validation is
probably simpler than a tagged enum, but the implementer should pick.

## Deliverables

- `src/config/mod.rs`:
  - `LlmProvider::Mistralrs` becomes a unit variant (or carries only true
    runtime knobs, if any survive the move -- none today).
  - `Model` gains optional `model-id` / `model-path` / `model-file` / `revision`
    / `context-length` fields (or whichever representation wins).
- `src/config/validate.rs`: the existing mistralrs rules
  (`MistralrsMissingModelSource`, `MistralrsBothModelSources`,
  `MistralrsExtraFieldRequiresModelId`, `MistralrsModelPathMissing`) move from
  per-provider to per-model. Add a new rule that an openai-style provider must
  not have weight-source fields on its models, and a mistralrs-style model
  must not have `identifier`.
- `src/llm.rs`: `ResolvedProvider::Mistralrs` no longer carries weight fields;
  they now live on the resolved model. The `RigAgent::Mistralrs` builder loads
  weights from the model spec instead of the provider spec.
- `src/llm/mistralrs.rs` and `src/llm/registry.rs`: signature updates to take
  the weights from the model.
- `src/config/init.rs`: the prompts move out of the provider loop and into the
  model loop. Provider-side mistralrs prompt collapses to "Provider name
  [default: local]" plus the "Add another?" loop. Model-side gets the
  `Use auto-download by model ID? [Y/n]` gate and weight-source prompts.
- `tests/fixtures/config-full.toml` and any other fixtures that exercise the
  mistralrs branch.
- Docs:
  - `doc/reference/config.md` -- regenerate the `style = "mistralrs"` table
    under `[providers.<name>]` (mostly empty after the move) and add a parallel
    "mistralrs models" subsection under `[models.<name>]`.
  - `doc/concepts/in-process-llm.md` -- update the example TOML and the
    field-by-field walkthrough.
  - `doc/usage/config.md` -- update the `outrig config init` walkthrough so
    it reflects the new prompt order (provider loop, then model loop with
    weight prompts).

## Acceptance

- `cargo test` passes; the mistralrs validation tests in `validate.rs` move to
  exercise model-side rules, and `tests/config_init_scripted.rs` adds a
  scripted-stdin case for the mistralrs branch (skipped today because the
  current schema makes the prompt path unwieldy to script).
- `cargo build --features mistralrs` still produces a working binary against
  the new schema; an e2e check (or a manual smoke) loads a real model through
  one provider entry and two distinct `[models.*]`.
- A re-grep of `doc/` for the old shape (`style = "mistralrs"` immediately
  followed by `model-id`/`model-path` on the same `[providers.*]` table) finds
  nothing.

## Dependencies

- 0005-config-merge-validate
- 0024-config-init

## Notes

- Surfaced during 0024 implementation: prompting for `model-id` while inside
  the provider loop felt wrong because the field is per-model. The current
  prompt UX faithfully reflects the as-shipped schema; this task fixes the
  schema and the prompts together.
- The `style` tag stays on the provider -- it's still the right home for "wire
  format / runtime kind." Only the weight-source fields move.
- `model-cache-root` (top-level config key) is unaffected; it's a runtime-wide
  setting, not per-model.
- Migration from any in-the-wild configs is a hand-edit: move the four/five
  fields from `[providers.<name>]` to a new `[models.<name>]` and add
  `provider = "<name>"`. Worth a one-paragraph note in the eventual changelog
  but not a programmatic migrator -- v0.

## Decisions

- **Flat optional fields on `Model`, not a tagged enum.** Matches `Agent`'s
  shape; cross-validation already lived in `validate.rs`; a tagged enum would
  force a discriminator on every model row for no reader benefit. `Model`
  grows `identifier`, `model_id`, `model_path`, `model_file`, `revision`,
  `context_length`, all `Option`. Validation is what enforces the
  per-provider-style invariants.
- **`Model::identifier` becomes `Option<String>`.** Required for openai-style
  providers, forbidden for mistralrs-style (the weights *are* the model).
  Validation enforces both rules. Two new error variants
  (`OpenAiModelMissingIdentifier`, `MistralrsModelHasOpenAiField`) plus the
  symmetric `OpenAiModelHasMistralrsField` for openai models that wandered
  into a weight-source field.
- **`LlmProvider::Mistralrs` and `ResolvedProvider::Mistralrs` are unit
  variants.** No runtime knobs survived the move. The `style` discriminator
  on the config provider stays; on the resolved provider the unit shape is
  trivially what the agent loop needs (the mistralrs branch reads weights
  off the resolved model).
- **Resolved-side weight spec lives in a new `MistralrsWeights` substruct on
  `ResolvedAgent`.** `Option<MistralrsWeights>` is `Some` for mistralrs
  models, `None` for openai. Considered putting the five fields directly on
  `ResolvedAgent`; the substruct keeps the openai path from carrying dead
  fields and reads cleaner at the consume site (`build_agent`).
- **Registry rekey from provider name to model name.** With weights now
  per-model, two models on one mistralrs provider can declare distinct
  weight specs and need distinct loaded engines. The registry's generic
  string key didn't change shape; only the call site
  (`registry.get_or_init(&resolved.model_name, ...)`) and the doc comment
  shifted. `tests/llm_registry.rs` uses opaque labels, so it kept passing
  unchanged.
- **`mistralrs::load`'s first arg renamed `provider_name` -> `model_name`.**
  Used for tracing and error context only. `LlmResolveError::MistralrsLoad`
  and `MistralrsContextTooLong` field renamed `provider:` -> `model:` to
  match. `MistralrsFeatureDisabled { name }` kept its provider-name
  semantics: the feature gate is a build-of-the-binary fact, and the error
  orients the user toward the provider definition.
- **`ResolvedAgent.model_identifier` stayed `String`.** Display use only
  (banner). Filled from `model.identifier` for openai (validation guarantees
  Some; defensive `unwrap_or_else` falls back to model name otherwise) and
  from a derived chain `model_id -> basename(model_path) -> model_name` for
  mistralrs. The `mistralrs::load` shim derives its own internal identifier;
  this is a separate display concern.
- **Prompt flow: provider loop becomes minimal for mistralrs; weight
  prompts move into the model loop, dispatched on the chosen provider's
  style.** `prompt_mistralrs_provider` is gone. `prompt_models` switches on
  the provider's style after asking for `provider`. `model-file` stays
  unprompted (per 0024's decision, multi-file repos hand-edit).
- **`tests/config_init_scripted.rs` answers `ask_select` by value, not
  index.** First scripted attempt sent `"1"`; the prompt parses values and
  re-prompted, eating an extra line. Switched to `"mistralrs"`.
- **Fixture `[models.llama-local]` uses a relative `model-path` and the
  `fixture_loads_end_to_end` test creates the dummy GGUF.** An absolute path
  would have failed the existence check on tempdir-rooted validation. The
  HF-id form alone would have skipped the path-existence rule entirely;
  keeping both forms exercises both validation branches.
