//! Tests for [`ProxyServer`] driven against an in-process [`BackingClient`]
//! fake. Exercises the namespace + dispatch contract without spinning up real
//! MCP children. In-crate rather than under `tests/` because `BackingClient`
//! is sealed, so only this crate can supply the fake.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use rmcp::ServerHandler;
use rmcp::model::{CacheScope, CallToolRequestParams, ProtocolVersion};
use serde_json::{Value, json};

use super::{BackingClient, ProxyServer, SUPPORTED_PROTOCOL_VERSIONS, TOOLS_TTL_MS, sealed};
use crate::error::OutrigError;
use crate::mcp_content::mcp_content_tests::{MIXED_RENDERING, rmcp_mixed_result, rmcp_rich_tool};
use crate::mcp_content::{McpTool, McpToolResult, result_from_rmcp, tool_from_rmcp};
use crate::process::process_tests::CaptureWriter;

/// Per-tool canned response. `Ok` becomes a successful `CallToolResult`;
/// `Err` becomes the "backing client failed" path that surfaces as
/// `CallToolResult { is_error: Some(true), ... }` carrying the error text.
type CallResponse = Result<McpToolResult, OutrigError>;

#[derive(Default)]
struct FakeClient {
    name: String,
    tools: Vec<McpTool>,
    /// Recorded `(tool_name, args)` for every `call_tool` invocation.
    received: Mutex<Vec<(String, Value)>>,
    /// Canned response per backend tool name.
    responses: HashMap<String, CallResponse>,
}

impl FakeClient {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    fn with_tool(mut self, tool_name: &str) -> Self {
        let mut tool = McpTool::new(tool_name, json!({"type": "object"}));
        tool.description = Some(format!("desc for {tool_name}"));
        self.tools.push(tool);
        self
    }

    fn respond_ok(mut self, tool_name: &str, body: &str, is_error: bool) -> Self {
        self.responses.insert(
            tool_name.to_string(),
            Ok(if is_error {
                McpToolResult::error(body)
            } else {
                McpToolResult::ok(body)
            }),
        );
        self
    }

    /// Register a tool whose descriptor is more than a name and a blurb.
    fn with_mcp_tool(mut self, tool: McpTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Register a canned response that is not reducible to one text block.
    fn respond_with(mut self, tool_name: &str, result: McpToolResult) -> Self {
        self.responses.insert(tool_name.to_string(), Ok(result));
        self
    }

    fn respond_err(mut self, tool_name: &str, msg: &str) -> Self {
        self.responses.insert(
            tool_name.to_string(),
            Err(OutrigError::Configuration(msg.to_string())),
        );
        self
    }
}

impl sealed::Sealed for FakeClient {}

impl BackingClient for FakeClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list_tools(&self) -> crate::error::Result<Vec<McpTool>> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&self, name: &str, args: Value) -> crate::error::Result<McpToolResult> {
        self.received.lock().unwrap().push((name.to_string(), args));
        match self.responses.get(name) {
            Some(Ok(r)) => Ok(r.clone()),
            Some(Err(e)) => Err(OutrigError::Configuration(e.to_string())),
            None => Err(OutrigError::Configuration(format!(
                "FakeClient({}) has no response for {name:?}",
                self.name
            ))),
        }
    }
}

/// Every block rendered the way [`McpToolResult::render_text`] renders it,
/// recovered from the wire form -- what a client that only knows how to read
/// text sees.
fn text_body(result: &rmcp::model::CallToolResult) -> String {
    result_from_rmcp(result.clone()).render_text()
}

fn call(name: &str, args: Value) -> CallToolRequestParams {
    let arguments = match args {
        Value::Object(m) => Some(m),
        Value::Null => None,
        other => panic!("test args must be object or null, got {other:?}"),
    };
    let mut request = CallToolRequestParams::new(name.to_string());
    if let Some(arguments) = arguments {
        request = request.with_arguments(arguments);
    }
    request
}

#[tokio::test]
async fn list_tools_unions_namespaces_and_preserves_order() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let git = Arc::new(FakeClient::new("git").with_tool("commit"));

    let proxy = ProxyServer::build(vec![fs, git]).await.unwrap();

    let listing = proxy.list_tools_inner();
    let names: Vec<&str> = listing.tools.iter().map(|t| t.name.as_ref()).collect();

    // Order: clients in input order, tools in each client's `list_tools` order.
    assert_eq!(
        names,
        vec!["fs__read_file", "fs__write_file", "git__commit"]
    );

    // Description and schema pass through unchanged.
    let read_file = &listing.tools[0];
    assert_eq!(read_file.description.as_deref(), Some("desc for read_file"));
    assert_eq!(
        Value::Object(read_file.input_schema.as_ref().clone()),
        json!({"type": "object"})
    );
}

