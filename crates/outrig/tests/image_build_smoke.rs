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
        first.tag.as_str().starts_with("outrig-cache:"),
        "tag should be outrig-cache:<key>, got {}",
        first.tag.as_str()
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

/// A `${VAR}` build-arg goes to buildah as a bare `--build-arg KEY`, the value
/// in buildah's own environment rather than on its command line. buildah's
/// man page does not document that form; this is what pins it.
#[tokio::test]
async fn a_referenced_build_arg_reaches_buildah_by_name() {
    common::init_tracing();

    let var = "OUTRIG_E2E_IMAGE_BUILD_REFERENCED";
    // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
    // races; no other test reads or writes this name.
    unsafe { std::env::set_var(var, "by-name-324") };

    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = dir.path();
    let marker = ctx
        .file_name()
        .and_then(|name| name.to_str())
        .expect("tempdir path should have a UTF-8 filename");
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM docker.io/library/alpine:latest\n# cache-bust: {marker}\nARG PROBE\n\
             RUN test \"$PROBE\" = by-name-324\n",
        ),
    )
    .expect("write Dockerfile");

    let mut cfg = common::fixture_build_config();
    cfg.build_args = std::collections::BTreeMap::from([(
        "PROBE".to_string(),
        outrig::config::EnvValue::EnvRef(var.to_string()),
    )]);

    let built = image::ensure_image(&cfg, ctx, false)
        .await
        .expect("the RUN sees the referenced value");

    let _ = std::process::Command::new("buildah")
        .args(["rmi", built.tag.as_str()])
        .output();
}
