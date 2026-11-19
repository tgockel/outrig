//! The rmcp the MCP client was reviewed against is the one the workspace
//! locks.
//!
//! The two classifiers in `src/mcp.rs` -- `session_error_from_rmcp` over
//! `ServiceError` and `init_error_class` over `ClientInitializeError` -- match
//! `#[non_exhaustive]` enums, so a variant an upgrade adds lands in a wildcard
//! without a compile error. This holds the lock to the release they were last
//! read against, so the upgrade cannot land without the review.
//!
//! It reads the workspace's `Cargo.lock`, which a package does not carry: in
//! an unpacked or vendored copy the path is missing, or is a consumer's lock
//! with an rmcp of its own. So it is `exclude`d from the package, as
//! `public_api_boundary.rs` is, and strict in the repo.

use std::path::Path;

/// The rmcp release the classifiers were last reviewed against.
const REVIEWED_RMCP: &str = "3.4.1";

#[test]
fn the_locked_rmcp_is_the_reviewed_one() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock =
        std::fs::read_to_string(&lock).unwrap_or_else(|e| panic!("read {}: {e}", lock.display()));
    let locked: Vec<&str> = lock
        .split("[[package]]")
        .filter(|package| package.contains("\nname = \"rmcp\"\n"))
        .filter_map(|package| {
            package
                .lines()
                .find_map(|line| line.strip_prefix("version = \""))
                .map(|version| version.trim_end_matches('"'))
        })
        .collect();
    assert_eq!(
        locked,
        [REVIEWED_RMCP],
        "rmcp moved from the {REVIEWED_RMCP} the MCP client was reviewed against. Review, then \
         raise REVIEWED_RMCP: `session_error_from_rmcp` against `ServiceError` \
         (every_rmcp_service_error_is_classified) and `init_error_class` against \
         `ClientInitializeError` (every_rmcp_init_error_is_classified) for variants the \
         wildcards would file away; `SUPPORTED_PROTOCOL_VERSIONS` in mcp_proxy.rs against the \
         revisions the release speaks; and plan/next/rmcp-list-result-spec-gaps.md"
    );
}
