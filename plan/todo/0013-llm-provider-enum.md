# 0013 -- `LlmProvider` tagged-enum refactor

## Goal

Migrate `LlmProvider` in `src/config/mod.rs` from a flat struct keyed on a `style` string
to a `#[serde(tag = "style")]` enum with two variants -- `OpenAi` and `Mistralrs`. The
shape change has to land before any `mistralrs`-specific code can attach to the config; this
task is pure refactor and contains no `mistralrs` runtime behavior.

The Mistralrs variant is **always present in the enum**, regardless of the `mistralrs`
Cargo feature (which doesn't exist yet -- it lands in 0014). The "feature off but config
mentions mistralrs" failure mode is handled at agent-resolve time, not at parse or
validate. See `plan/next/in-process-llm.md` for the rationale.

## Deliverables

- `src/config/mod.rs::LlmProvider`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  #[serde(tag = "style", rename_all = "kebab-case", deny_unknown_fields)]
  pub enum LlmProvider {
      OpenAi {
          base_url: String,
          api_key: ApiKeyRef,
          #[serde(default, skip_serializing_if = "Option::is_none")]
          request_timeout_secs: Option<u64>,
      },
      Mistralrs {
          // Exactly one of model_id / model_path must be set; enforced in validate,
          // not by serde, so the error message can be useful.
          #[serde(default, skip_serializing_if = "Option::is_none")]
          model_id: Option<String>,
          #[serde(default, skip_serializing_if = "Option::is_none")]
          model_path: Option<PathBuf>,
          #[serde(default, skip_serializing_if = "Option::is_none")]
          model_file: Option<String>,
          #[serde(default, skip_serializing_if = "Option::is_none")]
          revision: Option<String>,
          #[serde(default, skip_serializing_if = "Option::is_none")]
          context_length: Option<u32>,
      },
  }
  ```
- New top-level optional `model_cache_root: Option<PathBuf>` field on `Config`, alongside
  `session_root`. Default at use time: `<XDG_CACHE_HOME>/outrig/models/` (resolved by
  callers, not in the schema).
- `src/config/validate.rs` extensions:
  - On every `Mistralrs` variant: exactly one of `model_id` / `model_path` must be set.
    `model_file` and `revision` only meaningful with `model_id` (warning is enough; error
    is also acceptable -- pick one and pin in tests).
  - `model_path`, if set and relative, is resolved against `repo_root` for the existence
    check (mirroring how `dockerfile` / `context` are checked).
  - `model_cache_root`, if set, must be absolute; outrig creates it on first use, not
    at validate time.
  - New `ConfigValidationError` variants: `MistralrsMissingModelSource`,
    `MistralrsBothModelSources`, `MistralrsMissingModelPath`,
    `ModelCacheRootNotAbsolute`.
- `src/config/merge.rs` extension: merge handles the new top-level field by repo-overrides-
  global (same rule as `session_root`).
- All call sites in 0012's resolver (and elsewhere) that match `provider.style == "openai"`
  switch to a `match` on the enum or a small accessor helper. The resolver's existing
  "unsupported style" error path is replaced by the enum exhaustiveness check.
- `tests/config_provider_enum.rs` covering:
  - Existing `style = "openai"` fixtures round-trip identically.
  - `style = "mistralrs"` with `model-id` parses; without `model-id` *or* `model-path`
    fails validate with a clear message.
  - `style = "mistralrs"` with both `model-id` *and* `model-path` fails validate.
  - A typo like `style = "mistral-rs"` produces a useful error -- pin the message in a
    regression test (the spec's "Watch-outs" warns this can be ugly with serde's default
    `unknown variant` text; a helper `Deserialize` impl or a specific test of the message
    text is the contract).
  - `model-cache-root = "relative/path"` fails validate with `ModelCacheRootNotAbsolute`.

## Acceptance

- `cargo test` passes; existing 0005 fixtures still round-trip.
- `style = "mistralrs"` parses on a build without any `mistralrs` feature flag (which
  doesn't exist yet -- this task adds the schema only).
- The unknown-style error message is pinned in a regression test.
- No `> TODO: Incomplete` markers dropped yet -- the docs describe a feature whose
  runtime hasn't landed.

## Dependencies

- 0005-config-merge-validate
- 0012-llm-resolver

## Notes

- `deny_unknown_fields` plus internally-tagged enums has historically been finicky in
  serde. The implementer should verify it works under the project's current serde pin
  before relying on it. Workaround if it doesn't: drop `deny_unknown_fields` on the outer
  enum, keep it on each variant.
- Resist the urge to enforce "exactly one of model-id/model-path" at the serde layer
  (e.g. with an untagged sub-enum). The validate-phase check produces a much better error
  message than serde's default.
- The mistralrs runtime doesn't exist yet (0014-0016 add it), so `resolve_agent` against
  a `Mistralrs` provider should fail with a clear `feature 'mistralrs' is not enabled in
  this build` -- but that error path lands in 0015, not here. For now, the resolver can
  panic or return a placeholder error on `Mistralrs`; tests in this task only exercise
  parse + validate, not resolve.

### Rejected schema shapes

- **Flat-struct `LlmProvider` with optional fields.** Add `model_path: Option<PathBuf>`,
  `model_id: Option<String>`, etc. directly to the existing flat struct, gated behind
  the `style` field. Rejected: pushes "is this combination valid?" from serde into
  hand-rolled validation, weakens error messages on schema drift, and rots as more
  styles land. The tagged enum makes the variant boundary explicit.
- **`device` field reserved for later.** Considered adding `device: Option<String>` now
  with a `"cpu"` default, so when GPU support arrives in a later batch the schema
  doesn't break. Rejected for v0: no consumers, bloats the surface, and the GPU work
  will likely need more than a single string anyway. Revisit if/when GPU support shows
  up; if it does, this batch's `mistralrs` schema is the place to add the field.
