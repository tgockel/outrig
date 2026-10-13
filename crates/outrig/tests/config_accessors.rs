//! The migration a downstream consumer has to perform, proven to compile.
//!
//! `MountConfig::host_path` / `container_path` / `access` and
//! `ImageConfig::dockerfile` / `context` were public fields until they were
//! paired with a recorded `ConfigSource`, and `ImageConfig::image_name`,
//! `build_args`, `security` and `mcp` were public until no `ImageConfig` field
//! was. This file reads and writes every one of them through the accessors that
//! replaced them, so the replacement surface is exercised from *outside* the
//! crate -- an integration test sees only `pub` items, exactly as a consumer
//! does.
//!
//! Deliberately not `#![cfg(feature = "e2e")]`, unlike `library_surface.rs`:
//! nothing here needs podman, and a gated fixture would not run.

use std::fs;
use std::path::{Path, PathBuf};

use outrig::config::{
    Config, ConfigValidationError, EnvValue, ImageConfig, ImageSourceRef, McpServerSpec,
    MountAccess, MountConfig,
};
use outrig::error::OutrigError;

/// Every accessor that replaced a field, in both directions, on a hand-built
/// value. `MountConfig::new` is unchanged -- it already produced a sourceless
/// entry -- so the whole migration is field syntax becoming call syntax.
#[test]
fn mount_accessors_read_and_write_every_replaced_field() {
    let mut mount = MountConfig::new("data", "/data", MountAccess::ReadOnly);

    assert_eq!(mount.host_path(), Path::new("data"));
    assert_eq!(mount.declared_container_path(), Some(Path::new("/data")));
    assert_eq!(mount.access(), MountAccess::ReadOnly);

    mount.set_host_path("cache");
    mount.set_container_path("/cache");
    mount.set_access(MountAccess::ReadWrite);

    assert_eq!(mount.host_path(), Path::new("cache"));
    assert_eq!(mount.declared_container_path(), Some(Path::new("/cache")));
    assert_eq!(mount.access(), MountAccess::ReadWrite);
    assert_eq!(
        mount.resolved_host_path(Path::new("/repo")),
        PathBuf::from("/repo/cache"),
        "a hand-built entry has always resolved against the argument",
    );
}

/// The image half. `set_build_paths` and `set_image_name` are the only writers
/// of the source shape. The paths go together because one recorded source backs
/// both, and replacing one alone would rebase the other.
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

    assert_eq!(image.image_name(), None);

    let mut pulled = ImageConfig::from_image_name("docker.io/library/alpine:3");
    assert_eq!(pulled.image_name(), Some("docker.io/library/alpine:3"));
    assert_eq!(pulled.dockerfile(), None);
    assert_eq!(pulled.context(), None);

    pulled.set_image_name("docker.io/library/debian:12");
    assert_eq!(pulled.image_name(), Some("docker.io/library/debian:12"));
}

/// `build_args`, `security` and `mcp` take no part in the source shape, so each
/// reads through a getter and is edited in place through its `_mut`, both
/// compiled from outside the crate.
#[test]
fn the_other_fields_read_and_write_through_accessors() {
    let mut image = ImageConfig::from_dockerfile("Dockerfile", ".");

    image
        .build_args_mut()
        .insert("NODE_VERSION".into(), EnvValue::Literal("20".into()));
    assert_eq!(
        image.build_args()["NODE_VERSION"],
        EnvValue::Literal("20".into()),
    );

    image.security_mut().no_new_privileges = false;
    assert!(!image.security().no_new_privileges);

    let server = McpServerSpec::Short(vec!["mcp-server-git".into()]);
    image.mcp_mut().insert("git".into(), server.clone());
    assert_eq!(image.mcp()["git"], server);
    assert_eq!(image.mcp_mut().remove("git"), Some(server));
}

const PULL: &str = "image-name = \"docker.io/library/alpine:3\"\n";
const BUILD: &str = "dockerfile = \"Dockerfile\"\ncontext = \".\"\n";

/// A config file on disk declaring one image, `[images.probe]`, loaded the way
/// the CLI loads one. Unlike `Config::load_from_str` this records a
/// `ConfigSource` on the image, which is what the setters have to clear. The
/// directory is returned so it outlives the config and can be the `repo_root`
/// a second `validate` resolves against.
fn load_probe(image_toml: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").expect("write Dockerfile");
    let file = dir.path().join("outrig.toml");
    fs::write(&file, format!("[images.probe]\n{image_toml}")).expect("write config");
    let cfg = Config::load_file(&file, dir.path(), None).expect("fixture loads");
    (dir, cfg)
}

/// #270: `set_build_paths` on a config that names an image used to leave the
/// name set, producing both shapes at once -- a value `source()` panics on and
/// `validate` rejects, made by a public method.
#[test]
fn set_build_paths_on_a_pull_source_config_replaces_the_shape() {
    let (dir, mut cfg) = load_probe(PULL);

    cfg.images
        .get_mut("probe")
        .unwrap()
        .set_build_paths("Dockerfile", ".");

    let image = &cfg.images["probe"];
    assert!(matches!(image.source(), ImageSourceRef::Build { .. }));
    assert_eq!(image.image_name(), None);
    cfg.validate(Some(dir.path()))
        .expect("the converted config validates");
}

