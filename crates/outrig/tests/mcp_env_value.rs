//! Integration tests for `EnvValue`: parse classification, mixed-table
//! deserialize, Serde round-trip, and resolve set/unset.

use outrig::config::{Config, EnvValue, McpServerSpec, ResolvedEnvValue};

mod env_value {
    use super::*;

    #[test]
    fn from_raw_classifies_well_formed_ref() {
        assert_eq!(
            EnvValue::from_raw("${FOO}".to_string()).unwrap(),
            EnvValue::EnvRef("FOO".to_string()),
        );
    }

    #[test]
    fn from_raw_classifies_underscore_and_digit_var() {
        assert_eq!(
            EnvValue::from_raw("${_VAR_2}".to_string()).unwrap(),
            EnvValue::EnvRef("_VAR_2".to_string()),
        );
    }

    #[test]
    fn malformed_references_are_refused_without_echoing_the_value() {
        for raw in [
            "${lower}",
            "a-${X",
            "${}",
            "${1X}",
            "${X:-secret}",
            "${${X}}",
        ] {
            let error = EnvValue::from_raw(raw.to_string()).unwrap_err().to_string();
            assert!(error.contains("invalid env reference"), "{error}");
            assert!(!error.contains(raw), "{error}");
            assert!(serde_json::from_value::<EnvValue>(serde_json::json!(raw)).is_err());
        }
    }

    #[test]
    fn embedded_references_expand_once_and_round_trip_as_written() {
        let raw = "é-a-${X}-b-${Y}-${X}-$${Z}";
        let source = EnvValue::from_raw(raw.into()).unwrap();
        assert_eq!(source.references(), ["X", "Y", "X"]);
        let asked = std::sync::Mutex::new(Vec::new());
        let secrets = |var: &str| {
            asked.lock().unwrap().push(var.to_string());
            match var {
                "X" => Some("${NOT_EXPANDED}".to_string()),
                "Y" => Some(String::new()),
                _ => None,
            }
        };
        let resolved = ResolvedEnvValue::resolve_with(source.clone(), &secrets).unwrap();
        assert_eq!(
            resolved.value(),
            "é-a-${NOT_EXPANDED}-b--${NOT_EXPANDED}-${Z}"
        );
        assert_eq!(*asked.lock().unwrap(), ["X", "Y", "X"]);
        assert_eq!(resolved.source().to_raw(), raw);
        assert!(!format!("{resolved:?}").contains("NOT_EXPANDED"));
        let encoded = serde_json::to_string(&source).unwrap();
        assert_eq!(serde_json::from_str::<EnvValue>(&encoded).unwrap(), source);
        let missing = ResolvedEnvValue::resolve_with(source, &|_: &str| None).unwrap_err();
        assert!(missing.to_string().contains("X"));
    }

    #[test]
    fn escapes_are_literals_and_literal_values_serialize_safely() {
        for (raw, value) in [
            ("$${X}", "${X}"),
            ("$$${X}", "$${X}"),
            ("$${lower}", "${lower}"),
            ("$${", "${"),
            ("plain $X }", "plain $X }"),
        ] {
            let source = EnvValue::from_raw(raw.into()).unwrap();
            assert!(!source.has_references());
            let resolved = ResolvedEnvValue::resolve_with(source.clone(), &|_: &str| {
                panic!("literal asks nothing")
            })
            .unwrap();
            assert_eq!(resolved.value(), value);
            assert_eq!(source.to_raw(), raw);
        }
        let literal = EnvValue::Literal("${X}".into());
        let round_trip = EnvValue::from_raw(literal.to_raw()).unwrap();
        assert!(!round_trip.has_references());
        assert_eq!(
            ResolvedEnvValue::resolve(round_trip).unwrap().value(),
            "${X}"
        );
    }

    #[test]
    fn from_raw_classifies_path_as_literal() {
        let raw = "/workspace/.cargo".to_string();
        assert_eq!(
            EnvValue::from_raw(raw.clone()).unwrap(),
            EnvValue::Literal(raw),
        );
    }

    #[test]
    fn from_raw_classifies_empty_string_as_literal() {
        assert_eq!(
            EnvValue::from_raw(String::new()).unwrap(),
            EnvValue::Literal(String::new()),
        );
    }
}

mod mcp_env_table {
    use super::*;

    fn build_env(toml_src: &str) -> std::collections::BTreeMap<String, EnvValue> {
        let cfg = Config::load_from_str(toml_src).expect("config parses");
        let McpServerSpec::Full { env, .. } = cfg.images["c"].mcp()["srv"].clone() else {
            panic!("expected Full form, got Short");
        };
        env
    }

    #[test]
    fn mixed_table_classifies_each_value() {
        let env = build_env(
            r#"
[images.c]
dockerfile = "D"
context    = "ctx"

[images.c.mcp]
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
[images.c]
dockerfile = "D"
context    = "ctx"

[images.c.mcp]
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
        assert_eq!(ResolvedEnvValue::resolve(v).unwrap().value(), "hello");
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
        let resolved = ResolvedEnvValue::resolve(v).expect("resolves");
        unsafe {
            std::env::remove_var(var);
        }
        assert_eq!(resolved.value(), "hello-from-env");
    }

    #[test]
    fn envref_errors_when_unset_and_names_var() {
        let var = "OUTRIG_TEST_MCP_ENV_VALUE_RESOLVE_UNSET";
        // SAFETY: see envref_resolves_when_set; unique var name.
        unsafe {
            std::env::remove_var(var);
        }
        let v = EnvValue::EnvRef(var.to_string());
        let err = ResolvedEnvValue::resolve(v).expect_err("must error on unset var");
        let msg = err.to_string();
        assert!(
            msg.contains(var),
            "resolve error should name the missing var, got: {msg}",
        );
    }
}

#[test]
fn malformed_config_values_are_refused_but_api_keys_keep_their_grammar() {
    assert!(
        Config::load_from_str(
            r#"
[images.c]
image-name = "img"
build-args = { VALUE = "prefix-${lower}" }
"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<outrig::config::ApiKeyRef>(r#""Bearer ${TOKEN}""#).is_err());
}
