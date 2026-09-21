//! The one rule `crates/outrig/public-api.txt` has to keep.
//!
//! 0002-43 settled where the MCP SDK stops being an implementation detail: an
//! SDK type is public only where the item exists to participate in the SDK's
//! own machinery. After 0002-47 applied that to the error surface, the rule
//! has a mechanical form -- every `rmcp::` in the snapshot is under
//! `outrig::mcp_proxy`, and nowhere else.
//!
//! This is not the snapshot gate in `scripts/check-public-api.py`, and the two
//! are not symmetric. That one says "the file is current"; this one says "the
//! file obeys the boundary". The second used to be worth only what the first
//! was, because nothing verified the snapshot's currency and a surface change
//! that was never regenerated left this green. This repo's CI now regenerates
//! and fails on any difference, so here the committed file is fresh data. In a
//! vendored or unpacked copy it is only as fresh as the release it shipped
//! with, which is the right thing for it to be and is why both it and this
//! test are `exclude`d from the package.
//!
//! What it catches that a byte-exact diff does not is a *deliberate*
//! regeneration carrying an SDK type back into `outrig::error` -- the diff
//! would accept that as the new truth.
//!
//! The rule stays here rather than moving into the generator. Reading the
//! committed file costs neither nightly nor `cargo-public-api`, so it runs on
//! every `cargo test` rather than only where the pinned toolchain is
//! available; restating it in Python would create the second copy it exists to
//! prevent.

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
