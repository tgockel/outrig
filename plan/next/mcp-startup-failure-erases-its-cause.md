# A startup failure hands back a string where a session failure hands back a kind

> **Cheaper before 0.2.0 than after**, like
> `plan/next/mcp-service-error-does-not-name-its-server.md`: it retypes a public field, so it
> is free while the freeze window is open and a major version once `0002-54` ships.

## Context

`0002-47` narrowed `OutrigError`'s MCP errors to an outrig-owned `McpSessionError`, carrying an
`McpFailureKind` beside the SDK's wording. Its decision 3 rejected the cheaper option, erasure to
`Box<dyn Error + Send + Sync>`, on the grounds that erasure leaves a caller with text to parse.

`McpStartupFailure::source` (`crates/outrig/src/error.rs`) is exactly that erasure, and `0002-47`
left it standing. It is built by `enrich_startup_error` (`crates/outrig/src/mcp.rs`) from the
SDK's `ClientInitializeError`, so the crate now classifies the *rarer* failure -- a session that
dies mid-request -- and hands back a string for the more common one, a server that will not start.

Two costs:

- A consumer handling "MCP went wrong" writes two shapes of code for one concept.
- The review obligation `plan/next/rmcp-list-result-spec-gaps.md` records covers
  `ServiceError` only. An SDK major that reshapes `ClientInitializeError` changes what a
  downcaster sees, and nothing in the tree records that anyone can be downcasting.

`crates/outrig/tests/public_api_boundary.rs` cannot see this: it matches lines naming `rmcp::`,
and a `Box<dyn Error>` names nothing.

## The argument for leaving it, which is not nothing

`McpStartupFailure` is a struct, not a bare cause: it already carries `command`, `exit`,
`stderr_path`, and `stderr_tail`. For a server that failed to start, *those* are the diagnostic --
a classification of the handshake error underneath adds little that the exit status and the
captured stderr do not already say. That asymmetry is defensible on its merits, which is why
`0002-47` recorded it rather than treating it as an oversight.

What is not defensible is that it is undocumented in the type. Whichever way this goes, the field
should say what it holds.

## Sketch

Either:

1. Type it `McpSessionError` and add a second classifier beside `session_error_from_rmcp` for
   `ClientInitializeError`'s six variants. One more match block, and the crate has one
   representation of "an MCP SDK failure crossed outrig's boundary". The `Box<dyn Error>` goes,
   and so does the downcast nobody wrote down.
2. Or keep the erasure and say so on the field: that it holds the SDK's handshake error, that
   downcasting it is not a supported surface, and that `exit` and `stderr_tail` are what the
   variant exists to carry.

Option 1 is the one `0002-47`'s decision 3 argues for. Option 2 is honest and costs a doc
comment.

Related: `plan/done/phase/0002-sidecars/tasks/0002-47-narrow-or-freeze-the-low-level-surfaces.md`,
decision 1 (the line is named and excluded) and decision 3 (why erasure was rejected elsewhere).
