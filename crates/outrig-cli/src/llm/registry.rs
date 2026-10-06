//! Lazy, shared loading for in-process LLM models.
//!
//! Two agents that point at the same `[models.<name>]` block share one loaded
//! engine. `LlmRegistry` is keyed by model name; the value is a
//! lazily-initialized `Arc` produced by a caller-supplied loader closure.
//! Concurrent first-load callers wait on the same init; subsequent callers
//! get the cached `Arc` without further work.
//!
//! Failure semantics: if the loader returns `Err`, the slot stays empty so
//! the next caller retries (good for transient HF-download failures).
//!
//! A load belongs to its slot, not to whoever is waiting on it: it runs in a
//! task of its own, so a waiter that is dropped -- a turn a Ctrl-C cut short
//! -- stops waiting without stopping the load or losing what it produces, and
//! the next caller waits on that same load rather than starting a second one
//! beside it. A load runs for minutes and holds gigabytes, and its heavy part
//! keeps going on the blocking pool whatever its waiters do, so a second one
//! would double both.

use std::collections::BTreeMap;
use std::future::Future;
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

    /// The loaded model for `model_name`, loading it through `init` if no
    /// load has succeeded yet. `init` is called only on a miss; its future is
    /// run only if no load is already under way, and runs to the end even if
    /// this call is dropped first.
    pub async fn get_or_init<F, Fut>(&self, model_name: &str, init: F) -> Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let cell = {
            let mut map = self.models.lock().expect("registry mutex poisoned");
            // Skip the `to_string` on cache hits so the steady-state lookup
            // is allocation-free.
            if let Some(existing) = map.get(model_name) {
                existing.clone()
            } else {
                map.entry(model_name.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone()
            }
        };

        if let Some(loaded) = cell.get() {
            return Ok(loaded.clone());
        }
        let load = init();
        let task = tokio::spawn(async move {
            cell.get_or_try_init(|| async move { load.await.map(Arc::new) })
                .await
                .cloned()
        });
        match task.await {
            Ok(loaded) => loaded,
            // A loader that panicked panics here, in its caller, as it did when
            // the caller ran it. Cancellation means the runtime is shutting
            // down, which drops this call before it could see the error.
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    /// Whether this model is actually *loaded* -- the slot exists and its cell
    /// holds a model. Takes the same lock [`Self::get_or_init`] does and
    /// constructs nothing, so it is safe to ask on a path that must not load.
    ///
    /// Slot existence is deliberately not the test. A load that failed leaves
    /// its `OnceCell` behind, empty, and `get_or_try_init` will re-run the
    /// initializer on the next attempt -- so a retry after a failed load is
    /// every bit as cold as the first try. Answering on membership alone
    /// reported it as warm and swallowed the advisory line before exactly the
    /// wait it exists to explain.
    ///
    /// Racing a concurrent load is still acceptable: the caller uses this to
    /// decide whether to *announce* a cold load, and the worst case there is a
    /// spurious or missing advisory line.
    pub(crate) fn is_loaded(&self, model_name: &str) -> bool {
        self.models
            .lock()
            .expect("registry mutex poisoned")
            .get(model_name)
            .is_some_and(|cell| cell.get().is_some())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;

    struct Stub;

    fn boom() -> crate::error::CliError {
        crate::llm::LlmResolveError::RigClientBuild("boom".to_string()).into()
    }

    /// A slot is not a model. The module's failure semantics promise that a
    /// loader returning `Err` leaves the slot empty so the next caller retries
    /// -- so that next caller is facing a cold load, and the advisory line
    /// explaining the wait must not be suppressed by the corpse of the attempt
    /// that failed.
    #[tokio::test]
    async fn a_failed_load_does_not_count_as_loaded() {
        let registry: LlmRegistry<Stub> = LlmRegistry::new();
        assert!(!registry.is_loaded("qwen"), "nothing has been tried yet");

        let failed = registry.get_or_init("qwen", || async { Err(boom()) }).await;
        assert!(failed.is_err(), "the loader failed");
        assert!(
            !registry.is_loaded("qwen"),
            "a failed load leaves an empty slot behind, and an empty slot is \
             still a cold load for whoever comes next",
        );

        // ...and the retry the empty slot exists to allow does load.
        registry
            .get_or_init("qwen", || async { Ok(Stub) })
            .await
            .expect("the retry loads");
        assert!(registry.is_loaded("qwen"), "now it is genuinely warm");
    }

    /// A waiter dropped mid-load -- the turn a Ctrl-C cut short -- does not
    /// take the load with it. The next caller waits on that same load and gets
    /// what it produced, and the loader runs once: a second load beside the
    /// first would hold the weights twice.
    #[tokio::test]
    async fn a_dropped_waiter_leaves_its_load_for_the_next_caller() {
        let registry: LlmRegistry<Stub> = LlmRegistry::new();
        let loads = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let loader = || {
            let loads = loads.clone();
            let release = release.clone();
            async move {
                loads.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(Stub)
            }
        };

        let gave_up = tokio::time::timeout(
            Duration::from_millis(50),
            registry.get_or_init("qwen", loader),
        )
        .await;
        assert!(gave_up.is_err(), "the first waiter is dropped mid-load");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "its load is under way");

        let releaser = release.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            releaser.notify_one();
        });
        registry
            .get_or_init("qwen", loader)
            .await
            .expect("the next caller gets the load already under way");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the loader ran once");
        assert!(registry.is_loaded("qwen"));
    }
}