#[tokio::test]
async fn list_tools_carries_sep_2549_cache_metadata() {
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file"));
    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    // Required fields as of protocol revision 2026-07-28; omitting them makes a
    // conforming client reject `tools/list` outright.
    let listing = proxy.list_tools_inner();
    assert_eq!(listing.ttl_ms, Some(TOOLS_TTL_MS));
    // `Private`: the union is specific to this session's config and overrides.
    assert_eq!(listing.cache_scope, Some(CacheScope::Private));
}

#[tokio::test]
async fn advertises_the_audited_protocol_versions() {
    // Pinned rather than inherited from `ProtocolVersion::KNOWN_VERSIONS`, so an
    // rmcp upgrade cannot widen what this server agrees to speak without review.
    let proxy = ProxyServer::build(vec![Arc::new(FakeClient::new("fs"))])
        .await
        .unwrap();
    assert_eq!(
        proxy.supported_protocol_versions().as_ref(),
        SUPPORTED_PROTOCOL_VERSIONS,
    );
    assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&ProtocolVersion::V_2026_07_28));
}

#[tokio::test]
async fn call_tool_routes_to_correct_backend() {
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file").respond_ok(
        "read_file",
        "fs payload",
        false,
    ));
    let git = Arc::new(FakeClient::new("git").with_tool("commit").respond_ok(
        "commit",
        "git payload",
        false,
    ));
    let fs_for_assert = fs.clone();
    let git_for_assert = git.clone();

    let proxy = ProxyServer::build(vec![fs, git]).await.unwrap();

    let result = proxy
        .dispatch_call(call("git__commit", json!({"msg": "hi"})))
        .await;
    assert_eq!(result.is_error, Some(false));
    assert_eq!(text_body(&result), "git payload");

    // The backend tool name (not the public one) is what hits the client,
    // and the args object passes through untouched.
    let git_calls = git_for_assert.received.lock().unwrap();
    assert_eq!(git_calls.len(), 1);
    assert_eq!(git_calls[0].0, "commit");
    assert_eq!(git_calls[0].1, json!({"msg": "hi"}));

    // `fs` was not invoked.
    assert!(fs_for_assert.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn null_arguments_become_value_null() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("ping")
            .respond_ok("ping", "pong", false),
    );
    let fs_for_assert = fs.clone();

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    // `arguments: None` on the request becomes `Value::Null` on the wire.
    let req = CallToolRequestParams::new("fs__ping".to_string());
    let result = proxy.dispatch_call(req).await;
    assert_eq!(result.is_error, Some(false));

    let calls = fs_for_assert.received.lock().unwrap();
    assert_eq!(calls[0].1, Value::Null);
}

#[tokio::test]
async fn backend_error_surfaces_as_call_tool_result_is_error() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .respond_err("read_file", "permission denied"),
    );

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__read_file", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    let body = text_body(&result);
    // Body identifies the backing server by name and carries the upstream
    // error verbatim. The prefix lets an MCP client distinguish a proxy
    // failure from a backend's own `is_error=true` response.
    assert!(
        body.contains("outrig: backing server `fs`"),
        "body should identify the backing server, was {body:?}"
    );
    assert!(
        body.contains("permission denied"),
        "body should carry the backend error, was {body:?}"
    );
}

#[tokio::test]
async fn backend_semantic_error_propagates_unchanged() {
    // Backing client returns Ok(...) but with `is_error: true` -- this is the
    // "tool ran but reported failure" path (e.g. `read_file` on a missing
    // path). The flag and body must reach the caller verbatim.
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file").respond_ok(
        "read_file",
        "ENOENT: missing",
        true,
    ));

    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__read_file", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(text_body(&result), "ENOENT: missing");
}

#[tokio::test]
async fn unknown_tool_returns_is_error_not_protocol_error() {
    let fs = Arc::new(FakeClient::new("fs").with_tool("read_file"));
    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let result = proxy.dispatch_call(call("fs__nope", json!({}))).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_body(&result).contains("unknown tool"));
}

