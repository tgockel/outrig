# A free-text answer is validated by its caller, outside the prompt that asked for it

## Symptom

`PromptSource` says "Validation failures retry internally"
(`crates/outrig-cli/src/init/prompt/mod.rs`), and for the answers it parses itself -- `ask_bool`,
`ask_select`, `ask_multiselect` -- they do, reported through the prompt's own stream. A free-text
answer checked against anything else is checked by its caller instead, in a hand-written loop that
prints the refusal with `eprintln!` and calls `ask_string` again:

- `config_init.rs`: `ask_required`, the GGUF model-file pick, and the default-model pick;
  `ask_unused_name`, added for #346; `ask_base_url`, `ask_api_key_env`, and `ask_optional_u32`,
  added for #347
- `init/repo.rs`: `ask_agent_model`, `pick_global_model`, and `ask_workspace`'s two loops, added
  for #348
- `image_setup/add.rs`: `ask_name`, added for #184

The refusals bypass the stream a `TerminalPrompt` is built over, so no scripted test can see
them: `a_prompted_name_it_cannot_build_is_asked_again` shows the second answer was taken, not
that the first was refused, and nothing asserts "no model named" or "this field requires a value"
at all. An answer source that isn't a person -- the module docs anticipate one -- is never told
why its answer was refused, and none of the loops has a bound.

## Where it goes

One validating entry point on the trait, e.g. `ask_string_with(field, default, check: impl
Fn(&str) -> Result<T, String>) -> Result<T>`, with each implementation reporting a refusal where
it reports its own: `TerminalPrompt::write_error`, the dialoguer module's `write_error`, and
`AutoPrompt` forwarding. The loops above become single calls; the provider-name loop in
`prompt_models_loop` stays as is, since a miss there branches into more questions rather than
asking again.
