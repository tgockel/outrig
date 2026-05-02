# 0010 -- MCP client

## Goal

Speak MCP (Model Context Protocol) to a server running inside the container. Use `rmcp` (the
official Rust SDK) over stdio piped from `podman exec -i`. Capture per-server stderr to a log
file under the session dir.

## Deliverables

- `src/mcp.rs` thin facade over `rmcp` with `features = ["client",
  "transport-child-process"]`.
- Public types:
  - `McpClient` (opaque; holds the rmcp peer).
  - `McpTool { name: String, description: Option<String>, input_schema: serde_json::Value }`.
  - `McpToolResult { content_text: String, is_error: bool }`.
- `McpClient::connect_via_podman_exec(container: &Container, server_cfg: &McpServerSpec, name:
  &str, log_dir: &Path) -> Result<Self>`:
  1. Build a `tokio::process::Command` for `podman exec -i --user=<uid>:<gid> --env ...
     <container_name> <argv...>` with stdio piped.
  2. Redirect the child's stderr to `<log_dir>/<name>.stderr` (open the file via
     `tokio::fs::File`, hand to `Command::stderr(Stdio::from(file))`).
  3. Hand the `Command` to `rmcp::transport::child_process::TokioChildProcess::new`.
  4. Call `rmcp::service::serve_client((), transport).await` (or whatever the current rmcp
     API expects -- verify against the locked rmcp version).
  5. Wrap the resulting peer in `McpClient`.
- `McpClient::list_tools(&self) -> Result<Vec<McpTool>>` -- calls rmcp's `tools/list`.
- `McpClient::call_tool(&self, name: &str, args: serde_json::Value) -> Result<McpToolResult>`
  -- calls rmcp's `tools/call`; flattens content blocks (text / json) into a single string.
  Sets `is_error: true` if rmcp reports an error result.
- `McpClient::shutdown(self) -> Result<()>` -- closes stdin, waits for the child up to a small
  grace period, then `kill_on_drop`.
- `tests/mcp_handshake.rs` (`#[cfg(feature = "e2e")]`):
  - Build a fixture image containing
    `npm install -g @modelcontextprotocol/server-filesystem`.
  - Start the container, bootstrap user.
  - `connect_via_podman_exec` to the filesystem MCP server; assert `list_tools()` returns at
    least 3 tools; `call_tool("list_directory", {"path": "/workspace"})` returns a non-error
    result.

## Acceptance

- `cargo test --features e2e mcp_handshake` passes against a real filesystem MCP.
- Server stderr is captured to `<log_dir>/<name>.stderr` even when the server crashes mid-call.
- The hand-rolled JSON-RPC fallback is **not** built; we commit fully to rmcp.

## Dependencies

- 0009-runtime-user-bootstrap

## Notes

- rmcp is a moving target. The first thing the implementer should do: open the locked version's
  docs, verify the trait names and `serve_client` signature. If rmcp's public API doesn't fit
  cleanly, isolate the breakage inside `mcp.rs` -- callers should only see our facade.
- Per-server stderr in `<log_dir>/<name>.stderr` matches `doc/usage/sessions.md`'s layout. The
  `log_dir` here is the per-session `logs/` subdir; the caller (run subcommand, task 0014)
  passes it in.