fn build_err(result: Result<ProxyServer<Arc<FakeClient>>, OutrigError>) -> OutrigError {
    match result {
        Ok(_) => panic!("expected ProxyServer::build to fail"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn duplicate_client_name_is_rejected() {
    let a = Arc::new(FakeClient::new("fs").with_tool("read_file"));
    let b = Arc::new(FakeClient::new("fs").with_tool("write_file"));

    let err = build_err(ProxyServer::build(vec![a, b]).await);
    let msg = err.to_string();
    assert!(
        msg.contains("duplicate backing-client name") && msg.contains("\"fs\""),
        "error was {msg:?}"
    );
}

#[tokio::test]
async fn lossily_and_faithfully_named_tools_coexist() {
    // These two used to abort the whole proxy: ("fs/", "bar") and
    // ("fs", "_bar") both collapsed to `fs___bar`, so one server's bad tool
    // name cost the session every other server's tools too. Now only the
    // first is lossy -- its `/` is replaced -- so only it carries a suffix,
    // and both are advertised.
    let a = Arc::new(FakeClient::new("fs/").with_tool("bar").respond_ok(
        "bar",
        "from the slashed server",
        false,
    ));
    let b = Arc::new(FakeClient::new("fs").with_tool("_bar").respond_ok(
        "_bar",
        "from the plain server",
        false,
    ));

    let proxy = ProxyServer::build(vec![a, b]).await.expect("build");
    let names: Vec<&str> = proxy.iter_public_names().collect();
    assert_eq!(names.len(), 2, "both tools must be advertised: {names:?}");
    assert!(names[0].starts_with("fs___bar_"), "got {names:?}");
    assert_eq!(names[1], "fs___bar");

    assert_eq!(
        text_body(&proxy.dispatch_call(call(names[0], json!({}))).await),
        "from the slashed server"
    );
    assert_eq!(
        text_body(&proxy.dispatch_call(call("fs___bar", json!({}))).await),
        "from the plain server"
    );
}

/// Two distinct lossy tool names on `server` whose width-1 advertised names
/// agree. Every candidate is `x<c>y` for a character the sanitizer replaces,
/// so they all share the body `x_y` and only the digest tells them apart;
/// 29 such characters against 16 buckets makes a collision certain. Found
/// rather than pinned, so the search survives a change to the digest.
fn colliding_at_width_one(server: &str) -> (String, String) {
    let mut seen: HashMap<String, String> = HashMap::new();
    for c in ' '..='~' {
        // `"` and `\` are left out only because the diagnostic renders tool
        // names with `{:?}`, and a test asserting on it compares raw strings.
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '"' | '\\') {
            continue;
        }
        let tool = format!("x{c}y");
        let name = crate::tool_name::sanitize_at(server, &tool, 1);
        if let Some(prev) = seen.insert(name, tool.clone()) {
            return (prev, tool);
        }
    }
    panic!("no width-1 digest collision within the scan");
}

/// Every advertised name, and the body each one dispatches to.
async fn advertised(proxy: &ProxyServer<Arc<FakeClient>>) -> (Vec<String>, Vec<String>) {
    let names: Vec<String> = proxy.iter_public_names().map(str::to_string).collect();
    let mut bodies = Vec::new();
    for name in &names {
        bodies.push(text_body(&proxy.dispatch_call(call(name, json!({}))).await));
    }
    (names, bodies)
}

/// Run `body` with `tracing` captured, returning what it emitted. The
/// subscriber is thread-local, so the runtime is current-thread and built
/// here rather than by `#[tokio::test]` -- the same shape `process_tests`
/// uses.
fn with_captured_tracing<T>(body: impl Future<Output = T>) -> (T, String) {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(buf.clone()))
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");
    let out = rt.block_on(body);
    let captured =
        String::from_utf8(buf.lock().unwrap().clone()).expect("captured output must be UTF-8");
    (out, captured)
}

#[test]
fn residual_digest_collision_widens_the_loser() {
    // 24 bits will not collide by accident, so the ladder is driven with a
    // 4-bit digest instead. The first tool keeps the narrow name; the second
    // is re-derived at 16 hex, and both stay reachable.
    let (first, second) = colliding_at_width_one("fs");
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool(&first)
            .with_tool(&second)
            .respond_ok(&first, "from the first", false)
            .respond_ok(&second, "from the second", false),
    );

    let ((names, bodies), captured) = with_captured_tracing(async move {
        let proxy = ProxyServer::build_with_width(vec![fs], 1)
            .await
            .expect("a residual collision must not fail the build");
        advertised(&proxy).await
    });

    assert_eq!(names.len(), 2, "nothing may be dropped: {names:?}");
    assert_ne!(names[0], names[1], "the loser must be re-derived");
    assert_eq!(bodies, ["from the first", "from the second"]);

    // A collision report that names one tool tells an operator nothing.
    assert!(
        captured.contains(&first) && captured.contains(&second) && captured.contains(&names[1]),
        "diagnostic must name both tools and the new name: {captured}"
    );
}

