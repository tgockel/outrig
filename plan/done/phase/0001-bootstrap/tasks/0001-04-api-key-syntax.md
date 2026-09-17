# 0004 -- API-key syntax

## Goal

Enforce the `${VAR}` env-var-substitution syntax for `providers.<name>.api-key` at config-load
time, and resolve from the environment at use time. Refuse literal keys unconditionally so
they never end up in committed config files.

## Deliverables

- `src/config/api_key.rs::ApiKeyRef(String)` -- newtype holding the env var name (without the
  `${...}` wrapper).
- `ApiKeyRef::parse(raw: &str) -> Result<ApiKeyRef>` -- regex `^\$\{[A-Z_][A-Z0-9_]*\}$`.
  Rejects:
  - Literal keys (`sk-...`)
  - Missing braces (`$OPENAI_API_KEY`)
  - Lowercase or mixed-case names (`${OpenAi_key}`)
  - Empty (`${}`)
  - Anything else not matching the regex.
  Each rejection produces a clear error with the offending value and the expected syntax.
- `ApiKeyRef::resolve(&self) -> Result<String>` -- reads `std::env::var(self.0)`; clear error
  on `NotPresent` or `NotUnicode`.
- Custom serde `Deserialize` for `ApiKeyRef` so `providers.<name>.api-key` deserializes
  straight to the typed value.
- Replace the `String` field on `LlmProvider::api_key` from 0003 with `ApiKeyRef`.
- `tests/api_key.rs` covering every accept/reject case from `doc/reference/config.md`'s
  "api-key syntax" section, plus resolve-when-set and resolve-when-unset.

## Acceptance

- `cargo test api_key` passes every case.
- A config containing `api-key = "sk-..."` fails to load with the message documented in
  `doc/concepts/llm-providers.md`'s "API keys are env-var-only" section.
- A config with `api-key = "${UNSET_VAR}"` parses fine but `resolve()` errors clearly.

## Dependencies

- 0003-config-schema

## Notes

- The error message format should be quotable in CI logs; include the `[providers.<name>]` path
  for context.
- Use `regex::Regex` lazily (`once_cell` or `std::sync::OnceLock`); the regex is compiled once.

## Decisions

- **Custom `Deserialize` instead of `#[serde(try_from = "String")]`.** The custom impl is
  five lines and reads as cleanly as the attribute path; keeping the validation flow
  visible in the file paid off when wiring the error type. Custom `Serialize` writes
  `${VAR}` back so the existing round-trip test in `tests/config_schema.rs` stays green.
- **`ApiKeyError` lives in `src/config/api_key.rs`, surfaced through `OutrigError::ApiKey`
  via `#[from]`.** Mirrors the `Config(#[from] toml::de::Error)` pattern from 0003. Avoids
  inflating `error.rs` with three extra variants for a feature that's contained to one
  module.
- **Path context comes from `toml`, not from us.** Inside the custom `Deserialize`,
  errors are raised via `serde::de::Error::custom`, which `toml::de` wraps with line/column
  + the offending source line. That diagnostic is richer than a hand-built
  `[providers.<name>]` prefix for CI logs, so the test asserts on `api-key` and the
  offending value rather than the section path.
- **Regex anchored on both ends.** Even though the task only specified the leading anchor,
  `tests/api_key.rs::parse_reject_trailing_junk` pins down `${VAR} extra` rejection to
  guard against future drift.
- **`std::sync::OnceLock` over `once_cell`.** Stdlib in edition 2024; `once_cell` would be
  a new dep for no benefit.
- **`var_name(&self) -> &str` exposed on `ApiKeyRef`.** Lets callers (incl. error
  formatters and `tests/config_schema.rs`'s spot-check) read the env var name without
  going through `resolve()`. Cheap surface, and the type is otherwise opaque.
- **`InvalidSyntax { value: String }` formats with `{value:?}`.** Debug formatting reveals
  whitespace/newlines/escapes that would otherwise hide in error output -- matters for
  pasted secrets containing stray characters.
