//! Integration tests for `ApiKeyRef`: every accept/reject case from
//! `doc/reference/config.md`'s "api-key syntax" section, plus resolve set/unset.

use outrig::config::{ApiKeyRef, Config};
use outrig::error::OutrigError;

mod api_key {
    use super::*;

    #[test]
    fn parse_accept_simple() {
        let r = ApiKeyRef::parse("${OPENAI_API_KEY}").expect("accepts simple env-var ref");
        assert_eq!(r.var_name(), "OPENAI_API_KEY");
    }

    #[test]
    fn parse_accept_leading_underscore() {
        let r = ApiKeyRef::parse("${_LEADING_UNDERSCORE}").expect("accepts leading underscore");
        assert_eq!(r.var_name(), "_LEADING_UNDERSCORE");
    }

    #[test]
    fn parse_accept_with_digits() {
        let r = ApiKeyRef::parse("${VAR_WITH_2_DIGITS}").expect("accepts internal digits");
        assert_eq!(r.var_name(), "VAR_WITH_2_DIGITS");
    }

    #[test]
    fn parse_reject_literal_key() {
        assert_invalid("sk-abc123def");
    }

    #[test]
    fn parse_reject_no_braces() {
        assert_invalid("$OPENAI_API_KEY");
    }

    #[test]
    fn parse_reject_mixed_case() {
        assert_invalid("${OpenAi_key}");
    }

    #[test]
    fn parse_reject_digit_first() {
        assert_invalid("${1FOO}");
    }

    #[test]
    fn parse_reject_empty_var() {
        assert_invalid("${}");
    }

    #[test]
    fn parse_reject_bare_name() {
        assert_invalid("OPENAI_API_KEY");
    }

    #[test]
    fn parse_reject_empty_string() {
        assert_invalid("");
    }

    #[test]
    fn parse_reject_trailing_junk() {
        // Guards against an unanchored regex letting trailing garbage through.
        assert_invalid("${OPENAI_API_KEY} extra");
    }

    #[test]
    fn resolve_when_set() {
        let var = "OUTRIG_TEST_API_KEY_RESOLVE_SET";
        // SAFETY: edition 2024 marks env::set_var unsafe due to multi-thread
        // races; this test uses a unique var name not touched elsewhere.
        unsafe {
            std::env::set_var(var, "the-secret");
        }
        let r = ApiKeyRef::parse(&format!("${{{var}}}")).unwrap();
        assert_eq!(r.resolve().unwrap(), "the-secret");
        unsafe {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn resolve_when_unset() {
        let var = "OUTRIG_TEST_API_KEY_RESOLVE_UNSET";
        // SAFETY: see resolve_when_set; unique var name.
        unsafe {
            std::env::remove_var(var);
        }
        let r = ApiKeyRef::parse(&format!("${{{var}}}")).unwrap();
        let err = r.resolve().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(var),
            "resolve error should name the missing var, got: {msg}",
        );
    }

    #[test]
    fn config_load_rejects_literal_api_key() {
        let bad = r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "sk-abc123"
"#;
        let err = Config::load_from_str(bad).unwrap_err();
        let OutrigError::Config(toml_err) = err else {
            panic!("expected OutrigError::Config, got: {err:?}");
        };
        let msg = toml_err.to_string();
        assert!(
            msg.contains("sk-abc123"),
            "error should quote the offending value, got: {msg}",
        );
        // toml renders line/column with the offending source line, which
        // includes `api-key` -- richer for CI logs than a bare path string.
        assert!(
            msg.contains("api-key"),
            "error should point at the api-key field, got: {msg}",
        );
    }

    fn assert_invalid(raw: &str) {
        let err = ApiKeyRef::parse(raw).expect_err(&format!("{raw:?} must be rejected"));
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("{raw:?}")) || msg.contains(raw),
            "error should quote the offending value {raw:?}, got: {msg}",
        );
    }
}
