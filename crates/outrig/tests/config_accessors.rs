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

use outrig::config::{Config, ImageConfig, MountAccess, MountConfig};

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

/// The image half. `set_build_paths` is the only writer, because one recorded
/// source backs both paths and replacing one alone would rebase the other.
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
