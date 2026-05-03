//! Lazy, shared loading for in-process LLM models.
//!
//! Two agents that point at the same `[providers.<name>]` block should share
//! one loaded model. `LlmRegistry` is keyed by provider name; the value is a
//! lazily-initialized `Arc` produced by a caller-supplied loader closure.
//! Concurrent first-load callers wait on the same init; subsequent callers
//! get the cached `Arc` without further work.
//!
//! Failure semantics: if the loader returns `Err`, the slot stays empty so
//! the next caller retries (good for transient HF-download failures). If the
//! awaiting future is dropped mid-init, the slot also stays empty -- losing
//! racers wait on a `Notify`, not on the original future, so dropping one
//! does not strand the others.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::OnceCell;

use crate::error::Result;
use crate::llm::mistralrs::MistralrsModel;

/// Generic `T` defaults to [`MistralrsModel`]; tests override with a
/// lightweight stub via `LlmRegistry::<TestStub>::new()` to avoid having
/// to construct a real engine.
pub struct LlmRegistry<T = MistralrsModel> {
    models: Mutex<BTreeMap<String, Arc<OnceCell<Arc<T>>>>>,
}

impl<T> Default for LlmRegistry<T> {
    fn default() -> Self {
        Self {
            models: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<T: Send + Sync + 'static> LlmRegistry<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_init<F, Fut>(&self, provider_name: &str, init: F) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let cell = {
            let mut map = self.models.lock().expect("registry mutex poisoned");
            // Skip the `to_string` on cache hits so the steady-state lookup
            // is allocation-free.
            if let Some(existing) = map.get(provider_name) {
                existing.clone()
            } else {
                map.entry(provider_name.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone()
            }
        };

        let arc = cell
            .get_or_try_init(|| async { init().await.map(Arc::new) })
            .await?;
        Ok(arc.clone())
    }
}
