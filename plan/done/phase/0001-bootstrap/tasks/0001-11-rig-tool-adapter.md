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
  - Build adapters with `McpToolAdapter::from_client_tools` over the filesystem MCP.
  - Call `<McpToolAdapter as ToolDyn>::call(adapter, args_json_string).await` directly.
  - Verify the tool call landed inside the container (file read returns expected content).
  - Cover the error path (missing file -> `ToolError::ToolCallError`).
  - **Why no `Agent`:** the LLM stack lands in tasks 0012-0016. `ToolDyn::call`
    is the same code path an `Agent` invokes, just without the LLM in front of it.
    An Agent-shaped test belongs in 0019.

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

## Decisions

- **E2E test exercises `ToolDyn::call` directly, not an `Agent`.** The LLM
  provider stack doesn't exist yet (tasks 0012-0016); spinning up a real
  `Agent` would need a mock provider invented just for this test. Calling
  `ToolDyn::call(adapter, args_string).await` runs every line of the dispatch
  path an `Agent` would. The Agent-shaped variant belongs in 0019 once the
  loop infrastructure exists. Confirmed with the user before execution.
- **Truncation hash is over the pre-sanitization `<server>__<tool>`.** Two
  originals that map to the same sanitized prefix (e.g. one with `/` in the
  middle, one with `_`) get distinct 6-hex suffixes. Confirmed with the user
  before execution.
- **`ToolRegistry` facade skipped.** `Vec<McpToolAdapter>` is enough for the
  caller (`AgentBuilder::dynamic_tools` accepts a list of dyn-tools), and the
  notes-section only flagged it as "may be useful." Adding the facade now
  would be premature; if 0019 finds it useful, it lands then.
- **`McpAdapterError(String)` is the bridge type into `ToolError`.** The
  natural shape -- `ToolError::ToolCallError(Box::new(self_err))` -- needs a
  `'static` `std::error::Error`. We can't directly box `OutrigError` because
  its variants might one day include non-`'static` borrows; flattening to a
  string at the boundary is the cleanest hand-off. Mirrors what rig-core's
  own `tool::rmcp::McpToolError` does.
- **Bug fix in `src/image.rs::hash_git_context`.** While exercising the e2e
  test, both my new `tests/rig_tool_dispatch.rs` and the previously-merged
  `tests/mcp_handshake.rs` were failing with `git hash-object --stdin-paths`
  exit 128. Root cause: `git ls-files -z .` from a subdirectory emits
  cwd-relative paths, but `git hash-object --stdin-paths` resolves them as
  repo-root-relative -- they disagree whenever the build context is a
  subdirectory of the working tree. Fixed by passing `--full-name` to
  `ls-files`, which forces repo-root-relative output. Tiny, contained change;
  unblocks every e2e test that builds a fixture image from a tracked
  subdirectory. Verified `mcp_handshake` regains green.
