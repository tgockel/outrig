//! Build the LLM-facing `<server>__<tool>` name an MCP tool is exposed under.
//!
//! [`sanitize`] enforces OpenAI's `^[a-zA-Z0-9_-]{1,64}$` constraint on tool
//! names. Over-long names are truncated and tagged with a stable 6-hex blake3
//! suffix derived from the *pre-sanitization* `<server>__<tool>` so two tools
//! that would otherwise collide after character replacement get distinct
//! suffixes.
//!
//! Both `outrig run` (via [`crate::rig_tool::McpToolAdapter`]) and the
//! `mcp_proxy` server share this so they advertise identical public names for
//! the same upstream tool.

/// Maximum length OpenAI accepts for a tool name. Other providers are more
/// liberal but this is the safe lower bound.
const MAX_NAME_LEN: usize = 64;

/// Width of the truncation suffix's hex portion. The full suffix is
/// `_` + this many hex chars.
const HASH_HEX_LEN: usize = 6;
const SUFFIX_LEN: usize = 1 + HASH_HEX_LEN;

/// Build the LLM-facing name from `<server>__<tool>`, replacing any character
/// outside `[a-zA-Z0-9_-]` with `_` and truncating with a stable 6-hex blake3
/// suffix when the result would exceed `MAX_NAME_LEN`.
///
/// The hash is over the *pre-sanitization* concatenation, so two distinct
/// originals that would map to the same sanitized prefix get different
/// suffixes.
pub fn sanitize(server: &str, tool: &str) -> String {
    let original = format!("{server}__{tool}");

    let mut sanitized = String::with_capacity(original.len());
    for c in original.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }

    if sanitized.len() <= MAX_NAME_LEN {
        return sanitized;
    }

    let hash = blake3::hash(original.as_bytes());
    let hex = hash.to_hex();
    let suffix_hex = &hex.as_str()[..HASH_HEX_LEN];
    sanitized.truncate(MAX_NAME_LEN - SUFFIX_LEN);
    sanitized.push('_');
    sanitized.push_str(suffix_hex);
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize("fs", "read_file"), "fs__read_file");
    }

    #[test]
    fn sanitize_replaces_invalid_chars() {
        // `/`, ` `, `!` all get replaced with `_`. The `__` separator survives.
        let got = sanitize("fs", "weird/name with spaces!");
        assert_eq!(got, "fs__weird_name_with_spaces_");
        assert!(got.starts_with("fs__"));
        for c in got.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '_' || c == '-',
                "char {c:?} survived sanitization"
            );
        }
    }

    #[test]
    fn sanitize_short_names_unchanged_after_replacement() {
        // 9 + 2 + 9 = 20 chars, well under the 64-char limit. No truncation.
        let got = sanitize("server-01", "do_a_thing");
        assert_eq!(got, "server-01__do_a_thing");
    }

    #[test]
    fn sanitize_truncates_with_stable_hash_suffix() {
        let server = "fs";
        let tool = "x".repeat(120);
        let got = sanitize(server, &tool);
        assert_eq!(got.len(), 64, "got: {got:?}");

        // Stable: same inputs produce identical output.
        let again = sanitize(server, &tool);
        assert_eq!(got, again, "sanitize must be deterministic");
    }

    #[test]
    fn sanitize_distinguishes_long_inputs_with_shared_prefix() {
        // The hash suffix is over the *pre-sanitization* original, so two long
        // names sharing a 100-char prefix but differing at the tail still produce
        // different sanitized outputs.
        let prefix = "p".repeat(100);
        let a = sanitize("srv", &format!("{prefix}_aaaa"));
        let b = sanitize("srv", &format!("{prefix}_bbbb"));
        assert_ne!(
            a, b,
            "distinct originals must yield distinct sanitized names"
        );
        assert_eq!(a.len(), 64);
        assert_eq!(b.len(), 64);

        // The truncated prefix portion is identical -- only the hash suffix differs.
        let a_prefix = &a[..a.len() - 7];
        let b_prefix = &b[..b.len() - 7];
        assert_eq!(
            a_prefix, b_prefix,
            "shared 100-char prefix should survive truncation identically"
        );
        assert_ne!(
            &a[a.len() - 6..],
            &b[b.len() - 6..],
            "hash suffixes must differ"
        );
    }

    #[test]
    fn sanitize_truncated_output_still_charset_clean() {
        // Pre-sanitization input contains `/` which becomes `_` *and* the result
        // exceeds 64 chars. The hash suffix is hex (alphanumeric), so the final
        // string remains within `[a-zA-Z0-9_-]`.
        let tool = format!("{}{}{}", "a".repeat(50), "/", "b".repeat(50));
        let got = sanitize("svr", &tool);
        assert_eq!(got.len(), 64);
        for c in got.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '_' || c == '-',
                "char {c:?} survived sanitization in truncated form"
            );
        }
    }
}
