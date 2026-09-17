# 0015 -- Rig `CompletionModel` shim for `mistralrs` (with HF download)

## Goal

Wire the in-process LLM end-to-end: Rig's `CompletionModel` trait gets an implementation
that hands off to `mistralrs`, including the HuggingFace download path so a fresh outrig
install can resolve a `model-id` config without any user pre-staging. After this task,
`resolve_agent` against a `mistralrs` provider returns something the agent loop can call.

## Deliverables

- `src/llm/mistralrs.rs` (under `#[cfg(feature = "mistralrs")]`):
  - `pub struct MistralrsModel { /* loaded mistralrs handle */ }`
  - `pub async fn load(provider: &LlmProvider, cache_root: &Path) -> Result<MistralrsModel>`:
    1. Pattern-match the provider variant into a `(model_id | model_path, model_file,
       revision, context_length)` tuple.
    2. If `model_path` is set: open it directly. Done.
    3. If `model_id` is set: resolve to a local file under `<cache_root>/<model_id>/<rev>/
       <model_file>`. If absent, download from HuggingFace and write to that path
       atomically (download to a temp file in the same dir, then rename). Verify the
       file's checksum if the HF API returns one.
    4. Build the `mistralrs` model handle with `context_length` if set, else the
       model's default.
    5. Return the loaded handle.
  - Implements the Rig completion-model trait (verify exact name in the locked rig-core
    version -- likely `rig::completion::CompletionModel`). Translation surface:

    | Rig                                | mistralrs                              |
    |------------------------------------|----------------------------------------|
    | `CompletionRequest::preamble`      | system message at the head of the chat |
    | `CompletionRequest::chat_history`  | sequence of user/assistant messages    |
    | `CompletionRequest::tools`         | tool definitions (JSON schema)         |
    | `CompletionRequest::temperature`   | sampler temperature                    |
    | `CompletionRequest::max_tokens`    | max output tokens                      |
    | `Message::ToolCall { ... }`        | mistralrs tool-call message            |
    | `Message::ToolResult { ... }`      | mistralrs tool-response message        |

  - Tool-call passthrough fidelity is bounded by the underlying GGUF model. Models that
    emit valid tool-call JSON when prompted work end-to-end; models that don't produce
    free-form text only.
- `src/llm.rs::resolve_agent` is extended to dispatch on the provider variant. The
  `Mistralrs` arm calls `MistralrsModel::load` and wraps the returned handle in whatever
  shape `build_agent` expects.
- Without `--features mistralrs`, the resolver's `Mistralrs` arm returns the error pinned
  in 0014 (`feature 'mistralrs' is not enabled in this build...`).
- `tests/mistralrs_smoke.rs` (`#[cfg(feature = "mistralrs")]`):
  - **Offline path test**, gated behind `OUTRIG_MISTRALRS_TEST_MODEL=/path/to/file.gguf`.
    Skips with a `println!` notice if the env var isn't set. Loads the model via
    `load(...)`, runs a one-shot prompt, asserts a non-empty completion.
  - **Download path test**, gated behind `OUTRIG_MISTRALRS_TEST_MODEL_ID=<org/repo>` plus
    `OUTRIG_MISTRALRS_TEST_MODEL_FILE=<file.gguf>`. Same shape; verifies the downloaded
    file lands in the cache dir and a second `load(...)` doesn't re-download (mtime
    unchanged).

## Acceptance

- `cargo build --features mistralrs` succeeds with the shim attached to the resolver.
- `cargo test --features mistralrs` passes the unit suite. The smoke tests skip cleanly
  when env vars are unset, and pass when set against a small public GGUF repo.
- `resolve_agent` against a `Mistralrs` provider on a non-feature build still produces
  the 0014 error message verbatim.
- Drop the `> TODO: Incomplete` marker on `doc/concepts/llm-providers.md` for the
  in-process providers section -- only that section, not the whole page (the page
  itself is still TODO until 0017).

## Dependencies

- 0014-mistralrs-feature

## Notes

- HF download mechanism: prefer mistralrs's built-in downloader if it ships one (verify
  at task time). Otherwise, `hf-hub` is the standard Rust crate for this; pin behind the
  same feature flag.
- Cache layout: `<cache_root>/<model_id>/<revision>/<model_file>`. Concurrent downloads
  for the same model file should not corrupt the cache -- use a `<file>.part` plus
  atomic rename pattern.
- The download blocks the calling task until complete. v0 ships without a progress UI;
  the docs already warn about this. Logging a "downloading <repo>:<file>..." line on
  stderr at the start, and "downloaded <bytes> in <duration>" at the end, is enough --
  no incremental progress.
- Don't try to be clever about partial-feature subsets of the request shape. If
  mistralrs lacks a parameter Rig surfaces (e.g. some sampling knob), pass through
  what's expressible and document the gap.
