# 0016 -- `LlmRegistry` (lazy-load + sharing for in-process models)

## Goal

Two agents that point at the same `[providers.<name>]` block should share one loaded
`mistralrs` model. Without sharing, declaring two agents in a repo (a "coding" agent and
a "review" agent both backed by the same local provider) would double the RAM bill and
double the load time. The `LlmRegistry` is the smallest piece of state that fixes this:
keyed by provider name, value is a lazily-loaded `Arc<MistralrsModel>`.

## Deliverables

- `src/llm/registry.rs`:
  ```rust
  pub struct LlmRegistry {
      models: Mutex<BTreeMap<String, Arc<OnceCell<Arc<MistralrsModel>>>>>,
  }

  impl LlmRegistry {
      pub fn new() -> Self;

      /// Get-or-load the model for a provider. Concurrent callers for the same
      /// provider name wait on a single load.
      pub async fn get_or_load(
          &self,
          provider_name: &str,
          provider: &LlmProvider,
          cache_root: &Path,
      ) -> Result<Arc<MistralrsModel>>;
  }
  ```
  All under `#[cfg(feature = "mistralrs")]`. The non-feature build doesn't have a
  registry -- the resolver short-circuits with the 0014 error before reaching one.
- The registry lives **in the host outrig process**, not the container. Construct it
  once per `outrig run` invocation; pass it into the resolver alongside `Config`.
- `resolve_agent` (0012/0015) takes the registry as a parameter; the `Mistralrs` arm
  calls `registry.get_or_load(...)` instead of `MistralrsModel::load(...)` directly.
  Two `resolve_agent` calls against the same provider now share one `Arc`.
- `tests/llm_registry.rs` (`#[cfg(feature = "mistralrs")]`):
  - Two `resolve_agent` calls against the same provider name return clients backed by
    the same `Arc` (`Arc::ptr_eq` after downcast, or a `Arc::strong_count` check).
  - Two calls against *different* provider names return *different* `Arc`s, even when
    they happen to point at the same model file.
  - Concurrent first-load: spawn N tasks that all call `get_or_load` for the same
    provider; assert the loader runs exactly once (use a counter inside a test-only
    `MistralrsModel::load_for_test` injected via a feature/cfg).

## Acceptance

- `cargo test --features mistralrs llm_registry` passes.
- Two agents in a fixture config that point at the same `mistralrs` provider load the
  model once.
- `resolve_agent` on a non-feature build is unchanged from 0015's behavior (still errors
  on `Mistralrs` with the 0014 message).
- Drop the `> TODO: Incomplete` marker on `doc/concepts/llm-providers.md` for the
  in-process providers section if 0015 left it in place.

## Dependencies

- 0015-mistralrs-shim

## Notes

- `OnceCell<Arc<...>>` (rather than `tokio::sync::OnceCell<...>`): the `Arc` is the
  shared handle; the `OnceCell` ensures one-load. Use `tokio::sync::OnceCell` if its
  async-friendly `get_or_init` is the cleanest fit; verify the import.
- No eviction in v0. One model per provider name, lifetime bounded by `outrig run`.
  If multi-tenant pressure shows up later, add it; don't pre-build for it.
- Drop happens implicitly when the registry drops at the end of `outrig run`. Don't
  hand-roll explicit `unload`.
- The registry is keyed by *provider name*, not by `(model_id, revision, model_file)`.
  Two providers that happen to point at the same underlying file load it twice. That's
  intentional: provider name is the user's identity for the model, and conflating
  same-file-different-providers would surprise the user (separate `context-length`
  settings, for instance, would silently merge).
