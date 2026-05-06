//! MCP server fronting a pool of [`McpClient`]s.
//!
//! [`ProxyServer`] implements [`rmcp::ServerHandler`] over `Vec<C>` where
//! each `C` is a [`BackingClient`] -- in production, `Arc<McpClient>`. The
//! union of every backing server's tools is exposed as a single namespaced
//! surface (`<server>__<tool>`, via [`crate::tool_name::sanitize`]) so an
//! external MCP client sees one server with many tools instead of *N* servers
//! with overlapping names.
//!
//! The dynamic-handler form (override `list_tools` / `call_tool`) is used
//! rather than the `#[tool]` macros, because the tool set is unknown until
//! runtime.
//!
//! [`McpClient`]: crate::mcp::McpClient

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::Error as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParam, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;

use crate::error::{OutrigError, Result};
use crate::mcp::{self, McpClient, McpTool, McpToolResult};
use crate::tool_name;

/// Abstraction over the MCP client surface the proxy actually depends on:
/// a name, a `tools/list`, and a `tools/call`. [`McpClient`] is the
/// production impl; the integration test in `tests/mcp_proxy_dispatch.rs`
/// supplies an in-process fake.
///
/// The blanket `impl<T> BackingClient for Arc<T>` lets the proxy work with
/// `Vec<Arc<McpClient>>` directly -- no manual upcast at the call site.
///
/// [`McpClient`]: crate::mcp::McpClient
pub trait BackingClient: Send + Sync + 'static {
    /// The local config name of this server (the prefix half of
    /// `<server>__<tool>`).
    fn name(&self) -> &str;

    /// Mirror of [`McpClient::list_tools`](crate::mcp::McpClient::list_tools).
    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>>> + Send;

    /// Mirror of [`McpClient::call_tool`](crate::mcp::McpClient::call_tool).
    fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> impl Future<Output = Result<McpToolResult>> + Send;
}

impl BackingClient for McpClient {
    fn name(&self) -> &str {
        McpClient::name(self)
    }

    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>>> + Send {
        McpClient::list_tools(self)
    }

    fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> impl Future<Output = Result<McpToolResult>> + Send {
        McpClient::call_tool(self, name, args)
    }
}

impl<T> BackingClient for Arc<T>
where
    T: BackingClient + ?Sized,
{
    fn name(&self) -> &str {
        (**self).name()
    }

    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>>> + Send {
        (**self).list_tools()
    }

    fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> impl Future<Output = Result<McpToolResult>> + Send {
        (**self).call_tool(name, args)
    }
}

/// One entry in the proxy's flattened tool table. Each entry remembers both
/// the public namespaced name (what the LLM-side MCP client sees) and the
/// upstream name (what we send back through the `client_idx`'th backing
/// client when the LLM calls it).
#[derive(Debug, Clone)]
struct ToolEntry {
    public_name: String,
    backend_tool: String,
    description: String,
    input_schema: Arc<JsonObject>,
    client_idx: usize,
}

struct ProxyInner<C> {
    clients: Vec<C>,
    tools: Vec<ToolEntry>,
    by_public_name: HashMap<String, usize>,
    server_info: ServerInfo,
}

/// MCP server fronting a pool of backing clients. Generic over the client
/// type so tests can drive the same dispatch path with in-process fakes;
/// production uses the default `C = Arc<`[`McpClient`]`>`.
///
/// `Clone` is hand-rolled (no `where C: Clone` bound) so [`ServerHandler`]'s
/// `Self: Clone` requirement is satisfied for any `C`.
///
/// [`McpClient`]: crate::mcp::McpClient
pub struct ProxyServer<C = Arc<McpClient>> {
    inner: Arc<ProxyInner<C>>,
}

