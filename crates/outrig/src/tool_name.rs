//! Build the LLM-facing `<server>__<tool>` name an MCP tool is exposed under.
//!
//! [`sanitize`] enforces OpenAI's `^[a-zA-Z0-9_-]{1,64}$` constraint on tool
//! names. A pair whose composition already satisfies it *faithfully* is
//! returned byte-identical; every other pair is truncated to fit and tagged
//! with a stable `_<hex>` suffix over a length-delimited blake3 of the pair.
//!
//! "Faithfully" is the whole rule, and it has three parts: the composition is
//! already in the character set, it fits, and its first `__` is the separator.
//! The third is what makes the encoding *decodable* -- cutting a faithful name
//! at its first `__` recovers `(server, tool)` exactly -- and therefore
//! injective, so two distinct faithful names can never collide. `("a", "_b")`
//! composes to `a___b` whose first `__` starts at index 1, which is where
//! `"a"` ends, so it is faithful; `("a_", "b")` composes to the same string
//! but ends one byte later, so it is lossy and carries a suffix.
//!
//! What the suffix buys is collision *resistance*, not collision freedom. The
//! input is an unbounded pair of Unicode strings and the output is 64
//! characters over a 64-symbol alphabet, so no injection exists and no suffix
//! width creates one. Two lossy names collide only through a blake3 collision;
//! a lossy name can also land on a faithful one, which needs no collision at
//! all -- a tool literally named `read_file_5d2270` reaches the same name as
//! `read/file` does. [`mcp_proxy`](crate::mcp_proxy) handles either by
//! re-deriving the later name through [`suffixed`] at a wider width, which is
//! why that entry point exists.
//!
//! Both `outrig run` (through the companion `outrig-cli` crate) and the
//! [`mcp_proxy`](crate::mcp_proxy) server share this so they advertise
//! identical public names for the same upstream tool.

/// Server name reserved for OutRig's own built-in tools, which reach the model
/// as `outrig__<tool>` through the same [`sanitize`] path as MCP tools. A
/// configured MCP server may not claim it, or its tools would collide with the
/// built-ins.
pub const RESERVED_SERVER: &str = "outrig";

/// Maximum length OpenAI accepts for a tool name. Other providers are more
/// liberal but this is the safe lower bound.
const MAX_NAME_LEN: usize = 64;

/// Width of the hex a suffix carries. The full suffix is `_` plus this many
/// hex characters.
pub(crate) const HASH_HEX_LEN: usize = 6;

/// Widest hex a suffix can carry: `_` plus this many characters is exactly
/// [`MAX_NAME_LEN`], leaving no room for a body. [`crate::mcp_proxy`] widens
/// up to here when two tools land on one name.
pub(crate) const MAX_HASH_HEX_LEN: usize = MAX_NAME_LEN - 1;

/// Build the LLM-facing name for `(server, tool)`, returning the composition
/// unchanged when it faithfully encodes the pair and otherwise truncating it
/// to fit and appending a stable blake3 suffix.
pub fn sanitize(server: &str, tool: &str) -> String {
    sanitize_at(server, tool, HASH_HEX_LEN)
}

/// [`sanitize`] with the suffix width spelled out, so a test can drive a
/// digest narrow enough to collide on purpose.
pub(crate) fn sanitize_at(server: &str, tool: &str, hex_len: usize) -> String {
    let composed = format!("{server}__{tool}");
    if composed.len() <= MAX_NAME_LEN
        && composed.find("__") == Some(server.len())
        && composed.chars().all(in_charset)
    {
        return composed;
    }
    suffixed(server, tool, hex_len)
}

/// The suffixed form, whether or not the pair needed one. [`crate::mcp_proxy`]
/// calls this rather than [`sanitize_at`] when re-deriving a name that is
/// already taken: a faithful pair ignores `hex_len`, so widening through
/// `sanitize_at` would hand back the same name it was asked to move off.
pub(crate) fn suffixed(server: &str, tool: &str, hex_len: usize) -> String {
    let hex_len = hex_len.clamp(1, MAX_HASH_HEX_LEN);

    // The preimage is the *pair*, each side length-prefixed, rather than a
    // concatenation whose separator can appear on either side: `("a", "_b")`
    // and `("a_", "b")` share a concatenation but not a preimage.
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(server.len() as u64).to_le_bytes());
    hasher.update(server.as_bytes());
    hasher.update(&(tool.len() as u64).to_le_bytes());
    hasher.update(tool.as_bytes());
    let hex = hasher.finalize().to_hex();

    // Every replaced character becomes one ASCII `_`, so the body is pure
    // ASCII however exotic the input was and truncating it by byte is
    // char-safe. The truncation is a no-op when the body already leaves room,
    // which is how it composes with the suffix rather than racing it.
    let mut name: String = format!("{server}__{tool}")
        .chars()
        .map(|c| if in_charset(c) { c } else { '_' })
        .collect();
    name.truncate(MAX_NAME_LEN - 1 - hex_len);
    name.push('_');
    name.push_str(&hex.as_str()[..hex_len]);
    name
}

fn in_charset(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
#[path = "tool_name_tests.rs"]
mod tool_name_tests;
