//! The container facade and `harness` in one process, from one image: what an application running
//! an engine over each relies on, through the public surface alone.
//!
//! That a facade's container and a session stop only their own is tested beside the agent loop's
//! other session tests (`src/agent/agent_tests.rs`), since a round needs the crate's mock model.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::process::Command;

use outrig::config::{Config, Workspace};
use outrig::container::Container;
use outrig::harness::{self, DEFAULT_DRAIN, SessionBuilder, Verdict};
use outrig::image;
use outrig::{LaunchSpec, Outrig};

/// What `podman <object> inspect --format <format> <name>` prints.
fn inspect(object: &str, format: &str, name: &str) -> String {
    let output = common::run_capture(
        Command::new("podman").args([object, "inspect", "--format", format, name]),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The ID of the image `tag` names.
fn image_id(tag: &str) -> String {
    inspect("image", "{{.Id}}", tag)
}

/// The ID of the image the container `name` runs.
fn container_image_id(name: &str) -> String {
    inspect("container", "{{.Image}}", name)
}

/// An image ensured once serves the facade and a session both: each runs from the tag
/// `ensure_image` returned, and nothing builds it again or under another name.
#[tokio::test]
async fn one_ensured_image_serves_the_facade_and_a_session() {
    common::init_tracing();
    common::pull_alpine();

    let context = tempfile::tempdir().expect("a build context");
    let workspace = tempfile::tempdir().expect("a workspace");
    // Outside the context, which the image's key hashes.
    let logs = tempfile::tempdir().expect("a log dir");
    let marker = context
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a UTF-8 tempdir name");
    std::fs::write(
        context.path().join("Dockerfile"),
        format!("FROM {}\n# cache-bust: {marker}\n", common::ALPINE),
    )
    .expect("write the Dockerfile");
    let mut config = Config::default();
    config.workspace = Workspace::new(workspace.path(), "/workspace");
    config
        .images
        .insert("shared".to_string(), common::fixture_build_config());
    let shared = &config.images["shared"];

    let ensured = image::ensure_image(shared, context.path(), false)
        .await
        .expect("ensure the image");
    assert!(!ensured.cache_hit, "the marker makes the image new");
    let tag = ensured.tag;
    assert_eq!(
        image::compute_tag(shared, context.path())
            .await
            .expect("compute the tag"),
        tag
    );
    // What `LaunchSpec::from_config` would build the block under, unpinned.
    let under_its_name = image::compute_tag_for("shared", shared, context.path())
        .await
        .expect("compute the block's own tag");
    let built = image_id(tag.as_str());

    let facade_id = format!("facade-beside-harness-{}", std::process::id());
    let facade = Outrig::launch(
        &LaunchSpec::from_image(tag.as_str(), BTreeMap::new(), logs.path().join("facade"))
            .with_session_id(&facade_id),
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    let facade_container = format!("outrig-{facade_id}");

    let pinned = harness::container_spec(
        &config,
        "shared",
        Some(&tag),
        context.path(),
        logs.path().join("session"),
    )
    .await
    .expect("the session's container spec");
    let session = SessionBuilder::new(config, None, None)
        .no_primary()
        .container(pinned.spec)
        .start()
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert!(
        !image::probe_cached(&under_its_name)
            .await
            .expect("probe the cache"),
        "nothing built the block under its own name"
    );
    assert_eq!(image_id(tag.as_str()), built, "nothing built the tag again");
    assert_eq!(container_image_id(&facade_container), built);
    assert_eq!(container_image_id(session.container_name()), built);
    let facade_image = Container::inspect_existing(&facade_container, None)
        .await
        .expect("inspect the facade's container")
        .image_tag;
    let session_image = Container::inspect_existing(session.container_name(), None)
        .await
        .expect("inspect the session's container")
        .image_tag;
    assert_eq!(facade_image, session_image);

    let report = session.shutdown(DEFAULT_DRAIN).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    facade.shutdown().await.expect("the facade stops");
    common::run_capture(Command::new("buildah").args(["rmi", tag.as_str()]));
}
