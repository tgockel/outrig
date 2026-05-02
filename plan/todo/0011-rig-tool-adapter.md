# 0011 -- Rig tool adapter

## Goal

Expose every MCP-discovered tool to Rig as a dynamic tool. The agent loop is then provided by
Rig's `AgentBuilder.dynamic_tools(...)` (or equivalent in the locked rig-core version);
outrig's job is the adapter glue + name sanitization.

## Deliverables

- `src/rig_tool.rs::McpToolAdapter`:
  ```rust
  pub struct McpToolAdapter {
      pub openai_name: String,    // "<server>__<tool>", sanitized
      pub mcp_tool_name: String,  // original name from tools/list
      pub description: String,
      pub input_schema: serde_json::Value,
      pub client: Arc<McpClient>,
  }
  ```
  Implements Rig's dynamic-tool trait (verify the trait name in the locked rig-core
  version; likely `rig::tool::ToolDyn` or similar).
- `src/rig_tool.rs::sanitize(server: &str, tool: &str) -> String`:
  - Concatenates as `<server>__<tool>` (double underscore).
  - Replaces every char not matching `[a-zA-Z0-9_-]` with `_`.
  - Truncates to 64 chars; if truncated, replace the last 7 chars with `_<6-hex>` derived
    from blake3 of the original name (stable).
- Tool dispatch in the trait impl:
  - Parse args from the JSON the LLM provides.
  - Call `self.client.call_tool(&self.mcp_tool_name, args).await`.
  - On `is_error == true` -> return Err so Rig surfaces it to the model as an error.
  - Else return `Ok(content_text)`.
- `tests/rig_tool.rs` (pure-Rust unit, no rig-core required):
  - `sanitize("fs", "read_file") == "fs__read_file"`.
  - Non-alphanumeric chars in tool name -> replaced with `_`.
  - Long names truncate with stable 6-hex suffix; same input always produces same output.
  - Different inputs that would collide before truncation produce different suffixes.
- `tests/rig_tool_dispatch.rs` (`#[cfg(feature = "e2e")]`):
  - Build an `Agent` with one `McpToolAdapter` over the filesystem MCP.
  - Issue a prompt that triggers a tool call.
  - Verify the tool call landed inside the container (file read returns expected content).

## Acceptance

- `cargo test rig_tool` passes.
- `cargo test --features e2e rig_tool_dispatch` passes.
- Drop the `> TODO: Incomplete` marker on `doc/concepts/mcp-servers.md`.

## Dependencies

- 0010-mcp-client

## Notes

- rig-core is also a moving target. Same advice as 0010: implementer verifies the trait name
  + signature against the locked version; isolate breakage inside `rig_tool.rs`.
- The 64-char limit comes from OpenAI's tool-name regex constraints; other providers may be
  more liberal but the limit is a safe lowest common denominator.
- A `ToolRegistry { routes: BTreeMap<String, McpToolAdapter> }` may be useful as a facade,
  but the adapter itself is the only object Rig actually needs.
