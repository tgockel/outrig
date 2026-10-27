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

use std::time::{Duration, Instant};

use outrig::error::OutrigError;
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

/// buildah reads the context as each `COPY` runs, not when the build starts, so
/// a file saved during an earlier `RUN` is in the image although the key was
/// hashed without it (#392). The build is refused rather than tagged under that
/// key, which would serve the edited image for the original content.
#[tokio::test]
async fn a_context_edited_mid_build_is_not_tagged_under_the_old_key() {
    common::init_tracing();

    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = dir.path();
    let marker = ctx
        .file_name()
        .and_then(|name| name.to_str())
        .expect("tempdir path should have a UTF-8 filename");
    std::fs::write(ctx.join("payload"), "one\n").expect("write payload");
    std::fs::write(
        ctx.join("Dockerfile"),
        format!(
            "FROM docker.io/library/alpine:latest\n# cache-bust: {marker}\n\
             RUN sleep 5\nCOPY payload /payload\n",
        ),
    )
    .expect("write Dockerfile");

    let cfg = common::fixture_build_config();
    let original = image::compute_tag(&cfg, ctx).await.expect("compute_tag");

    // The edit lands inside the `RUN`: after the key was hashed, before the `COPY`.
    let edit = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        std::fs::write(ctx.join("payload"), "two\n").expect("edit payload");
    };
    let (built, ()) = tokio::join!(image::ensure_image(&cfg, ctx, false), edit);

    let err = built.expect_err("a build that read an edit must not be saved as the old key");
    assert!(
        matches!(&err, OutrigError::BuildContextChanged { tag, .. } if tag == original.as_str()),
        "expected BuildContextChanged for {original}, got {err}"
    );
    assert!(
        !image::probe_cached(&original)
            .await
            .expect("probe the original key"),
        "{original} must not exist after a refused build"
    );

    let rebuilt = image::ensure_image(&cfg, ctx, false)
        .await
        .expect("the rerun builds the edited context");
    assert_ne!(rebuilt.tag, original, "the edit moves the key");

    let _ = std::process::Command::new("buildah")
        .args(["rmi", rebuilt.tag.as_str()])
        .output();
}
