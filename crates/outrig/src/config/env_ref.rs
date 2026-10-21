//! Shared `${VAR}` parser used by `ApiKeyRef` and `EnvValue`, and the name
//! check behind `[repo] env-allow`. Variable names match `^[A-Z_][A-Z0-9_]*$`;
//! the regex is anchored on both ends, so a value either *is* `${VAR}` whole
//! or it isn't.

use std::sync::OnceLock;

use regex::Regex;

/// If `raw` whole-matches `${VAR}`, return `Some(VAR)`; otherwise `None`.
pub(crate) fn parse_env_ref(raw: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"^\$\{([A-Z_][A-Z0-9_]*)\}$").expect("env-ref regex compiles"));
    re.captures(raw).map(|caps| caps.get(1).unwrap().as_str())
}

/// Whether `name` is a variable name a `${VAR}` reference can carry. Asked of
/// the reference itself rather than a second copy of the grammar, so a list of
/// names can never hold an entry that no reference could match.
pub(crate) fn is_env_var_name(name: &str) -> bool {
    parse_env_ref(&format!("${{{name}}}")).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_simple_uppercase() {
        assert_eq!(parse_env_ref("${FOO}"), Some("FOO"));
    }

    #[test]
    fn accepts_leading_underscore_and_digits() {
        assert_eq!(parse_env_ref("${_VAR_2}"), Some("_VAR_2"));
    }

    #[test]
    fn rejects_lowercase() {
        assert_eq!(parse_env_ref("${foo}"), None);
    }

    #[test]
    fn rejects_digit_first() {
        assert_eq!(parse_env_ref("${1FOO}"), None);
    }

    #[test]
    fn rejects_empty_braces() {
        assert_eq!(parse_env_ref("${}"), None);
    }

    #[test]
    fn rejects_no_braces() {
        assert_eq!(parse_env_ref("$FOO"), None);
    }

    #[test]
    fn rejects_trailing_junk() {
        assert_eq!(parse_env_ref("${FOO} extra"), None);
    }

    #[test]
    fn rejects_leading_junk() {
        assert_eq!(parse_env_ref("prefix-${FOO}"), None);
    }

    #[test]
    fn rejects_empty_string() {
        assert_eq!(parse_env_ref(""), None);
    }

    #[test]
    fn rejects_bare_name() {
        assert_eq!(parse_env_ref("FOO"), None);
    }

    #[test]
    fn a_name_is_what_a_reference_can_carry() {
        for name in ["FOO", "_VAR_2", "_"] {
            assert!(is_env_var_name(name), "{name:?}");
        }
        for name in [
            "", "foo", "1FOO", "FOO}", "${FOO}", "FOO BAR", "FOO\n", "*", "A-B",
        ] {
            assert!(!is_env_var_name(name), "{name:?}");
        }
    }
}