#[test]
fn a_faithful_name_can_claim_a_suffixed_one_and_is_still_widened() {
    // No injected digest and no collision: `read/file` is lossy, so it is
    // advertised as `fs__read_file_<hex>`, and a server that also exposes a
    // tool literally called `read_file_<hex>` composes that same name
    // faithfully. Widening has to force a suffix onto the faithful loser --
    // re-deriving it as an ordinary name returns the name it must move off,
    // and the tool is lost.
    let lossy = crate::tool_name::sanitize("fs", "read/file");
    let twin = lossy
        .strip_prefix("fs__")
        .expect("the lossy name keeps its server prefix")
        .to_string();
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read/file")
            .with_tool(&twin)
            .respond_ok("read/file", "from the lossy tool", false)
            .respond_ok(&twin, "from its twin", false),
    );

    let ((names, bodies), captured) = with_captured_tracing(async move {
        let proxy = ProxyServer::build(vec![fs])
            .await
            .expect("a clash must not fail the build");
        advertised(&proxy).await
    });

    assert_eq!(names.len(), 2, "the twin must keep a place: {names:?}");
    assert_eq!(names[0], lossy, "the lossy tool keeps the contested name");
    assert_ne!(names[1], lossy, "the twin must be moved off it");
    assert_eq!(bodies, ["from the lossy tool", "from its twin"]);
    assert!(
        captured.contains("read/file") && captured.contains(&twin),
        "diagnostic must name both tools: {captured}"
    );
}

#[test]
fn a_contested_name_is_awarded_by_identity_not_by_arrival() {
    // `tools/list` promises no order, and a backing server that restarts may
    // relist in a different one. If the contested name went to whichever tool
    // arrived first, a rebuilt proxy would bind a name a client had cached to
    // the *other* tool -- which then answers plausibly rather than failing,
    // the worst way for this to go wrong.
    let lossy = crate::tool_name::sanitize("fs", "read/file");
    let twin = lossy
        .strip_prefix("fs__")
        .expect("the lossy name keeps its server prefix")
        .to_string();

    let mapping = |order: [&str; 2]| {
        let fs = Arc::new(
            FakeClient::new("fs")
                .with_tool(order[0])
                .with_tool(order[1])
                .respond_ok("read/file", "from the lossy tool", false)
                .respond_ok(&twin, "from its twin", false),
        );
        let ((names, bodies), _) = with_captured_tracing(async move {
            let proxy = ProxyServer::build(vec![fs]).await.expect("build");
            advertised(&proxy).await
        });
        let mut pairs: Vec<(String, String)> = names.into_iter().zip(bodies).collect();
        pairs.sort();
        pairs
    };

    let forward = mapping(["read/file", &twin]);
    let reverse = mapping([&twin, "read/file"]);
    assert_eq!(
        forward.len(),
        2,
        "both tools must be advertised: {forward:?}"
    );
    assert_eq!(
        forward, reverse,
        "which tool answers to a name must not depend on listing order"
    );
}

#[test]
fn a_tool_no_width_can_separate_goes_unadvertised() {
    // The terminal case. A pair hashes the same at every width, so an
    // upstream `tools/list` naming one lossy tool twice cannot be widened
    // apart; started at the widest suffix there is nothing else to try
    // either. The first registrant keeps the name and the second is dropped,
    // rather than two tools sharing one name.
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("dup/1")
            .with_tool("dup/1")
            .respond_ok("dup/1", "the one backend", false),
    );

    let ((names, bodies), captured) = with_captured_tracing(async move {
        let proxy = ProxyServer::build_with_width(vec![fs], crate::tool_name::MAX_HASH_HEX_LEN)
            .await
            .expect("an unadvertised tool must not fail the build");
        advertised(&proxy).await
    });

    assert_eq!(names.len(), 1, "exactly one must survive: {names:?}");
    assert_eq!(
        bodies,
        ["the one backend"],
        "the surviving name must route to the first registrant"
    );
    assert_eq!(
        captured.matches("\"dup/1\"").count(),
        2,
        "diagnostic must name both identities: {captured}"
    );
    assert!(
        captured.contains("not advertised"),
        "diagnostic must say the tool was dropped: {captured}"
    );
}

