# 0098 -- Native Anthropic Messages API provider

## Context

OutRig currently exposes two Rig-backed provider styles:

- `openai` for remote OpenAI Chat Completions-compatible APIs.
- `mistralrs` for in-process models.

Claude can be used today through an OpenAI-compatible bridge such as OpenRouter, but OutRig cannot
connect directly to Anthropic's native `/v1/messages` API. `style = "anthropic"` is rejected by the
config enum, and runtime dispatch only builds OpenAI or mistralrs completion models.

The limitation is in OutRig, not Rig. The pinned `rig-core = 0.40.0` already ships an Anthropic
provider with:

- `x-api-key` authentication plus the required `anthropic-version` header;
- the native Messages request and response shapes;
- system prompts, conversation history, tool definitions, tool use, and tool results;
- ordinary and SSE-streaming completions;
- reasoning blocks, images, documents, citations, structured output, and prompt caching;
- custom Anthropic-compatible base URLs.

The implementation lives under `rig::providers::anthropic` and requires no additional Anthropic
SDK or Cargo feature. OutRig already enables Rig's `reqwest` and `rustls` features.

This task wires that existing provider through OutRig. It does not implement an Anthropic protocol
adapter from scratch.

## Goal

Allow an OutRig agent to use Anthropic's native Messages API, including OutRig's MCP-backed tool
loop, by selecting `style = "anthropic"` in config.

## User and config surface

```toml
[providers.anthropic]
style                = "anthropic"
base-url             = "https://api.anthropic.com"
api-key              = "${ANTHROPIC_API_KEY}"
request-timeout-secs = 600 # optional; defaults to 600

[models.sonnet]
provider   = "anthropic"
identifier = "claude-sonnet-4-6"

[agents.coding]
model      = "sonnet"
preamble   = "You are a careful coding assistant."
max-tokens = 16384 # optional for model identifiers Rig recognizes
```

Design commitments:

- `base-url` is required, matching the existing `openai` provider shape. Anthropic's official
  endpoint is `https://api.anthropic.com`, with no `/v1`; Rig's `normalize_anthropic_base_url`
  additionally strips a trailing `/v1`, `/messages`, or `/v1/messages`, so both forms work. The
  repo currently writes the `/v1` form in `doc/concepts/llm-providers.md` and in
  `crates/outrig/tests/fixtures/config-full.toml`. Normalize every one of those on the bare form
  as part of this task so config init, the docs, and the fixtures agree.
- `api-key` keeps the existing `${ENV_VAR}`-only contract and is resolved only when the provider is
  used.
- `request-timeout-secs` has the same per-attempt meaning and 600-second default as `openai`.
- Anthropic models use `identifier`, just like other remote models. mistralrs weight fields remain
  invalid.
- OutRig relies on Rig's default `anthropic-version` (`2023-06-01` in the pinned release).
  Configurable versions and `anthropic-beta` headers are deferred until a concrete feature needs
  them.
- The initial integration uses OutRig's existing non-streaming remote turn path. Rig already
  supports Anthropic SSE, but adding remote streaming to the REPL should be a provider-neutral
  follow-up rather than an Anthropic-only behavior change.

## Deliverables

### Config and validation

- Add an `Anthropic` variant to `LlmProvider` in `crates/outrig/src/config/mod.rs`:

  ```rust
  Anthropic {
      base_url: String,
      api_key: ApiKeyRef,
      request_timeout_secs: Option<u64>,
  }
  ```

- Update `crates/outrig/src/config/validate.rs` so Anthropic models require `identifier` and reject
  `model-id`, `model-path`, `model-file`, `revision`, `context-length`, and `device`.
- Rename/factor the OpenAI-specific remote-model validation errors and helper where doing so gives
  clear provider-neutral messages. Errors should still name the actual provider style.
- Preserve provider merge semantics: provider maps merge by name, and a repo provider replaces the
  global provider with the same name as one complete enum value.
- Keep unknown fields rejected by the tagged enum.

### Resolution and runtime dispatch

- Add `ResolvedProvider::Anthropic` in `crates/outrig-cli/src/llm.rs`, resolving the key from its
  environment reference and retaining base URL and timeout.
- Add a distinct `RigAgent::Anthropic` variant. Rig's completion trait has associated concrete
  response and client types, so the Anthropic model cannot be stored in the existing OpenAI
  variant.
- In `build_agent`, construct the shared `reqwest::Client` with the resolved per-request timeout,
  then build `rig::providers::anthropic::Client` with its API key, base URL, and HTTP client.
- Obtain the native Anthropic completion model for `model_identifier` through
  `CompletionClient::completion_model`, exactly as the OpenAI arm does. **Not**
  `CompletionModel::with_model`: the two constructors differ in their missing-`max_tokens`
  behavior, and only the former produces the error contract described below. See *Token limit
  behavior*.
- Wrap the model in `retry::RetryingModel` and pass it through the existing `finish_agent` path.
- Route primary and captured/subagent turns through the same non-streaming `run_turn_inner` path as
  the current OpenAI provider.
