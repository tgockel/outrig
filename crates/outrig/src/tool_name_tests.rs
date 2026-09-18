//! Tests for [`sanitize`]. Out of line rather than an inline `mod tests`
//! because the golden table is a wire contract and reads better with room.

use super::*;

/// The constraint the whole module exists to satisfy: `^[a-zA-Z0-9_-]{1,64}$`.
#[track_caller]
fn assert_well_formed(name: &str) {
    assert!(
        !name.is_empty() && name.len() <= MAX_NAME_LEN,
        "length out of range: {name:?} is {} chars",
        name.len()
    );
    for c in name.chars() {
        assert!(
            c.is_ascii_alphanumeric() || c == '_' || c == '-',
            "char {c:?} survived sanitization in {name:?}"
        );
    }
}

/// The suffix's expected hex, derived from the documented preimage without
/// reusing the implementation's hasher: the pair, each side length-prefixed.
fn expected_hex(server: &str, tool: &str, hex_len: usize) -> String {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(&(server.len() as u64).to_le_bytes());
    preimage.extend_from_slice(server.as_bytes());
    preimage.extend_from_slice(&(tool.len() as u64).to_le_bytes());
    preimage.extend_from_slice(tool.as_bytes());
    blake3::hash(&preimage).to_hex().as_str()[..hex_len].to_string()
}

/// Pairs worth running every property against: clean, replaced, over-long,
/// non-ASCII, empty, and separator-ambiguous on either side.
fn corpus() -> Vec<(String, String)> {
    let servers = [
        "fs",
        "server-01",
        "a",
        "a_",
        "fs__a",
        "_",
        "",
        RESERVED_SERVER,
        &"s".repeat(70),
    ];
    let tools = [
        "read_file",
        "read/file",
        "read file",
        "_b",
        "b__c",
        "léer_archivo",
        "日本語",
        "",
        "-",
        &"x".repeat(120),
        &format!("{}/{}", "a".repeat(50), "b".repeat(50)),
    ];
    servers
        .iter()
        .flat_map(|s| tools.iter().map(move |t| (s.to_string(), t.to_string())))
        .collect()
}

#[test]
fn unchanged_names_are_returned_byte_identical() {
    // The common case, and the one worth not disturbing: a composition that
    // already satisfies the constraint and decodes back to its pair.
    assert_eq!(sanitize("fs", "read_file"), "fs__read_file");
    assert_eq!(sanitize("server-01", "do_a_thing"), "server-01__do_a_thing");
    // A `__` inside the *tool* is still faithful: the split is at the first
    // one, so the rest of the name is unambiguously the tool's.
    assert_eq!(sanitize("fs", "a__b"), "fs__a__b");
    // Exactly at the limit, so the length test is `<=` and not `<`.
    let at_limit = sanitize("fs", &"t".repeat(60));
    assert_eq!(at_limit.len(), MAX_NAME_LEN);
    assert_eq!(at_limit, format!("fs__{}", "t".repeat(60)));
}

#[test]
fn replacement_disambiguates_short_names() {
    // The defect this task closes: both collapse to `s__read_file`, and
    // before the suffix moved off the length path they were the same name.
    let slash = sanitize("s", "read/file");
    let space = sanitize("s", "read file");
    assert_ne!(slash, space, "replacement must not merge distinct tools");
    assert!(slash.starts_with("s__read_file_"), "got {slash:?}");
    assert!(space.starts_with("s__read_file_"), "got {space:?}");
    assert_well_formed(&slash);
    assert_well_formed(&space);
}

#[test]
fn preimage_is_length_delimited() {
    // `("a", "_b")` and `("a_", "b")` share the concatenation `a___b`, so a
    // preimage that is that concatenation cannot tell them apart. Only the
    // first is faithful -- its first `__` is where `"a"` ends.
    assert_eq!(sanitize("a", "_b"), "a___b");
    let ambiguous = sanitize("a_", "b");
    assert_ne!(sanitize("a", "_b"), ambiguous);
    assert!(ambiguous.starts_with("a___b_"), "got {ambiguous:?}");

    // Same shape one level up: a server carrying the separator itself.
    assert_eq!(sanitize("fs", "a__b"), "fs__a__b");
    assert_ne!(sanitize("fs__a", "b"), sanitize("fs", "a__b"));

    // And the digests themselves differ, not just the names.
    assert_ne!(
        expected_hex("a", "_b", HASH_HEX_LEN),
        expected_hex("a_", "b", HASH_HEX_LEN)
    );
}

