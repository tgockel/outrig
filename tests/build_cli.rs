//! End-to-end tests for `outrig::cli::build::execute`. Gated behind
//! `--features e2e` because each test shells out to a real `buildah` and
//! pulls the alpine base image on first run.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e build_cli -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::Path;
use std::time::Instant;

use outrig::cli::build::{self, BuildArgs};
use outrig::config::Config;
use outrig::image;
use outrig::process::{self, Cmd};

const ALPINE_DOCKERFILE: &str = "FROM docker.io/library/alpine:latest\n";

fn install_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn write_repo(repo: &Path, container_blocks: &[(&str, &str)], default_container: Option<&str>) {
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();

    let mut cfg = String::new();
    if let Some(name) = default_container {
        cfg.push_str(&format!("default-container = \"{name}\"\n\n"));
    }
    for (name, dockerfile_body) in container_blocks {
        let dir = repo.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), dockerfile_body).unwrap();
        cfg.push_str(&format!(
            "[containers.{name}]\ndockerfile = \"{name}/Dockerfile\"\ncontext = \"{name}\"\n\n"
        ));
    }
    std::fs::write(agents.join("config.toml"), cfg).unwrap();
}

async fn buildah_image_id(tag: &str) -> String {
    let out = process::try_capture(Cmd::new("buildah").args(["images", "--quiet"]).arg(tag))
        .await
        .expect("buildah images");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn tag_for(repo: &Path, container: &str) -> String {
    let cfg_text = std::fs::read_to_string(repo.join(".agents/outrig/config.toml")).unwrap();
    let cfg = Config::load_from_str(&cfg_text).expect("config parses");
    let cc = cfg.containers.get(container).expect("container exists");
    image::compute_tag(cc, repo).await.unwrap().0
}

#[tokio::test]
async fn build_default_container_then_cache_hits() {
    install_tracing();

    let tmp = tempfile::tempdir().unwrap();
    write_repo(tmp.path(), &[("coding", ALPINE_DOCKERFILE)], Some("coding"));

    let repo_cfg = tmp.path().join(".agents/outrig/config.toml");
    let global_cfg = tmp.path().join("nonexistent-global.toml");

    let args = BuildArgs {
        container: None,
        all: false,
        no_cache: false,
    };

    let exit = build::execute(&repo_cfg, &global_cfg, &args)
        .await
        .expect("first build must succeed");
    assert_eq!(exit, 0);

    let tag = tag_for(tmp.path(), "coding").await;
    assert!(
        !buildah_image_id(&tag).await.is_empty(),
        "image {tag} should exist after build"
    );

    let started = Instant::now();
    let exit2 = build::execute(&repo_cfg, &global_cfg, &args)
        .await
        .expect("second call must succeed");
    assert_eq!(exit2, 0);
    assert!(
        started.elapsed().as_millis() < 500,
        "cache hit should be near-instant, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn build_all_iterates_every_container() {
    install_tracing();

    let tmp = tempfile::tempdir().unwrap();
    // Two distinct Dockerfiles so the cache keys can't collide.
    let dockerfile_a = "FROM docker.io/library/alpine:latest\nRUN true\n";
    let dockerfile_b = "FROM docker.io/library/alpine:latest\nRUN echo build_all_iterates\n";
    write_repo(
        tmp.path(),
        &[("coding", dockerfile_a), ("planning", dockerfile_b)],
        None,
    );

    let repo_cfg = tmp.path().join(".agents/outrig/config.toml");
    let global_cfg = tmp.path().join("nonexistent-global.toml");

    let args = BuildArgs {
        container: None,
        all: true,
        no_cache: false,
    };

    let exit = build::execute(&repo_cfg, &global_cfg, &args)
        .await
        .expect("--all must succeed");
    assert_eq!(exit, 0);

    for name in ["coding", "planning"] {
        let tag = tag_for(tmp.path(), name).await;
        assert!(
            !buildah_image_id(&tag).await.is_empty(),
            "image {tag} for {name} should exist after --all"
        );
    }
}

#[tokio::test]
async fn build_all_short_circuits_on_first_failure() {
    install_tracing();

    let tmp = tempfile::tempdir().unwrap();
    // BTreeMap iteration is alphabetical, so "aaa" runs before "zzz".
    // The empty Dockerfile fails buildah's parser fast (no FROM), so we
    // don't pay a base-image pull just to verify short-circuit semantics.
    let broken = "";
    let valid = "FROM docker.io/library/alpine:latest\nRUN echo short_circuit\n";
    write_repo(tmp.path(), &[("aaa", broken), ("zzz", valid)], None);

    let repo_cfg = tmp.path().join(".agents/outrig/config.toml");
    let global_cfg = tmp.path().join("nonexistent-global.toml");

    let args = BuildArgs {
        container: None,
        all: true,
        no_cache: false,
    };
    let result = build::execute(&repo_cfg, &global_cfg, &args).await;
    assert!(
        result.is_err(),
        "--all must propagate the first failure; got {result:?}"
    );

    let zzz_tag = tag_for(tmp.path(), "zzz").await;
    assert!(
        buildah_image_id(&zzz_tag).await.is_empty(),
        "short-circuit failed: {zzz_tag} was built despite earlier failure"
    );
}

#[tokio::test]
async fn no_cache_rebuilds_after_cache_hit() {
    install_tracing();

    let tmp = tempfile::tempdir().unwrap();
    // Unique RUN line so this fixture's tag is distinct from other tests'
    // -- otherwise a parallel test run could see "ours" pre-warmed.
    let dockerfile =
        "FROM docker.io/library/alpine:latest\nRUN echo no_cache_rebuilds_after_cache_hit\n";
    write_repo(tmp.path(), &[("coding", dockerfile)], Some("coding"));

    let repo_cfg = tmp.path().join(".agents/outrig/config.toml");
    let global_cfg = tmp.path().join("nonexistent-global.toml");

    let args = BuildArgs {
        container: None,
        all: false,
        no_cache: false,
    };
    build::execute(&repo_cfg, &global_cfg, &args)
        .await
        .expect("warm-up build must succeed");
    let tag = tag_for(tmp.path(), "coding").await;
    let id_before = buildah_image_id(&tag).await;
    assert!(!id_before.is_empty(), "image must exist after warm-up");

    let no_cache_args = BuildArgs {
        container: None,
        all: false,
        no_cache: true,
    };
    build::execute(&repo_cfg, &global_cfg, &no_cache_args)
        .await
        .expect("--no-cache rebuild must succeed");
    let id_after = buildah_image_id(&tag).await;
    assert!(
        !id_after.is_empty(),
        "image must still exist after --no-cache"
    );
    assert_ne!(
        id_before, id_after,
        "--no-cache should produce a fresh image id"
    );
}
