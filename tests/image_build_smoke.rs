//! End-to-end smoke for `outrig::image::ensure_image`. Gated behind
//! `--features e2e` because it shells out to a real `buildah` and pulls a
//! real base image (`alpine`).
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e image_build_smoke -- --nocapture
//! ```
//!
//! The `--nocapture` flag is what lets you see the `[buildah]` lines on
//! stdout. The test leaves the cached image in buildah storage on purpose --
//! re-runs exercise the cache-hit path. Manual cleanup:
//!
//! ```sh
//! buildah rmi outrig-cache:<key>
//! ```

#![cfg(feature = "e2e")]

use std::collections::BTreeMap;
use std::time::Instant;

use outrig::config::ContainerConfig;
use outrig::image;

#[tokio::test]
async fn build_then_cache_hit_under_100ms() {
    // Install a tracing subscriber so the `[buildah]` stderr lines emitted by
    // `process::run_streamed` are visible under `--nocapture`. Best-effort:
    // ignore the error if a subscriber is already set (e.g. a future test
    // sets a global one).
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = dir.path();
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM docker.io/library/alpine:latest\nRUN apk add --no-cache shadow\n",
    )
    .expect("write Dockerfile");

    let cfg = ContainerConfig {
        dockerfile: "Dockerfile".into(),
        context: ".".into(),
        build_args: BTreeMap::new(),
        mcp: BTreeMap::new(),
    };

    let first = image::ensure_image(&cfg, ctx, false)
        .await
        .expect("first build must succeed");
    assert!(
        first.tag.0.starts_with("outrig-cache:"),
        "tag should be outrig-cache:<key>, got {}",
        first.tag.0
    );
    assert!(!first.cache_hit, "first call should miss the cache");

    let start = Instant::now();
    let second = image::ensure_image(&cfg, ctx, false)
        .await
        .expect("second call must succeed");
    let elapsed = start.elapsed();
    assert_eq!(first.tag, second.tag);
    assert!(second.cache_hit, "second call should hit the cache");
    assert!(
        elapsed.as_millis() < 100,
        "cache hit should be near-instant, took {elapsed:?}"
    );
}
