//! MCP server fronting a pool of MCP clients.
//!
//! [`ProxyServer`] implements [`rmcp::ServerHandler`] over `Vec<C>` where
//! each `C` is a [`BackingClient`] -- in production, `Arc<McpClient>`. The
//! union of every backing server's tools is exposed as a single namespaced
//! surface (`<server>__<tool>`, through [`crate::sanitize_tool_name`]) so an
//! external MCP client sees one server with many tools instead of *N* servers
//! with overlapping names.
//!
//! The dynamic-handler form (override `list_tools` / `call_tool`) is used
//! rather than the `#[tool]` macros, because the tool set is unknown until
//! runtime.

#![deny(clippy::print_stdout)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListPromptsRequestMethod, ListPromptsResult,
    ListResourceTemplatesRequestMethod, ListResourceTemplatesResult, ListResourcesRequestMethod,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;

use crate::error::{OutrigError, Result};
use crate::mcp::{self, McpClient};
use crate::mcp_content::{McpTool, McpToolResult, result_to_rmcp, tool_to_rmcp};
use crate::tool_name;

/// Protocol revisions OutRig's MCP servers are known to serve correctly.
///
/// Deliberately explicit rather than deferring to rmcp's default of
/// [`ProtocolVersion::KNOWN_VERSIONS`]: that default moves whenever rmcp learns a
/// new revision, so a dependency bump silently widens what OutRig agrees to speak.
/// That is exactly how the servers began negotiating `2026-07-28` -- which requires
/// the SEP-2549 `ttlMs`/`cacheScope` fields on every list result -- without emitting
/// them, leaving clients unable to fetch the tool list at all.
///
/// Adding an entry here asserts that both [`ProxyServer`] and the `outrig mcp self`
/// server meet that revision's requirements. Review it on every rmcp upgrade.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2026_07_28,
];

/// How long a client may treat a `tools/list` response as fresh (SEP-2549).
///
/// The proxy's tool table is frozen at [`ProxyServer::build`] time and the server
/// advertises no `listChanged` capability, so the list genuinely cannot change for
/// the life of the connection -- a far longer TTL would still be honest. Five
/// minutes is chosen instead so that a config edit or a rebuilt image is picked up
/// promptly on the next session, which matters more than cache efficiency for a
/// list this small.
const TOOLS_TTL_MS: u64 = 300_000;

/// Private supertrait bound: nothing outside this crate can name
/// [`sealed::Sealed`], so nothing outside can implement [`BackingClient`].
mod sealed {
    pub trait Sealed {}
}

/// Abstraction over the MCP client surface the proxy actually depends on:
/// a name, a `tools/list`, and a `tools/call`. `McpClient` is the production
/// impl; the crate's own tests supply an in-process fake.
///
/// Sealed: implementable only inside this crate. Callers *use*
/// [`ProxyServer`] rather than backing it, and sealing means a fourth method
/// here is an addition rather than a break.
///
/// The blanket `impl<T> BackingClient for Arc<T>` lets the proxy work with
/// `Vec<Arc<McpClient>>` directly -- no manual upcast at the call site.
pub trait BackingClient: sealed::Sealed + Send + Sync + 'static {
    /// The local config name of this server (the prefix half of
    /// `<server>__<tool>`).
    fn name(&self) -> &str;

    /// Mirror of `McpClient::list_tools`.
    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>>> + Send;

    /// Mirror of `McpClient::call_tool`.
    fn call_tool(
        &self,
        name: &str,
        args: Value,
    ) -> impl Future<Output = Result<McpToolResult>> + Send;
}

impl sealed::Sealed for McpClient {}

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

impl<T> sealed::Sealed for Arc<T> where T: BackingClient + ?Sized {}

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

