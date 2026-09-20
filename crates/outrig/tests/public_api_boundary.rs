//! The one rule `crates/outrig/public-api.txt` has to keep.
//!
//! 0002-43 settled where the MCP SDK stops being an implementation detail: an
//! SDK type is public only where the item exists to participate in the SDK's
//! own machinery. After 0002-47 applied that to the error surface, the rule
//! has a mechanical form -- every `rmcp::` in the snapshot is under
//! `outrig::mcp_proxy`, and nowhere else.
//!
//! This is not the snapshot gate `0002-48` adds, and the two are not
//! symmetric. That one says "the file is current"; this one says "the file
//! obeys the boundary", and the second is only worth what the first is:
//! nothing verifies the snapshot's currency yet, so a surface change that is
//! never regenerated leaves this green. What it does catch, which a byte-exact
//! diff does not, is a *deliberate* regeneration that carries an SDK type back
//! into `outrig::error` -- the diff would accept it as the new truth.
//!
//! It reads the committed file, so it needs neither nightly nor
//! `cargo-public-api`. `0002-48` should fold the assertion into whatever it
//! generates, at which point this becomes the same check against fresh data.

use std::path::Path;

#[test]
fn mcp_sdk_types_reach_the_public_surface_only_through_mcp_proxy() {
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("public-api.txt");
    let text = std::fs::read_to_string(&snapshot)
        .unwrap_or_else(|e| panic!("read {}: {e}", snapshot.display()));

    let sdk_lines: Vec<&str> = text
        .lines()
        // The header is `# ...`; an item line may start with `#[non_exhaustive]`
        // and must not be skipped along with it.
        .filter(|line| !line.starts_with('#') || line.starts_with("#["))
        .filter(|line| line.contains("rmcp::"))
        .collect();

    // Without this, a snapshot truncated to nothing would pass the real check
    // below by saying nothing at all.
    assert!(
        !sdk_lines.is_empty(),
        "the proxy's SDK-typed surface is frozen, so some lines must name `rmcp::`; \
         an empty result means {} is not what it should be",
        snapshot.display()
    );

    let escaped: Vec<&str> = sdk_lines
        .into_iter()
        .filter(|line| !line.contains("outrig::mcp_proxy::"))
        .collect();
    assert!(
        escaped.is_empty(),
        "an MCP SDK type escaped `outrig::mcp_proxy`, so an SDK major would be an \
         outrig major for more than the proxy:\n  {}",
        escaped.join("\n  ")
    );
}