/// #270's other direction, which had no setter at all: assigning the name on a
/// build-source config was the only way, and reached the same panic. The paths
/// and the source that backed them go together.
#[test]
fn set_image_name_on_a_build_source_config_replaces_the_shape() {
    let (dir, mut cfg) = load_probe(BUILD);
    let image = cfg.images.get_mut("probe").unwrap();
    assert!(
        image.config_source().is_some(),
        "the fixture must have a recorded source for this test to clear it",
    );

    image.set_image_name("docker.io/library/alpine:3");

    assert!(matches!(
        image.source(),
        ImageSourceRef::Image { image_name, .. } if image_name == "docker.io/library/alpine:3",
    ));
    assert_eq!(image.dockerfile(), None);
    assert_eq!(image.context(), None);
    assert!(
        image.config_source().is_none(),
        "no path is left for the recorded source to resolve",
    );
    cfg.validate(Some(dir.path()))
        .expect("the converted config validates");
}

/// `set_image_name` does not empty `build_args`. A pull source that still has
/// some is the caller's to fix, and `validate` says so rather than the setter
/// deciding the arguments were not wanted.
#[test]
fn set_image_name_keeps_build_args_and_validate_reports_them() {
    let (dir, mut cfg) = load_probe(&format!("{BUILD}build-args = {{ FOO = \"bar\" }}\n"));

    cfg.images
        .get_mut("probe")
        .unwrap()
        .set_image_name("docker.io/library/alpine:3");

    match cfg.validate(Some(dir.path())) {
        Err(OutrigError::ConfigValidation(ConfigValidationError::ImageNameWithBuildArgs {
            image,
        })) => assert_eq!(image, "probe"),
        other => panic!("expected ImageNameWithBuildArgs, got: {other:?}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Build,
    Pull,
}

/// Whether `image` holds a shape `source()` accepts, and which one. Reads only
/// the getters, so a failure names the sequence that made it rather than being
/// a bare panic from inside the library.
fn is_build_shape(image: &ImageConfig, how: &str) -> bool {
    match (
        image.image_name().is_some(),
        image.dockerfile().is_some(),
        image.context().is_some(),
    ) {
        (true, false, false) => false,
        (false, true, true) => true,
        other => panic!("{how}: (image-name, dockerfile, context) set = {other:?}"),
    }
}

/// #461: no sequence of public calls reaches a state `source()` rejects. The
/// two setters are the whole write surface for the shape, so this is exhaustive
/// over them -- every sequence up to `LONGEST`, from each way of obtaining a
/// valid config -- rather than a sampled pair. Each setter replaces the shape
/// whole, so the last one called decides it and the history never does.
#[test]
fn every_sequence_of_the_shape_setters_leaves_source_callable() {
    const LONGEST: u32 = 4;

    let (_build_dir, built) = load_probe(BUILD);
    let (_pull_dir, pulled) = load_probe(PULL);
    let starts = [
        (
            "from_dockerfile",
            ImageConfig::from_dockerfile("Dockerfile", "."),
        ),
        ("from_image_name", ImageConfig::from_image_name("alpine:3")),
        ("a loaded build source", built.images["probe"].clone()),
        ("a loaded pull source", pulled.images["probe"].clone()),
    ];

    for (origin, start) in &starts {
        let started_as = is_build_shape(start, origin);
        for len in 0..=LONGEST {
            for bits in 0u32..(1 << len) {
                let steps: Vec<Step> = (0..len)
                    .map(|i| {
                        if bits >> i & 1 == 0 {
                            Step::Build
                        } else {
                            Step::Pull
                        }
                    })
                    .collect();
                let how = format!("{origin}, then {steps:?}");

                let mut image = start.clone();
                for step in &steps {
                    match step {
                        Step::Build => image.set_build_paths("Dockerfile", "."),
                        Step::Pull => image.set_image_name("docker.io/library/alpine:3"),
                    }
                }

                let is_build = is_build_shape(&image, &how);
                let expected = steps.last().map_or(started_as, |s| *s == Step::Build);
                assert_eq!(
                    is_build, expected,
                    "{how}: the last setter decides the shape"
                );
                assert_eq!(
                    matches!(image.source(), ImageSourceRef::Build { .. }),
                    is_build,
                    "{how}: source() disagrees with the getters",
                );
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

[images.pulled]
image-name = "docker.io/library/alpine:3"
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
    for key in [
        "host-path",
        "container-path",
        "dockerfile",
        "context",
        "image-name",
    ] {
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
    for key in [
        "dockerfile",
        "context",
        "image-name",
        "build-args",
        "security",
        "mcp",
    ] {
        assert!(!props[key].is_null(), "ImageConfig schema lost {key:?}");
    }
    assert!(props["source"].is_null(), "provenance is not a config key");
}
