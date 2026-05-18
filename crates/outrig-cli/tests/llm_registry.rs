#![cfg(feature = "local-llm")]
//! Sharing semantics of [`outrig_cli::llm::LlmRegistry`].
//!
//! Real `MistralrsModel`s wrap a heavy `MistralRs` engine, so these tests
//! drive the registry with a tiny local stub via the type-defaulted generic
//! (`LlmRegistry::<TestStub>::new()`). That keeps the test cheap while still
//! exercising the `Arc`-sharing, distinct-key, and one-load-under-contention
//! invariants the registry promises.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use outrig_cli::error::Result;
use outrig_cli::llm::{LlmRegistry, LlmResolveError};
use tokio::sync::Notify;
use tokio::time::timeout;

#[derive(Debug)]
struct TestStub {
    id: String,
}

#[tokio::test]
async fn same_provider_shares_arc() {
    let registry: LlmRegistry<TestStub> = LlmRegistry::new();
    let counter = Arc::new(AtomicUsize::new(0));

    let first = registry
        .get_or_init("alpha", || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(TestStub { id: "alpha".into() })
            }
        })
        .await
        .expect("first init");

    let second = registry
        .get_or_init("alpha", || async {
            panic!("loader must not be called for cached slot");
        })
        .await
        .expect("second init");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_providers_separate_arcs() {
    let registry: LlmRegistry<TestStub> = LlmRegistry::new();

    let a = registry
        .get_or_init("alpha", || async {
            Ok(TestStub {
                id: "shared".into(),
            })
        })
        .await
        .expect("alpha init");
    let b = registry
        .get_or_init("beta", || async {
            Ok(TestStub {
                id: "shared".into(),
            })
        })
        .await
        .expect("beta init");

    assert!(!Arc::ptr_eq(&a, &b));
    assert_eq!(a.id, b.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_load_runs_once() {
    const RACERS: usize = 16;

    let registry: Arc<LlmRegistry<TestStub>> = Arc::new(LlmRegistry::new());
    let counter = Arc::new(AtomicUsize::new(0));
    // The loader future blocks on this notify until every racer has had a
    // chance to call `get_or_init`. Without it the winner could finish before
    // the other tasks even arrive, defeating the contention check.
    let release = Arc::new(Notify::new());

    let mut handles = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let registry = registry.clone();
        let counter = counter.clone();
        let release = release.clone();
        handles.push(tokio::spawn(async move {
            registry
                .get_or_init("contended", move || {
                    let counter = counter.clone();
                    let release = release.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(TestStub {
                            id: "contended".into(),
                        })
                    }
                })
                .await
        }));
    }

    // Yield until at least one racer has entered the loader; then release.
    while counter.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    release.notify_waiters();

    let arcs: Vec<Arc<TestStub>> = timeout(Duration::from_secs(5), async {
        let mut out = Vec::with_capacity(RACERS);
        for h in handles {
            out.push(h.await.expect("racer joined").expect("racer ok"));
        }
        out
    })
    .await
    .expect("all racers complete within timeout");

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "loader ran more than once under contention",
    );
    let head = arcs.first().expect("at least one racer");
    for other in arcs.iter().skip(1) {
        assert!(
            Arc::ptr_eq(head, other),
            "concurrent racers received distinct Arcs",
        );
    }
}

#[tokio::test]
async fn loader_failure_leaves_slot_empty() {
    let registry: LlmRegistry<TestStub> = LlmRegistry::new();
    let attempts = Arc::new(AtomicUsize::new(0));

    let attempts_clone = attempts.clone();
    let result: Result<Arc<TestStub>> = registry
        .get_or_init("flaky", move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(LlmResolveError::MistralrsLoad {
                    model: "flaky".into(),
                    source: anyhow!("transient load failure"),
                }
                .into())
            }
        })
        .await;
    assert!(result.is_err());

    let stub = registry
        .get_or_init("flaky", || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(TestStub {
                    id: "recovered".into(),
                })
            }
        })
        .await
        .expect("retry succeeds");

    assert_eq!(stub.id, "recovered");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
