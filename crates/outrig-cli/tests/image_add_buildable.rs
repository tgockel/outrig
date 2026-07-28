//! End-to-end test that `outrig image add` produces a Dockerfile that
//! actually builds. Gated behind `--features e2e` because it shells out to
//! a real `buildah` and pulls a real base image.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e container_add_buildable -- --nocapture
//! ```
//!
//! Uses the smallest viable combination -- alpine base, no toolchains, fs
//! MCP server -- so the e2e cost stays low.

#![cfg(feature = "e2e")]

mod common;

use std::time::Duration;

use tokio::time::timeout;

use outrig::config::Config;
use outrig::image;
use outrig_cli::image_setup::add::run_with;

use common::scripted_prompt;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test]
async fn generated_alpine_image_builds() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_dir = tmp.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();

    // Script: base = alpine:latest, toolchains = [], MCPs = [fs] (default).
    let (mut prompt, _stderr) = scripted_prompt(b"alpine:latest\n\n\n").await;

    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with must succeed");

    let cfg_text = std::fs::read_to_string(tmp.path().join(".agents/outrig/config.toml")).unwrap();
    let cfg = Config::load_from_str(&cfg_text).expect("config must parse");
    let image = cfg.images.get("coding").expect("coding image present");

    let outcome = image::ensure_image(image, tmp.path(), false)
        .await
        .expect("ensure_image must succeed against generated config");
    assert!(
        !outcome.tag.as_str().is_empty(),
        "image tag must not be empty"
    );
}