impl<C> Clone for ProxyServer<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<C: BackingClient> ProxyServer<C> {
    /// Connect every backing client's `tools/list` into a single namespaced
    /// surface. Order across clients matches the input `Vec`; order within a
    /// single client matches that client's `tools/list` response.
    ///
    /// Errors:
    /// - [`OutrigError::Configuration`] if two clients share a `name()`
    ///   (every tool would collide).
    /// - [`OutrigError::Configuration`] if two `(server, tool)` pairs
    ///   sanitize to the same public name -- shouldn't happen with the
    ///   blake3-suffix scheme, but failing loudly beats silently routing to
    ///   the wrong backend.
    /// - Any error from a backing client's `list_tools` propagates.
    pub async fn build(clients: Vec<C>) -> Result<Self> {
        let mut seen_names: HashMap<&str, usize> = HashMap::with_capacity(clients.len());
        for (idx, client) in clients.iter().enumerate() {
            let name = client.name();
            if let Some(prev) = seen_names.insert(name, idx) {
                return Err(OutrigError::Configuration(format!(
                    "mcp_proxy: duplicate backing-client name {name:?} \
                     (clients[{prev}] and clients[{idx}])"
                )));
            }
        }

        let mut tools: Vec<ToolEntry> = Vec::new();
        let mut by_public_name: HashMap<String, usize> = HashMap::new();

        for (client_idx, client) in clients.iter().enumerate() {
            let server_name = client.name().to_string();
            let upstream = client.list_tools().await?;
            for tool in upstream {
                let public_name = tool_name::sanitize(&server_name, &tool.name);

                if let Some(prev_idx) = by_public_name.get(&public_name) {
                    let prev = &tools[*prev_idx];
                    let prev_server = clients[prev.client_idx].name();
                    return Err(OutrigError::Configuration(format!(
                        "mcp_proxy: public name {public_name:?} produced by both \
                         ({prev_server:?}, {prev_tool:?}) and ({server_name:?}, {tool_name:?})",
                        prev_tool = prev.backend_tool,
                        tool_name = tool.name,
                    )));
                }

                let input_schema = match tool.input_schema {
                    Value::Object(map) => Arc::new(map),
                    other => {
                        return Err(OutrigError::Configuration(format!(
                            "mcp_proxy: tool {server_name:?}::{tool_name:?} input_schema is \
                             not a JSON object (got {kind})",
                            tool_name = tool.name,
                            kind = mcp::kind_of(&other),
                        )));
                    }
                };

                by_public_name.insert(public_name.clone(), tools.len());
                tools.push(ToolEntry {
                    public_name,
                    backend_tool: tool.name,
                    description: tool.description.unwrap_or_default(),
                    input_schema,
                    client_idx,
                });
            }
        }

        let server_info = ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "outrig".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            instructions: Some(
                "Tools are namespaced as <server>__<tool>; the prefix identifies which \
                 backing MCP server hosts the tool."
                    .to_string(),
            ),
        };

        Ok(Self {
            inner: Arc::new(ProxyInner {
                clients,
                tools,
                by_public_name,
                server_info,
            }),
        })
    }

    /// Iterate the public (namespaced) names this proxy exposes, in the
    /// order they were registered. `0040` consumes this for the startup
    /// banner.
    pub fn iter_public_names(&self) -> impl Iterator<Item = &str> {
        self.inner.tools.iter().map(|t| t.public_name.as_str())
    }

    /// Build a `tools/list` response: every backing server's tools, in
    /// registration order, namespaced via [`tool_name::sanitize`]. Exposed
    /// (rather than living inline in [`ServerHandler::list_tools`]) so the
    /// dispatch can be exercised in `tests/mcp_proxy_dispatch.rs` without
    /// fabricating an rmcp [`RequestContext`].
    pub fn list_tools_inner(&self) -> ListToolsResult {
        let tools = self
            .inner
            .tools
            .iter()
            .map(|entry| Tool {
                name: entry.public_name.clone().into(),
                description: entry.description.clone().into(),
                input_schema: entry.input_schema.clone(),
            })
            .collect();
        ListToolsResult {
            next_cursor: None,
            tools,
        }
    }

    /// Dispatch a `tools/call` to the appropriate backing client. Returns a
    /// [`CallToolResult`] in every case -- unknown tool names and
    /// backing-client errors surface as `is_error: Some(true)` results, not
    /// rmcp protocol errors. Exposed for the same testability reason as
    /// [`Self::list_tools_inner`].
    pub async fn dispatch_call(&self, request: CallToolRequestParam) -> CallToolResult {
        let public_name = request.name.as_ref();
        let Some(&idx) = self.inner.by_public_name.get(public_name) else {
            return CallToolResult::error(vec![Content::text(format!(
                "unknown tool: {public_name}"
            ))]);
        };
        let entry = &self.inner.tools[idx];
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        match self.inner.clients[entry.client_idx]
            .call_tool(&entry.backend_tool, args)
            .await
        {
            Ok(result) => CallToolResult {
                content: vec![Content::text(result.content_text)],
                is_error: Some(result.is_error),
            },
            Err(e) => CallToolResult::error(vec![Content::text(e.to_string())]),
        }
    }
}

impl<C: BackingClient> ServerHandler for ProxyServer<C> {
    fn get_info(&self) -> ServerInfo {
        self.inner.server_info.clone()
    }

    async fn list_tools(
        &self,
        _request: PaginatedRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(self.list_tools_inner())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(self.dispatch_call(request).await)
    }
}
