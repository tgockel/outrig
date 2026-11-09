//! Shared `${VAR}` parser used by `ApiKeyRef` and `EnvValue`, the name check
//! behind `[repo] env-allow`, and the grammar an `env` or `build-args` key is
//! held to. Variable names match `^[A-Z_][A-Z0-9_]*$`; the regex is anchored
//! on both ends, so a value either *is* `${VAR}` whole or it isn't. A key is
//! held to the wider `^[A-Za-z_][A-Za-z0-9_]*$` -- see [`is_env_key`].

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

/// Whether `key` can name a variable an `env` or `build-args` table sets:
/// `^[A-Za-z_][A-Za-z0-9_]*$`.
///
/// Wider than the grammar of a `${VAR}` *reference*, which is upper-case only.
/// The two name variables on opposite sides of the container boundary: a
/// reference reads one from the operator's environment, where the convention
/// is upper case, while a key sets one inside the container, where
/// `http_proxy` and `cargo_target_dir` are ordinary. Neither admits what the
/// engines misread -- an empty key, one holding `=` (podman reads
/// `--env A=B=c` as `A` set to `B=c`), one podman takes for a glob over its
/// own environment (`PATH*`), or one with the whitespace podman trims off a
/// bare key.
///
/// Config validation holds every `[images.<name>.mcp]` `env` key and every
/// `build-args` key to this, as do an image's `org.outrig.mcp` label and a
/// standalone `image.toml`. It is public so that a caller building an
/// [`McpServerSpec`](super::McpServerSpec) or an `--env` overlay of its own
/// can refuse the same keys with the same answer.
pub fn is_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
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

    /// A key names a variable in the container and a reference one in the
    /// operator's environment, so the key grammar is the wider of the two:
    /// every name a reference can carry is also a key, and a lower-case key
    /// is a key without being a reference.
    #[test]
    fn a_key_is_a_letter_or_underscore_then_alphanumerics() {
        for key in ["FOO", "_VAR_2", "_", "http_proxy", "MixedCase9"] {
            assert!(is_env_key(key), "{key:?}");
            assert!(is_env_var_name(key) || key.chars().any(|c| c.is_ascii_lowercase()));
        }
    }

    /// The shapes the issue found: a key holding `=` sets a shorter name with
    /// the rest of itself in the value, and an empty one is refused by podman
    /// at the first server connect rather than by the config that wrote it.
    #[test]
    fn a_key_is_not_an_assignment_a_glob_or_empty() {
        for key in [
            "", "A=B", "=", "my-key", "1AB", "K*", " K", "K ", "a.b", "FOO}", "${FOO}", "A\nB",
        ] {
            assert!(!is_env_key(key), "{key:?}");
        }
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
