# A failed `tools/call` does not say which server failed

> **Cheaper before 0.2.0 than after.** This is a public break on `OutrigError`, so it is free
> while the freeze window is open and costs a major version once `0002-54` ships. Filed rather
> than folded into `0002-47` because `0002-47` settled the *payload* of the MCP error variants
> and this is their *shape*; doing both under one verdict would have made the verdict harder to
> read. If the window is still open when this is groomed, it belongs in it.

## Context

The two MCP request failures are asymmetric:

```rust
McpToolsListFailed { name: String, source: McpSessionError },
McpService(McpSessionError),
```

`McpClient::call_tool` has `self.name` in hand at the failure site (`crates/outrig/src/mcp.rs`)
and drops it. `list_tools`, two methods up, keeps it.

The gap is visible, not theoretical: `ProxyServer::dispatch_call`
(`crates/outrig/src/mcp_proxy.rs:413-427`) reassembles the missing context by hand when it
renders the failure into a tool result --

```rust
CallToolResult::error(vec![ContentBlock::text(format!(
    "outrig: backing server `{server}` call failed: {e}"
))])
```

-- because `{e}` alone reads "mcp service: Transport closed" with nothing to act on. Every other
caller propagates the error upward unchanged (`outrig_.rs`, `outrig-cli/src/rig_tool.rs`,
`outrig-cli/src/image_setup/build.rs`), so outside the proxy's one rendering the server is simply
not recoverable from the error at all.

## Sketch

Give the variant the field its sibling has, and the name that goes with it:

```rust
#[error("mcp server {name:?} tools/call failed: {source}")]
#[non_exhaustive]
McpCallToolFailed {
    name: String,
    #[source]
    source: McpSessionError,
},
```

One construction site changes. `dispatch_call`'s hand-prepended prefix becomes redundant and
should go with it, so the same fact stops being stated twice in one string.

## Notes

`McpService` is also rmcp's vocabulary rather than outrig's -- "service" is what
`rmcp::service::RunningService` is called, and after `0002-47` the payload is outrig's own
`McpSessionError`. Renaming is the same break as adding the field, so the two travel together.

Related: `plan/done/phase/0002-sidecars/tasks/0002-47-narrow-or-freeze-the-low-level-surfaces.md`,
decision 9.