- Reuse all existing behavior around preambles, temperature, `max-tokens`, MCP tool registration,
  tool-result truncation, tool-call limits, conversation history, rebuilding after sidecar changes,
  and subagents.
- Audit provider matches so the new variant is handled explicitly. The non-test sites are
  `crates/outrig-cli/src/llm.rs` (resolution and `build_agent`) and the provider-style label in
  `crates/outrig-cli/src/cli/run.rs`, which is user-visible and needs an `"anthropic"` arm. The
  `ResolvedProvider` use in `crates/outrig-cli/src/subagent/mod.rs` is a test fixture; it and the
  fixtures in `crates/outrig-cli/tests/llm_resolve.rs` still need updating, but they are tests.

The Anthropic client, rather than OutRig, owns the protocol details: requests go to
`/v1/messages`, keys use `x-api-key`, the API version header is present, and native content blocks
are converted through Rig's generic message/tool interfaces.

### Token limit behavior

Anthropic requires `max_tokens`, and Rig 0.40 offers **two constructors that disagree about what
happens when it cannot supply one**. Getting this wrong is silent, so the choice is a design
commitment rather than an implementation detail:

- `CompletionClient::completion_model` -> `CompletionModel::make` -> `new()` sets
  `default_max_tokens` from `default_max_tokens_for_model`, which returns `None` for an
  unrecognized identifier. A request that also has no `agents.<name>.max-tokens` then fails with
  `` `max_tokens` must be set for Anthropic ``. **This is the path to use.**
- `CompletionModel::with_model` falls back to `default_max_tokens_with_fallback`, which silently
  yields **2048**. A model on that path quietly truncates long replies with no error anywhere, and
  the acceptance test below would fail for a reason that looks nothing like its cause.

The recognized-identifier set is also narrower than "current Claude models" suggests.
`default_max_tokens_for_model` matches only `claude-opus-4-8`, `claude-opus-4-7`, and
`claude-opus-4-6` (128k), plus the `claude-opus-4`, `claude-sonnet-4`, and `claude-haiku-4-5`
prefixes (64k). Every other identifier -- newer families, and anything 3.x -- returns `None`. The
`claude-sonnet-4-6` used in the config example above happens to be recognized, which makes the
unknown-identifier path look like an edge case. It is not: it is what a user picking a
current-generation model hits on their first run.

That changes what "keep Rig's defaults" costs. Two things follow:

- Keep Rig's recognized-model defaults and surface its explicit error otherwise, pointing users at
  `agents.<name>.max-tokens`. Do not invent a second OutRig-wide token default -- a wrong global
  ceiling is the same silent-truncation failure as the 2048 fallback, just further from the
  provider.
- Have `outrig config init` prompt for `max-tokens` alongside an Anthropic model identifier, so a
  generated config works for an unrecognized identifier instead of failing on the first turn.

Pin the constructor's behavior with a unit or mock integration test covering both a recognized and
an unrecognized identifier, so a Rig upgrade -- or a later refactor reaching for `with_model` --
cannot silently change it.

### Retry and timeout behavior

Apply `RetryingModel` to Anthropic just as it is applied to OpenAI. Each model call may retry a
request timeout, connection failure, or HTTP `408`, `425`, `429`, `500`, `502`, `503`, or `504`,
with the existing bounded exponential backoff. Retrying a model call rather than the whole agent
turn preserves the invariant that completed container tool calls are never replayed.

Update OpenAI-specific comments and names in `crates/outrig-cli/src/llm/retry.rs` and timeout
constants where they now describe all remote HTTP providers. No retry-policy change is intended.

### Interactive configuration

Update `crates/outrig-cli/src/config_init.rs` to:

- offer `anthropic` as a provider style;
- describe it as the native Anthropic Messages wire format;
- default its base URL to `https://api.anthropic.com` and key variable to
  `ANTHROPIC_API_KEY`;
- write `LlmProvider::Anthropic`;
- prompt for a remote model `identifier`, as for OpenAI, and for `max-tokens` alongside it, for
  the reason given under *Token limit behavior*.

Keep provider-specific wording: the OpenAI base URL prompt should not claim to describe an
Anthropic endpoint, and vice versa.

### Tests

Add or extend tests for:

- config parse/serialize round trips for `style = "anthropic"`;
- required and unknown provider fields;
- an Anthropic model requiring `identifier` and rejecting every mistralrs-only field;
- provider replacement during global/repo merge;
- resolver key lookup, base URL, timeout, identifier, and unset-key diagnostics;
- construction of the distinct `RigAgent::Anthropic` variant without making a paid API call;
- scripted `outrig config init` output for an Anthropic provider and model;
- existing OpenAI and mistralrs behavior remaining unchanged.

Add a local mock-HTTP integration test that exercises the actual Rig Anthropic adapter. It must
assert at least:

1. the request uses `POST /v1/messages`;
2. `x-api-key` and `anthropic-version` headers are present and no OpenAI Bearer-auth assumption is
   made;
3. the body carries the configured model, system preamble, `max_tokens`, and native tool schemas;
4. a native `tool_use` response executes the corresponding OutRig tool;
5. the continuation sends a native `tool_result` and returns final assistant text;
6. transient status handling is compatible with the shared retry wrapper.