/// One entry in the proxy's flattened tool table.
#[derive(Debug, Clone)]
struct ToolEntry {
    /// The tool exactly as `tools/list` advertises it: the public namespaced
    /// name, and the backing server's own title, description, schemas, hints,
    /// icons, and `_meta`. Assembled once in [`ProxyServer::build`] rather
    /// than per request, because the table is frozen for the life of the
    /// proxy -- so a listing is a clone of a finished answer.
    listed: Tool,
    /// The upstream name, which is what a dispatch sends back through the
    /// `client_idx`'th backing client.
    backend_tool: String,
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
/// production uses the default `C = Arc<McpClient>`.
///
/// `Clone` is hand-rolled (no `where C: Clone` bound) so [`ServerHandler`]'s
/// `Self: Clone` requirement is satisfied for any `C`.
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

/// Assign one advertised name per upstream tool, positionally: `None` means
/// the tool is not advertised.
///
/// Names are handed out in a canonical order -- sorted by `(server, tool)`,
/// not the order `tools/list` happened to return -- so the whole map is a
/// function of the *set* of tools rather than of their arrival. Two tools
/// contesting a name would otherwise settle it by position, and a backing
/// server that relisted in a different order would bind that name to the
/// other tool: a client replaying a cached name would reach a different
/// backend and get a plausible answer instead of an error. Listing order is
/// separate and stays the caller's.
///
/// Widening goes through [`tool_name::suffixed`] rather than
/// [`tool_name::sanitize_at`] because a clash is not always a digest
/// collision -- a tool whose own name ends in `_<hex>` can land on a name the
/// suffix produced, and `sanitize_at` returns such a name unchanged at every
/// width, handing back the very name it was asked to move off.
fn assign_public_names<C: BackingClient>(
    clients: &[C],
    upstream: &[(usize, McpTool)],
    hex_len: usize,
) -> Vec<Option<String>> {
    let identity = |i: usize| {
        let (client_idx, tool) = &upstream[i];
        (clients[*client_idx].name(), tool.name.as_str())
    };

    let mut order: Vec<usize> = (0..upstream.len()).collect();
    order.sort_by(|&a, &b| identity(a).cmp(&identity(b)));

    let mut names = vec![None; upstream.len()];
    let mut taken: HashMap<String, usize> = HashMap::with_capacity(upstream.len());
    for i in order {
        let (server, tool) = identity(i);
        let mut name = tool_name::sanitize_at(server, tool, hex_len);

        if let Some(&prev) = taken.get(&name) {
            let (prev_server, prev_tool) = identity(prev);
            let clash = format!(
                "mcp_proxy: advertised name {name:?} is claimed by both \
                 ({prev_server:?}, {prev_tool:?}) and ({server:?}, {tool:?})"
            );
            // The first width nobody holds. The starting width is in the
            // range because the clash may have been with a *faithful* name,
            // in which case the suffixed form at that width is itself free.
            let free = (hex_len..=tool_name::MAX_HASH_HEX_LEN)
                .map(|width| tool_name::suffixed(server, tool, width))
                .find(|candidate| !taken.contains_key(candidate));
            match free {
                Some(renamed) => {
                    tracing::error!(
                        target: "outrig::mcp_proxy",
                        "{clash}; advertising the second as {renamed:?} instead"
                    );
                    name = renamed;
                }
                None => {
                    tracing::error!(
                        target: "outrig::mcp_proxy",
                        "{clash}, and no wider suffix is free; the second is not advertised"
                    );
                    continue;
                }
            }
        }

        taken.insert(name.clone(), i);
        names[i] = Some(name);
    }
    names
}

impl<C: BackingClient> ProxyServer<C> {
    /// Connect every backing client's `tools/list` into a single namespaced
    /// surface. Order across clients matches the input `Vec`; order within a
    /// single client matches that client's `tools/list` response.
    ///
    /// Two `(server, tool)` pairs landing on the same advertised name is not
    /// an error. One of them keeps the name and the other is re-derived at a
    /// wider hash suffix, so both stay reachable and one unlucky pair does
    /// not cost the session every other tool. Which one moves is decided by
    /// the pairs themselves rather than by listing order -- see
    /// `assign_public_names`. Both that and the terminal case -- no width
    /// left, so the tool goes unadvertised -- are logged at ERROR naming both
    /// upstream identities.
    ///
    /// Errors:
    /// - [`OutrigError::Configuration`] if two clients share a `name()`
    ///   (every tool would collide, and no suffix distinguishes them).
    /// - [`OutrigError::Configuration`] if a tool's `input_schema` is not a
    ///   JSON object.
    /// - Any error from a backing client's `list_tools` propagates.
    pub async fn build(clients: Vec<C>) -> Result<Self> {
        Self::build_with_width(clients, tool_name::HASH_HEX_LEN).await
    }

