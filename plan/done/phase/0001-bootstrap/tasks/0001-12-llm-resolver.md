# 0012 -- LLM resolver

## Goal

Turn an agent name (from `--agent` or `default-agent`) plus the loaded `Config` into a
constructed Rig provider client + agent state ready to receive a turn. Surfaces every
configurable knob (preamble, temperature, max-tokens) and resolves api-key from the env at
this point.

## Deliverables

- `src/llm.rs::ResolvedAgent`:
  ```rust
  pub struct ResolvedAgent {
      pub agent_name: String,
      pub model_name: String,
      pub model_identifier: String,
      pub provider_name: String,
      pub provider_style: String,        // "openai", etc.
      pub provider_base_url: String,
      pub api_key: String,                // resolved from env
      pub preamble: String,
      pub temperature: Option<f32>,
      pub max_tokens: Option<u32>,
      pub container: Option<String>,
  }
  ```
- `src/llm.rs::resolve_agent(cfg: &Config, agent_name: &str) -> Result<ResolvedAgent>`:
  - Look up `cfg.agents.get(agent_name)`; error if missing.
  - Resolve model: agent's `model` if set, else `cfg.default_model`.
  - Look up the model entry; error if missing.
  - Look up the provider entry; error if missing.
  - Resolve `api_key` via `ApiKeyRef::resolve()`; error if env var unset.
  - Apply preamble default if agent's preamble is `None`: a minimal one-liner like
    `"You are a careful assistant operating inside a sandboxed container."`.
- `src/llm.rs::build_rig_client(resolved: &ResolvedAgent)` returning a Rig openai client:
  - For `style == "openai"`: `rig::providers::openai::Client::from_url(&resolved.api_key,
    &resolved.provider_base_url)` (verify against the locked rig-core version).
  - For any other style: error with `style "<x>" is not yet supported in v0; v0 wires "openai"
    only`.
- `src/llm.rs::build_agent(resolved: &ResolvedAgent, client: &..., tools: Vec<McpToolAdapter>)
  -> Result<rig::agent::Agent<...>>`:
  - `client.completion_model(&resolved.model_identifier)`.
  - `AgentBuilder::new(...)` with preamble + sampling params + dynamic tools.
- `tests/llm_resolve.rs` covering:
  - Agent inheriting `default-model`.
  - Agent with explicit model override.
  - Missing agent -> clear error naming `--agent` and `default-agent`.
  - Missing model -> error.
  - Unsupported provider style -> error with the message above.
  - Unset api-key env var -> error.

## Acceptance

- `cargo test llm_resolve` passes every case.
- Drop the `> TODO: Incomplete` marker on `doc/concepts/llm-providers.md`.

## Dependencies

- 0005-config-merge-validate

## Notes

- The `build_rig_client` / `build_agent` signatures are likely going to require generics or
  `dyn`; defer the exact return type to whichever shape rig-core's API actually uses. The
  tests in this task can pure-test `resolve_agent` only -- `build_*` lives behind a feature
  gate or simply isn't tested in unit form here, since it depends on rig-core specifics.
- Don't construct the Rig agent eagerly during config validation; it requires network state
  and is a per-run thing.