Use a local server and fake key; no standard test may require network access or a paid Anthropic
account. A manually enabled live smoke test may be added but is not acceptance-critical.

### Documentation

- Update `doc/concepts/llm-providers.md` to distinguish direct native Anthropic from Claude through
  an OpenAI-compatible bridge and remove Anthropic from the unwired-provider TODO.
- Add `style = "anthropic"` fields, model rules, timeout behavior, and an example to
  `doc/reference/config.md`.
- Update provider-style help in `doc/usage/config.md` and any prompt/doc synchronization fixtures.
  That page's `?`-help block is *already* out of sync with `STYLES`, independently of this work: it
  lists `openai` and `anthropic` but omits `mistralrs`, which is a real style. Fix that here rather
  than leaving a page that claims prompts and reference stay in sync while they do not.
- Several `doc/` pages are symlinks into `crates/outrig-cli/src/mcp_self/docs/`, including
  `doc/reference/config.md`. Edit the file under `crates/outrig-cli/src/`; there is no second copy
  to keep in step.
- Keep TODO markers for other Rig provider styles that remain unavailable.

## Runtime behavior

For a selected Anthropic model, OutRig resolves the provider's environment-backed key, constructs
a timeout-configured HTTP client, and gives it to Rig's native Anthropic client. The rest of the
agent path remains provider-neutral: OutRig registers session tools with Rig, Rig serializes them as
Anthropic `tools`, Claude returns `tool_use`, OutRig executes the MCP-backed tool, and Rig sends its
result back as `tool_result` in the next Messages request.

Failures retain the existing boundary: config and reference errors fail before session startup;
an unset key fails while resolving the selected agent; HTTP/protocol failures fail the active turn
after any eligible per-model-call retries. API keys must not enter session metadata, logs, or
container environments.

## Acceptance

- `style = "anthropic"` parses, validates, merges, and resolves without affecting existing provider
  styles.
- An agent can complete an ordinary text turn against a local Anthropic Messages mock endpoint.
- The same agent can complete a native tool-use -> OutRig tool execution -> tool-result round trip.
- The mock observes the native path and headers, not OpenAI-compatible request or auth shapes.
- Timeout, transient retry, tool-call cap, tool-result truncation, history, sidecar-added tools, and
  subagent construction use the existing shared behavior.
- Anthropic config initialization emits parseable, validated TOML with sensible official defaults.
- Missing `max_tokens` behavior is explicit and tested for both recognized and unknown model ids.
- Documentation no longer says native Anthropic is unwired.
- `cargo test`, `cargo clippy`, `cargo fmt --check`, the mdBook build, and documentation style/link
  checks pass.

## Design decisions and open questions

1. **Resolved: use Rig's native provider.** Do not emulate Anthropic through the OpenAI client and
   do not add a separate Anthropic SDK.
2. **Resolved: first-class provider variant.** Anthropic gets explicit config, resolved-provider,
   and runtime-agent variants rather than a generic user-selectable wire-format string.
3. **Resolved: non-streaming first.** Match the existing remote-provider UX. Provider-neutral remote
   streaming is separate work, although Rig's Anthropic implementation already supports it.
4. **Resolved: required base URL.** This matches the OpenAI config shape and keeps native-compatible
   proxies expressible. Config init supplies the official endpoint as its default.
5. **Resolved: fixed API version initially.** Use the pinned Rig default; beta headers and prompt
   caching controls are not exposed yet.
6. **Resolved: Rig owns known-model token defaults.** Unknown identifiers require agent
   `max-tokens` rather than receiving a new OutRig default. Because Rig's recognized-prefix list
   is narrow, this makes `max-tokens` a routine part of an Anthropic model definition rather than
   a rescue hatch, which is why config init now prompts for it.
7. **Open: provider-specific capabilities.** Reasoning display, citations, documents, server-side
   web search, structured output, and prompt caching exist in Rig but OutRig has no config/UI for
   most of them. They are not required for the basic provider integration and should be planned
   separately when their user surface is clear.

## Dependencies

- **0094.** Adding an `Anthropic` variant to `LlmProvider` is a breaking change while the enum
  is exhaustive, which it is today. 0094's sweep marks it `#[non_exhaustive]` -- citing this task
  as the reason -- so landing that first makes this variant purely additive.
- **Hard: `rig-core = 0.40.0` Anthropic provider.** The implementation must confirm builder and
  concrete model type signatures against the locked version rather than newer online docs.
- **Soft: current provider enum/resolver design.** This feature extends the pattern established by
  the OpenAI and mistralrs variants.
- **Soft: shared retry wrapper.** Its HTTP error classification must work with Rig's Anthropic error
  mapping.

## See also

- `doc/concepts/llm-providers.md` -- current provider model and the pending-native note.
- `doc/reference/config.md` -- current accepted provider styles and model validation rules.
- `crates/outrig/src/config/mod.rs` -- `LlmProvider` config enum.
- `crates/outrig-cli/src/llm.rs` -- provider resolution and concrete Rig agent dispatch.
- `crates/outrig-cli/src/llm/retry.rs` -- per-model-call retry policy.