- `context_length` validation: if the user-set `context-length` exceeds what the
  model declares, error early (don't silently truncate).
- **Tool-calling fidelity is the model's problem, not ours.** The shim passes through
  whatever tool calls Rig requests; the model emits whatever it emits. v0 makes no
  guarantee that any particular GGUF will tool-call cleanly. A stricter promise
  ("works only with this curated list of models") was considered and rejected as too
  much surface area to maintain. If a user reports that their model emits malformed
  tool-call JSON, the answer is "pick a model that doesn't" -- the policy-oracle path
  (out of scope for this batch) won't depend on free-form tool-call competence anyway,
  since it uses constrained-JSON decoding.

## Decisions

- **HF cache layout follows `hf-hub`'s default**, not the spec's flat
  `<cache_root>/<model_id>/<rev>/<model_file>`. `mistralrs-core 0.8.1` exposes a
  process-global `OnceLock<Cache>` (`GLOBAL_HF_CACHE`) that the shim sets to
  `Cache::new(cache_root)` on first call. Files land at
  `<cache_root>/models--<org>--<repo>/snapshots/<rev_hash>/<file>`. Going around
  mistralrs's downloader to honour the spec layout would re-implement HF auth /
  redirects / etag handling for no real benefit; the spec's wording was
  aspirational, not load-bearing. The `.part`-then-rename atomicity the spec
  asked for is provided by `hf-hub` already.

- **`RigAgent` reshape: enum, not concrete type alias.** The pre-0015
  `pub type RigAgent = rig::agent::Agent<openai::CompletionModel>` couldn't
  carry both an OpenAi-backed and a mistralrs-backed agent (Rig's
  `CompletionModel` trait has associated types `Response` /
  `StreamingResponse` / `Client` that make `dyn CompletionModel` non
  object-safe). `RigAgent` now is a runtime enum with one variant per
  provider style. `MistralrsRuntimeUnavailable` was deleted along with the
  feature-on resolver test that asserted it.

- **`build_rig_client` collapsed into `build_agent`.** The `client -> model
  -> agent` two-step is artificial for in-process providers (the
  loaded engine *is* the "client"), so the public surface is now a single
  async `pub async fn build_agent(resolved, tools, cache_root) ->
  Result<RigAgent>`. The OpenAi arm is sync-in-async (free); the Mistralrs
  arm awaits `mistralrs::load`. A generic `finish_agent<M>` helper shares
  the `AgentBuilder` setup between arms.

- **Direct optional Cargo deps for the three transitives (`hf-hub`,
  `candle-core`, `indexmap`).** mistralrs-core re-exports neither
  `hf_hub::Cache`, `candle_core::Device`, nor `indexmap::IndexMap`, but the
  shim constructs values of all three (cache root, CPU device, message
  IndexMap). Pinning them with `=<exact-version>` matched against
  mistralrs-core's locked picks keeps the dep tree free of duplicates.
  Feature gate: `mistralrs = ["dep:mistralrs-core", "dep:hf-hub",
  "dep:candle-core", "dep:indexmap"]`.

- **`Self::Response` is a JSON wrapper, not the raw mistralrs response.**
  `mistralrs_core::ChatCompletionResponse` impls `Serialize` only, but Rig's
  trait demands `Serialize + DeserializeOwned`. `MistralrsRawResponse {
  raw: serde_json::Value }` satisfies the trait via `Value`'s round-trip.
  v0 has no consumer for `raw`; it pays the cost of one
  `serde_json::to_value` per completion. Acceptable; revisit if the agent
  loop ever inspects raw responses.

- **`Self::StreamingResponse = ()`.** `()` already implements
  `Clone + Unpin + Send + Sync + Serialize + DeserializeOwned +
  GetTokenUsage` (rig provides the last impl). The `stream()` method
  returns `CompletionError::ProviderError("streaming not supported by the
  mistralrs in-process backend")`; v0's agent loop is non-streaming, so
  this never fires.

- **Tool-call argument fallback to raw string.** mistralrs returns
  `arguments: String`, rig wants `arguments: serde_json::Value` (parsed).
  On `serde_json::from_str` failure, the shim wraps the raw string as
  `Value::String(raw)` instead of erroring. Rationale: the agent loop's
  schema-validation produces a clearer downstream error ("tool 'foo' got
  non-JSON arguments") than the shim would. Pinned by
  `malformed_tool_call_arguments_fall_back_to_string` in the unit suite.

- **`send_request` is offloaded via `tokio::task::spawn_blocking`.**
  `MistralRs::send_request` calls `Sender::blocking_send` internally, which
  blocks the calling thread. Calling it directly from an async task would
  stall the runtime; `spawn_blocking` moves the call to the blocking
  thread pool. The response arrives over an `mpsc::Sender<Response>`
  embedded in the request; one channel per call, depth 1 (no concurrency
  per `MistralrsModel::completion`).

- **`context_length` is post-load validation, not pre-load peek.** The
  alternative was to lock the loaded `Pipeline` before passing it to
  `MistralRsBuilder` and read `metadata.max_seq_len` early. Cleaner code
  to validate after `engine.config(None)` returns; the cost is one already-
  loaded model in memory at the moment we error. v0 ships with that
  trade-off.

- **`tool_choice = Required` downgrades to `Auto`.** mistralrs has no
  "Required" variant; the closest analogue is `Specific(tool)`, but that
  needs a chosen tool name. Downgrading to `Auto` and relying on the
  model is the lossy option; documented in the conversion site.

- **Smoke tests gated on env vars, not `--ignored`.** `tests/mistralrs_
  smoke.rs` reads `OUTRIG_MISTRALRS_TEST_MODEL{,_ID,_FILE}` and emits a
  `println!("skip: ...")` when unset. Rationale: `cargo test --features
  mistralrs` stays green in CI without any flag dance, and a single env
  var unlocks the test for local verification. `#[ignore]` would force a
  separate `cargo test -- --ignored` invocation.
