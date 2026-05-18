//! Unit tests for `build-args` values parsed as `EnvValue` and
//! resolved into concrete strings for image builds.

use crate::config::{Config, ContainerConfig, EnvValue};
use crate::error::OutrigError;

use super::resolve_build_args;

fn container_config(toml_src: &str) -> ContainerConfig {
    let cfg = Config::load_from_str(toml_src).expect("config parses");
    cfg.containers["coding"].clone()
}

mod parse {
    use super::*;

    #[test]
    fn build_args_classify_literals_and_refs() {
        let container = container_config(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"
build-args = { NODE_VERSION = "20", GH_TOKEN = "${GITHUB_TOKEN}", STILL_LIT = "${lower_case}" }
"#,
        );

        assert_eq!(
            container.build_args["NODE_VERSION"],
            EnvValue::Literal("20".to_string()),
        );
        assert_eq!(
            container.build_args["GH_TOKEN"],
            EnvValue::EnvRef("GITHUB_TOKEN".to_string()),
        );
        assert_eq!(
            container.build_args["STILL_LIT"],
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

        let container = container_config(&format!(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"
build-args = {{ NODE_VERSION = "20", GH_TOKEN = "${{{var}}}" }}
"#,
        ));
        let resolved = resolve_build_args("coding", &container).expect("resolves");

        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(resolved["NODE_VERSION"], "20");
        assert_eq!(resolved["GH_TOKEN"], "secret-token");
    }

    #[test]
    fn unset_env_ref_is_framed_as_build_arg_failure() {
        let var = "OUTRIG_TEST_BUILD_ARGS_ENV_VALUE_UNSET";
        // SAFETY: see mixed_build_args_resolve_literals_and_env_refs; unique
        // var name.
        unsafe {
            std::env::remove_var(var);
        }

        let container = container_config(&format!(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"
build-args = {{ GH_TOKEN = "${{{var}}}" }}
"#,
        ));

        let err = resolve_build_args("coding", &container).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("coding"),
            "error should name the container-config, got: {msg}",
        );
        assert!(
            msg.contains("GH_TOKEN"),
            "error should name the build-arg key, got: {msg}",
        );
        assert!(
            msg.contains(var),
            "error should name the missing env var, got: {msg}",
        );

        let OutrigError::BuildArgResolveFailed { container, key, .. } = err else {
            panic!("expected BuildArgResolveFailed");
        };
        assert_eq!(container, "coding");
        assert_eq!(key, "GH_TOKEN");
    }
}