#[tokio::test]
async fn iter_public_names_matches_list_tools() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let proxy = ProxyServer::build(vec![fs]).await.unwrap();

    let from_iter: Vec<&str> = proxy.iter_public_names().collect();
    let from_list: Vec<String> = proxy
        .list_tools_inner()
        .tools
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();

    assert_eq!(from_iter, vec!["fs__read_file", "fs__write_file"]);
    assert_eq!(from_iter, from_list);
}

#[tokio::test]
async fn per_server_counts_preserves_registration_order() {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_tool("read_file")
            .with_tool("write_file"),
    );
    let git = Arc::new(FakeClient::new("git").with_tool("commit"));
    let empty = Arc::new(FakeClient::new("empty"));

    let proxy = ProxyServer::build(vec![fs, git, empty]).await.unwrap();

    assert_eq!(
        proxy.per_server_counts(),
        vec![("fs", 2), ("git", 1), ("empty", 0)]
    );
}

/// A proxy fronting one server whose single tool returns [`rmcp_mixed_result`].
async fn mixed_proxy() -> ProxyServer<Arc<FakeClient>> {
    let fs = Arc::new(
        FakeClient::new("fs")
            .with_mcp_tool(tool_from_rmcp(rmcp_rich_tool()))
            .respond_with("read_file", result_from_rmcp(rmcp_mixed_result())),
    );
    ProxyServer::build(vec![fs]).await.expect("build proxy")
}

#[tokio::test]
async fn list_tools_readvertises_the_upstream_descriptor() {
    let proxy = mixed_proxy().await;

    let listed = proxy.list_tools_inner().tools;

    assert_eq!(listed.len(), 1);
    let tool = &listed[0];
    // The name is outrig's -- it is the namespaced one. Everything else is
    // the backing server's, unchanged.
    assert_eq!(tool.name, "fs__read_file");
    assert_eq!(tool.title.as_deref(), Some("Read File"));
    assert_eq!(tool.description.as_deref(), Some("read one file"));
    assert!(tool.output_schema.is_some(), "output schema must survive");
    let annotations = tool.annotations.as_ref().expect("tool annotations");
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(tool.icons.as_ref().expect("icons").len(), 1);
    assert!(tool.meta.is_some(), "tool `_meta` must survive");
}

#[tokio::test]
async fn dispatch_call_forwards_every_block_intact() {
    let proxy = mixed_proxy().await;

    let out = proxy.dispatch_call(call("fs__read_file", json!({}))).await;

    assert_eq!(
        serde_json::to_value(&out).unwrap(),
        serde_json::to_value(rmcp_mixed_result()).unwrap(),
        "what the backing server returned is what the proxy emits"
    );
    // And the reduced view a model would see still reads the way it did.
    assert_eq!(text_body(&out), MIXED_RENDERING);
}

/// The whole path, spoken rather than inspected: a real rmcp client on one
/// end of an in-memory pipe, [`ProxyServer`] on the other. Nothing here
/// reaches inside the proxy, so a result that only survives because both
/// sides share outrig's types would not pass.
#[tokio::test]
async fn a_client_sees_the_mixed_result_through_the_proxy() {
    let proxy = mixed_proxy().await;
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let running = rmcp::service::serve_server(proxy, server_io)
            .await
            .expect("serve the proxy");
        running.waiting().await
    });
    let client = rmcp::service::serve_client((), client_io)
        .await
        .expect("connect to the proxy");

    let listed = client.list_all_tools().await.expect("tools/list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "fs__read_file");
    assert_eq!(listed[0].title.as_deref(), Some("Read File"));
    assert!(listed[0].output_schema.is_some());

    let result = client
        .call_tool(call("fs__read_file", json!({})))
        .await
        .expect("tools/call");

    // Compared field by field rather than whole: the SDK strips `resultType`
    // from the envelope when the peer negotiated a revision that predates it,
    // which is its business and not outrig's.
    let expected = rmcp_mixed_result();
    assert_eq!(
        serde_json::to_value(&result.content).unwrap(),
        serde_json::to_value(&expected.content).unwrap(),
        "the blocks reach a client that speaks the protocol, not just the fake"
    );
    assert_eq!(result.structured_content, expected.structured_content);
    assert_eq!(
        result.meta.map(|m| m.0),
        expected.meta.map(|m| m.0),
        "result-level `_meta` survives the whole path"
    );
    assert_eq!(result.is_error, Some(false));

    client.cancel().await.expect("client shutdown");
    let _ = server.await;
}
