//! The migration a downstream consumer has to perform, proven to compile.
//!
//! `MountConfig::host_path` / `container_path` / `access` and
//! `ImageConfig::dockerfile` / `context` were public fields until they were
//! paired with a recorded `ConfigSource`. This file reads and writes every one
//! of them through the accessors that replaced them, so the replacement surface
//! is exercised from *outside* the crate -- an integration test sees only `pub`
//! items, exactly as a consumer does.
//!
//! Deliberately not `#![cfg(feature = "e2e")]`, unlike `library_surface.rs`:
//! nothing here needs podman, and a gated fixture would not run.

use std::path::{Path, PathBuf};

use outrig::config::{Config, ImageConfig, ImageSourceRef, MountAccess, MountConfig};

/// Every accessor that replaced a field, in both directions, on a hand-built
/// value. `MountConfig::new` is unchanged -- it already produced a sourceless
/// entry -- so the whole migration is field syntax becoming call syntax.
#[test]
fn mount_accessors_read_and_write_every_replaced_field() {
    let mut mount = MountConfig::new("data", "/data", MountAccess::ReadOnly);

    assert_eq!(mount.host_path(), Path::new("data"));
    assert_eq!(mount.container_path(), Path::new("/data"));
    assert_eq!(mount.access(), MountAccess::ReadOnly);

    mount.set_host_path("cache");
    mount.set_container_path("/cache");
    mount.set_access(MountAccess::ReadWrite);

    assert_eq!(mount.host_path(), Path::new("cache"));
    assert_eq!(mount.container_path(), Path::new("/cache"));
    assert_eq!(mount.access(), MountAccess::ReadWrite);
    assert_eq!(
        mount.resolved_host_path(Path::new("/repo")),
        PathBuf::from("/repo/cache"),
        "a hand-built entry has always resolved against the argument",
    );
}

/// The image half. `set_build_paths` is the only writer for the build paths,
/// because one recorded source backs both and replacing one alone would rebase
/// the other.
#[test]
fn image_accessors_read_and_write_both_build_paths() {
    let mut image = ImageConfig::from_dockerfile("Dockerfile", ".");

    assert_eq!(image.dockerfile(), Some(Path::new("Dockerfile")));
    assert_eq!(image.context(), Some(Path::new(".")));

    image.set_build_paths("build/Containerfile", "build");

    assert_eq!(image.dockerfile(), Some(Path::new("build/Containerfile")));
    assert_eq!(image.context(), Some(Path::new("build")));
    assert_eq!(
        image.resolved_build_paths(Path::new("/repo")),
        (
            PathBuf::from("/repo/build/Containerfile"),
            PathBuf::from("/repo/build"),
        ),
    );

    let pulled = ImageConfig::from_image_name("docker.io/library/alpine:3");
    assert_eq!(pulled.dockerfile(), None);
    assert_eq!(pulled.context(), None);
}

/// One image-config per source shape, in the form a library caller meets them:
/// loaded from a file, so each carries the `ConfigSource` the loader recorded.
const PULL_BODY: &str = r#"
[images.probe]
image-name = "docker.io/library/alpine:latest"
"#;

const BUILD_BODY: &str = r#"
[images.probe]
dockerfile = "images/probe/Dockerfile"
context    = "images/probe"
"#;

/// Load `body` from a file rather than a string: only a file load records a
/// `ConfigSource`, and clearing it is part of what the setters are for. The
/// directory is returned because it must outlive the config's use.
fn load_stamped(body: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("outrig.toml");
    std::fs::write(&path, body).expect("write config");
    let cfg = Config::load_file_unvalidated(&path, None).expect("fixture loads");
    assert!(
        cfg.images["probe"].config_source().is_some(),
        "the fixture must start with a recorded source, or clearing it proves nothing",
    );
    (dir, cfg)
}

/// A setter that writes the build shape replaces the pull shape rather than
/// joining it. Before, `image_name` survived, so `source()` panicked and
/// `validate` reported a conflict the caller never wrote.
#[test]
fn set_build_paths_on_a_pull_source_replaces_the_shape() {
    let (_dir, mut cfg) = load_stamped(PULL_BODY);
    cfg.validate(None).expect("a pull-source config validates");

    let image = cfg.images.get_mut("probe").unwrap();
    image.set_build_paths("Dockerfile", ".");

    assert_eq!(image.image_name, None);
    assert_eq!(image.dockerfile(), Some(Path::new("Dockerfile")));
    assert_eq!(image.context(), Some(Path::new(".")));
    assert!(matches!(image.source(), ImageSourceRef::Build { .. }));
    cfg.validate(None).expect("the converted config validates");
}

/// The reverse conversion, which had no setter: the build paths are private,
/// so a caller could not clear them by hand.
#[test]
fn set_image_name_on_a_build_source_replaces_the_shape() {
    let (_dir, mut cfg) = load_stamped(BUILD_BODY);
    cfg.validate(None).expect("a build-source config validates");

    let image = cfg.images.get_mut("probe").unwrap();
    image.set_image_name("docker.io/library/alpine:latest");

    assert_eq!(
        image.image_name.as_deref(),
        Some("docker.io/library/alpine:latest")
    );
    assert_eq!(image.dockerfile(), None);
    assert_eq!(image.context(), None);
    assert!(
        image.config_source().is_none(),
        "the recorded source existed to resolve the paths that are gone",
    );
    assert!(matches!(image.source(), ImageSourceRef::Image { .. }));
    cfg.validate(None).expect("the converted config validates");
}

