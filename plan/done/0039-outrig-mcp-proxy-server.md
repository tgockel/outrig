# 0039 -- Land `ProxyServer`

## Goal

A new module `src/mcp_proxy.rs` that implements `rmcp::ServerHandler` over a pool
of `Arc<McpClient>`s -- exposing the union of every backing server's tools to a
single MCP client, namespaced with `tool_name::sanitize` (0037). No CLI surface
yet; 0040 wires it up.

## Deliverables

- `src/mcp_proxy.rs`:
  ```rust
  pub struct ProxyServer { inner: Arc<ProxyInner> }

  struct ProxyInner {
      clients: Vec<Arc<McpClient>>,
      tools:   Vec<ToolEntry>,
      by_public_name: HashMap<String, usize>,
      server_info: ServerInfo,
  }

  struct ToolEntry {
      public_name: String,                          // e.g. fs__read_file
      backend_tool: String,                         // upstream name
      description: String,
      input_schema: Arc<serde_json::Map<String, Value>>,
      client_idx: usize,
  }

  impl ProxyServer {
      pub async fn build(clients: Vec<Arc<McpClient>>) -> Result<Self> { ... }
      pub fn iter_public_names(&self) -> impl Iterator<Item = &str> { ... }
  }

  impl ServerHandler for ProxyServer {
      fn get_info(&self) -> ServerInfo { self.inner.server_info.clone() }

      async fn list_tools(&self, _: PaginatedRequestParam, _: RequestContext<RoleServer>)
          -> Result<ListToolsResult, rmcp::Error> { ... }

      async fn call_tool(&self, req: CallToolRequestParam, _: RequestContext<RoleServer>)
          -> Result<CallToolResult, rmcp::Error> {
          // Lookup public_name -> entry; map None -> CallToolResult { is_error: true }.
          // Forward to clients[entry.client_idx].call_tool(entry.backend_tool, args).
          // Backing-client Err -> CallToolResult { is_error: true } with text body.
      }
  }
  ```
- Use the dynamic-handler approach (override `list_tools` / `call_tool`); do **not**
  use the `#[tool]` macros. The macros register tools at compile time from typed
  Rust functions, but the proxy's tool set is unknown until runtime. Trait defaults
  for `list_resources` / `list_prompts` (empty / `method_not_found`) are correct
  for v0.
- A startup-time assertion in `ProxyServer::build` that catches public-name
  collisions across servers; if `tool_name::sanitize`'s blake3 suffix scheme ever
  regresses, fail loudly rather than silently routing to the wrong backend.
- `src/lib.rs` -- add `mod mcp_proxy;` (or `pub mod` per the eventual decision in
  0042).
- `tests/mcp_proxy_dispatch.rs` -- unit test using a small `BackingClient` trait
  (or test double) so the proxy can be exercised against in-process fakes without
  spinning up real MCP children:
  - `tools/list` returns the union of two fake servers' tools, namespaced.
  - `tools/call <server>__<name>` routes to the right fake and returns its output.
  - Backing-client `Err` surfaces as `CallToolResult { is_error: true, ... }` with
    the error text.
  - Unknown tool name -> `CallToolResult { is_error: true }` (not an rmcp protocol
    error).

## Acceptance

- `cargo test mcp_proxy_dispatch` passes.
- `cargo build` clean.
- `cargo doc --no-deps` clean for the new module.
- `clippy` clean.
- The proxy's `list_tools` returns deterministic ordering (config-map iteration
  order from `connect_mcp_clients`).

## Dependencies

- 0037-outrig-mcp-tool-name-extract
- 0038-outrig-mcp-rmcp-features

## Notes

- Tool descriptions and `input_schema` pass through unchanged from each backend.
  The proxy is a thin namespace + dispatch layer.
- This is the largest of the seven outrig-mcp phases. Set aside time for the
  `BackingClient`-trait test scaffolding -- that's where most of the surface area
  lives.
- `ServerInfo` defaults: `name = "outrig"`, `version = env!("CARGO_PKG_VERSION")`,
  one-line `instructions` mentioning the `<server>__<tool>` namespace prefix scheme
  so an LLM client sees it. Final wording can land in 0040 alongside the banner.

## Decisions

- **`ProxyServer<C>` is generic, not `dyn`-based.** Edition 2024 lets the
  `BackingClient` trait use plain `impl Future` returns, which prevents
  `dyn` trait objects -- so the proxy is parameterized by client type
  instead. The default `C = Arc<McpClient>` keeps the production
  `ProxyServer::build(arcs)` call site identical to the deliverable's
  literal `Vec<Arc<McpClient>>` signature; the test substitutes
  `Arc<FakeClient>` via the blanket `impl<T> BackingClient for Arc<T>`.

- **`BackingClient` is `pub`.** The integration test in
  `tests/mcp_proxy_dispatch.rs` is the only out-of-crate consumer for now;
  gating to `pub(crate)` would force the test inline. `0042` revisits
  library-surface visibility for the whole crate at once.

- **Dispatch extracted to `list_tools_inner` / `dispatch_call`.** The
  `ServerHandler::list_tools` and `call_tool` methods take a
  `RequestContext<RoleServer>` (holding `Peer<R>`, `RequestId`,
  `CancellationToken`) which is impractical to fabricate in a unit test.
  The trait impls are now thin wrappers over the inner methods, which the
  integration test drives directly.

- **Duplicate `client.name()` is rejected up front in `build`.** Clearer
  failure than letting every sanitized tool name collide -- one error,
  one location, identifies the bad pair.

- **Reused `crate::mcp::kind_of`** for the "input_schema is not an object"
  error message; promoted from private to `pub` so the proxy doesn't
  reimplement the same Value-variant match.
