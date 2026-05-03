//! Pure-Rust unit tests for `outrig::rig_tool::sanitize`. No container, no
//! rmcp -- just the name-mangling rules.

use outrig::rig_tool::sanitize;

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