#[test]
fn golden_vectors_pin_the_advertised_mapping() {
    // An advertised name is what a model calls and what a client may persist
    // across sessions, so changing this table invalidates anything that
    // cached a name. These are pinned to catch that happening by accident.
    let long_x = "x".repeat(70);
    let both = format!("{}/{}", "a".repeat(50), "b".repeat(50));
    let at_limit = "t".repeat(60);
    let past_limit = "t".repeat(61);
    let cases: &[(&str, &str, &str)] = &[
        // unchanged
        ("fs", "read_file", "fs__read_file"),
        // replacement only
        ("fs", "read/file", "fs__read_file_5d2270"),
        ("fs", "read file", "fs__read_file_4e31b7"),
        // truncation only
        (
            "fs",
            &long_x,
            "fs__xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx_d23c4f",
        ),
        // replacement and truncation together
        (
            "svr",
            &both,
            "svr__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_b_d0b7db",
        ),
        // boundary: exactly 64 stays, 65 does not
        (
            "fs",
            &at_limit,
            "fs__tttttttttttttttttttttttttttttttttttttttttttttttttttttttttttt",
        ),
        (
            "fs",
            &past_limit,
            "fs__ttttttttttttttttttttttttttttttttttttttttttttttttttttt_9c668e",
        ),
        // non-ASCII: one multi-byte char becomes one `_`
        ("fs", "léer_archivo", "fs__l_er_archivo_696f7a"),
        // separator ambiguity, and its faithful counterpart
        ("a_", "b", "a___b_1b7557"),
        ("a", "_b", "a___b"),
        ("fs__a", "b", "fs__a__b_f08ff9"),
    ];

    for (server, tool, expected) in cases {
        let got = sanitize(server, tool);
        assert_eq!(&got, expected, "({server:?}, {tool:?})");
        assert_well_formed(&got);
    }
}

#[test]
fn sanitized_names_satisfy_the_constraint() {
    // The achievable claim, asserted over the corpus: every output is
    // well-formed, the function is deterministic, and equal inputs give equal
    // outputs. Deliberately *not* injectivity -- an unbounded pair of Unicode
    // strings cannot inject into 64 characters, so no implementation could
    // pass such a test.
    for (server, tool) in corpus() {
        let got = sanitize(&server, &tool);
        assert_well_formed(&got);
        assert_eq!(got, sanitize(&server, &tool), "({server:?}, {tool:?})");
        // Freshly built values rather than the same allocations again, so
        // this asserts equal *inputs* give equal outputs and not merely that
        // one call repeated itself.
        let (twin_server, twin_tool) = (String::from(&server[..]), String::from(&tool[..]));
        assert_eq!(
            got,
            sanitize(&twin_server, &twin_tool),
            "equal inputs must give equal outputs"
        );
    }
}

#[test]
fn suffix_is_blake3_over_the_documented_preimage() {
    // The mapping is a pure function of the pair, with no process-local state
    // anywhere in it, which is what makes a reconnecting session see the same
    // names. Recomputed here from the documented preimage rather than from
    // the implementation's hasher.
    for (server, tool) in corpus() {
        let got = sanitize(&server, &tool);
        if got == format!("{server}__{tool}") {
            continue; // faithful, no suffix to check
        }
        let hex = expected_hex(&server, &tool, HASH_HEX_LEN);
        assert!(
            got.ends_with(&format!("_{hex}")),
            "({server:?}, {tool:?}) => {got:?} should end with _{hex}"
        );
    }
}

