//! Unit tests for `build-args` values parsed as `EnvValue` and
//! resolved into concrete strings for image builds.

use std::collections::BTreeMap;

use crate::config::{Config, EnvValue, ImageConfig};
use crate::error::OutrigError;

use super::{compute_tag_for, resolve_build_args};

fn image_config(toml_src: &str) -> ImageConfig {
    let cfg = Config::load_from_str(toml_src).expect("config parses");
    cfg.images["coding"].clone()
}

mod parse {
    use super::*;

    #[test]
    fn build_args_classify_literals_and_refs() {
        let image = image_config(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"
build-args = { NODE_VERSION = "20", GH_TOKEN = "${GITHUB_TOKEN}", STILL_LIT = "${lower_case}" }
"#,
        );

        assert_eq!(
            image.build_args["NODE_VERSION"],
            EnvValue::Literal("20".to_string()),
        );
        assert_eq!(
            image.build_args["GH_TOKEN"],
            EnvValue::EnvRef("GITHUB_TOKEN".to_string()),
        );
        assert_eq!(
            image.build_args["STILL_LIT"],
            EnvValue::Literal("${lower_case}".to_string()),
        );
    }
}

mod resolve {
    use super::*;

    #[test]
    fn mixed_build_args_resolve_literals_and_env_refs() {
        let var = "OUTRIG_TEST_BUILD_ARGS_ENV_VALUE_SET";
        // SAFETY: edition 2024 marks env::set_var unsafe due to multi-thread
        // races; this test uses a unique var name not touched elsewhere.
        unsafe {
            std::env::set_var(var, "secret-token");
        }

        let image = image_config(&format!(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"
build-args = {{ NODE_VERSION = "20", GH_TOKEN = "${{{var}}}" }}
"#,
        ));
        let resolved = resolve_build_args("coding", &image).expect("resolves");

        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(resolved["NODE_VERSION"].value(), "20");
        assert_eq!(
            resolved["NODE_VERSION"].source(),
            &EnvValue::Literal("20".to_string())
        );
        assert_eq!(resolved["GH_TOKEN"].value(), "secret-token");
        assert_eq!(
            resolved["GH_TOKEN"].source(),
            &EnvValue::EnvRef(var.to_string())
        );
    }

    /// Passing a reference by name changes how buildah receives it, not what
    /// it receives, so the cache key still covers the value.
    #[tokio::test]
    async fn the_cache_key_covers_a_referenced_value_as_it_does_a_literal() {
        let var = "OUTRIG_TEST_BUILD_ARGS_ENV_VALUE_KEYED";
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("Dockerfile"), "FROM alpine\n").expect("write Dockerfile");
        let tag_with = |value: EnvValue| {
            let mut cfg = ImageConfig::from_dockerfile("Dockerfile", ".");
            cfg.build_args = BTreeMap::from([("TOKEN".to_string(), value)]);
            let root = repo.path().to_path_buf();
            async move { compute_tag_for("coding", &cfg, &root).await.expect("tag") }
        };

        // SAFETY: see mixed_build_args_resolve_literals_and_env_refs; unique
        // var name.
        unsafe {
            std::env::set_var(var, "v1");
        }
        let referenced = tag_with(EnvValue::EnvRef(var.to_string())).await;
        unsafe {
            std::env::set_var(var, "v2");
        }
        let changed = tag_with(EnvValue::EnvRef(var.to_string())).await;
        unsafe {
            std::env::remove_var(var);
        }

        assert_eq!(
            referenced,
            tag_with(EnvValue::Literal("v1".to_string())).await
        );
        assert_ne!(referenced, changed);
    }

    #[test]
    fn unset_env_ref_is_framed_as_build_arg_failure() {
        let var = "OUTRIG_TEST_BUILD_ARGS_ENV_VALUE_UNSET";
        // SAFETY: see mixed_build_args_resolve_literals_and_env_refs; unique
        // var name.
        unsafe {
            std::env::remove_var(var);
        }

        let image = image_config(&format!(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"
build-args = {{ GH_TOKEN = "${{{var}}}" }}
"#,
        ));

        let err = resolve_build_args("coding", &image).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("coding"),
            "error should name the image-config, got: {msg}",
        );
        assert!(
            msg.contains("GH_TOKEN"),
            "error should name the build-arg key, got: {msg}",
        );
        assert!(
            msg.contains(var),
            "error should name the missing env var, got: {msg}",
        );

        let OutrigError::BuildArgResolveFailed { image, key, .. } = err else {
            panic!("expected BuildArgResolveFailed");
        };
        assert_eq!(image, "coding");
        assert_eq!(key, "GH_TOKEN");
    }
}
