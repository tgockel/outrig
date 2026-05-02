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