#[test]
fn suffix_never_pushes_past_the_limit() {
    // Truncation and suffixing compose rather than race: a name just under
    // the limit still fits once it is tagged.
    for len in 1..80usize {
        // The trailing `/` forces lossiness at every length.
        let tool = format!("{}/", "t".repeat(len));
        let got = sanitize("srv", &tool);
        assert_well_formed(&got);
        let hex = expected_hex("srv", &tool, HASH_HEX_LEN);
        assert!(got.ends_with(&format!("_{hex}")), "len {len}: {got:?}");
    }
    // Every width the proxy's ladder can reach also fits, including the
    // widest, where the suffix fills the name and the body is squeezed out.
    let long = "t".repeat(200);
    for width in 1..=MAX_HASH_HEX_LEN {
        let got = suffixed("srv", &long, width);
        assert_well_formed(&got);
        assert_eq!(got.len(), MAX_NAME_LEN, "width {width}: {got:?}");
        assert!(got.ends_with(&expected_hex("srv", &long, width)));
    }
}

#[test]
fn widening_moves_a_name_whether_or_not_it_needed_a_suffix() {
    // Both halves of what the proxy's ladder relies on. First: the same pair
    // renders differently at every width, so re-deriving a taken name
    // actually moves it.
    let mut seen = Vec::new();
    for width in 1..=MAX_HASH_HEX_LEN {
        let got = suffixed("fs", "read/file", width);
        assert!(!seen.contains(&got), "width {width} repeated {got:?}");
        seen.push(got);
    }

    // Second: `suffixed` suffixes a *faithful* pair too. `sanitize` returns
    // that one unchanged at every width, so a ladder built on `sanitize`
    // would hand back the name it was asked to move off -- which is how a
    // tool named `read_file_<hex>` colliding with `read/file` lost its place
    // in the listing.
    assert_eq!(sanitize("fs", "read_file"), "fs__read_file");
    assert_eq!(sanitize_at("fs", "read_file", 32), "fs__read_file");
    let forced = suffixed("fs", "read_file", HASH_HEX_LEN);
    assert_ne!(forced, "fs__read_file");
    assert!(forced.starts_with("fs__read_file_"), "got {forced:?}");
    assert_well_formed(&forced);
}

#[test]
fn long_inputs_with_a_shared_prefix_stay_distinct() {
    // Truncation throws away the tail, so the suffix is the only thing left
    // to tell these apart.
    let prefix = "p".repeat(100);
    let a = sanitize("srv", &format!("{prefix}_aaaa"));
    let b = sanitize("srv", &format!("{prefix}_bbbb"));
    assert_ne!(a, b);
    assert_eq!(a.len(), MAX_NAME_LEN);
    assert_eq!(b.len(), MAX_NAME_LEN);
    let body = MAX_NAME_LEN - (1 + HASH_HEX_LEN);
    assert_eq!(a[..body], b[..body], "the shared prefix should survive");
    assert_ne!(a[body..], b[body..], "the suffixes must differ");
}

#[test]
fn reserved_prefix_is_not_reachable_from_another_server() {
    // The built-in tools reach the model through this same path. They are all
    // faithful, so they are returned byte-identical and no suffix appears in
    // a name a model has been told to call. Kept in step with
    // `outrig-cli`'s `builtin_tool::name_of` and `self_tool::name`.
    let builtins = [
        "subagent",
        "subagent_send",
        "subagent_release",
        "wait_results",
        "get_result",
        "set_result",
        "list_docs",
        "get_doc",
        "validate_config",
    ];
    for tool in builtins {
        assert_eq!(
            sanitize(RESERVED_SERVER, tool),
            format!("{RESERVED_SERVER}__{tool}")
        );
    }

    // Nothing else can produce one. A faithful name decodes uniquely to its
    // pair, so reaching `outrig__<tool>` needs the server to *be* `outrig`,
    // which config validation rejects; every near miss lands elsewhere.
    let reserved: Vec<String> = builtins
        .iter()
        .map(|t| sanitize(RESERVED_SERVER, t))
        .collect();
    for tool in builtins {
        for server in [
            "outrig_", "outrig__", "outri", "outrigg", "outrig-", "outrig/", "outrig ", "OUTRIG",
            "0utrig",
        ] {
            for candidate in [
                tool.to_string(),
                format!("_{tool}"),
                format!("{tool}_"),
                format!("/{tool}"),
                format!("g__{tool}"),
            ] {
                let got = sanitize(server, &candidate);
                assert!(
                    !reserved.contains(&got),
                    "({server:?}, {candidate:?}) shadowed built-in {got:?}"
                );
            }
        }
    }
}