/// `set_image_name` leaves `build_args` to the caller, as `set_build_paths`
/// does, and validation says so rather than letting them ride along unseen.
#[test]
fn set_image_name_leaves_build_args_for_validation_to_report() {
    let (_dir, mut cfg) = load_stamped(
        r#"
[images.probe]
dockerfile = "images/probe/Dockerfile"
context    = "images/probe"
build-args = { FOO = "bar" }
"#,
    );

    cfg.images
        .get_mut("probe")
        .unwrap()
        .set_image_name("docker.io/library/alpine:latest");

    let err = cfg.validate(None).expect_err("build-args with image-name");
    assert!(
        err.to_string()
            .contains("`build-args` cannot be used with `image-name`"),
        "{err}",
    );
}

/// Exhaustive over the two setters, not a sampled pair: every sequence of up to
/// four calls, from each starting shape, leaves a config that `source()`
/// accepts and `validate` passes -- and that reports the shape the last call
/// wrote. A call that merely *added* its shape would pass the first two
/// checks on some sequences and fail the third on the rest.
#[test]
fn every_sequence_of_the_two_setters_leaves_source_callable() {
    for start in [PULL_BODY, BUILD_BODY] {
        for len in 0..=4u32 {
            for choices in 0..(1u32 << len) {
                let (_dir, mut cfg) = load_stamped(start);
                let image = cfg.images.get_mut("probe").unwrap();
                let started_as_build = image.dockerfile().is_some();

                // Bit `i` of `choices` picks call `i`: set => `set_build_paths`.
                let mut last_wrote_build = started_as_build;
                for call in 0..len {
                    if choices & (1 << call) != 0 {
                        image.set_build_paths("Dockerfile", ".");
                        last_wrote_build = true;
                    } else {
                        image.set_image_name("docker.io/library/alpine:latest");
                        last_wrote_build = false;
                    }
                }

                let label = format!("start {start:?}, {len} calls, choices {choices:#b}");
                match image.source() {
                    ImageSourceRef::Build { .. } => {
                        assert!(last_wrote_build, "{label}: reports a shape nobody wrote")
                    }
                    ImageSourceRef::Image { .. } => {
                        assert!(!last_wrote_build, "{label}: reports a shape nobody wrote")
                    }
                    _ => unreachable!("{label}: ImageSourceRef has two shapes"),
                }
                cfg.validate(None)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
            }
        }
    }
}

const BODY: &str = r#"
[[workspace.mounts]]
host-path      = "data"
container-path = "/data"

[images.coding]
dockerfile = "images/coding/Dockerfile"
context    = "images/coding"
"#;

/// Privatizing a field changes `Serialize` / `Deserialize` derivation if the
/// rename attributes do not come with it, so the TOML keys are asserted by
/// name. The equality check is *semantic* -- a byte-identical comparison would
/// break on an unrelated serializer change and claims more than is needed.
#[test]
fn mutated_config_round_trips_with_its_keys_intact() {
    let mut cfg = Config::load_from_str(BODY).expect("fixture parses");
    cfg.workspace.mounts[0].set_host_path("cache");
    cfg.images
        .get_mut("coding")
        .unwrap()
        .set_build_paths("build/Containerfile", "build");

    let encoded = toml::to_string(&cfg).expect("config serializes");
    for key in ["host-path", "container-path", "dockerfile", "context"] {
        assert!(
            encoded.contains(key),
            "the TOML key {key:?} must survive privatization: {encoded}",
        );
    }

    let again = Config::load_from_str(&encoded).expect("reserialized parses");
    assert_eq!(cfg, again);

    let root = Path::new("/repo");
    assert_eq!(
        cfg.workspace.mounts[0].resolved_host_path(root),
        again.workspace.mounts[0].resolved_host_path(root),
    );
    assert_eq!(
        cfg.images["coding"].resolved_build_paths(root),
        again.images["coding"].resolved_build_paths(root),
    );
}

/// `schema_for!(ImageConfig)` is published to agents through the
/// `get_config_schema` MCP tool, and both types are `deny_unknown_fields`, so a
/// key that moved would be advertised as writable and then rejected on write.
/// The recorded source must stay absent for the same reason: it is
/// `#[schemars(skip)]` precisely because no caller may set it.
#[test]
fn generated_schema_still_advertises_the_kebab_case_keys() {
    /// The shape `get_config_schema` publishes, so the assertions below read
    /// the same JSON an agent would.
    fn properties<T: schemars::JsonSchema>() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(T)).unwrap()["properties"].clone()
    }

    let props = properties::<MountConfig>();
    for key in ["host-path", "container-path", "access"] {
        assert!(!props[key].is_null(), "MountConfig schema lost {key:?}");
    }
    assert!(
        props["source"].is_null() && props["host_path"].is_null(),
        "neither the provenance field nor a snake_case key may be advertised",
    );

    let props = properties::<ImageConfig>();
    for key in ["dockerfile", "context", "image-name"] {
        assert!(!props[key].is_null(), "ImageConfig schema lost {key:?}");
    }
    assert!(props["source"].is_null(), "provenance is not a config key");
}
