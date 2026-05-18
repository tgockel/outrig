//! Integration tests for `EnvValue`: parse classification, mixed-table
//! deserialize, Serde round-trip, and resolve set/unset.

use outrig::config::{Config, EnvValue, McpServerSpec};

mod env_value {
    use super::*;

    #[test]
    fn from_raw_classifies_well_formed_ref() {
        assert_eq!(
            EnvValue::from_raw("${FOO}".to_string()),
            EnvValue::EnvRef("FOO".to_string()),
        );
    }

    #[test]
    fn from_raw_classifies_underscore_and_digit_var() {
        assert_eq!(
            EnvValue::from_raw("${_VAR_2}".to_string()),
            EnvValue::EnvRef("_VAR_2".to_string()),
        );
    }

    #[test]
    fn from_raw_treats_lowercase_brace_form_as_literal() {
        let raw = "${lower}".to_string();
        assert_eq!(EnvValue::from_raw(raw.clone()), EnvValue::Literal(raw),);
    }

    #[test]
    fn from_raw_treats_embedded_substitution_as_literal() {
        let raw = "prefix-${FOO}-suffix".to_string();
        assert_eq!(EnvValue::from_raw(raw.clone()), EnvValue::Literal(raw),);
    }

    #[test]
    fn from_raw_classifies_path_as_literal() {
        let raw = "/workspace/.cargo".to_string();
        assert_eq!(EnvValue::from_raw(raw.clone()), EnvValue::Literal(raw),);
    }

    #[test]
    fn from_raw_classifies_empty_string_as_literal() {
        assert_eq!(
            EnvValue::from_raw(String::new()),
            EnvValue::Literal(String::new()),
        );
    }
}

mod mcp_env_table {
    use super::*;

    fn build_env(toml_src: &str) -> std::collections::BTreeMap<String, EnvValue> {
        let cfg = Config::load_from_str(toml_src).expect("config parses");
        let McpServerSpec::Full { env, .. } = cfg.containers["c"].mcp["srv"].clone() else {
            panic!("expected Full form, got Short");
        };
        env
    }

    #[test]
    fn mixed_table_classifies_each_value() {
        let env = build_env(
            r#"
[containers.c]
dockerfile = "D"
context    = "ctx"

[containers.c.mcp]
srv = { command = ["bin"], env = { CARGO_HOME = "/workspace/.cargo", GH_TOKEN = "${GITHUB_TOKEN}" } }
"#,
        );

        assert_eq!(
            env["CARGO_HOME"],
            EnvValue::Literal("/workspace/.cargo".to_string()),
        );
        assert_eq!(
            env["GH_TOKEN"],
            EnvValue::EnvRef("GITHUB_TOKEN".to_string()),
        );
    }

    #[test]
    fn round_trip_preserves_literal_and_envref() {
        let original = r#"
[containers.c]
dockerfile = "D"
context    = "ctx"

[containers.c.mcp]
srv = { command = ["bin"], env = { CARGO_HOME = "/workspace/.cargo", GH_TOKEN = "${GITHUB_TOKEN}" } }
"#;

        let parsed = Config::load_from_str(original).expect("parses");
        let reserialized = toml::to_string(&parsed).expect("serializes");
        assert!(
            reserialized.contains(r#""${GITHUB_TOKEN}""#),
            "EnvRef should round-trip as a `${{VAR}}` string, got:\n{reserialized}",
        );
        assert!(
            reserialized.contains(r#""/workspace/.cargo""#),
            "Literal should round-trip as a bare string, got:\n{reserialized}",
        );

        let again = Config::load_from_str(&reserialized).expect("reserialized parses");
        assert_eq!(parsed, again);
    }
}

mod resolve {
    use super::*;

    #[test]
    fn literal_resolves_to_itself() {
        let v = EnvValue::Literal("hello".to_string());
        assert_eq!(v.resolve().unwrap(), "hello");
    }

    #[test]
    fn envref_resolves_when_set() {
        let var = "OUTRIG_TEST_MCP_ENV_VALUE_RESOLVE_SET";
        // SAFETY: edition 2024 marks env::set_var unsafe due to multi-thread
        // races; this test uses a unique var name not touched elsewhere.
        unsafe {
            std::env::set_var(var, "hello-from-env");
        }
        let v = EnvValue::EnvRef(var.to_string());
        let resolved = v.resolve().expect("resolves");
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(resolved, "hello-from-env");
    }

    #[test]
    fn envref_errors_when_unset_and_names_var() {
        let var = "OUTRIG_TEST_MCP_ENV_VALUE_RESOLVE_UNSET";
        // SAFETY: see envref_resolves_when_set; unique var name.
        unsafe {
            std::env::remove_var(var);
        }
        let v = EnvValue::EnvRef(var.to_string());
        let err = v.resolve().expect_err("must error on unset var");
        let msg = err.to_string();
        assert!(
            msg.contains(var),
            "resolve error should name the missing var, got: {msg}",
        );
    }
}
