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
  `log_dir` here is the per-session `logs/` subdir; the caller (run subcommand, task 0019)
  passes it in.

## Decisions

- **Spawn the child ourselves rather than `rmcp::transport::TokioChildProcess::new`.** The
  deliverable bullet said to hand the `Command` to `TokioChildProcess::new`, but that helper
  spawns the child internally and returns a transport wrapper -- it never surfaces a `Child`
  handle. The `shutdown` deliverable ("close stdin, wait grace, then kill") needs that handle.
  Workaround: spawn ourselves with `cmd.spawn()`, take `child.stdin`/`child.stdout`, and hand
  the `(stdout, stdin)` pair to `serve_client` via the blanket `IntoTransport for (R, W)` impl
  in `rmcp::transport::io`. `McpClient` owns the `Child` directly; `shutdown` does
  `service.cancel().await` (closes the sink, hence the child's stdin) then
  `tokio::time::timeout(SHUTDOWN_GRACE, child.wait())` with `start_kill` fallback. This is the
  Notes-section "If rmcp's public API doesn't fit cleanly, isolate the breakage inside `mcp.rs`"
  escape valve.
- **Refactored `Container::build_exec_argv` out of `Container::exec_stdio`.** `exec_stdio`
  built the `podman exec -i --user --env HOME ...` argv inline before handing to
  `process::spawn_stdio`. MCP needs the same argv but with stderr redirected to a file
  (not piped) and the `Child` retained -- neither fits `spawn_stdio`. Pulled the argv
  construction into `build_exec_argv` returning a `process::Cmd`; `exec_stdio` is now a
  one-line wrapper around it. `Cmd::to_tokio_command()` was promoted from a private free
  function to a public method so `mcp.rs` can layer stderr-to-file on the result.
- **`McpServerSpec::normalize` switched from `self` to `&self`.** Every existing caller did
  `spec.clone().normalize()` to work around `normalize`'s consume-by-value signature. Same
  total clone cost; cleaner ergonomics. Touched two pre-existing call sites in
  `tests/config_schema.rs` to match.
- **`OutrigError::McpService(#[from] rmcp::service::ServiceError)` rather than flattening.**
  `ServiceError` already discriminates `McpError` / `Transport(io::Error)` /
  `UnexpectedResponse` / `Cancelled` / `Timeout` -- enough detail at the top level. A
  separate `McpArgsNotObject { kind: &'static str }` variant covers the call_tool argument
  shape mistake (which is a programmer error, not a service error).
- **`call_tool` flattens content into a single `String` directly.** The first cut built a
  `Vec<String>` and called `parts.join("\n")`. Switched to `String::push_str`/`write!` after
  the simplify pass since this is the per-tool-invocation hot path -- saves N+1 allocations
  per call (per-segment `clone` + the join's reallocation).
- **`kind_of(&Value) -> &'static str` helper instead of a 5-arm match at the call site.**
  `call_tool`'s argument validation matches `Value::Object` and `Value::Null` first, then a
  catch-all `other` arm that calls `kind_of` to label the error. Cleaner than enumerating
  every non-object variant inline.
- **Test fixture is a buildah-managed image, not a one-shot Dockerfile**. `tests/mcp_handshake.rs`
  reuses `outrig::image::ensure_image` with `tests/fixtures/mcp-fs/Dockerfile`
  (alpine + nodejs/npm/shadow + `npm install -g @modelcontextprotocol/server-filesystem`).
  The buildah content cache makes re-runs near-instant -- same pattern as
  `tests/image_build_smoke.rs`.
- **`init_tracing` duplication left as `plan/next/dedup-init-tracing.md`.** The same six-line
  helper now exists in four e2e test files (runtime_user, container_lifecycle,
  image_build_smoke, mcp_handshake). Consolidating into `tests/common/mod.rs` is a real
  cleanup but touches three files outside this task's scope; deferred to the buffer.
