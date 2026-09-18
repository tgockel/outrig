# A non-object `input_schema` still costs the session every tool

## Context

`0002-44` stopped a tool-name clash from failing `ProxyServer::build`: the losing name is
widened and the rest of the listing survives. The schema check two statements later still has
the old blast radius (`crates/outrig/src/mcp_proxy.rs`, the `input_schema` match in
`build_with_width`):

```rust
let input_schema = match &tool.input_schema {
    Value::Object(map) => Arc::new(map.clone()),
    other => {
        return Err(OutrigError::Configuration(format!(
            "mcp_proxy: tool {server_name:?}::{tool_name:?} input_schema is \
             not a JSON object (got {kind})", ...
        )));
    }
};
```

One upstream tool advertising `"inputSchema": []` takes down every other server's tools with
it. That is the exact shape `0002-44` argued was unacceptable for names, left standing for
schemas -- not by decision, just because the name path was the one under review. The behavior
is unchanged from before `0002-44`; what changed is that the file now contains both policies
side by side and the inconsistency is visible.

## Goal

A tool outrig cannot represent costs that tool, not the listing.

## Deliverables

- **Skip and log, the way the clash path does.** A `tracing::error!` naming the server, the
  tool, and the offending JSON kind, then `continue`. The tool is not advertised; everything
  else is.
- **Decide whether `build` keeps the error variant at all.** With both per-tool failures
  demoted to logs, the only remaining `OutrigError::Configuration` from `build` is the
  duplicate backing-client name, which is a caller error rather than an upstream one. Say so
  in the doc comment rather than leaving a stale `Errors:` list.
- **Check the `outrig run` path for the same shape.** `McpToolAdapter::from_client_tools`
  carries `input_schema` as an opaque `Value` and never validates it, so the two halves
  disagree about what a malformed schema costs. Whatever is decided here should be the answer
  for both; this overlaps `plan/next/run-path-has-no-tool-name-guard.md` and the two may want
  to land together.

## Acceptance

- A listing where one tool's `input_schema` is not a JSON object advertises every other tool,
  and the diagnostic names the server, the tool, and the kind that arrived.
- A listing where *every* tool is malformed yields an empty tool list rather than an error.
- The startup banner's per-server count matches what was actually advertised.

## Dependencies

- None. `0002-44` (landed) is where the inconsistency comes from.
