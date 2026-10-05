# The per-upgrade rmcp review is prose, so nothing prompts it

## Symptom

Two things need a look on every rmcp upgrade: `SUPPORTED_PROTOCOL_VERSIONS`
(`crates/outrig/src/mcp_proxy.rs`), and `session_error_from_rmcp` (`crates/outrig/src/mcp.rs`),
whose wildcard files a `ServiceError` variant it has never seen under `Other` without a compile
error. [rmcp-list-result-spec-gaps](rmcp-list-result-spec-gaps.md) records both obligations, and
the doc comment on `every_rmcp_service_error_is_classified` names the rmcp version the variant list
was last checked against. Nothing fails when either goes stale. The workspace requirement is a
caret, so a routine `cargo update` can move rmcp without touching a manifest, and nothing asks for
the review. The #191 bump updated the version in the test's doc comment and missed the same
sentence in the note until review caught it.

## Where it goes

A test in the shape of `crates/outrig/tests/public_api_boundary.rs`, which checks a committed file
on every `cargo test`: read the workspace `Cargo.lock`, find the locked `rmcp`, and assert it
equals a `REVIEWED_RMCP` constant kept beside the classifier, with a failure message that names
both reviews. Bumping the constant becomes the record, and the version can leave the doc comment.
The workspace lock is absent from a packaged crate, so the test wants the same `exclude` entry
`public_api_boundary.rs` has.