    /// [`Self::build`] with the starting suffix width spelled out, so a test
    /// can drive a digest narrow enough to collide on purpose. Production
    /// passes [`tool_name::HASH_HEX_LEN`].
    pub(crate) async fn build_with_width(clients: Vec<C>, hex_len: usize) -> Result<Self> {
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

        let mut upstream: Vec<(usize, McpTool)> = Vec::new();
        for (client_idx, client) in clients.iter().enumerate() {
            for tool in client.list_tools().await? {
                upstream.push((client_idx, tool));
            }
        }
        let public_names = assign_public_names(&clients, &upstream, hex_len);

        let mut tools: Vec<ToolEntry> = Vec::new();
        let mut by_public_name: HashMap<String, usize> = HashMap::new();
        for ((client_idx, tool), public_name) in upstream.into_iter().zip(public_names) {
            let Some(public_name) = public_name else {
                continue;
            };
            let server_name = clients[client_idx].name();

            let input_schema = match &tool.input_schema {
                Value::Object(map) => Arc::new(map.clone()),
                other => {
                    return Err(OutrigError::Configuration(format!(
                        "mcp_proxy: tool {server_name:?}::{tool_name:?} input_schema is \
                         not a JSON object (got {kind})",
                        tool_name = tool.name,
                        kind = mcp::kind_of(other),
                    )));
                }
            };

            let listed = tool_to_rmcp(public_name.clone(), &tool, input_schema);
            by_public_name.insert(public_name, tools.len());
            tools.push(ToolEntry {
                listed,
                backend_tool: tool.name,
                client_idx,
            });
        }

        let server_info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("outrig", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Tools are namespaced as <server>__<tool>; the prefix identifies which \
                 backing MCP server hosts the tool.",
            );

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
    /// order they were registered. `0001-40` consumes this for the startup
    /// banner.
    pub fn iter_public_names(&self) -> impl Iterator<Item = &str> {
        self.inner.tools.iter().map(|t| t.listed.name.as_ref())
    }

    /// Per-backing-client tool counts, in client registration order.
    /// Used by the `outrig mcp` startup banner to print one
    /// `[outrig] mcp <name>: initialized (<n> tools)` line per server.
    pub fn per_server_counts(&self) -> Vec<(&str, usize)> {
        let mut counts = vec![0usize; self.inner.clients.len()];
        for entry in &self.inner.tools {
            counts[entry.client_idx] += 1;
        }
        self.inner
            .clients
            .iter()
            .zip(counts)
            .map(|(c, n)| (c.name(), n))
            .collect()
    }

    /// Build a `tools/list` response: every backing server's tools, in
    /// registration order, namespaced through [`crate::sanitize_tool_name`]. Public
    /// (rather than living inline in [`ServerHandler::list_tools`]) so the
    /// listing can be read without fabricating an rmcp [`RequestContext`] --
    /// what a caller driving the proxy outside an rmcp server needs, and what
    /// the crate's own dispatch tests use.
    ///
    /// Only the name is outrig's; title, description, both schemas, hints,
    /// icons, and `_meta` are the upstream server's, unchanged.
    pub fn list_tools_inner(&self) -> ListToolsResult {
        let tools = self
            .inner
            .tools
            .iter()
            .map(|entry| entry.listed.clone())
            .collect();
        // `Private`: the union depends on this session's config, image, and `--env`
        // overrides, so it is not shareable across users or intermediaries.
        ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TOOLS_TTL_MS)
            .with_cache_scope(CacheScope::Private)
    }

    /// Dispatch a `tools/call` to the appropriate backing client. Returns a
    /// [`CallToolResult`] in every case -- unknown tool names and
    /// backing-client errors surface as `is_error: Some(true)` results, not
    /// rmcp protocol errors. Public for the same reason as
    /// [`Self::list_tools_inner`]: it is the [`RequestContext`]-free half of
    /// the dispatch path.
    ///
    /// A result the backing server produced is forwarded whole -- every block
    /// in order, plus structured content and `_meta`. Only the two failures
    /// outrig itself reports are synthesized here, and both are text.
    pub async fn dispatch_call(&self, request: CallToolRequestParams) -> CallToolResult {
        let public_name = request.name.as_ref();
        let Some(&idx) = self.inner.by_public_name.get(public_name) else {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool: {public_name}"
            ))]);
        };
        let entry = &self.inner.tools[idx];
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        let client = &self.inner.clients[entry.client_idx];
        match client.call_tool(&entry.backend_tool, args).await {
            Ok(result) => result_to_rmcp(result),
            Err(e) => {
                let server = client.name();
                tracing::warn!(
                    target: "outrig::mcp_proxy",
                    "backing server {server:?} call to {tool:?} failed: {e}",
                    tool = entry.backend_tool,
                );
                CallToolResult::error(vec![ContentBlock::text(format!(
                    "outrig: backing server `{server}` call failed: {e}"
                ))])
            }
        }
    }
}

impl<C: BackingClient> ServerHandler for ProxyServer<C> {
    fn get_info(&self) -> ServerInfo {
        self.inner.server_info.clone()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(self.list_tools_inner())
    }

    // rmcp answers these from default handler bodies with an empty, successful
    // result -- advertised capabilities do not gate dispatch. That is wrong twice
    // over: it claims a surface the proxy does not have, and from revision `2026-07-28`
    // the default result is malformed, carrying `resultType` but neither `ttlMs`
    // nor `cacheScope`. It advertises tools only, so say so.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        Err(McpError::method_not_found::<ListResourcesRequestMethod>())
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, McpError> {
        Err(McpError::method_not_found::<
            ListResourceTemplatesRequestMethod,
        >())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<ListPromptsResult, McpError> {
        Err(McpError::method_not_found::<ListPromptsRequestMethod>())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        Ok(self.dispatch_call(request).await.into())
    }
}

#[cfg(test)]
#[path = "mcp_proxy_dispatch_tests.rs"]
mod mcp_proxy_dispatch_tests;
