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
