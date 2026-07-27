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

mod common;

use std::time::Instant;

use outrig::image;

#[tokio::test]
async fn build_then_cache_hit_under_100ms() {
    common::init_tracing();

    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = dir.path();
    let marker = ctx
        .file_name()
        .and_then(|name| name.to_str())
        .expect("tempdir path should have a UTF-8 filename");
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM docker.io/library/alpine:latest\n# cache-bust: {marker}\nRUN apk add --no-cache shadow\n",
        ),
    )
    .expect("write Dockerfile");

    let cfg = common::fixture_build_config();

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
